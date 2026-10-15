//! Agent-facing services operations, served by the Agent Bus at
//! `POST /api/services/v1` after it authenticated the MCP caller. The caller's
//! durable session is the owner; its current project is the project.
//!
//! Body: `{"op": "register"|"update"|"unregister"|"list", "args": {...}}` with
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
}

impl AgentServicePolicy {
    /// No visibility limit and no approvals (tests, legacy behaviour).
    pub fn permissive() -> Self {
        Self {
            boundary: None,
            managed_within_envelope: true,
            approvals: None,
            requester_name: None,
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
        let managed_within_envelope = record
            .as_ref()
            .and_then(|record| PermissionEnvelope::from_record(record).ok())
            .is_some_and(|envelope| envelope.runs_unsandboxed_without_approval());
        Self {
            boundary,
            managed_within_envelope,
            approvals: ApprovalStore::open_default().ok(),
            requester_name: record.and_then(|record| record.formal_agent_name),
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

fn redact_for(caller: &str, record: &ServiceRecord, mut view: Value) -> Value {
    if record.owner_cutex_session_id.as_deref() != Some(caller) {
        if let Some(object) = view.as_object_mut() {
            for field in PRIVATE_VIEW_FIELDS {
                object.remove(field);
            }
        }
    }
    view
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
) -> Result<(), Value> {
    if policy.managed_within_envelope {
        return Ok(());
    }
    let Caller::Agent {
        cutex_session_id,
        project_id,
    } = caller
    else {
        return Ok(());
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
    let outcome = store.gate(
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
        chrono::Utc::now(),
    );
    match outcome {
        Ok(GateOutcome::Approved(_)) => Ok(()),
        Ok(outcome) => {
            if let GateOutcome::Pending {
                record,
                created: true,
            } = &outcome
            {
                crate::management::v2::approvals::publish_changed(record, "requested");
            }
            Err(outcome.refusal().unwrap_or_default())
        }
        Err(error) => Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            error.to_string(),
        ))),
    }
}

fn lifetime_gate(
    policy: &AgentServicePolicy,
    caller: &Caller,
    op: &str,
    slug: &str,
    args: &Value,
    managed: bool,
) -> Result<(), Value> {
    let Caller::Agent {
        cutex_session_id,
        project_id,
    } = caller
    else {
        return Ok(());
    };
    let Some(store) = &policy.approvals else {
        return Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            "persistent services require owner approval",
        )));
    };
    // One exact-payload approval covers both grants, so consuming one approval
    // cannot strand a request behind a second gate and cause an approval loop.
    let execution = managed && !policy.managed_within_envelope;
    let outcome = store.gate(ApprovalRequest {
        kind: ApprovalKind::ServiceLifetime, reason: "persistent_service".into(),
        requester_cutex_session_id: cutex_session_id.clone(), requester_name: policy.requester_name.clone(), project_id: project_id.clone(),
        summary: format!("Keep service {slug} persistently{}", if execution { "; run its command unrestricted as the owner" } else { "" }),
        details: json!({"op":op,"slug":slug,"lifetime":"persistent","managedExecution":execution,"command":args.get("command"),"cwd":args.get("cwd"),"envKeys":args.get("env").and_then(Value::as_object).map(|v| v.keys().collect::<Vec<_>>()),"grants":if execution { "persistent lifetime and unrestricted execution of this exact command/cwd/env" } else { "persistent lifetime" }}),
        request_sha256: request_digest(&json!({"op":op,"slug":slug,"args":args,"managedExecution":execution})),
    }, chrono::Utc::now());
    match outcome {
        Ok(GateOutcome::Approved(_)) => Ok(()),
        Ok(outcome) => {
            if let GateOutcome::Pending {
                record,
                created: true,
            } = &outcome
            {
                crate::management::v2::approvals::publish_changed(record, "requested");
            }
            Err(outcome.refusal().unwrap_or_default())
        }
        Err(error) => Err(rejected(ServicesError::new(
            503,
            "approvals_unavailable",
            error.to_string(),
        ))),
    }
}

/// The caller's current project (member or Director seat), if any.
pub fn caller_project(cutex_session_id: &str) -> Option<String> {
    let snapshot = crate::memory::load_agent_management_snapshot().ok()?;
    crate::memory::project_for_session(&snapshot, cutex_session_id)
        .map(|id| id.as_str().to_string())
}

/// Serve one authenticated agent request on the default store.
pub fn handle(cutex_session_id: &str, body: &[u8]) -> Value {
    let store = match ServiceStore::open_default() {
        Ok(store) => store,
        Err(error) => return rejected(ServicesError::unavailable(error)),
    };
    let caller = Caller::Agent {
        cutex_session_id: cutex_session_id.to_string(),
        project_id: caller_project(cutex_session_id),
    };
    handle_with_policy(
        &store,
        &caller,
        body,
        &AgentServicePolicy::for_caller(cutex_session_id),
    )
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
            // With `agentPersistentApproval = off` a persistent request is
            // granted without the lifetime approval; a managed command then
            // still goes through the separate `managed_service` gate below.
            let needs_lifetime = registry.settings.agent_persistent_approval
                == AgentPersistentApproval::Ask
                && request.lifetime.as_deref() == Some("persistent")
                && store.get(&request.slug).ok().is_none_or(|record| {
                    record.lifetime != super::store::ServiceLifetime::Persistent
                });
            if needs_lifetime {
                if let Ok(record) = store.get(&request.slug) {
                    if record.owner_cutex_session_id.as_deref() != Some(cutex_session_id.as_str()) {
                        return rejected(ServicesError::new(
                            409,
                            "slug_taken",
                            "service belongs to another owner",
                        ));
                    }
                }
                if let Err(refusal) =
                    lifetime_gate(policy, caller, "register", &request.slug, &args, managed)
                {
                    return refusal;
                }
            }
            let reveal = request.reveal;
            if managed && request.command.is_some() && !needs_lifetime {
                // Gate only what the store would accept: another agent's slug
                // is refused below anyway, and an idempotent re-register of
                // the caller's own identical service changes nothing.
                let existing = match store.load() {
                    Ok(registry) => registry.services.get(&request.slug).cloned(),
                    Err(error) => return rejected(error),
                };
                let skip = existing.is_some_and(|existing| {
                    existing.owner_cutex_session_id.as_deref() != Some(cutex_session_id.as_str())
                        || (existing.runtime == ServiceRuntime::Managed
                            && existing.command == request.command
                            && existing.cwd == request.cwd
                            && request.env.as_ref().is_none_or(|env| *env == existing.env))
                });
                if !skip {
                    if let Err(refusal) = managed_gate(
                        policy,
                        caller,
                        "register",
                        &request.slug,
                        &args,
                        request.command.as_ref(),
                    ) {
                        return refusal;
                    }
                }
            }
            match store.register(request, caller) {
                Ok((record, created)) => {
                    after_change();
                    let state = snapshots(store);
                    let mut result = json!({"ok": true, "created": created, "service": service_view(&record, state.get(&record.slug)), "link": super::store::service_link(&record)});
                    if reveal {
                        result["reveal"] = json!(super::reveal::request(&record, cutex_session_id));
                    }
                    result
                }
                Err(error) => rejected(error),
            }
        }
        "update" => {
            let UpdateArgs { slug, patch } = match serde_json::from_value(args) {
                Ok(args) => args,
                Err(error) => return parse_error(error),
            };
            let request: UpdateRequest = match serde_json::from_value(patch.clone()) {
                Ok(request) => request,
                Err(error) => return parse_error(error),
            };
            let (existing, lifetime_approval) = match store.load() {
                Ok(registry) => (
                    registry.services.get(&slug).cloned(),
                    registry.settings.agent_persistent_approval,
                ),
                Err(error) => return rejected(error),
            };
            // A change to what a managed service runs is gated like a new one;
            // the store itself refuses non-owners.
            if let Some(existing) = existing.filter(|existing| {
                existing.owner_cutex_session_id.as_deref() == Some(cutex_session_id.as_str())
            }) {
                let runtime = match request.runtime.as_deref() {
                    Some(value) => ServiceRuntime::parse(value),
                    None => Some(existing.runtime),
                };
                let changes_process = request.runtime.is_some()
                    || request.command.is_some()
                    || request.cwd.is_some()
                    || request.env.is_some();
                let needs_lifetime = lifetime_approval == AgentPersistentApproval::Ask
                    && request.lifetime.as_deref() == Some("persistent")
                    && existing.lifetime != super::store::ServiceLifetime::Persistent;
                if needs_lifetime {
                    if let Err(refusal) = lifetime_gate(
                        policy,
                        caller,
                        "update",
                        &slug,
                        &patch,
                        runtime == Some(ServiceRuntime::Managed) && changes_process,
                    ) {
                        return refusal;
                    }
                }
                if runtime == Some(ServiceRuntime::Managed) && changes_process && !needs_lifetime {
                    let command = match &request.command {
                        Some(command) => command.clone(),
                        None => existing.command.clone(),
                    };
                    if let Err(refusal) =
                        managed_gate(policy, caller, "update", &slug, &patch, command.as_ref())
                    {
                        return refusal;
                    }
                }
            }
            match store.update(&slug, request, caller) {
                Ok(record) => {
                    after_change();
                    let state = snapshots(store);
                    json!({"ok": true, "service": service_view(&record, state.get(&record.slug))})
                }
                Err(error) => rejected(error),
            }
        }
        "unregister" => {
            let SlugArgs { slug } = match serde_json::from_value(args) {
                Ok(args) => args,
                Err(error) => return parse_error(error),
            };
            match store.remove(&slug, caller) {
                Ok(_) => {
                    after_change();
                    json!({"ok": true, "slug": slug, "removed": true})
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
                    policy
                        .visible(cutex_session_id, record)
                        .then(|| redact_for(cutex_session_id, record, view))
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

    fn gated_policy(dir: &std::path::Path) -> AgentServicePolicy {
        AgentServicePolicy {
            boundary: None,
            managed_within_envelope: false,
            approvals: Some(ApprovalStore::at(dir.join("approvals.json"))),
            requester_name: None,
        }
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
        assert_eq!(call(explicit)["ok"], true);
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
}
