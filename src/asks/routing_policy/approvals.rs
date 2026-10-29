use super::*;
use chrono::{DateTime, Utc};

impl RoutingState {
    /// Returns (record, newly_created). Pending is durable, not a no-write
    /// rejection: parent records ask+approval+notice+action receipt atomically.
    pub fn request_approval(
        &mut self,
        id: &str,
        ctx: &RoutingContext,
        request: &RoutingRequest,
        root: &RootStatus,
        now: DateTime<Utc>,
    ) -> RoutingResult<(RoutingApproval, bool)> {
        validate_id(id)?;
        if self.check_route(ctx, request, root, RouteAuthorization::None, now)?
            != RouteVerdict::ApprovalRequired
        {
            return Err(RoutingError::ScopeMismatch);
        }
        let digest = request.digest()?;
        let policy_revision = self.policy(&ctx.project_id).revision;
        if let Some(existing) = self.approvals.values().find(|a| {
            a.delegation_ref == request.delegation_ref
                && a.project_id == ctx.project_id
                && a.request_digest == digest
                && a.policy_revision == policy_revision
                && a.authority_epoch == ctx.authority_epoch
                && matches!(
                    a.state,
                    ApprovalState::Pending | ApprovalState::Approved | ApprovalState::Denied
                )
        }) {
            return Ok((existing.clone(), false));
        }
        if self.approvals.contains_key(id) {
            return Err(RoutingError::ResourceExists);
        }
        let superseded: Vec<_> = self
            .approvals
            .iter()
            .filter(|(_, a)| {
                a.project_id == ctx.project_id
                    && a.delegation_ref == request.delegation_ref
                    && matches!(a.state, ApprovalState::Pending | ApprovalState::Approved)
            })
            .map(|(id, a)| Ok((id.clone(), next_revision(a.revision)?)))
            .collect::<RoutingResult<_>>()?;
        let approval = RoutingApproval {
            id: id.into(),
            project_id: ctx.project_id.clone(),
            delegation_ref: request.delegation_ref.clone(),
            request_digest: digest,
            policy_revision,
            authority_epoch: ctx.authority_epoch,
            revision: 1,
            state: ApprovalState::Pending,
            requested_by: ctx.actor.audit(),
            decided_by: None,
            decision_authority: None,
            decision_note: None,
            created_at: now,
            decided_at: None,
        };
        for (id, revision) in superseded {
            let old = self.approvals.get_mut(&id).expect("selected approval");
            old.state = ApprovalState::Stale;
            old.revision = revision;
        }
        self.approvals.insert(id.into(), approval.clone());
        Ok((approval, true))
    }

    /// First valid CAS wins. Parent must check existing action BEFORE this
    /// method; an exact HTTP retry replays that receipt, rather than deciding
    /// the resource twice. `request` is reconstructed from current ask facts.
    pub fn decide_approval(
        &mut self,
        id: &str,
        ctx: &RoutingContext,
        request: &RoutingRequest,
        root: &RootStatus,
        input: &DecideRoutingRequest,
        now: DateTime<Utc>,
    ) -> RoutingResult<RoutingApproval> {
        let authority = ctx.management_authority()?;
        validate_id(&input.action_id)?;
        validate_note(input.decision_note.as_deref())?;
        root.check_request(request)?;
        let approval = self
            .approvals
            .get(id)
            .ok_or(RoutingError::ApprovalNotFound)?;
        self.check_approval(approval, ctx, request)?;
        if matches!(
            approval.state,
            ApprovalState::Approved | ApprovalState::Denied
        ) {
            return Err(RoutingError::ApprovalAlreadyDecided);
        }
        if approval.state != ApprovalState::Pending {
            return Err(RoutingError::ApprovalStale);
        }
        if input.expected_revision != approval.revision {
            return Err(RoutingError::ApprovalRevisionConflict);
        }
        if input.expected_digest != approval.request_digest {
            return Err(RoutingError::DigestMismatch);
        }
        let revision = next_revision(approval.revision)?;
        let approval = self.approvals.get_mut(id).expect("checked approval");
        approval.state = match input.decision {
            RoutingDecision::Approve => ApprovalState::Approved,
            RoutingDecision::Deny => ApprovalState::Denied,
        };
        approval.decided_by = Some(ctx.actor.audit());
        approval.decision_authority = Some(authority);
        approval.decision_note = input.decision_note.clone();
        approval.decided_at = Some(now);
        approval.revision = revision;
        Ok(approval.clone())
    }

    pub(super) fn check_approval(
        &self,
        approval: &RoutingApproval,
        ctx: &RoutingContext,
        request: &RoutingRequest,
    ) -> RoutingResult<()> {
        if approval.project_id != ctx.project_id
            || request.project_id != ctx.project_id
            || approval.delegation_ref != request.delegation_ref
        {
            return Err(RoutingError::ScopeMismatch);
        }
        if approval.authority_epoch != ctx.authority_epoch
            || approval.policy_revision != self.policy(&ctx.project_id).revision
        {
            return Err(RoutingError::ApprovalStale);
        }
        if approval.request_digest != request.digest()? {
            return Err(RoutingError::DigestMismatch);
        }
        // An approved legacy/corrupt record without actual decision evidence
        // is displayable, but cannot authorize delivery.
        if approval.state == ApprovalState::Approved && !valid_decision_audit(approval) {
            return Err(RoutingError::ApprovalStale);
        }
        Ok(())
    }

    /// Stops/root closure cancel pending work without erasing old decisions.
    /// Parent selects only actually affected refs, under its stop fence lock.
    pub fn cancel_for_delegation(&mut self, delegation: &RoutingRef) -> RoutingResult<usize> {
        let affected: Vec<_> = self
            .approvals
            .iter()
            .filter(|(_, a)| {
                &a.delegation_ref == delegation
                    && matches!(a.state, ApprovalState::Pending | ApprovalState::Approved)
            })
            .map(|(id, a)| Ok((id.clone(), next_revision(a.revision)?)))
            .collect::<RoutingResult<_>>()?;
        let count = affected.len();
        for (id, revision) in affected {
            let a = self.approvals.get_mut(&id).expect("selected approval");
            a.state = ApprovalState::Cancelled;
            a.revision = revision;
        }
        Ok(count)
    }

    pub fn approval_actions(
        &self,
        id: &str,
        ctx: &RoutingContext,
        request: &RoutingRequest,
        root: &RootStatus,
    ) -> ApprovalActions {
        let can = ctx.management_authority().is_ok()
            && root.check_request(request).is_ok()
            && self.approvals.get(id).is_some_and(|a| {
                a.state == ApprovalState::Pending && self.check_approval(a, ctx, request).is_ok()
            });
        ApprovalActions {
            approve: can,
            deny: can,
        }
    }
}

fn validate_note(note: Option<&str>) -> RoutingResult<()> {
    if note.is_some_and(|s| {
        s.chars().count() > 200
            || s.chars()
                .any(|c| matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}'))
    }) {
        return Err(RoutingError::InvalidDecisionNote);
    }
    Ok(())
}

fn valid_decision_audit(approval: &RoutingApproval) -> bool {
    let Some(actor) = &approval.decided_by else {
        return false;
    };
    if approval.decided_at.is_none() || actor.redacted {
        return false;
    }
    match approval.decision_authority {
        Some(DecisionAuthority::Owner) => {
            actor.kind == ActorKind::Owner
                && actor.host_id.is_none()
                && actor.session_id.is_none()
                && actor.name.is_none()
        }
        Some(DecisionAuthority::Director) => {
            actor.kind == ActorKind::Agent
                && actor.host_id.as_deref().is_some_and(|s| !s.is_empty())
                && actor.session_id.as_deref().is_some_and(|s| !s.is_empty())
        }
        None => false,
    }
}
