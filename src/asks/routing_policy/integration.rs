//! Real AskStore/tool/HTTP/scheduler adapter. Decisions and admission hold the
//! roster mutation fence, then AskStore. Creation captures a provisional
//! snapshot only; final admission always revalidates it under the fence.
//! No native submit, no second store, and no identity from tool parameters.
use super::*;
use crate::agent_management::{AgentManagementSnapshot, ProjectId, ProjectLifecycle};
use crate::asks::directory::Directory;
use crate::asks::model::{AskRecord, ExecutionState, Requester};
use crate::asks::store::{AskError, AskState};
use chrono::{DateTime, Utc};
use serde_json::json;

pub const ROUTING_CHANGED: &str = "cutex/delegation/routingApprovalChanged";

/// One immutable ownership+roster view, loaded before the AskStore transaction.
/// Hold the authority mutation fence for mutation/admission use. This preserves
/// provenance, accepted handover, owner/reconcile holds, and private ownership;
/// a current Director is never substituted for the recorded controller.
pub struct ControllerView<'a> {
    ownership: crate::director_address::ReadView<'a>,
}

impl<'a> ControllerView<'a> {
    pub fn load(authority: &'a AgentManagementSnapshot) -> Result<Self, AskError> {
        Ok(Self {
            ownership: crate::director_address::ReadView::load(authority)?,
        })
    }

    pub fn controller(&self, ask: &AskRecord) -> Result<Option<String>, AskError> {
        #[cfg(test)]
        if let Some(pin) = crate::asks::model::route::pinned(&ask.ask_id) {
            return pin
                .controller
                .map(|controller| controller.or_else(|| ask.requester.session().map(str::to_owned)))
                .map_err(|code| AskError::new(code, "handover unknown (test)"));
        }
        self.ownership.controller(ask)
    }

    pub fn controlled_by(&self, ask: &AskRecord, caller: &str) -> Result<bool, AskError> {
        Ok(self.controller(ask)?.as_deref() == Some(caller))
    }

    pub fn readable_by(&self, ask: &AskRecord, caller: &str) -> bool {
        if ask.requester.session() == Some(caller) {
            return true;
        }
        #[cfg(test)]
        if let Some(pin) = crate::asks::model::route::pinned(&ask.ask_id) {
            return pin.history.iter().any(|id| id == caller)
                || matches!(pin.controller,Ok(Some(ref controller)) if controller==caller);
        }
        self.ownership.readable_by(ask, caller)
    }
}

pub fn error(error: RoutingError) -> AskError {
    AskError::new(error.code(), "Routing permission changed or is incomplete; inspect this delegation's routing approval and reviewer grant")
}

/// Canonical daemon host identity, shared with authenticated peer records.
pub fn local_host_id() -> String {
    crate::peers::pair::host()
}

/// Scope/role facts from the current roster snapshot under the mutation fence.
/// Remote agent management authority is unsupported; peers use a separate
/// authenticated inbound adapter and never acquire Director/operator authority.
pub fn trusted_context(
    snapshot: &AgentManagementSnapshot,
    project: &str,
    actor: RoutingActor,
    host: &str,
) -> Result<RoutingContext, AskError> {
    let id = ProjectId::new(project.to_owned()).map_err(|_| error(RoutingError::ScopeMismatch))?;
    let (authority, lifecycle) = crate::agent_management::project_entry(snapshot, &id)
        .filter(|(_, state)| state.lifecycle == ProjectLifecycle::Active)
        .ok_or_else(|| error(RoutingError::ScopeMismatch))?;
    let _ = lifecycle;
    let director = authority
        .map(|a| RoutingRef::new(host, a.authorized_director_session.as_str()))
        .transpose()
        .map_err(error)?;
    let mut operator = false;
    if let Some(reference) = actor.reference() {
        if reference.host_id() != host {
            return Err(cross_host_unsupported());
        }
        let member = snapshot
            .agents
            .values()
            .find(|a| a.cutex_session_id.as_str() == reference.id());
        if member.is_some_and(|a| a.retired_at.is_some()) {
            return Err(error(RoutingError::NotAuthorized));
        }
        let member_project = member
            .and_then(|a| crate::agent_management::current_project_id(snapshot, a))
            .or_else(|| {
                snapshot
                    .current_project_memberships
                    .iter()
                    .find(|(s, _)| s.as_str() == reference.id())
                    .and_then(|(_, m)| m.project_id.clone())
            });
        if director.as_ref() != Some(reference) && member_project.as_ref() != Some(&id) {
            return Err(error(RoutingError::ScopeMismatch));
        }
        operator = member_project.as_ref() == Some(&id)
            && snapshot
                .operator_grants
                .get(&id)
                .is_some_and(|grants| grants.keys().any(|s| s.as_str() == reference.id()));
    }
    let ctx = RoutingContext {
        project_id: project.into(),
        authority_epoch: crate::agent_management::effective_authority_epoch(
            snapshot, &id, authority,
        ),
        current_director: director,
        actor,
        operator_may_route: operator,
    };
    ctx.validate().map_err(error)?;
    Ok(ctx)
}

fn actor_of(
    ask: &AskRecord,
    host: &str,
    controllers: &ControllerView<'_>,
) -> Result<RoutingActor, AskError> {
    match &ask.requester {
        Requester::Owner => Ok(RoutingActor::Owner),
        Requester::Agent {
            cutex_session_id,
            name,
        } => {
            let controller = controllers.controller(ask)?;
            let current = controller.as_deref().unwrap_or(cutex_session_id);
            Ok(RoutingActor::Agent {
                reference: RoutingRef::new(host, current).map_err(error)?,
                name: if current == cutex_session_id {
                    name.clone()
                } else {
                    None
                },
            })
        }
        Requester::Peer { name, .. } => {
            let reference = ask
                .routing_request
                .as_ref()
                .and_then(|r| r.requester_ref.clone())
                .ok_or_else(cross_host_unsupported)?;
            Ok(RoutingActor::Agent {
                reference,
                name: name.clone(),
            })
        }
    }
}

fn context_of(
    snapshot: &AgentManagementSnapshot,
    ask: &AskRecord,
    host: &str,
    controllers: &ControllerView<'_>,
) -> Result<RoutingContext, AskError> {
    let project = ask
        .project_id
        .as_deref()
        .ok_or_else(|| error(RoutingError::ScopeMismatch))?;
    if matches!(ask.requester, Requester::Peer { .. }) {
        // The creation adapter obtained the requester ref from a verified peer.
        // Existing peer policy validation remains mandatory on each admission.
        let mut ctx = trusted_context(snapshot, project, RoutingActor::Owner, host)?;
        ctx.actor = actor_of(ask, host, controllers)?;
        ctx.operator_may_route = false;
        Ok(ctx)
    } else {
        trusted_context(snapshot, project, actor_of(ask, host, controllers)?, host)
    }
}

/// Reconstruct actual content/target from the ask; preserve only the captured,
/// authenticated origin. Legacy records without origin retain unknown lineage.
pub fn request_and_root(
    state: &AskState,
    ask: &AskRecord,
    host: &str,
    controllers: &ControllerView<'_>,
) -> Result<(RoutingRequest, RootStatus), AskError> {
    let mut request = match &ask.routing_request {
        Some(captured) => captured.clone(),
        None => RoutingRequest {
            project_id: ask
                .project_id
                .clone()
                .ok_or_else(|| error(RoutingError::ScopeMismatch))?,
            delegation_ref: RoutingRef::new(host, &ask.ask_id).map_err(error)?,
            requester_ref: actor_of(ask, host, controllers)?.reference().cloned(),
            target_ref: RoutingRef::new(host, &ask.target_cutex_session_id).map_err(error)?,
            parent: None,
            root: None,
            source_attempt_id: None,
            source_turn_id: None,
            brief: String::new(),
            attachments: vec![],
        },
    };
    request.project_id = ask
        .project_id
        .clone()
        .ok_or_else(|| error(RoutingError::ScopeMismatch))?;
    request.delegation_ref = RoutingRef::new(host, &ask.ask_id).map_err(error)?;
    request.target_ref = RoutingRef::new(host, &ask.target_cutex_session_id).map_err(error)?;
    request.requester_ref = actor_of(ask, host, controllers)?.reference().cloned();
    let actual_parent = ask
        .parent_ask
        .as_ref()
        .map(|id| RoutingRef::new(host, id))
        .transpose()
        .map_err(error)?;
    if ask.routing_request.is_some() && request.parent != actual_parent {
        return Err(error(RoutingError::ScopeMismatch));
    }
    if let Some(source) = ask
        .source_lineage
        .as_ref()
        .filter(|_| ask.routing_request.is_some())
    {
        if request.source_attempt_id.as_deref() != Some(source.parent_attempt_id.as_str())
            || request.source_turn_id.as_deref() != Some(source.parent_turn_id.as_str())
        {
            return Err(error(RoutingError::ScopeMismatch));
        }
    }
    // The content-addressed full-body hash binds long briefs as well as inline
    // text. Include revisions/options/follow-up, not just the visible prefix.
    let revisions: Vec<_> = ask
        .revisions
        .iter()
        .map(|r| json!({"id":r.id,"text":r.text,"supersedes":r.supersedes}))
        .collect();
    let mut directions: Vec<_> = ask
        .attempts
        .iter()
        .filter_map(|a| a.continuation.as_ref())
        .filter(|c| c.by.is_some() && c.note.is_some())
        .map(|c| json!({"of":c.of,"by":c.by,"reason":c.reason,"note":c.note}))
        .collect();
    if let Some(resume) = ask
        .resume
        .as_ref()
        .filter(|r| r.by.is_some() && r.note.is_some())
    {
        let direction = json!({"of":resume.from_attempt,"by":resume.by,"reason":resume.reason,"note":resume.note});
        if directions.last() != Some(&direction) {
            directions.push(direction);
        }
    }
    request.brief = json!({"body":ask.body,"revisions":revisions,"context":ask.options.context,"followUpOf":ask.follow_up_of,
        "source":ask.source_lineage,"directions":directions}).to_string();
    request.attachments = ask.body.attachments.iter().map(|id| json!({"id":id,
        "bytes":state.artifacts.get(id).map(|a|a.bytes),"mediaType":state.artifacts.get(id).map(|a|&a.media_type)})).collect();
    let root = match request.root.as_ref() {
        Some(root) if root.host_id() == host => match state.asks.get(root.id()) {
            Some(root_ask) if !root_ask.is_open() => RootStatus::Closed(root.clone()),
            Some(_) => RootStatus::VerifiedOpen(root.clone()),
            None => RootStatus::Unknown,
        },
        _ => RootStatus::Unknown,
    };
    request.validate().map_err(error)?;
    Ok((request, root))
}

fn capture_origin(
    state: &AskState,
    ask: &mut AskRecord,
    host: &str,
    peer_host: Option<&str>,
    controllers: &ControllerView<'_>,
) -> Result<(), AskError> {
    // First construct an unknown request, then promote only evidence verified
    // against the active source lease. No most-recent-turn inference.
    if let Requester::Peer {
        cutex_session_id, ..
    } = &ask.requester
    {
        let requester = RoutingRef::new(
            peer_host.ok_or_else(cross_host_unsupported)?,
            cutex_session_id,
        )
        .map_err(error)?;
        ask.routing_request = Some(RoutingRequest {
            project_id: ask
                .project_id
                .clone()
                .ok_or_else(|| error(RoutingError::ScopeMismatch))?,
            delegation_ref: RoutingRef::new(host, &ask.ask_id).map_err(error)?,
            requester_ref: Some(requester),
            target_ref: RoutingRef::new(host, &ask.target_cutex_session_id).map_err(error)?,
            parent: None,
            root: None,
            source_attempt_id: None,
            source_turn_id: None,
            brief: String::new(),
            attachments: vec![],
        });
    }
    let (mut request, _) = request_and_root(state, ask, host, controllers)?;
    // Universal AskState::create_ask has already authenticated and persisted
    // this exact occurrence under this SAME lock. Read that proof; never
    // rebuild a parallel lineage authority or infer a historical root.
    let reference = RoutingRef::new(host, &ask.ask_id).map_err(error)?;
    let record = state
        .lineage
        .get(&reference)
        .ok_or_else(|| error(RoutingError::RootUnverified))?;
    request.root = record.root.clone();
    match &record.parent {
        crate::asks::lineage::ParentFact::Verified(parent) => {
            request.parent = Some(parent.reference.clone());
            request.source_attempt_id = Some(parent.attempt_id.clone());
            request.source_turn_id = Some(parent.turn_id.clone());
        }
        crate::asks::lineage::ParentFact::None => {
            request.parent = None;
            request.source_attempt_id = None;
            request.source_turn_id = ask
                .requester
                .session()
                .and_then(|s| state.presence.get(s))
                .and_then(|p| p.turn_id.clone());
        }
        crate::asks::lineage::ParentFact::Unknown => {
            request.parent = ask
                .parent_ask
                .as_ref()
                .map(|id| RoutingRef::new(host, id))
                .transpose()
                .map_err(error)?;
            request.source_attempt_id = None;
            request.source_turn_id = None;
        }
    }
    ask.routing_request = Some(request);
    Ok(())
}

fn evidence<'a>(approval: Option<&'a str>, grant: Option<&'a str>) -> RouteAuthorization<'a> {
    match (approval, grant) {
        (Some(approval_id), Some(grant_id)) => RouteAuthorization::ApprovalAndGrant {
            approval_id,
            grant_id,
        },
        (Some(id), None) => RouteAuthorization::Approval(id),
        (None, Some(id)) => RouteAuthorization::Grant(id),
        _ => RouteAuthorization::None,
    }
}

/// Shared local/peer creation hook. Ask, pending approval, notice and binding
/// are written atomically by the surrounding candidate-state transaction.
pub fn register_created(
    state: &mut AskState,
    snapshot: &AgentManagementSnapshot,
    ask: &mut AskRecord,
    host: &str,
    peer_host: Option<&str>,
    now: DateTime<Utc>,
    controllers: &ControllerView<'_>,
) -> Result<RouteVerdict, AskError> {
    if ask.project_id.is_none() {
        return Ok(RouteVerdict::Pass(RouteBasis::Open));
    }
    capture_origin(state, ask, host, peer_host, controllers)?;
    let ctx = context_of(snapshot, ask, host, controllers)?;
    let (request, root) = request_and_root(state, ask, host, controllers)?;
    let grant = state
        .routing
        .grants
        .iter()
        .find(|(_, g)| {
            state
                .routing
                .check_grant(g, &ctx, &request, &root, now)
                .is_ok()
        })
        .map(|(id, _)| id.clone());
    let verdict = state
        .routing
        .check_route(&ctx, &request, &root, evidence(None, grant.as_deref()), now)
        .map_err(error)?;
    ask.routing_grant_id = grant;
    if verdict == RouteVerdict::ApprovalRequired {
        if matches!(ask.requester, Requester::Peer { .. }) {
            return Err(cross_host_unsupported());
        }
        let (approval, created) = state
            .routing
            .request_approval(
                &super::super::new_id("routing-approval"),
                &ctx,
                &request,
                &root,
                now,
            )
            .map_err(error)?;
        ask.routing_approval_id = Some(approval.id.clone());
        ask.execution = ExecutionState::PendingReview;
        if created {
            state.notice(ROUTING_CHANGED,ctx.current_director.as_ref().map(|r|vec![r.id().to_owned()]).unwrap_or_else(||vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.into()]),
            json!({"projectId":ctx.project_id,"resourceId":approval.id,"revision":approval.revision}));
        }
    }
    ask.routing_request = Some(request);
    Ok(verdict)
}

/// Called under authority fence + final AskStore write lock. `consume=true`
/// belongs immediately before lease insertion, after every other check.
pub fn check_admission(
    state: &mut AskState,
    snapshot: &AgentManagementSnapshot,
    ask_id: &str,
    host: &str,
    consume: bool,
    now: DateTime<Utc>,
    controllers: &ControllerView<'_>,
) -> Result<RouteVerdict, AskError> {
    let mut ask = state
        .asks
        .get(ask_id)
        .cloned()
        .ok_or_else(|| AskError::new("ask_not_found", "No such delegation"))?;
    if ask.project_id.is_none() {
        return Ok(RouteVerdict::Pass(RouteBasis::Open));
    }
    let ctx = context_of(snapshot, &ask, host, controllers)?;
    let (request, root) = request_and_root(state, &ask, host, controllers)?;
    // A current role/open policy is fresh independent routing authority; do
    // not let a historical evidence pointer accidentally turn that into reuse
    // of an expired grant. There is no grant write on an exempt route.
    let baseline = state
        .routing
        .check_route(&ctx, &request, &root, RouteAuthorization::None, now);
    if let Ok(RouteVerdict::Pass(basis)) = &baseline {
        return Ok(RouteVerdict::Pass(basis.clone()));
    }
    if matches!(baseline, Ok(RouteVerdict::ApprovalRequired)) {
        let digest = request.digest().map_err(error)?;
        let policy_revision = state.routing.policy(&ctx.project_id).revision;
        let renew = ask
            .routing_approval_id
            .as_ref()
            .and_then(|id| state.routing.approvals.get(id))
            .is_none_or(|a| {
                a.request_digest != digest
                    || a.policy_revision != policy_revision
                    || a.authority_epoch != ctx.authority_epoch
                    || matches!(a.state, ApprovalState::Stale | ApprovalState::Cancelled)
            });
        if renew {
            let (approval, created) = state
                .routing
                .request_approval(
                    &crate::asks::new_id("routing-approval"),
                    &ctx,
                    &request,
                    &root,
                    now,
                )
                .map_err(error)?;
            ask.routing_approval_id = Some(approval.id.clone());
            if ask
                .routing_grant_id
                .as_ref()
                .and_then(|id| state.routing.grants.get(id))
                .is_some_and(|g| {
                    state
                        .routing
                        .check_grant(g, &ctx, &request, &root, now)
                        .is_err()
                })
            {
                ask.routing_grant_id = None;
            }
            if created {
                state.notice(ROUTING_CHANGED,ctx.current_director.as_ref().map(|r|vec![r.id().into()]).unwrap_or_else(||vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.into()]),
                    json!({"projectId":ctx.project_id,"resourceId":approval.id,"revision":approval.revision}));
            }
            state.update_ask(ask_id, now, |stored| {
                stored.routing_approval_id = ask.routing_approval_id.clone();
                stored.routing_grant_id = ask.routing_grant_id.clone();
                stored.routing_request = Some(request.clone());
                // A continuation uses resume_pending rather than Queued; keep
                // its execution and let its final admission gate hold it.
                if stored.execution == ExecutionState::Queued {
                    stored.execution = ExecutionState::PendingReview;
                }
            });
        }
    }
    // Find only exact matching evidence; never spend on this read/preflight.
    let grant_valid = ask
        .routing_grant_id
        .as_ref()
        .and_then(|id| state.routing.grants.get(id))
        .is_some_and(|grant| {
            state
                .routing
                .check_grant(grant, &ctx, &request, &root, now)
                .is_ok()
        });
    if !grant_valid {
        let replacement = state
            .routing
            .grants
            .iter()
            .find(|(_, g)| {
                state
                    .routing
                    .check_grant(g, &ctx, &request, &root, now)
                    .is_ok()
            })
            .map(|(id, _)| id.clone());
        // Only a newly issued, independently authorized live grant may
        // replace invalid evidence. Never switch away from a valid binding
        // on retry, and retain the old error if no replacement exists.
        if replacement.is_some() {
            ask.routing_grant_id = replacement;
        }
    }
    let auth = evidence(
        if state.routing.policy(&ctx.project_id).member_mode == MemberMode::ApprovalRequired {
            ask.routing_approval_id.as_deref()
        } else {
            None
        },
        ask.routing_grant_id.as_deref(),
    );
    // This is one authorization occurrence of the original business action.
    // Exact retries/automatic rounds keep it; explicit changed instructions or
    // a new authority/policy epoch require a fresh approval AND a fresh grant.
    // It is not a scheduler tick ID and cannot double-spend the same snapshot.
    let admission_action=format!("routing:{}",crate::asks::sha256_hex(
        json!({"action":ask.action_id,"digest":request.digest().map_err(error)?,"policy":state.routing.policy(&ctx.project_id).revision,"epoch":ctx.authority_epoch,
            "approval":ask.routing_approval_id,"grant":ask.routing_grant_id}).to_string().as_bytes()));
    let result = if consume {
        state
            .routing
            .consume_route(&ctx, &request, &root, auth, &admission_action, now)
    } else {
        state.routing.check_route(&ctx, &request, &root, auth, now)
    };
    if result == Err(RoutingError::ApprovalDenied) {
        let was_waiting = state.asks.get(ask_id).is_some_and(|stored|
            matches!(stored.execution, ExecutionState::Queued | ExecutionState::PendingReview));
        state.update_ask(ask_id, now, |stored| {
            if matches!(
                stored.execution,
                ExecutionState::Queued | ExecutionState::PendingReview
            ) {
                stored.execution = ExecutionState::Rejected;
                stored.closed = Some(crate::asks::model::CloseOutcome::Completed);
                stored.closed_at = Some(crate::asks::now_rfc3339(now));
                stored.hold = None;
            }
        });
        if was_waiting {
            if let Some(rejected) = state.asks.get(ask_id).cloned() {
                crate::asks::delivery::record_state_delivery(state, &rejected, "rejected", now);
            }
        }
    }
    let verdict = result.map_err(error)?;
    if let Some(stored) = state.asks.get_mut(ask_id) {
        stored.routing_grant_id = ask.routing_grant_id;
    }
    Ok(verdict)
}

/// Scheduler review refresh; does not consume a grant. Capability approval is
/// still checked separately by gate.rs. Return true only when routing is ready.
pub fn refresh_pending(
    state: &mut AskState,
    snapshot: &AgentManagementSnapshot,
    ask_id: &str,
    host: &str,
    now: DateTime<Utc>,
    controllers: &ControllerView<'_>,
) -> Result<bool, AskError> {
    let verdict = check_admission(state, snapshot, ask_id, host, false, now, controllers)?;
    let ready = matches!(verdict, RouteVerdict::Pass(_));
    let capability_ready = state
        .asks
        .get(ask_id)
        .and_then(|a| a.gate.as_ref())
        .is_none_or(|g| !g.required);
    if let Some(ask) = state.asks.get_mut(ask_id) {
        if ready && capability_ready && ask.execution == ExecutionState::PendingReview {
            ask.execution = ExecutionState::Queued;
        }
        if !ready && ask.execution == ExecutionState::Queued {
            ask.execution = ExecutionState::PendingReview;
        }
    }
    Ok(ready)
}

pub fn cross_host_unsupported() -> AskError {
    AskError::new("delegation_cross_host_routing_unsupported",
    "The peer contract has no verified project/root reviewer-grant evidence; this route requires upgraded linked evidence")
}

/// Lean authenticated mutation fence, with no Director Store or optional
/// bookkeeping/pruning. Never nest this fence or perform native RPC inside.
/// Runtime/bridge admission must use a nonblocking delivery-lock variant.
pub fn with_authority<T>(
    run: impl FnOnce(&AgentManagementSnapshot) -> Result<T, AskError>,
) -> Result<T, AskError> {
    let roster = crate::agent_management::AgentManagementStore::open_default().map_err(|e| {
        AskError::new(
            "persistence_unavailable",
            format!("routing authority: {e:#}"),
        )
    })?;
    let _fence = roster.lock_mutations().map_err(|e| {
        AskError::new(
            "persistence_unavailable",
            format!("routing authority fence: {e}"),
        )
    })?;
    let snapshot = roster.snapshot().map_err(|e| {
        AskError::new(
            "persistence_unavailable",
            format!("routing authority snapshot: {e}"),
        )
    })?;
    crate::director_address::with_authority_snapshot(&snapshot, || run(&snapshot))
}

/// Ordinary creation only records a provisional routing request. It consumes
/// no grant and performs no native work. Never wait on a lifecycle mutation
/// here: lifecycle may be joining the tool's bridge worker. A changed roster,
/// epoch, membership or policy must pass the final fenced admission again.
/// Role-addressed creation already has a fenced snapshot and does not use this.
pub fn with_creation_snapshot_for_world<T>(
    world: &dyn crate::asks::scheduler::SchedulerWorld,
    run: impl FnOnce(&AgentManagementSnapshot) -> Result<T, AskError>,
) -> Result<T, AskError> {
    #[cfg(test)]
    if let Some(fixture) = world.routing_fixture() {
        return run(&fixture.snapshot);
    }
    let _ = world;
    let snapshot = Directory::director_snapshot()?;
    run(&snapshot)
}

/// Admission/delivery never waits behind lifecycle joining bridge workers.
/// None means defer without running the callback or touching AskStore. There
/// is no pruning, Director Store, retry loop or native RPC inside this fence.
pub fn try_with_authority_for_world<T>(
    world: &dyn crate::asks::scheduler::SchedulerWorld,
    run: impl FnOnce(&AgentManagementSnapshot) -> Result<T, AskError>,
) -> Result<Option<T>, AskError> {
    let roster = crate::agent_management::AgentManagementStore::open_default().map_err(|e| {
        AskError::new(
            "persistence_unavailable",
            format!("routing authority: {e:#}"),
        )
    })?;
    let Some(_fence) = roster.try_lock_delivery_mutations().map_err(|e| {
        AskError::new(
            "persistence_unavailable",
            format!("routing authority fence: {e}"),
        )
    })?
    else {
        return Ok(None);
    };
    let snapshot = roster.snapshot().map_err(|e| {
        AskError::new(
            "persistence_unavailable",
            format!("routing authority snapshot: {e}"),
        )
    })?;
    #[cfg(test)]
    if let Some(fixture) = world.routing_fixture() {
        return crate::director_address::with_authority_snapshot(&fixture.snapshot, || run(&fixture.snapshot)).map(Some);
    }
    let _ = world;
    crate::director_address::with_authority_snapshot(&snapshot, || run(&snapshot)).map(Some)
}

/// Same production authority fence; tests may supply explicit synthetic roster
/// fixtures through their SchedulerWorld. No production option/bypass exists.
pub fn with_authority_for_world<T>(
    world: &dyn crate::asks::scheduler::SchedulerWorld,
    run: impl FnOnce(&AgentManagementSnapshot) -> Result<T, AskError>,
) -> Result<T, AskError> {
    with_authority(|snapshot| {
        #[cfg(test)]
        if let Some(fixture) = world.routing_fixture() {
            return crate::director_address::with_authority_snapshot(&fixture.snapshot, || run(&fixture.snapshot));
        }
        let _ = world;
        run(snapshot)
    })
}

pub fn configure_test_world(
    state: &mut AskState,
    world: &dyn crate::asks::scheduler::SchedulerWorld,
    now: DateTime<Utc>,
) -> Result<(), AskError> {
    #[cfg(test)]
    if let Some(fixture) = world.routing_fixture() {
        return super::test_support::configure(state, &fixture, now);
    }
    let _ = (state, world, now);
    Ok(())
}

/// Minimal preflight for source-host peer enqueue. No cross-host grant/root
/// claims can be verified by the v1 peer wire. Exempt trusted Director/owner
/// or open policy still use all original peer visibility/capability checks.
pub fn check_peer_source(
    snapshot: &AgentManagementSnapshot,
    state: &AskState,
    directory: &Directory,
    caller: &str,
    host: &str,
) -> Result<(), AskError> {
    let Some(project) = directory.get(caller).and_then(|a| a.project.as_deref()) else {
        return Ok(());
    };
    let ctx = trusted_context(
        snapshot,
        project,
        RoutingActor::Agent {
            reference: RoutingRef::new(host, caller).map_err(error)?,
            name: None,
        },
        host,
    )?;
    if ctx.management_authority().is_ok()
        || ctx.operator_may_route
        || state.routing.policy(project).member_mode == MemberMode::Open
    {
        Ok(())
    } else {
        Err(cross_host_unsupported())
    }
}

/// Queued peer source admission uses the persisted original project/requester,
/// not an absent Directory entry (which must not become a nonproject bypass).
/// v1 carries no verified cross-host grant/root evidence for a restricted route.
pub fn check_outbound_source(
    snapshot: &AgentManagementSnapshot,
    state: &AskState,
    ask: &AskRecord,
    host: &str,
    controllers: &ControllerView<'_>,
) -> Result<(), AskError> {
    crate::asks::lineage::check_admission(state, host, &ask.ask_id)?;
    if !ask.is_open() {
        return Err(AskError::new("ask_closed", "Delegation is closed"));
    }
    let paused = |source: &str| {
        crate::asks::pause_admission::is_paused_in(state, snapshot, source)
            || state.pauses.contains_key(source)
    };
    if ask.requester.session().is_some_and(paused) {
        return Err(AskError::new(
            "routing_source_paused",
            "Delegation source is paused",
        ));
    }
    if ask.project_id.is_none() {
        return Ok(());
    }
    let ctx = context_of(snapshot, ask, host, controllers)?;
    if ctx.actor.reference().is_some_and(|actor| paused(actor.id())) {
        return Err(AskError::new(
            "routing_source_paused",
            "Delegation controller is paused",
        ));
    }
    if ctx.management_authority().is_ok()
        || ctx.operator_may_route
        || state.routing.policy(&ctx.project_id).member_mode == MemberMode::Open
    {
        Ok(())
    } else {
        Err(cross_host_unsupported())
    }
}
