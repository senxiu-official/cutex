//! Complete one approved local project review, never a broader route or grant.
use super::*;

pub fn complete_approved_review(
    state: &mut AskState,
    snapshot: &AgentManagementSnapshot,
    ask_id: &str,
    host: &str,
    now: DateTime<Utc>,
    controllers: &ControllerView<'_>,
) -> Result<(), AskError> {
    let Some(mut ask) = state.asks.get(ask_id).cloned() else {
        return Ok(());
    };
    if ensure_grant(state, snapshot, &mut ask, host, now, controllers)? {
        state.update_ask(ask_id, now, |stored| {
            stored.routing_grant_id = ask.routing_grant_id.clone()
        });
    }
    Ok(())
}

pub(super) fn ensure_grant(
    state: &mut AskState,
    snapshot: &AgentManagementSnapshot,
    ask: &mut AskRecord,
    host: &str,
    now: DateTime<Utc>,
    controllers: &ControllerView<'_>,
) -> Result<bool, AskError> {
    if ask.remote.is_some()
        || !ask.is_open()
        || !matches!(ask.requester, Requester::Agent { .. })
        || ask.routing_grant_id.is_some()
    {
        return Ok(false);
    }
    let Some(project) = ask.project_id.as_deref() else {
        return Ok(false);
    };
    if state.routing.policy(project).member_mode != MemberMode::ApprovalRequired {
        return Ok(false);
    }
    let Some(approval) = ask
        .routing_approval_id
        .as_ref()
        .and_then(|id| state.routing.approvals.get(id))
        .cloned()
        .filter(|a| a.state == ApprovalState::Approved)
    else {
        return Ok(false);
    };
    let ctx = context_of(snapshot, ask, host, controllers)?;
    let (request, root) = request_and_root(state, ask, host, controllers)?;
    if state
        .routing
        .check_approval(&approval, &ctx, &request)
        .is_err()
    {
        return Ok(false);
    }
    let RootStatus::VerifiedOpen(root_ref) = &root else {
        return Ok(false);
    };
    if root_ref.host_id() != host
        || !state.asks.get(root_ref.id()).is_some_and(|root_ask| {
            root_ask.remote.is_none() && root_ask.project_id.as_deref() == Some(project)
        })
    {
        return Ok(false);
    }
    let Some(requester) = request
        .requester_ref
        .as_ref()
        .filter(|r| r.host_id() == host)
    else {
        return Ok(false);
    };
    if request.target_ref.host_id() != host {
        return Ok(false);
    }
    // Both participants must still be local members, not merely linked/visible.
    for reference in [requester, &request.target_ref] {
        if trusted_context(
            snapshot,
            project,
            RoutingActor::Agent {
                reference: reference.clone(),
                name: None,
            },
            host,
        )
        .is_err()
        {
            return Ok(false);
        }
    }
    let actor =
        match approval.decision_authority {
            Some(DecisionAuthority::Owner) => RoutingActor::Owner,
            Some(DecisionAuthority::Director) => {
                let Some(reference) = approval.decided_by.as_ref().and_then(|by| {
                    RoutingRef::new(by.host_id.clone()?, by.session_id.clone()?).ok()
                }) else {
                    return Ok(false);
                };
                RoutingActor::Agent {
                    reference,
                    name: None,
                }
            }
            None => return Ok(false),
        };
    // A historical decider can leave without invalidating an independently
    // issued live grant. Repair is opportunistic, never a new routing gate.
    let Ok(manager) = trusted_context(snapshot, project, actor, host) else {
        return Ok(false);
    };
    if manager.management_authority().is_err() {
        return Ok(false);
    }
    if let Some(existing) = state.routing.grants.values().find(|grant| {
        state
            .routing
            .check_grant(grant, &ctx, &request, &root, now)
            .is_ok()
    }) {
        ask.routing_grant_id = Some(existing.id.clone());
        return Ok(true);
    }
    let id = format!(
        "review-grant-{}",
        crate::asks::sha256_hex(approval.id.as_bytes())
    );
    // Never replace or revive a revoked/expired/consumed grant on replay.
    if let Some(grant) = state.routing.grants.get(&id) {
        if state
            .routing
            .check_grant(grant, &ctx, &request, &root, now)
            .is_err()
        {
            return Ok(false);
        }
    } else {
        let input = CreateGrantRequest {
            action_id: format!("review:{}", approval.id),
            requester_ref: requester.clone(),
            target_refs: vec![request.target_ref.clone()],
            root_delegation_ref: root_ref.clone(),
            purpose: GrantPurpose::Review,
            max_uses: 1,
            expires_at: None,
        };
        state
            .routing
            .create_grant(&id, &manager, &input, &root, now)
            .map_err(error)?;
    }
    ask.routing_grant_id = Some(id);
    Ok(true)
}
