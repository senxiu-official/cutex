use super::*;
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;

impl RoutingState {
    /// Scope visibility, membership, exact root provenance and authority must
    /// already have been verified by caller. IDs are allocated by the daemon.
    /// No grant can be edited, extended, re-targeted or delegated onward.
    pub fn create_grant(
        &mut self,
        id: &str,
        ctx: &RoutingContext,
        input: &CreateGrantRequest,
        root: &RootStatus,
        now: DateTime<Utc>,
    ) -> RoutingResult<DelegationGrant> {
        ctx.management_authority()?;
        validate_id(id)?;
        validate_id(&input.action_id)?;
        root.require_open(&input.root_delegation_ref)?;
        if input.max_uses != 1
            || input.target_refs.is_empty()
            || input.target_refs.len() > 64
            || input.expires_at.is_some_and(|expiry| expiry <= now)
            || input
                .target_refs
                .iter()
                .any(|target| target == &input.requester_ref)
            || input.target_refs.iter().collect::<BTreeSet<_>>().len() != input.target_refs.len()
        {
            return Err(RoutingError::InvalidGrant);
        }
        if self.grants.contains_key(id) {
            return Err(RoutingError::ResourceExists);
        }
        let mut targets = input.target_refs.clone();
        targets.sort();
        let grant = DelegationGrant {
            id: id.into(),
            project_id: ctx.project_id.clone(),
            requester_ref: input.requester_ref.clone(),
            target_refs: targets,
            root_delegation_ref: input.root_delegation_ref.clone(),
            purpose: GrantPurpose::Review,
            max_uses: 1,
            used_count: 0,
            expires_at: input.expires_at,
            authority_epoch: ctx.authority_epoch,
            policy_revision: self.policy(&ctx.project_id).revision,
            revision: 1,
            created_by: ctx.actor.audit(),
            created_at: now,
            revoked_at: None,
            revoked_by: None,
            root_closed_at: None,
            consumption: None,
        };
        self.grants.insert(id.into(), grant.clone());
        Ok(grant)
    }

    /// Expired/exhausted/old-epoch grants remain revocable for audit. Revoke
    /// cannot restore use count; management must replay its outer action row.
    pub fn revoke_grant(
        &mut self,
        id: &str,
        ctx: &RoutingContext,
        input: &RevokeGrantRequest,
        now: DateTime<Utc>,
    ) -> RoutingResult<DelegationGrant> {
        ctx.management_authority()?;
        validate_id(&input.action_id)?;
        let grant = self.grants.get_mut(id).ok_or(RoutingError::GrantNotFound)?;
        if grant.project_id != ctx.project_id {
            return Err(RoutingError::ScopeMismatch);
        }
        if grant.revision != input.expected_revision {
            return Err(RoutingError::GrantRevisionConflict);
        }
        if grant.revoked_at.is_some() {
            return Err(RoutingError::GrantRevoked);
        }
        let revision = next_revision(grant.revision)?;
        grant.revoked_by = Some(ctx.actor.audit());
        grant.revoked_at = Some(now);
        grant.revision = revision;
        Ok(grant.clone())
    }

    pub(super) fn check_grant(
        &self,
        grant: &DelegationGrant,
        ctx: &RoutingContext,
        request: &RoutingRequest,
        root: &RootStatus,
        now: DateTime<Utc>,
    ) -> RoutingResult<()> {
        if grant.project_id != ctx.project_id
            || Some(&grant.requester_ref) != request.requester_ref.as_ref()
            || request.root.as_ref() != Some(&grant.root_delegation_ref)
        {
            return Err(RoutingError::ScopeMismatch);
        }
        root.require_open(&grant.root_delegation_ref)?;
        if grant.root_closed_at.is_some() {
            return Err(RoutingError::RootClosed);
        }
        if grant.revoked_at.is_some() {
            return Err(RoutingError::GrantRevoked);
        }
        if grant.expires_at.is_some_and(|expiry| expiry <= now) {
            return Err(RoutingError::GrantExpired);
        }
        if grant.authority_epoch != ctx.authority_epoch
            || grant.policy_revision != self.policy(&ctx.project_id).revision
        {
            return Err(RoutingError::GrantStale);
        }
        // Also fail closed on malformed persisted authorization state.
        if grant.max_uses != 1
            || grant.used_count > 1
            || grant.target_refs.is_empty()
            || grant.consumption.is_some() != (grant.used_count == 1)
        {
            return Err(RoutingError::InvalidGrant);
        }
        if !grant.target_refs.contains(&request.target_ref) {
            return Err(RoutingError::TargetNotGranted);
        }
        if let Some(used) = &grant.consumption {
            if used.grant_id != grant.id || used.requester_ref != grant.requester_ref {
                return Err(RoutingError::InvalidGrant);
            }
            if used.delegation_ref != request.delegation_ref
                || used.request_digest != request.digest()?
            {
                return Err(RoutingError::GrantExhausted);
            }
        }
        Ok(())
    }

    /// Call in the SAME AskStore lock as closing/stopping/archiving the root.
    /// This marker is terminal even if that root is explicitly reopened later.
    /// Current-root checks still fail closed before eager invalidation runs.
    pub fn close_root_grants(
        &mut self,
        root: &RoutingRef,
        now: DateTime<Utc>,
    ) -> RoutingResult<usize> {
        let affected: Vec<_> = self
            .grants
            .iter()
            .filter(|(_, g)| &g.root_delegation_ref == root && g.root_closed_at.is_none())
            .map(|(id, g)| Ok((id.clone(), next_revision(g.revision)?)))
            .collect::<RoutingResult<_>>()?;
        let count = affected.len();
        for (id, revision) in affected {
            let grant = self.grants.get_mut(&id).expect("selected grant");
            grant.root_closed_at = Some(now);
            grant.revision = revision;
        }
        Ok(count)
    }
}
