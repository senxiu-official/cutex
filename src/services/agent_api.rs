//! Agent-facing services operations, served by the Agent Bus at
//! `POST /api/services/v1` after it authenticated the MCP caller. The caller's
//! durable session is the owner; its current project is the project.
//!
//! Body: `{"op": "register"|"update"|"restart"|"unregister"|"list", "args": {...}}` with
//! camelCase args (the MCP adapter translates). Always answers JSON
//! `{ok: true, ...}` or `{ok: false, code, detail}`.
//!
//! [`AgentServicePolicy`] adds the project boundary (an agent lists only
//! services it may interact with, and sees another agent's command, cwd, env
//! keys and process only for its own services) and the managed-service
//! approval gate (a supervised command outside the caller's own permission
//! envelope waits for the owner; `approval_required` / `approval_denied`).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use super::health::RuntimeSnapshot;
use super::store::AgentPersistentApproval;
use super::store::list_views;
use super::store::may_manage;
use super::store::ServiceOwnerScope;
use super::store::service_view;
use super::store::Caller;
use super::store::ListFilter;
use super::store::RegisterRequest;
use super::store::ServiceCommand;
use super::store::ServiceRecord;
use super::store::ServiceRuntime;
use super::store::ServiceStore;
use super::store::ServicesError;
use super::store::UpdateRequest;
use super::static_files::CheckedRoot;
use crate::agent_bus::boundary::AgentBoundary;
use crate::agent_management::envelope::PermissionEnvelope;
use crate::approvals::request_digest;
use crate::approvals::ApprovalKind;
use crate::approvals::ApprovalRequest;
use crate::approvals::ApprovalStore;
use crate::approvals::GateOutcome;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    op: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SlugArgs {
    slug: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    mine: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateArgs {
    slug: String,
    #[serde(flatten)]
    patch: Value,
}

fn rejected(error: ServicesError) -> Value {
    json!({"ok": false, "code": error.code, "detail": error.message})
}

fn snapshots(store: &ServiceStore) -> BTreeMap<String, RuntimeSnapshot> {
    match super::supervisor::global() {
        Some(supervisor) => supervisor.snapshots(),
        None => store.load_state(),
    }
}

fn after_change() {
    if let Some(supervisor) = super::supervisor::global() {
        supervisor.sync();
    }
}

/// Bus-side policy for one agent caller beyond ownership (see
/// `docs/agent-tool-security.md`): which services it may see, and whether a
/// managed (supervised) command it registers needs the owner's approval.
pub struct AgentServicePolicy {
    /// `None`: host-wide visibility (`agentSecurity.crossProjectAccess = host`
    /// or a permissive test). Otherwise the caller sees its own services,
    /// services of agents it may see (its domain, linked domains, external
    /// agents granted to its project), its own and linked projects'
    /// services, and the owner's unassigned host services.
    pub boundary: Option<AgentBoundary>,
    /// The caller itself runs unsandboxed with approval `never`, so a managed
    /// command is within its own envelope and needs no approval.
    pub managed_within_envelope: bool,
    /// Where approvals are recorded; `None` refuses managed commands that
    /// need an approval (fail closed).
    pub approvals: Option<ApprovalStore>,
    pub requester_name: Option<String>,
    /// The call may hold for the owner's decision (approval-wait); `None`
    /// answers `approval_required` at once (older façades / adapters).
    pub waiter: Option<crate::approvals::Waiter>,
    /// The caller is the current Director of exactly one project (its
    /// `Caller::Agent::project_id`): a new service it registers without
    /// `scope` becomes a project service (like `agent_delegate`'s
    /// `ownershipScope`).
    pub director_default_project: bool,
    /// The caller directs several projects and is no member of one: an
    /// explicit `scope: project` cannot name one (`director_project_ambiguous`).
    pub project_ambiguous: bool,
    /// Production writes refresh authority after approval and hold the roster
    /// mutation fence through the registry commit. Internal adapters supply
    /// their own trusted caller context instead.
    pub revalidate_authority: bool,
}

impl AgentServicePolicy {
    /// No visibility limit and no approvals (tests, legacy behaviour).
    pub fn permissive() -> Self {
        Self {
            boundary: None,
            managed_within_envelope: true,
            approvals: None,
            requester_name: None,
            waiter: None,
            director_default_project: false,
            project_ambiguous: false,
            revalidate_authority: false,
        }
    }

    /// Policy for an authenticated agent on this host. Fails closed: an
    /// unknown envelope requires approval, an unloadable boundary hides
    /// everything the caller does not own.
    pub fn for_caller(cutex_session_id: &str) -> Self {
        let boundary = match AgentBoundary::load() {
            Ok(boundary) if boundary.host_wide() => None,
            Ok(boundary) => Some(boundary),
            Err(_) => Some(AgentBoundary::default()),
        };
        let record = crate::session::store::load_cutex_session_store()
            .ok()
            .and_then(|store| store.sessions.get(cutex_session_id).cloned());
        let managed_within_envelope = record.as_ref().is_some_and(|record| {
            managed_within_envelope(
                record,
                crate::app_server::live_settings::current_for(record).as_ref(),
            )
        });
        let (director_default_project, project_ambiguous) =
            crate::memory::load_agent_management_snapshot()
                .map(|snapshot| {
                    let projects = crate::asks::directory::Directory::director_projects(
                        &snapshot,
                        cutex_session_id,
                    );
                    let directs = |project: &str| {
                        snapshot.projects.values().any(|authority| {
                            authority.project_id.as_str() == project
                                && authority.authorized_director_session.as_str()
                                    == cutex_session_id
                        })
                    };
                    (projects.len() == 1 && directs(&projects[0]), projects.len() > 1)
                })
                .unwrap_or((false, false));
        Self {
            boundary,
            managed_within_envelope,
            approvals: ApprovalStore::open_default().ok(),
            requester_name: record.and_then(|record| record.formal_agent_name),
            waiter: None,
            director_default_project,
            project_ambiguous,
            revalidate_authority: true,
        }
    }

    fn visible(&self, caller: &str, record: &ServiceRecord) -> bool {
        let Some(boundary) = &self.boundary else {
            return true;
        };
        match (&record.owner_cutex_session_id, &record.project_id) {
            (Some(owner), _) if boundary.may_see(caller, owner) => true,
            (_, Some(project)) => boundary.visible_projects(caller).contains(project),
            (None, None) => true,
            (Some(_), None) => false,
        }
    }
}

/// Whether a managed command is within the caller's own envelope (it runs
/// with full access: unsandboxed, approval `never`). Q21: judged like
/// `browser_open` (`browser_relay::owner::full_access`) from the live native
/// thread settings of its current runtime, so switching a running agent to
/// full access (完全访问) takes effect at once; without live settings, from
/// the record's stored launch permissions as before. Unknown is not full.
pub fn managed_within_envelope(
    record: &crate::session::model::CutexSessionRecord,
    live_settings: Option<&Value>,
) -> bool {
    match live_settings {
        Some(settings) => crate::browser_relay::owner::full_access(record, Some(settings)),
        None => PermissionEnvelope::from_record(record)
            .is_ok_and(|envelope| envelope.runs_unsandboxed_without_approval()),
    }
}

/// Fields that reveal how another agent's process runs (argv may carry
/// secrets); only the owning agent sees them.
const PRIVATE_VIEW_FIELDS: [&str; 6] = [
    "command",
    "cwd",
    "envKeys",
    "process",
    "root",
    "loginLinkCommand",
];
const OWNER_ORGANISATION_FIELDS: [&str; 6] = [
    "groupName",
    "starred",
    "sortOrder",
    "note",
    "hidden",
    "archivedAt",
];

/// The agent view of a record: owner organisation removed, and how it runs
/// shown only to agents that may manage it (its registrant; for a project
/// service also the project's current Director).
fn redact_for(caller: &Caller, record: &ServiceRecord, mut view: Value) -> Value {
    if let Some(object) = view.as_object_mut() {
        // Agent callers never receive the owner's login helper, including
        // the current Director and the original registrant.
        object.remove("loginLinkCommand");
        for field in OWNER_ORGANISATION_FIELDS {
            object.remove(field);
        }
    }
    if !may_manage(record, caller) {
        if let Some(object) = view.as_object_mut() {
            for field in PRIVATE_VIEW_FIELDS {
                object.remove(field);
            }
        }
    }
    view
}

/// Ask the owner for exactly this request (holding the call when the policy
/// has a waiter, approval-wait).
fn gate(
    policy: &AgentServicePolicy,
    store: &ApprovalStore,
    request: ApprovalRequest,
) -> anyhow::Result<GateOutcome> {
    crate::approvals::gate_with_wait(
        store,
        request,
        policy.waiter.as_ref(),
        |record| crate::management::v2::approvals::publish_changed(record, "requested"),
        crate::management::v2::approvals::queue_decision_notice,
    )
}

/// `Ok(note)` lets the request proceed (`note`: the owner approved it while
/// this call held, [`crate::approvals::ApprovalRecord::approved_after_wait`]);
/// otherwise the tool's refusal.
type GateResult = Result<Option<Value>, Value>;

fn gate_result(outcome: anyhow::Result<GateOutcome>) -> GateResult {
    match outcome {
        Ok(outcome @ GateOutcome::Approved(_)) => Ok(outcome.approved_after_wait()),
        Ok(outcome) => Err(outcome.refusal().unwrap_or_default()),
        Err(error) => Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            error.to_string(),
        ))),
    }
}

/// The approval gate for a request that would run (or change) a managed
/// command. `Ok(())` lets it proceed.
fn managed_gate(
    policy: &AgentServicePolicy,
    caller: &Caller,
    op: &str,
    slug: &str,
    args: &Value,
    command: Option<&ServiceCommand>,
) -> GateResult {
    if policy.managed_within_envelope {
        return Ok(None);
    }
    let Caller::Agent {
        cutex_session_id,
        project_id,
    } = caller
    else {
        return Ok(None);
    };
    let Some(store) = &policy.approvals else {
        return Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            "owner approval state is unavailable; nothing was changed",
        )));
    };
    let command_text = command
        .map(|command| match command {
            ServiceCommand::Shell(line) => line.clone(),
            ServiceCommand::Argv(argv) => argv.join(" "),
        })
        .unwrap_or_else(|| "(unchanged command)".into());
    // The card states what is granted (asks §4.4): unrestricted execution as
    // the owner, not "this command looks fine".
    let summary: String =
        format!("{op} managed service {slug} (runs unrestricted as the owner): {command_text}")
            .chars()
            .take(300)
            .collect();
    let env_keys: Vec<&String> = args
        .get("env")
        .and_then(Value::as_object)
        .map(|env| env.keys().collect())
        .unwrap_or_default();
    let outcome = gate(
        policy,
        store,
        ApprovalRequest {
            kind: ApprovalKind::ManagedService,
            reason: "outside_caller_envelope".into(),
            requester_cutex_session_id: cutex_session_id.clone(),
            requester_name: policy.requester_name.clone(),
            project_id: project_id.clone(),
            summary,
            // Env values may be secrets: the record shows only their keys;
            // the digest still binds the approval to the exact values.
            details: json!({
                "op": op,
                "slug": slug,
                "runtime": args.get("runtime"),
                "command": args.get("command"),
                "cwd": args.get("cwd"),
                "port": args.get("port"),
                "envKeys": env_keys,
                "ceiling": "unrestricted",
                "grants": "unrestricted execution as the owner (cutex runs managed services unsandboxed); restarts of this exact command, cwd and env are covered, any change needs a new approval",
                "warning": "a script or file the command runs can change later without a new approval",
            }),
            request_sha256: request_digest(&json!({"op": op, "slug": slug, "args": args})),
        },
    );
    gate_result(outcome)
}

pub(crate) fn lifetime_approval_request(
    policy: &AgentServicePolicy,
    caller: &Caller,
    op: &str,
    slug: &str,
    args: &Value,
    managed: bool,
    root: Option<&CheckedRoot>,
) -> Option<ApprovalRequest> {
    let Caller::Agent {
        cutex_session_id,
        project_id,
    } = caller
    else {
        return None;
    };
    // One exact-payload approval covers both grants, so consuming one approval
    // cannot strand a request behind a second gate and cause an approval loop.
    let execution = managed && !policy.managed_within_envelope;
    let mut details = json!({"op":op,"slug":slug,"lifetime":"persistent","managedExecution":execution,"command":args.get("command"),"cwd":args.get("cwd"),"envKeys":args.get("env").and_then(Value::as_object).map(|v| v.keys().collect::<Vec<_>>()),"grants":if execution { "persistent lifetime and unrestricted execution of this exact command/cwd/env" } else { "persistent lifetime" }});
    let mut digest = json!({"op":op,"slug":slug,"args":args,"managedExecution":execution});
    let mut summary = format!("Keep service {slug} persistently{}", if execution { "; run its command unrestricted as the owner" } else { "" });
    // A root outside the caller's directories rides on the same approval
    // (static-root-approval); without one the payload is unchanged.
    if let Some(root) = root {
        summary.push_str(&format!("; serve {} (outside the agent's cwd / project workspace)", root.path));
        details["staticRoot"] = json!(root.path);
        details["allowedRoots"] = json!(root.allowed_roots);
        details["grants"] = json!("persistent lifetime and serving every file under this exact root to the owner's browser");
        digest["staticRoot"] = json!(root.path);
    }
    Some(ApprovalRequest {
        kind: ApprovalKind::ServiceLifetime, reason: "persistent_service".into(),
        requester_cutex_session_id: cutex_session_id.clone(), requester_name: policy.requester_name.clone(), project_id: project_id.clone(),
        summary: summary.chars().take(300).collect(),
        details,
        request_sha256: request_digest(&digest),
    })
}

fn lifetime_gate(
    policy: &AgentServicePolicy,
    caller: &Caller,
    op: &str,
    slug: &str,
    args: &Value,
    managed: bool,
    root: Option<&CheckedRoot>,
) -> GateResult {
    let Some(request) = lifetime_approval_request(policy, caller, op, slug, args, managed, root)
    else {
        return Ok(None);
    };
    let Some(store) = &policy.approvals else {
        return Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            "persistent services require owner approval",
        )));
    };
    let outcome = gate(policy, store, request);
    gate_result(outcome)
}

/// The approval gate for a static root outside the caller's cwd / project
/// workspace (no persistent-lifetime approval in the same request).
fn static_root_gate(
    policy: &AgentServicePolicy,
    caller: &Caller,
    op: &str,
    slug: &str,
    args: &Value,
    root: &CheckedRoot,
) -> GateResult {
    let Caller::Agent {
        cutex_session_id,
        project_id,
    } = caller
    else {
        return Ok(None);
    };
    let Some(store) = &policy.approvals else {
        return Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            "a static root outside your cwd / project workspace requires owner approval; nothing was changed",
        )));
    };
    let outcome = gate(
        policy,
        store,
        ApprovalRequest {
            kind: ApprovalKind::StaticRoot,
            reason: "static_root_outside_workspace".into(),
            requester_cutex_session_id: cutex_session_id.clone(),
            requester_name: policy.requester_name.clone(),
            project_id: project_id.clone(),
            summary: format!(
                "{op} static service {slug}: serve {} (outside the agent's cwd / project workspace)",
                root.path
            )
            .chars()
            .take(300)
            .collect(),
            details: json!({
                "op": op,
                "slug": slug,
                "staticRoot": root.path,
                "allowedRoots": root.allowed_roots,
                "grants": "serving every file under this exact root to the owner's browser",
            }),
            request_sha256: request_digest(
                &json!({"op": op, "slug": slug, "args": args, "staticRoot": root.path}),
            ),
        },
    );
    gate_result(outcome)
}

/// services-project-scope: an agent removing a persistent project service
/// it did not register (e.g. the project's Director removing a member's
/// board) needs the owner's confirmation. Raised only for a request the
/// store would carry out; anything else is answered by the store.
fn removal_gate(policy: &AgentServicePolicy, caller: &Caller, record: &ServiceRecord) -> GateResult {
    if !may_manage(record, caller) {
        return Err(rejected(ServicesError::new(403, "not_service_owner", "you may not manage this service")));
    }
    project_release_gate(policy, caller, record, "unregister", &json!({}), false, None)
}

fn needs_project_release_confirmation(caller: &Caller, record: &ServiceRecord) -> bool {
    matches!(caller, Caller::Agent { cutex_session_id, .. }
        if record.owner_scope == ServiceOwnerScope::Project
            && record.lifetime == super::store::ServiceLifetime::Persistent
            && !record.archived
            && record.owner_cutex_session_id.as_deref() != Some(cutex_session_id.as_str()))
}

/// Approval retries must bind the same baseline and project as the first
/// request, including fields inherited by a partial update. These internal
/// digest fields never become a service patch or appear in approval details.
fn bound_approval_args(mut args: Value, caller: &Caller, basis: Option<&ServiceRecord>) -> Value {
    let project = match caller {
        Caller::Agent { project_id, .. } => project_id.as_deref(),
        Caller::Owner => None,
    };
    args["_serviceApprovalBasis"] = json!({"record": basis, "projectId": project});
    args
}

/// One exact approval covers releasing retention and any execution/root grant
/// in the same request, avoiding multiple consuming gates on a retry.
fn project_release_gate(
    policy: &AgentServicePolicy,
    caller: &Caller,
    record: &ServiceRecord,
    op: &str,
    args: &Value,
    managed: bool,
    root: Option<&CheckedRoot>,
) -> GateResult {
    let Caller::Agent {
        cutex_session_id,
        project_id,
    } = caller
    else {
        return Ok(None);
    };
    if !needs_project_release_confirmation(caller, record) {
        return Ok(None);
    }
    let Some(store) = &policy.approvals else {
        return Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            "releasing another registrant's persistent project service requires owner confirmation; nothing was changed",
        )));
    };
    let registrant = record.owner_cutex_session_id.as_deref().unwrap_or("(none)");
    let removing = op == "unregister";
    let execution = managed && !policy.managed_within_envelope;
    let action = if removing { "Remove" } else { "Make ephemeral" };
    let outcome = gate(
        policy,
        store,
        ApprovalRequest {
            // Always high risk and never granted by a Director role rule.
            kind: ApprovalKind::ManagedService,
            reason: if removing { "project_service_remove" } else { "project_service_downgrade" }.into(),
            requester_cutex_session_id: cutex_session_id.clone(),
            requester_name: policy.requester_name.clone(),
            project_id: project_id.clone(),
            summary: format!(
                "{action} persistent project service {} (registered by {registrant}); it may be permanently removed{}{}",
                record.slug,
                if execution { "; run the requested command unrestricted as the owner" } else { "" },
                root.map(|root| format!("; serve {} outside the agent's directories", root.path)).unwrap_or_default(),
            )
            .chars()
            .take(300)
            .collect(),
            details: json!({
                "op": op,
                "slug": record.slug,
                "displayName": record.display_name,
                "ownerScope": record.owner_scope.label(),
                "projectId": record.project_id,
                "registrant": record.owner_cutex_session_id,
                "runtime": record.runtime.label(),
                "lifetime": "persistent",
                "targetLifetime": if removing { Value::Null } else { json!("ephemeral") },
                "managedExecution": execution,
                "command": args.get("command"),
                "cwd": args.get("cwd"),
                "envKeys": args.get("env").and_then(Value::as_object).map(|env| env.keys().collect::<Vec<_>>()),
                "staticRoot": root.map(|root| &root.path),
                "grants": "release persistent retention; any listed managed execution or static root is also approved",
            }),
            request_sha256: request_digest(&json!({
                "op": op,
                "record": record,
                "args": args,
                "managedExecution": execution,
                "staticRoot": root.map(|root| &root.path),
            })),
        },
    );
    gate_result(outcome)
}

/// A gate's result for a request that collects approved-after-wait notes.
fn pass(result: GateResult, notes: &mut Vec<Value>) -> Result<(), Value> {
    result.map(|note| notes.extend(note))
}

/// The last approved-after-wait note on a successful result (Q21): the
/// agent sees the owner reviewed the request while the call waited, so it
/// neither assumes it ran unreviewed nor retries it.
fn with_notes(mut result: Value, notes: Vec<Value>) -> Value {
    if let Some(note) = notes.into_iter().last() {
        crate::approvals::attach_approval_note(&mut result, note);
    }
    result
}

/// The caller's current project (member or Director seat), if any.
pub fn caller_project(cutex_session_id: &str) -> Option<String> {
    let snapshot = crate::memory::load_agent_management_snapshot().ok()?;
    let projects = crate::asks::directory::Directory::director_projects(&snapshot, cutex_session_id);
    (projects.len() == 1).then(|| projects[0].clone())
}

/// Serve one authenticated agent request on the default store; `waiter`
/// lets an approval gate hold the call for the owner (approval-wait).
pub fn handle(
    cutex_session_id: &str,
    body: &[u8],
    waiter: Option<crate::approvals::Waiter>,
) -> Value {
    let store = match ServiceStore::open_default() {
        Ok(store) => store,
        Err(error) => return rejected(ServicesError::unavailable(error)),
    };
    let caller = Caller::Agent {
        cutex_session_id: cutex_session_id.to_string(),
        project_id: caller_project(cutex_session_id),
    };
    let mut policy = AgentServicePolicy::for_caller(cutex_session_id);
    policy.waiter = waiter;
    handle_with_policy(&store, &caller, body, &policy)
}

/// Ownership rules only (no boundary, no approvals).
pub fn handle_with(store: &ServiceStore, caller: &Caller, body: &[u8]) -> Value {
    handle_with_policy(store, caller, body, &AgentServicePolicy::permissive())
}

pub fn handle_with_policy(
    store: &ServiceStore,
    caller: &Caller,
    body: &[u8],
    policy: &AgentServicePolicy,
) -> Value {
    let Caller::Agent {
        cutex_session_id, ..
    } = caller
    else {
        return rejected(ServicesError::bad_request("agent caller required"));
    };
    let envelope: Envelope = match serde_json::from_slice(body) {
        Ok(envelope) => envelope,
        Err(error) => return rejected(ServicesError::bad_request(error.to_string())),
    };
    let args = if envelope.args.is_null() {
        json!({})
    } else {
        envelope.args
    };
    let parse_error =
        |error: serde_json::Error| rejected(ServicesError::bad_request(error.to_string()));
    match envelope.op.as_str() {
        "register" => {
            let mut request: RegisterRequest = match serde_json::from_value(args.clone()) {
                Ok(request) => request,
                Err(error) => return parse_error(error),
            };
            let managed = request.runtime.as_deref().and_then(ServiceRuntime::parse)
                == Some(ServiceRuntime::Managed);
            let registry = match store.load() {
                Ok(registry) => registry,
                Err(error) => return rejected(error),
            };
            // Resolve the default before the approval gate and pin that decision
            // for this mutation, including the configurable external default.
            if request.lifetime.is_none() {
                let lifetime = registry
                    .services
                    .get(&request.slug)
                    .map(|r| r.lifetime)
                    .unwrap_or(
                        if request.runtime.as_deref().unwrap_or("external") == "external" {
                            registry.settings.agent_external_default_lifetime
                        } else {
                            super::store::ServiceLifetime::Ephemeral
                        },
                    );
                request.lifetime = Some(
                    match lifetime {
                        super::store::ServiceLifetime::Persistent => "persistent",
                        super::store::ServiceLifetime::Ephemeral => "ephemeral",
                    }
                    .into(),
                );
            }
            // services-project-scope: an explicit project scope needs one
            // project; a sole-project Director's new services default to it.
            if request.owner_scope.as_deref() == Some("project") && policy.project_ambiguous {
                return rejected(ServicesError::new(
                    409,
                    "director_project_ambiguous",
                    "you direct several projects and are no member of one: scope project cannot pick one",
                ));
            }
            if request.owner_scope.is_none()
                && policy.director_default_project
                && !registry.services.contains_key(&request.slug)
                && matches!(caller, Caller::Agent { project_id: Some(_), .. })
            {
                request.owner_scope = Some(ServiceOwnerScope::Project.label().into());
            }
            let mut args = bound_approval_args(args, caller, registry.services.get(&request.slug));
            args["_serviceResolvedLifetime"] = json!(request.lifetime);
            args["_serviceResolvedScope"] = json!(request.owner_scope);
            // Approved-after-wait notes of the gates below (Q21).
            let mut notes = Vec::new();
            // Validate everything before any approval: an invalid request
            // never reaches the owner (static-root-approval).
            let root_review = match store.validate_register_if_unchanged(request.clone(), caller, registry.services.get(&request.slug)) {
                Ok(check) => check.static_root_approval,
                Err(error) => return rejected(error),
            };
            let downgrade = request.lifetime.as_deref() == Some("ephemeral")
                && registry.services.get(&request.slug)
                    .is_some_and(|record| needs_project_release_confirmation(caller, record));
            if downgrade {
                if let Err(refusal) = pass(project_release_gate(
                    policy, caller, &registry.services[&request.slug], "register", &args,
                    managed, root_review.as_ref(),
                ), &mut notes) {
                    return refusal;
                }
            }
            // With `agentPersistentApproval = off` a persistent request is
            // granted without the lifetime approval; a managed command then
            // still goes through the separate `managed_service` gate below.
            let needs_lifetime = registry.settings.agent_persistent_approval
                == AgentPersistentApproval::Ask
                && request.lifetime.as_deref() == Some("persistent")
                && registry.services.get(&request.slug).is_none_or(|record| {
                    record.lifetime != super::store::ServiceLifetime::Persistent
                });
            if needs_lifetime {
                if let Ok(record) = store.get(&request.slug) {
                    if !may_manage(&record, caller) {
                        return rejected(ServicesError::new(
                            409,
                            "slug_taken",
                            "service belongs to another owner",
                        ));
                    }
                }
                if let Err(refusal) = pass(lifetime_gate(
                    policy,
                    caller,
                    "register",
                    &request.slug,
                    &args,
                    managed,
                    root_review.as_ref(),
                ), &mut notes) {
                    return refusal;
                }
            } else if let Some(root) = root_review.as_ref().filter(|_| !downgrade) {
                if let Err(refusal) =
                    pass(static_root_gate(policy, caller, "register", &request.slug, &args, root), &mut notes)
                {
                    return refusal;
                }
            }
            request.static_root_approved = root_review.is_some();
            let reveal = request.reveal;
            if managed && request.command.is_some() && !needs_lifetime && !downgrade {
                // Gate only what the store would accept: another agent's slug
                // is refused below anyway, and an idempotent re-register of
                // the caller's own identical service changes nothing.
                let existing = registry.services.get(&request.slug).cloned();
                let skip = existing.is_some_and(|existing| {
                    existing.runtime == ServiceRuntime::Managed
                            && existing.command == request.command
                            && existing.cwd == request.cwd
                            && request.env.clone().unwrap_or_default() == existing.env
                });
                if !skip {
                    if let Err(refusal) = pass(managed_gate(
                        policy,
                        caller,
                        "register",
                        &request.slug,
                        &args,
                        request.command.as_ref(),
                    ), &mut notes) {
                        return refusal;
                    }
                }
            }
            let expected = registry.services.get(&request.slug);
            match super::authority::commit(caller, policy.revalidate_authority, |fresh| {
                store.register_if_unchanged(request, fresh, expected)
            }) {
                Ok((record, created)) => {
                    after_change();
                    let state = snapshots(store);
                    let view = redact_for(
                        caller,
                        &record,
                        service_view(&record, state.get(&record.slug)),
                    );
                    let mut result = json!({"ok": true, "created": created, "service": view, "link": super::store::service_link(&record)});
                    if reveal {
                        result["reveal"] = json!(super::reveal::request(&record, cutex_session_id));
                    }
                    with_notes(result, notes)
                }
                Err(error) => rejected(error),
            }
        }
        "update" => {
            let UpdateArgs { slug, patch } = match serde_json::from_value(args) {
                Ok(args) => args,
                Err(error) => return parse_error(error),
            };
            let mut request: UpdateRequest = match serde_json::from_value(patch.clone()) {
                Ok(request) => request,
                Err(error) => return parse_error(error),
            };
            let mut notes = Vec::new();
            let (existing, lifetime_approval) = match store.load() {
                Ok(registry) => (
                    registry.services.get(&slug).cloned(),
                    registry.settings.agent_persistent_approval,
                ),
                Err(error) => return rejected(error),
            };
            let Some(expected) = existing.as_ref() else {
                return rejected(ServicesError::not_found(&slug));
            };
            let patch = bound_approval_args(patch, caller, Some(expected));
            let (preview, root_review) = match store.preview_update_if_unchanged(&slug, request.clone(), caller, expected) {
                Ok((preview, check)) => (preview, check.static_root_approval),
                Err(error) => return rejected(error),
            };
            // A change to what a managed service runs is gated like a new one;
            // the store itself refuses agents that may not manage it.
            if let Some(existing) = existing.as_ref() {
                let runtime = Some(preview.runtime);
                // Compare what would run after the update with what runs now:
                // restating the same command / cwd / env / runtime (or an env
                // merge that ends up identical) is no change and is not gated
                // again, like an identical re-register.
                let changes_process = preview.runtime != existing.runtime
                    || preview.command != existing.command
                    || preview.cwd != existing.cwd
                    || preview.env != existing.env;
                let needs_lifetime = lifetime_approval == AgentPersistentApproval::Ask
                    && request.lifetime.as_deref() == Some("persistent")
                    && existing.lifetime != super::store::ServiceLifetime::Persistent;
                let downgrade = request.lifetime.as_deref() == Some("ephemeral")
                    && needs_project_release_confirmation(caller, &existing);
                if downgrade {
                    if let Err(refusal) = pass(project_release_gate(
                        policy, caller, &existing, "update", &patch,
                        preview.runtime == ServiceRuntime::Managed && changes_process,
                        root_review.as_ref(),
                    ), &mut notes) {
                        return refusal;
                    }
                }
                if needs_lifetime {
                    if let Err(refusal) = pass(lifetime_gate(
                        policy,
                        caller,
                        "update",
                        &slug,
                        &patch,
                        runtime == Some(ServiceRuntime::Managed) && changes_process,
                        root_review.as_ref(),
                    ), &mut notes) {
                        return refusal;
                    }
                } else if let Some(root) = root_review.as_ref().filter(|_| !downgrade) {
                    if let Err(refusal) =
                        pass(static_root_gate(policy, caller, "update", &slug, &patch, root), &mut notes)
                    {
                        return refusal;
                    }
                }
                if runtime == Some(ServiceRuntime::Managed) && changes_process && !needs_lifetime && !downgrade {
                    if let Err(refusal) = pass(managed_gate(
                        policy,
                        caller,
                        "update",
                        &slug,
                        &patch,
                        preview.command.as_ref(),
                    ), &mut notes) {
                        return refusal;
                    }
                }
            }
            request.static_root_approved = root_review.is_some();
            match super::authority::commit(caller, policy.revalidate_authority, |fresh| {
                store.update_if_unchanged(&slug, request, fresh, expected)
            }) {
                Ok(record) => {
                    after_change();
                    let state = snapshots(store);
                    with_notes(
                        json!({"ok": true,"service":redact_for(caller,&record,service_view(&record,state.get(&record.slug)))}),
                        notes,
                    )
                }
                Err(error) => rejected(error),
            }
        }
        "restart" => {
            // Same command / cwd / env as the record (the grant that let it
            // run covers its restarts), so no approval; ownership as for
            // update / unregister. The supervisor (Management serve) restarts
            // it on its next pass, about a second later.
            let SlugArgs { slug } = match serde_json::from_value(args) {
                Ok(args) => args,
                Err(error) => return parse_error(error),
            };
            match super::authority::commit(caller, policy.revalidate_authority, |fresh| {
                store.request_restart(&slug, fresh)
            }) {
                Ok(record) => {
                    after_change();
                    let state = snapshots(store);
                    json!({"ok": true, "slug": slug, "restartRequested": true, "service": redact_for(caller,&record,service_view(&record,state.get(&record.slug)))})
                }
                Err(error) => rejected(error),
            }
        }
        "unregister" => {
            let SlugArgs { slug } = match serde_json::from_value(args) {
                Ok(args) => args,
                Err(error) => return parse_error(error),
            };
            let mut notes = Vec::new();
            let expected = match store.get(&slug) {
                Ok(record) => record,
                Err(error) => return rejected(error),
            };
            if let Err(refusal) = pass(removal_gate(policy, caller, &expected), &mut notes) {
                return refusal;
            }
            match super::authority::commit(caller, policy.revalidate_authority, |fresh| {
                store.remove_if_unchanged(&slug, fresh, &expected)
            }) {
                Ok(_) => {
                    after_change();
                    with_notes(json!({"ok": true, "slug": slug, "removed": true}), notes)
                }
                Err(error) => rejected(error),
            }
        }
        "list" => {
            let ListArgs { mine } = match serde_json::from_value(args) {
                Ok(args) => args,
                Err(error) => return parse_error(error),
            };
            let registry = match store.load() {
                Ok(registry) => registry,
                Err(error) => return rejected(error),
            };
            let filter = ListFilter {
                owner: mine.then(|| cutex_session_id.clone()),
                ..Default::default()
            };
            let services: Vec<Value> = list_views(&registry, &snapshots(store), &filter)
                .into_iter()
                .filter_map(|view| {
                    let record = registry.services.get(view["slug"].as_str()?)?;
                    let manage = may_manage(record, caller);
                    (manage || policy.visible(cutex_session_id, record)).then(|| {
                        let mut view = redact_for(caller, record, view);
                        view["youMayManage"] = json!(manage);
                        view
                    })
                })
                .collect();
            json!({"ok": true, "host": store.host(), "services": services})
        }
        _ => rejected(ServicesError::bad_request("unknown services operation")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: &str = "cutex.00000000-0000-0000-0000-000000000001";
    const S2: &str = "cutex.00000000-0000-0000-0000-000000000002";

    fn agent(session: &str) -> Caller {
        Caller::Agent {
            cutex_session_id: session.into(),
            project_id: Some("proj".into()),
        }
    }

    #[test]
    fn agents_set_and_clear_their_service_tile() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-agent-tile-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = ServiceStore::new(&dir, "agenthost");
        let call =
            |caller: &Caller, body: Value| handle_with(&store, caller, body.to_string().as_bytes());
        let registered = call(
            &agent(S1),
            json!({"op": "register", "args": {"slug": "tiled", "tile": {"title": "Build", "badge": "OK", "x": 1}, "tilePath": "/tile.json"}}),
        );
        assert_eq!(registered["ok"], true, "{registered}");
        assert_eq!(registered["service"]["tile"], json!({"title": "Build", "badge": "OK"}));
        assert_eq!(registered["service"]["tilePath"], "/tile.json");
        let refused = call(
            &agent(S1),
            json!({"op": "update", "args": {"slug": "tiled", "tile": {"image": "https://example.com/a.png"}}}),
        );
        assert_eq!(refused["code"], "invalid_tile", "{refused}");
        assert_eq!(
            call(&agent(S2), json!({"op": "update", "args": {"slug": "tiled", "tile": null}}))["code"],
            "not_service_owner"
        );
        let cleared = call(
            &agent(S1),
            json!({"op": "update", "args": {"slug": "tiled", "tile": null, "tilePath": null}}),
        );
        assert_eq!(cleared["ok"], true, "{cleared}");
        assert!(cleared["service"].get("tile").is_none());
        assert!(cleared["service"].get("tilePath").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agent_operations_follow_ownership() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-agent-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = ServiceStore::new(&dir, "agenthost");
        let call =
            |caller: &Caller, body: Value| handle_with(&store, caller, body.to_string().as_bytes());
        let registered = call(
            &agent(S1),
            json!({"op": "register", "args": {"slug": "dev-web"}}),
        );
        assert_eq!(registered["ok"], true, "{registered}");
        assert_eq!(registered["created"], true);
        assert_eq!(registered["service"]["ownerCutexSessionId"], S1);
        assert_eq!(registered["service"]["projectId"], "proj");
        assert!(registered["service"]["port"].as_u64().unwrap() >= 24400);
        // Idempotent re-register by the owner agent.
        let again = call(
            &agent(S1),
            json!({"op": "register", "args": {"slug": "dev-web"}}),
        );
        assert_eq!(again["created"], false);
        assert_eq!(again["service"]["port"], registered["service"]["port"]);
        // Others cannot take, change or remove it.
        assert_eq!(
            call(
                &agent(S2),
                json!({"op": "register", "args": {"slug": "dev-web"}})
            )["code"],
            "slug_taken"
        );
        assert_eq!(
            call(
                &agent(S2),
                json!({"op": "update", "args": {"slug": "dev-web", "path": "/x"}})
            )["code"],
            "not_service_owner"
        );
        assert_eq!(
            call(
                &agent(S2),
                json!({"op": "unregister", "args": {"slug": "dev-web"}})
            )["code"],
            "not_service_owner"
        );
        // Agents cannot claim or reassign an owner; retired fields are rejected.
        assert_eq!(
            call(
                &agent(S1),
                json!({"op": "update", "args": {"slug": "dev-web", "pinned": true}})
            )["ok"],
            false
        );
        assert_eq!(
            call(
                &agent(S1),
                json!({"op": "update", "args": {"slug": "dev-web", "ownerCutexSessionId": S2}})
            )["ok"],
            false
        );
        assert_eq!(
            call(
                &agent(S1),
                json!({"op": "register", "args": {"slug": "x", "ownerCutexSessionId": S2}})
            )["ok"],
            false
        );
        let updated = call(
            &agent(S1),
            json!({"op": "update", "args": {"slug": "dev-web", "path": "/health", "displayName": "Dev web"}}),
        );
        assert_eq!(updated["service"]["path"], "/health");
        call(
            &agent(S2),
            json!({"op": "register", "args": {"slug": "other"}}),
        );
        let all = call(&agent(S1), json!({"op": "list"}));
        assert_eq!(all["services"].as_array().unwrap().len(), 2);
        let mine = call(&agent(S1), json!({"op": "list", "args": {"mine": true}}));
        assert_eq!(mine["services"].as_array().unwrap().len(), 1);
        assert_eq!(
            call(
                &agent(S1),
                json!({"op": "unregister", "args": {"slug": "dev-web"}})
            )["removed"],
            true
        );
        assert_eq!(call(&agent(S1), json!({"op": "bogus"}))["ok"], false);
        assert_eq!(handle_with(&store, &Caller::Owner, b"{}")["ok"], false);
        let _ = std::fs::remove_dir_all(dir);
    }

    const S3: &str = "cutex.00000000-0000-0000-0000-000000000003";
    const S4: &str = "cutex.00000000-0000-0000-0000-000000000004";

    fn in_project(session: &str, project: &str) -> Caller {
        Caller::Agent {
            cutex_session_id: session.into(),
            project_id: Some(project.into()),
        }
    }

    fn isolated_boundary() -> AgentBoundary {
        let project_of = [(S1, "proj"), (S4, "proj"), (S2, "proj2")]
            .into_iter()
            .map(|(session, project)| (session.to_string(), project.to_string()))
            .collect();
        AgentBoundary::new(
            &crate::config::agent_security::AgentSecurityConfig::default(),
            project_of,
            Default::default(),
        )
    }

    fn slugs(listing: &Value) -> Vec<String> {
        listing["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|service| service["slug"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn listing_respects_the_project_boundary_and_redacts_other_agents_processes() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-boundary-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = ServiceStore::new(&dir, "agenthost");
        let open = AgentServicePolicy::permissive();
        let call = |caller: &Caller, body: Value, policy: &AgentServicePolicy| {
            handle_with_policy(&store, caller, body.to_string().as_bytes(), policy)
        };
        let s1 = in_project(S1, "proj");
        let managed = json!({"op": "register", "args": {
            "slug": "web", "runtime": "managed", "command": "serve --token secret {port}",
            "cwd": std::env::temp_dir(), "env": {"API_KEY": "x"}}});
        assert_eq!(call(&s1, managed, &open)["ok"], true);
        assert_eq!(
            call(
                &in_project(S2, "proj2"),
                json!({"op": "register", "args": {"slug": "other"}}),
                &open
            )["ok"],
            true
        );
        let solo = Caller::Agent {
            cutex_session_id: S3.into(),
            project_id: None,
        };
        assert_eq!(
            call(
                &solo,
                json!({"op": "register", "args": {"slug": "solo"}}),
                &open
            )["ok"],
            true
        );
        store
            .register(
                serde_json::from_value(json!({"slug": "shared-db"})).unwrap(),
                &Caller::Owner,
            )
            .unwrap();

        let isolated = AgentServicePolicy {
            boundary: Some(isolated_boundary()),
            ..AgentServicePolicy::permissive()
        };
        let list = json!({"op": "list"});
        assert_eq!(
            slugs(&call(&s1, list.clone(), &isolated)),
            ["shared-db", "web"]
        );
        assert_eq!(
            slugs(&call(&solo, list.clone(), &isolated)),
            ["shared-db", "solo"]
        );
        assert_eq!(slugs(&call(&solo, list.clone(), &open)).len(), 4);

        // An external agent granted to `proj`: proj sees its own services,
        // the agent does not see proj's.
        let mut security = crate::config::agent_security::AgentSecurityConfig::default();
        security
            .external_agents
            .push(crate::config::agent_security::ExternalAgentGrant {
                project: "proj".into(),
                agent: S3.into(),
            });
        let granted = AgentServicePolicy {
            boundary: Some(AgentBoundary::new(
                &security,
                [(S1, "proj"), (S4, "proj"), (S2, "proj2")]
                    .into_iter()
                    .map(|(session, project)| (session.to_string(), project.to_string()))
                    .collect(),
                Default::default(),
            )),
            ..AgentServicePolicy::permissive()
        };
        assert_eq!(
            slugs(&call(&s1, list.clone(), &granted)),
            ["shared-db", "solo", "web"]
        );
        assert_eq!(
            slugs(&call(&solo, list.clone(), &granted)),
            ["shared-db", "solo"]
        );
        assert_eq!(
            slugs(&call(&in_project(S2, "proj2"), list.clone(), &granted)),
            ["other", "shared-db"]
        );

        // The owner sees how its service runs; a project peer does not.
        let own = call(&s1, list.clone(), &isolated);
        let own_web = &own["services"][1];
        assert_eq!(own_web["command"], "serve --token secret {port}");
        assert_eq!(own_web["envKeys"], json!(["API_KEY"]));
        let peer = call(&in_project(S4, "proj"), list, &isolated);
        let peer_web = peer["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|service| service["slug"] == "web")
            .unwrap()
            .clone();
        for field in PRIVATE_VIEW_FIELDS {
            assert!(peer_web.get(field).is_none(), "{field} leaked: {peer_web}");
        }
        assert_eq!(peer_web["port"], own_web["port"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn managed_services_outside_the_envelope_wait_for_the_owner() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-approval-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = ServiceStore::new(dir.join("services"), "agenthost");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let policy = AgentServicePolicy {
            boundary: None,
            managed_within_envelope: false,
            approvals: Some(ApprovalStore::at(dir.join("approvals.json"))),
            requester_name: Some("worker".into()),
            waiter: None,
            director_default_project: false,
            project_ambiguous: false,
            revalidate_authority: false,
        };
        let call = |body: Value| {
            handle_with_policy(&store, &agent(S1), body.to_string().as_bytes(), &policy)
        };
        // External services never need approval.
        assert_eq!(
            call(json!({"op": "register", "args": {"slug": "ext"}}))["ok"],
            true
        );

        let register = json!({"op": "register", "args": {
            "slug": "web", "runtime": "managed", "command": "serve {port}", "env": {"K": "secret"}}});
        let first = call(register.clone());
        assert_eq!(first["status"], "approval_required", "{first}");
        let approval_id = first["approval_id"].as_str().unwrap().to_string();
        assert!(store.load().unwrap().services.get("web").is_none());
        // Retrying while pending reuses the same request.
        assert_eq!(call(register.clone())["approval_id"], approval_id.as_str());
        let pending = approvals.list(false, chrono::Utc::now()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, ApprovalKind::ManagedService);
        assert_eq!(pending[0].details["envKeys"], json!(["K"]));
        assert!(!pending[0].details.to_string().contains("secret"));

        approvals
            .decide(&approval_id, true, None, chrono::Utc::now())
            .unwrap();
        let created = call(register.clone());
        assert_eq!(created["ok"], true, "{created}");
        // An identical re-register changes nothing and needs no approval.
        assert_eq!(call(register)["ok"], true);

        // Changing what it runs is gated again; a denial is final.
        let update =
            json!({"op": "update", "args": {"slug": "web", "command": "serve --debug {port}"}});
        let gated = call(update.clone());
        assert_eq!(gated["status"], "approval_required");
        approvals
            .decide(
                gated["approval_id"].as_str().unwrap(),
                false,
                Some("not now".into()),
                chrono::Utc::now(),
            )
            .unwrap();
        let denied = call(update);
        assert_eq!(denied["code"], "approval_denied");
        assert_eq!(denied["status"], "no_write");
        // A display-name change does not touch the process.
        assert_eq!(
            call(json!({"op": "update", "args": {"slug": "web", "displayName": "Web"}}))["ok"],
            true
        );
        // Without an approval store the gate fails closed.
        let closed = AgentServicePolicy {
            managed_within_envelope: false,
            ..AgentServicePolicy::permissive()
        };
        let refused = handle_with_policy(
            &store,
            &agent(S1),
            json!({"op": "register", "args": {"slug": "w2", "runtime": "managed", "command": "x"}})
                .to_string()
                .as_bytes(),
            &closed,
        );
        assert_eq!(refused["code"], "approvals_unavailable");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persistent_lifetime_and_managed_execution_share_one_exact_approval() {
        let dir =
            std::env::temp_dir().join(format!("cutex-lifetime-approval-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let policy = AgentServicePolicy {
            boundary: None,
            managed_within_envelope: false,
            approvals: Some(ApprovalStore::at(dir.join("approvals.json"))),
            requester_name: None,
            waiter: None,
            director_default_project: false,
            project_ambiguous: false,
            revalidate_authority: false,
        };
        let call = |body: Value| {
            handle_with_policy(&store, &agent(S1), body.to_string().as_bytes(), &policy)
        };
        let register = json!({"op":"register","args":{"slug":"web","runtime":"managed","command":"serve","lifetime":"persistent","env":{"SECRET":"not-in-card"}}});
        let refusal = call(register.clone());
        assert_eq!(refusal["status"], "approval_required");
        let pending = approvals.list(false, chrono::Utc::now()).unwrap();
        assert_eq!(pending[0].kind, ApprovalKind::ServiceLifetime);
        assert_eq!(pending[0].details["managedExecution"], true);
        assert!(!pending[0].details.to_string().contains("not-in-card"));
        approvals
            .decide(
                refusal["approval_id"].as_str().unwrap(),
                true,
                None,
                chrono::Utc::now(),
            )
            .unwrap();
        assert_eq!(call(register.clone())["ok"], true);
        assert_eq!(call(register)["ok"], true);
        assert_eq!(
            call(json!({"op":"update","args":{"slug":"web","lifetime":"ephemeral"}}))["ok"],
            true
        );
        let update = json!({"op":"update","args":{"slug":"web","lifetime":"persistent"}});
        assert_eq!(call(update.clone())["status"], "approval_required");
        store
            .update(
                "web",
                serde_json::from_value(json!({"lifetime":"persistent"})).unwrap(),
                &Caller::Owner,
            )
            .unwrap();
        assert_eq!(call(update)["ok"], true);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// service_update re-asks only when the effective command / cwd / env /
    /// runtime change; service_restart never asks and follows ownership.
    #[test]
    fn unchanged_update_and_restart_need_no_new_approval() {
        let dir = std::env::temp_dir().join(format!("cutex-services-restart-{}", uuid::Uuid::new_v4().simple()));
        let cwd = dir.join("app");
        let other_cwd = dir.join("other");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&other_cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let other_cwd = other_cwd.to_string_lossy().into_owned();
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let policy = gated_policy(&dir);
        let call = |session: &str, body: Value| {
            handle_with_policy(&store, &agent(session), body.to_string().as_bytes(), &policy)
        };
        let pending = || approvals.list(false, chrono::Utc::now()).unwrap().len();
        let register = json!({"op":"register","args":{"slug":"forum","runtime":"managed","command":"node server.js {port}","cwd":cwd,"env":{"MODE":"dev"}}});
        let refusal = call(S1, register.clone());
        assert_eq!(refusal["status"], "approval_required", "{refusal}");
        approvals
            .decide(refusal["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now())
            .unwrap();
        assert_eq!(call(S1, register)["ok"], true);
        assert_eq!(pending(), 0);

        // Restating what already runs (and an env merge that changes nothing) is no change.
        for args in [
            json!({"slug":"forum","runtime":"managed","command":"node server.js {port}","cwd":cwd,"env":{"MODE":"dev"}}),
            json!({"slug":"forum","command":"node server.js {port}"}),
            json!({"slug":"forum","env":{"MODE":"dev"}}),
            json!({"slug":"forum","cwd":cwd,"displayName":"Forum"}),
        ] {
            let result = call(S1, json!({"op":"update","args":args}));
            assert_eq!(result["ok"], true, "{args}: {result}");
            assert_eq!(pending(), 0, "{args} raised an approval");
        }
        assert_eq!(store.get("forum").unwrap().display_name, "Forum");
        // Any change of what runs is gated as before.
        for args in [
            json!({"slug":"forum","command":"node server.js --debug {port}"}),
            json!({"slug":"forum","cwd":other_cwd}),
            json!({"slug":"forum","cwd":null}),
            json!({"slug":"forum","env":{"MODE":"prod"}}),
            json!({"slug":"forum","env":{"TOKEN":"x"}}),
            json!({"slug":"forum","env":{"MODE":null}}),
        ] {
            let result = call(S1, json!({"op":"update","args":args}));
            assert_eq!(result["status"], "approval_required", "{args}: {result}");
        }
        let unchanged = store.get("forum").unwrap();
        assert_eq!(unchanged.cwd.as_deref(), Some(cwd.as_str()));
        assert_eq!(unchanged.env.get("MODE").map(String::as_str), Some("dev"));
        let before = pending();

        // Restart: no approval, a new request token each time.
        let restarted = call(S1, json!({"op":"restart","args":{"slug":"forum"}}));
        assert_eq!(restarted["ok"], true, "{restarted}");
        assert_eq!(restarted["restartRequested"], true);
        assert_eq!(restarted["service"]["slug"], "forum");
        let first = store.get("forum").unwrap().restart_requested_at.unwrap();
        assert_eq!(call(S1, json!({"op":"restart","args":{"slug":"forum"}}))["ok"], true);
        assert_ne!(store.get("forum").unwrap().restart_requested_at.unwrap(), first);
        assert_eq!(pending(), before);
        // Another agent's service, unknown slugs and extra arguments are refused.
        assert_eq!(call(S2, json!({"op":"restart","args":{"slug":"forum"}}))["code"], "not_service_owner");
        assert_eq!(call(S1, json!({"op":"restart","args":{"slug":"nope"}}))["ok"], false);
        assert_eq!(call(S1, json!({"op":"restart","args":{"slug":"forum","command":"x"}}))["ok"], false);
        // External services are not cutex's to restart.
        assert_eq!(call(S1, json!({"op":"register","args":{"slug":"ext"}}))["ok"], true);
        assert_eq!(call(S1, json!({"op":"restart","args":{"slug":"ext"}}))["code"], "not_managed");
        // A service the owner stopped stays stopped.
        store.set_stopped("forum", true).unwrap();
        let stopped = call(S1, json!({"op":"restart","args":{"slug":"forum"}}));
        assert_eq!(stopped["code"], "service_stopped", "{stopped}");
        assert!(store.get("forum").unwrap().stopped);
        assert_eq!(pending(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn managed_services_follow_the_live_thread_permissions() {
        let path = if cfg!(windows) { "C:\\work" } else { "/work" };
        let mut record = crate::session::model::CutexSessionRecord::new(
            S1.into(),
            None,
            "host".into(),
            path.into(),
            None,
        )
        .unwrap();
        record.sandbox_mode = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        let full = json!({"approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}});
        let auto = json!({"approvalPolicy":"on-request","sandboxPolicy":{"type":"workspaceWrite"}});
        // Launched restricted: a card, unless the running thread switched to full access.
        assert!(!managed_within_envelope(&record, None));
        assert!(managed_within_envelope(&record, Some(&full)));
        assert!(!managed_within_envelope(&record, Some(&auto)));
        // Launched with full access, switched away while running: a card again.
        record.sandbox_mode = Some("danger-full-access".into());
        record.approval_policy = Some("never".into());
        assert!(managed_within_envelope(&record, None));
        assert!(!managed_within_envelope(&record, Some(&auto)));
        // Live settings without the approval field use the record's.
        assert!(managed_within_envelope(&record, Some(&json!({"sandboxPolicy":{"type":"dangerFullAccess"}}))));
        // No permissions known: fail closed.
        record.sandbox_mode = None;
        record.approval_policy = None;
        assert!(!managed_within_envelope(&record, None));
    }

    /// End to end through the live-settings file the runtime manager writes.
    #[test]
    fn managed_services_read_the_current_runtimes_live_settings_file() {
        let dir = std::env::temp_dir().join(format!("cutex-services-live-{}", uuid::Uuid::new_v4().simple()));
        let path = if cfg!(windows) { "C:\\work" } else { "/work" };
        let mut record = crate::session::model::CutexSessionRecord::new(S1.into(), None, "host".into(), path.into(), None).unwrap();
        record.sandbox_mode = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        record.codex_session_id = Some("thread-1".into());
        record.runtime_generation = 2;
        let full = json!({"approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}});
        crate::app_server::live_settings::record_at(&dir, S1, "thread-1", 2, &full).unwrap();
        let live = crate::app_server::live_settings::current_at(&dir, &record);
        assert!(managed_within_envelope(&record, live.as_ref()));
        // The next runtime has not reported yet: the record decides.
        record.runtime_generation = 3;
        let live = crate::app_server::live_settings::current_at(&dir, &record);
        assert!(!managed_within_envelope(&record, live.as_ref()));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn gated_policy(dir: &std::path::Path) -> AgentServicePolicy {
        AgentServicePolicy {
            boundary: None,
            managed_within_envelope: false,
            approvals: Some(ApprovalStore::at(dir.join("approvals.json"))),
            requester_name: None,
            waiter: None,
            director_default_project: false,
            project_ambiguous: false,
            revalidate_authority: false,
        }
    }

    /// approval-wait: the register call holds while the owner decides and
    /// then returns the normal result; a denial answers denied; without a
    /// decision it answers approval_required as before.
    #[test]
    fn a_holding_service_call_proceeds_when_the_owner_approves() {
        let dir = std::env::temp_dir().join(format!("cutex-services-wait-{}", uuid::Uuid::new_v4().simple()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let path = dir.join("approvals.json");
        let mut policy = gated_policy(&dir);
        let hold = |millis: u64| {
            let mut waiter = crate::approvals::Waiter::new(
                1,
                Some("t".into()),
                None,
                Some(1),
                || true,
                || true,
            );
            waiter.duration = std::time::Duration::from_millis(millis);
            waiter.poll = std::time::Duration::from_millis(10);
            waiter
        };
        let decide_when_holding = |approve: bool| {
            let path = path.clone();
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let rows = crate::approvals::read_all_at(&path).unwrap_or_default();
                    if let Some(row) = rows.iter().find(|r| r.wait.as_ref().is_some_and(|w| w.end.is_none())) {
                        ApprovalStore::at(&path).decide(&row.approval_id, approve, None, chrono::Utc::now()).unwrap();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                panic!("never held");
            })
        };
        let register = json!({"op": "register", "args": {"slug": "web", "runtime": "managed", "command": "serve {port}"}});
        policy.waiter = Some(hold(5_000));
        let decider = decide_when_holding(true);
        let created = handle_with_policy(&store, &agent(S1), register.to_string().as_bytes(), &policy);
        decider.join().unwrap();
        assert_eq!(created["ok"], true, "{created}");
        assert!(store.load().unwrap().services.contains_key("web"));
        // Q21: the result says the owner approved it while the call waited.
        assert_eq!(created["approval"]["decision"], "approved", "{created}");
        assert!(created["approval"]["waitedSecs"].as_i64().is_some(), "{created}");
        assert!(created["approval"]["approvalId"].as_str().is_some_and(|id| !id.is_empty()), "{created}");
        assert!(created["detail"].as_str().unwrap().starts_with("The owner approved this request after "), "{created}");
        assert!(created["detail"].as_str().unwrap().ends_with("it has been carried out."), "{created}");
        // An update the gate does not stop (no process change) has no note.
        let renamed = json!({"op": "update", "args": {"slug": "web", "displayName": "Web"}});
        let plain = handle_with_policy(&store, &agent(S1), renamed.to_string().as_bytes(), &policy);
        assert_eq!(plain["ok"], true, "{plain}");
        assert!(plain.get("approval").is_none() && plain.get("detail").is_none(), "{plain}");

        let update = json!({"op": "update", "args": {"slug": "web", "command": "serve2 {port}"}});
        let decider = decide_when_holding(false);
        let denied = handle_with_policy(&store, &agent(S1), update.to_string().as_bytes(), &policy);
        decider.join().unwrap();
        assert_eq!(denied["code"], "approval_denied", "{denied}");

        let other = json!({"op": "register", "args": {"slug": "api", "runtime": "managed", "command": "api {port}"}});
        policy.waiter = Some(hold(100));
        let pending = handle_with_policy(&store, &agent(S1), other.to_string().as_bytes(), &policy);
        assert_eq!(pending["status"], "approval_required", "{pending}");
        assert!(!store.load().unwrap().services.contains_key("api"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persistent_approval_ask_gates_explicit_and_default_persistent() {
        let dir = std::env::temp_dir().join(format!("cutex-persist-ask-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let policy = gated_policy(&dir);
        let call = |body: Value| {
            handle_with_policy(&store, &agent(S1), body.to_string().as_bytes(), &policy)
        };
        assert_eq!(
            store.load().unwrap().settings.agent_persistent_approval,
            AgentPersistentApproval::Ask
        );
        // Explicit persistent on an external service: a lifetime-only card.
        let explicit = json!({"op":"register","args":{"slug":"ext","lifetime":"persistent"}});
        let refusal = call(explicit.clone());
        assert_eq!(refusal["status"], "approval_required", "{refusal}");
        let pending = approvals.list(false, chrono::Utc::now()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, ApprovalKind::ServiceLifetime);
        assert_eq!(pending[0].details["managedExecution"], false);
        approvals
            .decide(refusal["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now())
            .unwrap();
        let retried = call(explicit);
        assert_eq!(retried["ok"], true);
        // A retry that used an earlier approval did not wait: no note.
        assert!(retried.get("approval").is_none(), "{retried}");
        // The owner's persistent external default is gated the same way.
        store
            .update_settings(super::super::store::SettingsPatch {
                agent_external_default_lifetime: Some("persistent".into()),
                ..Default::default()
            })
            .unwrap();
        let defaulted = call(json!({"op":"register","args":{"slug":"ext2"}}));
        assert_eq!(defaulted["status"], "approval_required", "{defaulted}");
        assert!(store.load().unwrap().services.get("ext2").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_approval_off_grants_lifetime_and_keeps_the_managed_gate() {
        let dir = std::env::temp_dir().join(format!("cutex-persist-off-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let policy = gated_policy(&dir);
        let call = |body: Value| {
            handle_with_policy(&store, &agent(S1), body.to_string().as_bytes(), &policy)
        };
        store
            .update_settings(super::super::store::SettingsPatch {
                agent_persistent_approval: Some("off".into()),
                agent_external_default_lifetime: Some("persistent".into()),
                ..Default::default()
            })
            .unwrap();
        let persistent = super::super::store::ServiceLifetime::Persistent;
        // External: explicit persistent and the persistent default need nothing.
        let explicit = call(json!({"op":"register","args":{"slug":"ext","lifetime":"persistent"}}));
        assert_eq!(explicit["ok"], true, "{explicit}");
        assert_eq!(store.get("ext").unwrap().lifetime, persistent);
        let defaulted = call(json!({"op":"register","args":{"slug":"ext2"}}));
        assert_eq!(defaulted["ok"], true, "{defaulted}");
        assert_eq!(store.get("ext2").unwrap().lifetime, persistent);
        // An upgrade through update needs nothing either.
        assert_eq!(
            call(json!({"op":"update","args":{"slug":"ext","lifetime":"ephemeral"}}))["ok"],
            true
        );
        assert_eq!(
            call(json!({"op":"update","args":{"slug":"ext","lifetime":"persistent"}}))["ok"],
            true
        );
        assert!(approvals.list(false, chrono::Utc::now()).unwrap().is_empty());

        // Managed + persistent: only the managed-execution approval remains,
        // as the usual managed_service card.
        let combined = json!({"op":"register","args":{"slug":"web","runtime":"managed","command":"serve","lifetime":"persistent","env":{"SECRET":"not-in-card"}}});
        let refusal = call(combined.clone());
        assert_eq!(refusal["status"], "approval_required", "{refusal}");
        let pending = approvals.list(false, chrono::Utc::now()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, ApprovalKind::ManagedService);
        assert!(pending[0].details.get("managedExecution").is_none());
        assert!(!pending[0].details.to_string().contains("not-in-card"));
        assert!(store.load().unwrap().services.get("web").is_none());
        approvals
            .decide(refusal["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now())
            .unwrap();
        assert_eq!(call(combined)["ok"], true);
        assert_eq!(store.get("web").unwrap().lifetime, persistent);
        // Changing what it runs while upgrading lifetime: managed gate only.
        assert_eq!(
            call(json!({"op":"update","args":{"slug":"web","lifetime":"ephemeral"}}))["ok"],
            true
        );
        let update = call(json!({"op":"update","args":{"slug":"web","lifetime":"persistent","command":"serve --debug"}}));
        assert_eq!(update["status"], "approval_required", "{update}");
        let pending = approvals.list(false, chrono::Utc::now()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, ApprovalKind::ManagedService);

        // Within the caller's own envelope nothing is asked at all.
        let unrestricted = AgentServicePolicy {
            managed_within_envelope: true,
            ..gated_policy(&dir)
        };
        let own = handle_with_policy(
            &store,
            &agent(S1),
            json!({"op":"register","args":{"slug":"web2","runtime":"managed","command":"serve","lifetime":"persistent"}})
                .to_string()
                .as_bytes(),
            &unrestricted,
        );
        assert_eq!(own["ok"], true, "{own}");
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// A roster whose project `proj` is directed by `director` (installed as
    /// the request-local authority, like a caller holding the roster fence).
    fn directed_by(director: &str) -> crate::agent_management::AgentManagementSnapshot {
        let mut snapshot = crate::agent_management::AgentManagementSnapshot::empty();
        snapshot.projects.insert(
            crate::agent_management::ProjectId::new("proj").unwrap(),
            serde_json::from_value(json!({
                "project_id": "proj", "authorized_director_session": director,
                "authority_epoch": 1, "updated_at": "2026-10-28T00:00:00Z"
            }))
            .unwrap(),
        );
        snapshot
    }

    fn find<'a>(listing: &'a Value, slug: &str) -> &'a Value {
        listing["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|service| service["slug"] == slug)
            .unwrap_or_else(|| panic!("{slug} not listed: {listing}"))
    }

    /// services-project-scope: a project service is managed by whoever is
    /// the project's Director at the time of the call and by its registrant
    /// while in the project; session services and non-members are unchanged.
    #[test]
    fn project_services_follow_the_current_director() {
        let dir = std::env::temp_dir().join(format!("cutex-services-project-{}", uuid::Uuid::new_v4().simple()));
        let store = ServiceStore::new(&dir, "agenthost");
        let open = AgentServicePolicy::permissive();
        let call = |caller: &Caller, body: Value, policy: &AgentServicePolicy| {
            handle_with_policy(&store, caller, body.to_string().as_bytes(), policy)
        };
        let member = in_project(S1, "proj");
        let old_director = in_project(S3, "proj");
        let new_director = in_project(S4, "proj");
        let outsider = in_project(S2, "proj2");
        let board = json!({"op":"register","args":{"slug":"board","runtime":"managed","command":"serve --token t {port}","ownerScope":"project"}});
        let update = |slug: &str, name: &str| json!({"op":"update","args":{"slug":slug,"displayName":name}});
        let restart = json!({"op":"restart","args":{"slug":"board"}});

        crate::director_address::with_authority_snapshot(&directed_by(S3), || {
            let registered = call(&member, board.clone(), &open);
            assert_eq!(registered["ok"], true, "{registered}");
            assert_eq!(registered["service"]["ownerScope"], "project");
            assert_eq!(registered["service"]["ownerCutexSessionId"], S1);
            assert_eq!(registered["service"]["projectId"], "proj");
            store.update("board", serde_json::from_value(json!({
                "loginLinkCommand":"owner-only-login-helper"
            })).unwrap(), &Caller::Owner).unwrap();
            let member_listing = call(&member, json!({"op":"list"}), &open);
            assert!(find(&member_listing, "board").get("loginLinkCommand").is_none());
            // Default stays session for a member.
            let mine = call(&member, json!({"op":"register","args":{"slug":"mine","runtime":"managed","command":"x {port}"}}), &open);
            assert_eq!(mine["service"]["ownerScope"], "session", "{mine}");

            // The current Director manages the member's project service and sees how it runs.
            let listing = call(&old_director, json!({"op":"list"}), &open);
            assert_eq!(find(&listing, "board")["youMayManage"], true);
            assert_eq!(find(&listing, "board")["command"], "serve --token t {port}");
            assert!(find(&listing, "board").get("loginLinkCommand").is_none());
            assert_eq!(find(&listing, "mine")["youMayManage"], false);
            assert!(find(&listing, "mine").get("command").is_none());
            assert_eq!(call(&old_director, update("board", "Board"), &open)["ok"], true);
            assert_eq!(call(&old_director, restart.clone(), &open)["ok"], true);
            assert_eq!(call(&old_director, update("mine", "x"), &open)["code"], "not_service_owner");
            // Re-registering it keeps the registrant and project.
            let again = call(&old_director, board.clone(), &open);
            assert_eq!(again["ok"], true, "{again}");
            assert_eq!(store.get("board").unwrap().owner_cutex_session_id.as_deref(), Some(S1));
            // Only the owner turns it back into a session service.
            let demote = call(&old_director, json!({"op":"update","args":{"slug":"board","ownerScope":"session"}}), &open);
            assert_eq!(demote["code"], "not_service_owner", "{demote}");
            // A non-member neither manages nor sees how it runs.
            let refused = call(&outsider, update("board", "x"), &open);
            assert_eq!(refused["code"], "not_service_owner");
            assert!(refused["detail"].as_str().unwrap().contains("current Director"), "{refused}");
            assert_eq!(call(&outsider, board.clone(), &open)["code"], "slug_taken");
            let listing = call(&outsider, json!({"op":"list"}), &open);
            assert_eq!(find(&listing, "board")["youMayManage"], false);
            assert!(find(&listing, "board").get("command").is_none());
        });

        // The seat changes: the successor manages at once, the predecessor no longer.
        crate::director_address::with_authority_snapshot(&directed_by(S4), || {
            assert_eq!(call(&old_director, update("board", "x"), &open)["code"], "not_service_owner");
            assert_eq!(call(&old_director, restart.clone(), &open)["code"], "not_service_owner");
            assert_eq!(
                call(&old_director, json!({"op":"unregister","args":{"slug":"board"}}), &open)["code"],
                "not_service_owner"
            );
            assert_eq!(find(&call(&old_director, json!({"op":"list"}), &open), "board")["youMayManage"], false);
            assert_eq!(call(&new_director, update("board", "Board 2"), &open)["ok"], true);
            assert_eq!(call(&new_director, restart.clone(), &open)["ok"], true);
            // The registrant manages it while in the project, not after leaving it.
            assert_eq!(call(&member, update("board", "Board 3"), &open)["ok"], true);
            let left = Caller::Agent { cutex_session_id: S1.into(), project_id: None };
            assert_eq!(call(&left, update("board", "x"), &open)["code"], "not_service_owner");
            assert_eq!(call(&left, update("mine", "still mine"), &open)["ok"], true);

            // A sole-project Director's new services default to project; explicit session wins.
            let director_policy = AgentServicePolicy { director_default_project: true, ..AgentServicePolicy::permissive() };
            let defaulted = call(&new_director, json!({"op":"register","args":{"slug":"dir-board"}}), &director_policy);
            assert_eq!(defaulted["service"]["ownerScope"], "project", "{defaulted}");
            let personal = call(&new_director, json!({"op":"register","args":{"slug":"dir-own","ownerScope":"session"}}), &director_policy);
            assert_eq!(personal["service"]["ownerScope"], "session", "{personal}");
            // An existing session slug is not converted by the default.
            let kept = call(&member, json!({"op":"register","args":{"slug":"mine","runtime":"managed","command":"x {port}"}}), &director_policy);
            assert_eq!(kept["service"]["ownerScope"], "session", "{kept}");
            // Without a project or with several directed projects, project scope is refused.
            let solo = Caller::Agent { cutex_session_id: S2.into(), project_id: None };
            assert_eq!(
                call(&solo, json!({"op":"register","args":{"slug":"solo","ownerScope":"project"}}), &open)["code"],
                "project_scope_unavailable"
            );
            let ambiguous = AgentServicePolicy { project_ambiguous: true, ..AgentServicePolicy::permissive() };
            assert_eq!(
                call(&new_director, json!({"op":"register","args":{"slug":"amb","ownerScope":"project"}}), &ambiguous)["code"],
                "director_project_ambiguous"
            );
            // A member promotes its own session service; the Director then manages it.
            let promoted = call(&member, json!({"op":"update","args":{"slug":"mine","ownerScope":"project"}}), &open);
            assert_eq!(promoted["service"]["ownerScope"], "project", "{promoted}");
            assert_eq!(call(&new_director, update("mine", "Shared"), &open)["ok"], true);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// services-project-scope: removing a persistent project service another
    /// agent registered waits for the owner; own or ephemeral ones do not.
    #[test]
    fn removing_another_agents_persistent_project_service_asks_the_owner() {
        let dir = std::env::temp_dir().join(format!("cutex-services-project-rm-{}", uuid::Uuid::new_v4().simple()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let policy = gated_policy(&dir);
        for (slug, lifetime, registrant) in [("board", "persistent", S1), ("preview", "ephemeral", S1), ("own", "persistent", S3)] {
            store
                .register(
                    serde_json::from_value(json!({"slug":slug,"lifetime":lifetime,"ownerScope":"project","projectId":"proj","ownerCutexSessionId":registrant})).unwrap(),
                    &Caller::Owner,
                )
                .unwrap();
        }
        let call = |caller: &Caller, slug: &str| {
            handle_with_policy(&store, caller, json!({"op":"unregister","args":{"slug":slug}}).to_string().as_bytes(), &policy)
        };
        let director = in_project(S3, "proj");
        crate::director_address::with_authority_snapshot(&directed_by(S3), || {
            // A non-member is refused without a card.
            assert_eq!(call(&in_project(S2, "proj2"), "board")["code"], "not_service_owner");
            assert!(approvals.list(false, chrono::Utc::now()).unwrap().is_empty());
            let first = call(&director, "board");
            assert_eq!(first["status"], "approval_required", "{first}");
            assert!(store.get("board").is_ok());
            let cards = approvals.list(false, chrono::Utc::now()).unwrap();
            assert_eq!(cards.len(), 1);
            assert_eq!(cards[0].kind, ApprovalKind::ManagedService);
            assert_eq!(cards[0].reason, "project_service_remove");
            assert_eq!(cards[0].details["registrant"], S1);
            assert_eq!(cards[0].project_id.as_deref(), Some("proj"));
            approvals
                .decide(first["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now())
                .unwrap();
            assert_eq!(call(&director, "board")["removed"], true);
            assert!(store.get("board").is_err());
            // Ephemeral, or registered by the caller itself: no card.
            assert_eq!(call(&director, "preview")["removed"], true);
            assert_eq!(call(&director, "own")["removed"], true);
            assert!(approvals.list(false, chrono::Utc::now()).unwrap().is_empty());
            // Re-registering an owner-created project service, or one whose
            // deleted registrant was cleared, never makes the Director its
            // registrant and must not bypass persistent removal confirmation.
            store.register(serde_json::from_value(json!({
                "slug":"orphan", "lifetime":"persistent", "ownerScope":"project", "projectId":"proj"
            })).unwrap(), &Caller::Owner).unwrap();
            let registered = handle_with_policy(&store, &director,
                json!({"op":"register","args":{"slug":"orphan"}}).to_string().as_bytes(), &policy);
            assert_eq!(registered["ok"], true, "{registered}");
            assert!(store.get("orphan").unwrap().owner_cutex_session_id.is_none());
            assert_eq!(call(&director, "orphan")["status"], "approval_required");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reregister_compares_the_effective_replacement_env() {
        let dir = std::env::temp_dir().join(format!("cutex-project-env-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let policy = gated_policy(&dir);
        let original = json!({"slug":"board","runtime":"managed","command":"serve {port}",
            "env":{"MODE":"restricted"},"lifetime":"ephemeral","ownerScope":"project",
            "projectId":"proj","ownerCutexSessionId":S1});
        store.register(serde_json::from_value(original).unwrap(), &Caller::Owner).unwrap();
        let director = in_project(S3, "proj");
        crate::director_address::with_authority_snapshot(&directed_by(S3), || {
            let call = |args: Value| handle_with_policy(&store, &director,
                json!({"op":"register","args":args}).to_string().as_bytes(), &policy);
            let unchanged = call(json!({"slug":"board","runtime":"managed","command":"serve {port}",
                "env":{"MODE":"restricted"}}));
            assert_eq!(unchanged["ok"], true, "{unchanged}");
            assert!(approvals.list(false, chrono::Utc::now()).unwrap().is_empty());
            let replacement = json!({"slug":"board","runtime":"managed","command":"serve {port}"});
            let pending = call(replacement.clone());
            assert_eq!(pending["status"], "approval_required", "{pending}");
            assert_eq!(store.get("board").unwrap().env["MODE"], "restricted");
            let cards = approvals.list(false, chrono::Utc::now()).unwrap();
            assert_eq!(cards.len(), 1);
            assert_eq!(cards[0].requester_cutex_session_id, S3);
            assert_eq!(cards[0].kind, ApprovalKind::ManagedService);
            approvals.decide(pending["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now()).unwrap();
            assert_eq!(call(replacement.clone())["ok"], true);
            assert!(store.get("board").unwrap().env.is_empty());
            assert_eq!(call(replacement)["ok"], true);
            assert!(approvals.list(false, chrono::Utc::now()).unwrap().is_empty());
        });
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nonregistrant_downgrade_requires_one_exact_owner_approval() {
        let dir = std::env::temp_dir().join(format!("cutex-project-downgrade-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let director = in_project(S3, "proj");
        crate::director_address::with_authority_snapshot(&directed_by(S3), || {
            for op in ["register", "update"] {
                for full_access in [false, true] {
                    let slug = format!("{op}-{full_access}");
                    store.register(serde_json::from_value(json!({
                        "slug":slug,"runtime":"managed","command":"old {port}","lifetime":"persistent",
                        "ownerScope":"project","projectId":"proj","ownerCutexSessionId":S1,
                    })).unwrap(), &Caller::Owner).unwrap();
                    let policy = AgentServicePolicy { managed_within_envelope: full_access, ..gated_policy(&dir) };
                    let body = json!({"op":op,"args":{"slug":slug,"runtime":"managed",
                        "command":"new {port}","lifetime":"ephemeral"}});
                    let call = || handle_with_policy(&store, &director, body.to_string().as_bytes(), &policy);
                    let pending = call();
                    assert_eq!(pending["status"], "approval_required", "{pending}");
                    let before = store.get(&slug).unwrap();
                    assert_eq!(before.lifetime, super::super::store::ServiceLifetime::Persistent);
                    assert_eq!(before.command, Some(ServiceCommand::Shell("old {port}".into())));
                    let cards = approvals.list(false, chrono::Utc::now()).unwrap();
                    assert_eq!(cards.len(), 1);
                    assert_eq!(cards[0].reason, "project_service_downgrade");
                    assert_eq!(cards[0].kind, ApprovalKind::ManagedService);
                    assert_eq!(cards[0].requester_cutex_session_id, S3);
                    assert_eq!(cards[0].details["managedExecution"], !full_access);
                    approvals.decide(pending["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now()).unwrap();
                    let changed = call();
                    assert_eq!(changed["ok"], true, "{changed}");
                    assert_eq!(store.get(&slug).unwrap().lifetime, super::super::store::ServiceLifetime::Ephemeral);
                    // One approval also covers this caller's changed managed command.
                    assert!(approvals.list(false, chrono::Utc::now()).unwrap().is_empty());
                }
            }
            // A registrant releasing its own retention keeps the existing behavior.
            store.register(serde_json::from_value(json!({"slug":"own","lifetime":"persistent",
                "ownerScope":"project","projectId":"proj","ownerCutexSessionId":S3})).unwrap(), &Caller::Owner).unwrap();
            let own = handle_with_policy(&store, &director,
                json!({"op":"update","args":{"slug":"own","lifetime":"ephemeral"}}).to_string().as_bytes(), &gated_policy(&dir));
            assert_eq!(own["ok"], true, "{own}");
            assert!(approvals.list(false, chrono::Utc::now()).unwrap().is_empty());
        });
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unregister_refuses_a_replacement_created_during_owner_approval() {
        let dir = std::env::temp_dir().join(format!("cutex-project-remove-replaced-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let original = json!({"slug":"board","displayName":"Original","lifetime":"persistent",
            "ownerScope":"project","projectId":"proj","ownerCutexSessionId":S1});
        store.register(serde_json::from_value(original.clone()).unwrap(), &Caller::Owner).unwrap();
        let mut policy = gated_policy(&dir);
        let services_dir = dir.join("services");
        let approval_path = dir.join("approvals.json");
        // The first connected callback runs after begin_wait. Replace the
        // record and approve that waiting request before the gate resumes.
        let once = std::sync::atomic::AtomicBool::new(false);
        let replacement = original.clone();
        policy.waiter = Some(crate::approvals::Waiter::new(5, Some("t".into()), None, Some(1), move || {
            if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let other = ServiceStore::new(&services_dir, "host");
                other.remove("board", &Caller::Owner).unwrap();
                let mut replacement = replacement.clone();
                replacement["displayName"] = json!("Replacement");
                other.register(serde_json::from_value(replacement).unwrap(), &Caller::Owner).unwrap();
                let pending = ApprovalStore::at(&approval_path);
                let cards = pending.list(false, chrono::Utc::now()).unwrap();
                assert_eq!(cards.len(), 1);
                pending.decide(&cards[0].approval_id, true, None, chrono::Utc::now()).unwrap();
            }
            true
        }, || true));
        crate::director_address::with_authority_snapshot(&directed_by(S3), || {
            let body = json!({"op":"unregister","args":{"slug":"board"}});
            let result = handle_with_policy(&store, &in_project(S3, "proj"), body.to_string().as_bytes(), &policy);
            assert_eq!(result["code"], "service_changed", "{result}");
            assert_eq!(store.get("board").unwrap().display_name, "Replacement");
            // A retry must obtain a fresh approval for the replacement.
            policy.waiter = None;
            let retried = handle_with_policy(&store, &in_project(S3, "proj"), body.to_string().as_bytes(), &policy);
            assert_eq!(retried["status"], "approval_required", "{retried}");
            assert_eq!(approvals.list(false, chrono::Utc::now()).unwrap().len(), 1);
        });
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn managed_approval_retry_binds_the_inherited_service_baseline() {
        for op in ["register", "update"] {
            let dir = std::env::temp_dir().join(format!("cutex-service-approval-basis-{}", uuid::Uuid::new_v4()));
            let store = ServiceStore::new(dir.join("services"), "host");
            let approvals = ApprovalStore::at(dir.join("approvals.json"));
            let policy = gated_policy(&dir);
            store.register(serde_json::from_value(json!({
                "slug":"board", "runtime":"managed", "command":"old {port}",
                "env":{"MODE":"before"}, "ownerScope":"project", "projectId":"proj",
                "ownerCutexSessionId":S1, "lifetime":"ephemeral"
            })).unwrap(), &Caller::Owner).unwrap();
            crate::director_address::with_authority_snapshot(&directed_by(S3), || {
                let body = json!({"op":op,"args":{"slug":"board","runtime":"managed","command":"new {port}"}});
                let call = || handle_with_policy(&store, &in_project(S3,"proj"), body.to_string().as_bytes(), &policy);
                let first = call();
                assert_eq!(first["status"], "approval_required", "{first}");
                let old_card = approvals.list(false, chrono::Utc::now()).unwrap().remove(0);
                store.update("board", serde_json::from_value(json!({"env":{"MODE":"after"}})).unwrap(), &Caller::Owner).unwrap();
                approvals.decide(first["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now()).unwrap();
                let retry = call();
                assert_eq!(retry["status"], "approval_required", "{retry}");
                assert_ne!(retry["approval_id"], first["approval_id"]);
                assert_eq!(store.get("board").unwrap().command, Some(ServiceCommand::Shell("old {port}".into())));
                assert!(approvals.list(false, chrono::Utc::now()).unwrap().iter()
                    .any(|card| card.request_sha256 != old_card.request_sha256));
            });
        }
    }

    /// Thread-local cutex home with the given sessions (id -> cwd).
    fn static_fixture(sessions: &[(&str, &std::path::Path)], home: &std::path::Path) -> impl Drop {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::config::instance::set_test_selection(
                    crate::config::instance::Selection::Default,
                );
            }
        }
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
            home.to_path_buf(),
        ));
        for (id, cwd) in sessions {
            let record: crate::session::model::CutexSessionRecord = serde_json::from_value(json!({"cutex_session_id":id,"host_id":"fixture","cwd":cwd,"created_at":"2026-10-17T00:00:00Z","updated_at":"2026-10-17T00:00:00Z"})).unwrap();
            let before = crate::session::store::load_cutex_session_store().unwrap();
            let mut after = before.clone();
            after.sessions.insert(id.to_string(), record);
            crate::session::store::save_cutex_session_record(&before, &after, id).unwrap();
        }
        Reset
    }

    #[test]
    fn static_roots_are_validated_before_any_approval_and_outside_roots_ask_the_owner() {
        let base = std::env::temp_dir().join(format!("cutex-static-root-{}", uuid::Uuid::new_v4()));
        let home = base.join("state");
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        let scratch = home.join("agents-scratch").join("20261017-0a1b2c3d");
        let other_scratch = home.join("agents-scratch").join("20261017-ffffffff");
        for path in [&workspace, &outside, &scratch, &other_scratch, &home.join("profiles")] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(outside.join("blank.html"), "<html></html>").unwrap();
        std::fs::write(scratch.join("blank.html"), "<html></html>").unwrap();
        let _reset = static_fixture(&[(S1, &workspace), (S2, &scratch)], &home);
        let store = ServiceStore::new(home.join("services"), "host");
        let approvals = ApprovalStore::at(base.join("approvals.json"));
        let policy = gated_policy(&base);
        let call = |caller: &Caller, body: Value| {
            handle_with_policy(&store, caller, body.to_string().as_bytes(), &policy)
        };
        let pending = || approvals.list(false, chrono::Utc::now()).unwrap();
        let static_args = |slug: &str, root: &std::path::Path, lifetime: &str| {
            json!({"op":"register","args":{"slug":slug,"runtime":"static","root":root,"lifetime":lifetime}})
        };

        // 1. The reported case: a persistent static root inside the cutex
        // home is refused before the owner is asked, with an actionable detail.
        let refused = call(&agent(S1), static_args("blank-persist", &home.join("profiles"), "persistent"));
        assert_eq!(refused["code"], "static_root_forbidden", "{refused}");
        let detail = refused["detail"].as_str().unwrap();
        assert!(detail.contains("profiles") && detail.contains("cutex home"), "{detail}");
        assert!(detail.contains(&*workspace.canonicalize().unwrap().to_string_lossy()), "{detail}");
        // Other invalid requests never reach the owner either.
        assert_eq!(
            call(&agent(S1), json!({"op":"register","args":{"slug":"Bad Slug","lifetime":"persistent"}}))["code"],
            "invalid_slug"
        );
        assert_eq!(
            call(&agent(S1), json!({"op":"register","args":{"slug":"st","runtime":"static","root":outside,"port":9,"lifetime":"persistent"}}))["code"],
            "invalid_request"
        );
        assert_eq!(
            call(&agent(S1), static_args("gone", &base.join("missing"), "persistent"))["code"],
            "invalid_request"
        );
        assert!(pending().is_empty(), "no approval for invalid requests");

        // 2. Persistent + outside root: one service_lifetime approval naming
        // the canonical root; the retry succeeds; a re-register asks nothing.
        let persist = static_args("blank-persist", &outside.join("blank.html"), "persistent");
        let first = call(&agent(S1), persist.clone());
        assert_eq!(first["status"], "approval_required", "{first}");
        let cards = pending();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].kind, ApprovalKind::ServiceLifetime);
        let canonical = outside.join("blank.html").canonicalize().unwrap();
        assert_eq!(cards[0].details["staticRoot"], json!(canonical.to_string_lossy()));
        assert!(cards[0].summary.contains("blank.html"), "{}", cards[0].summary);
        approvals
            .decide(first["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now())
            .unwrap();
        let created = call(&agent(S1), persist.clone());
        assert_eq!(created["ok"], true, "{created}");
        assert_eq!(call(&agent(S1), persist)["ok"], true);

        // 3. Ephemeral + outside root: a static_root approval.
        let eph = static_args("eph", &outside, "ephemeral");
        let gated = call(&agent(S1), eph.clone());
        assert_eq!(gated["status"], "approval_required", "{gated}");
        let card = pending().into_iter().find(|c| c.kind == ApprovalKind::StaticRoot).unwrap();
        assert_eq!(card.details["staticRoot"], json!(outside.canonicalize().unwrap().to_string_lossy()));
        approvals
            .decide(gated["approval_id"].as_str().unwrap(), true, None, chrono::Utc::now())
            .unwrap();
        assert_eq!(call(&agent(S1), eph)["ok"], true);

        // 4. Inside the cwd: no approval.
        let before = pending().len();
        assert_eq!(call(&agent(S1), static_args("ws", &workspace, "ephemeral"))["ok"], true);
        // Update to the cwd: no approval; back outside: a new approval.
        assert_eq!(
            call(&agent(S1), json!({"op":"update","args":{"slug":"eph","root":workspace}}))["ok"],
            true
        );
        let moved = call(&agent(S1), json!({"op":"update","args":{"slug":"eph","root":outside.join("blank.html")}}));
        assert_eq!(moved["status"], "approval_required", "{moved}");
        assert_eq!(pending().len(), before + 1);
        let refused = call(&agent(S1), json!({"op":"update","args":{"slug":"eph","root":home.join("profiles")}}));
        assert_eq!(refused["code"], "static_root_forbidden");
        assert_eq!(pending().len(), before + 1);

        // 5. A scratch-cwd agent serves from its own scratch cwd only.
        assert_eq!(
            call(&agent(S2), static_args("scratch-page", &scratch.join("blank.html"), "ephemeral"))["ok"],
            true
        );
        for forbidden in [home.join("agents-scratch"), other_scratch.clone(), home.join("profiles")] {
            let refused = call(&agent(S2), static_args("scratch-bad", &forbidden, "ephemeral"));
            assert_eq!(refused["code"], "static_root_forbidden", "{forbidden:?}: {refused}");
        }
        // The first agent's cwd is not a scratch dir: the scratch area stays closed to it.
        assert_eq!(
            call(&agent(S1), static_args("x", &scratch, "ephemeral"))["code"],
            "static_root_forbidden"
        );
        // 6. Hard refusals: filesystem root, a directory containing the cutex home.
        for hard in [base.ancestors().last().unwrap().to_path_buf(), base.clone()] {
            assert_eq!(
                call(&agent(S1), static_args("hard", &hard, "ephemeral"))["code"],
                "static_root_forbidden",
                "{hard:?}"
            );
        }
        assert_eq!(pending().len(), before + 1);

        // 7. The store never accepts an unapproved outside root on its own.
        let direct: RegisterRequest = serde_json::from_value(json!({"slug":"direct","runtime":"static","root":outside})).unwrap();
        assert_eq!(store.register(direct.clone(), &agent(S1)).unwrap_err().code, "static_root_forbidden");
        let check = store.validate_register(direct, &agent(S1)).unwrap();
        assert!(check.static_root_approval.is_some());
        assert!(store.get("direct").is_err(), "validate-only writes nothing");
        // The approval flag is not part of the wire format.
        assert!(serde_json::from_value::<RegisterRequest>(json!({"slug":"a","staticRootApproved":true})).is_err());
        drop(_reset);
        std::fs::remove_dir_all(base).unwrap();
    }
}
