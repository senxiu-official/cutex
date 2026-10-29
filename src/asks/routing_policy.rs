//! Routing permission only. Embed `RoutingState` in `AskState`; call mutations
//! inside its existing `AskStore::with_state` lock, together with the ask/action
//! write. This module has no storage, locks, runtime calls or approval engine.
//! Capability escalation, visibility, peer links, pause/stop fences and attempt
//! admission remain mandatory caller checks, even when this gate returns Pass.
//!
//! Context/root facts must be rebuilt from trusted daemon state under the same
//! authority write ordering as Director rotation. Never deserialize a context
//! from HTTP/MCP. Wire mutation idempotency uses AskState's existing action map;
//! grant consumption additionally binds its one use to a stable action+request.

mod approvals;
mod grants;
pub mod integration;
#[cfg(test)]
pub mod test_support;
pub mod tools;
mod types;

pub use types::*;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingState {
    #[serde(default)]
    pub policies: BTreeMap<String, ProjectDelegationPolicy>,
    #[serde(default)]
    pub grants: BTreeMap<String, DelegationGrant>,
    #[serde(default)]
    pub approvals: BTreeMap<String, RoutingApproval>,
}

impl RoutingState {
    pub fn policy(&self, project_id: &str) -> ProjectDelegationPolicy {
        self.policies
            .get(project_id)
            .cloned()
            .unwrap_or_else(|| ProjectDelegationPolicy {
                project_id: project_id.into(),
                revision: 0,
                member_mode: MemberMode::ApprovalRequired,
                updated_by: None,
                updated_at: None,
            })
    }

    /// Caller wraps this in begin_action/finish_action in the SAME store lock.
    pub fn update_policy(
        &mut self,
        ctx: &RoutingContext,
        input: &UpdatePolicyRequest,
        now: DateTime<Utc>,
    ) -> RoutingResult<ProjectDelegationPolicy> {
        ctx.management_authority()?;
        validate_id(&input.action_id)?;
        let old = self.policy(&ctx.project_id);
        if input.expected_revision != old.revision {
            return Err(RoutingError::PolicyRevisionConflict);
        }
        let updated = ProjectDelegationPolicy {
            revision: next_revision(old.revision)?,
            member_mode: input.member_mode,
            updated_by: Some(ctx.actor.audit()),
            updated_at: Some(now),
            ..old
        };
        self.invalidate_project(&ctx.project_id, updated.revision, ctx.authority_epoch)?;
        self.policies
            .insert(ctx.project_id.clone(), updated.clone());
        Ok(updated)
    }

    /// Fail closed on changed authority/policy. Call on rotation as well as
    /// before projecting allowedActions. Historical decision actors are kept.
    /// Every mutation preflights overflow, so an error cannot partly write.
    pub fn invalidate_project(
        &mut self,
        project: &str,
        policy_revision: u64,
        authority_epoch: u64,
    ) -> RoutingResult<usize> {
        let stale: Vec<_> = self
            .approvals
            .iter()
            .filter(|(_, a)| {
                a.project_id == project
                    && matches!(a.state, ApprovalState::Pending | ApprovalState::Approved)
                    && (a.policy_revision != policy_revision
                        || a.authority_epoch != authority_epoch)
            })
            .map(|(id, a)| Ok((id.clone(), next_revision(a.revision)?)))
            .collect::<RoutingResult<_>>()?;
        let count = stale.len();
        for (id, revision) in stale {
            let a = self.approvals.get_mut(&id).expect("selected approval");
            a.state = ApprovalState::Stale;
            a.revision = revision;
        }
        Ok(count)
    }

    /// Pure preflight: never spends a grant. Use at creation/review; actual
    /// admission must call consume_route again with current trusted facts.
    pub fn check_route(
        &self,
        ctx: &RoutingContext,
        request: &RoutingRequest,
        root: &RootStatus,
        authorization: RouteAuthorization<'_>,
        now: DateTime<Utc>,
    ) -> RoutingResult<RouteVerdict> {
        ctx.validate()?;
        request.validate()?;
        if request.project_id != ctx.project_id
            || request.requester_ref.as_ref() != ctx.actor.reference()
        {
            return Err(RoutingError::ScopeMismatch);
        }
        root.check_request(request)?;
        if ctx.actor.is_owner() {
            return Ok(RouteVerdict::Pass(RouteBasis::Owner));
        }
        if ctx.actor.reference() == ctx.current_director.as_ref() {
            return Ok(RouteVerdict::Pass(RouteBasis::Director));
        }
        if ctx.operator_may_route {
            return Ok(RouteVerdict::Pass(RouteBasis::Operator));
        }
        if ctx.current_director.as_ref() == Some(&request.target_ref) {
            return Ok(RouteVerdict::Pass(RouteBasis::ToDirector));
        }
        let policy = self.policy(&ctx.project_id);
        // Explicit evidence is validated even in open mode: a named expired or
        // revoked grant cannot silently acquire new meaning after a change.
        match authorization {
            RouteAuthorization::Grant(id) => {
                let grant = self.grants.get(id).ok_or(RoutingError::GrantNotFound)?;
                self.check_grant(grant, ctx, request, root, now)?;
                if policy.member_mode == MemberMode::ApprovalRequired {
                    return Ok(RouteVerdict::ApprovalRequired);
                }
                Ok(RouteVerdict::Pass(RouteBasis::Grant {
                    grant_id: id.into(),
                }))
            }
            RouteAuthorization::Approval(id) => {
                if policy.member_mode == MemberMode::AllowlistedOnly {
                    return Err(RoutingError::TargetNotGranted);
                }
                let approval = self
                    .approvals
                    .get(id)
                    .ok_or(RoutingError::ApprovalNotFound)?;
                self.check_approval(approval, ctx, request)?;
                match approval.state {
                    ApprovalState::Approved
                        if policy.member_mode == MemberMode::ApprovalRequired =>
                    {
                        Err(RoutingError::ReviewerGrantRequired)
                    }
                    ApprovalState::Approved => Ok(RouteVerdict::Pass(RouteBasis::Approval {
                        approval_id: id.into(),
                    })),
                    ApprovalState::Denied => Err(RoutingError::ApprovalDenied),
                    ApprovalState::Pending => Ok(RouteVerdict::ApprovalRequired),
                    ApprovalState::Stale | ApprovalState::Cancelled => {
                        Err(RoutingError::ApprovalStale)
                    }
                }
            }
            RouteAuthorization::ApprovalAndGrant {
                approval_id,
                grant_id,
            } => {
                let grant = self
                    .grants
                    .get(grant_id)
                    .ok_or(RoutingError::GrantNotFound)?;
                self.check_grant(grant, ctx, request, root, now)?;
                let approval = self
                    .approvals
                    .get(approval_id)
                    .ok_or(RoutingError::ApprovalNotFound)?;
                self.check_approval(approval, ctx, request)?;
                match approval.state {
                    ApprovalState::Approved => Ok(RouteVerdict::Pass(RouteBasis::ApprovedGrant {
                        approval_id: approval_id.into(),
                        grant_id: grant_id.into(),
                    })),
                    ApprovalState::Denied => Err(RoutingError::ApprovalDenied),
                    ApprovalState::Pending => Ok(RouteVerdict::ApprovalRequired),
                    ApprovalState::Stale | ApprovalState::Cancelled => {
                        Err(RoutingError::ApprovalStale)
                    }
                }
            }
            RouteAuthorization::None => match policy.member_mode {
                MemberMode::Open => Ok(RouteVerdict::Pass(RouteBasis::Open)),
                MemberMode::AllowlistedOnly => Err(RoutingError::TargetNotGranted),
                MemberMode::ApprovalRequired => Ok(RouteVerdict::ApprovalRequired),
            },
        }
    }

    /// Commit routing evidence and ask admission in the same AskStore lock.
    /// `action_id` is the ORIGINAL stable delegation action, not a scheduler
    /// tick ID. A read-only preflight must NEVER call this function.
    pub fn consume_route(
        &mut self,
        ctx: &RoutingContext,
        request: &RoutingRequest,
        root: &RootStatus,
        authorization: RouteAuthorization<'_>,
        action_id: &str,
        now: DateTime<Utc>,
    ) -> RoutingResult<RouteVerdict> {
        validate_id(action_id)?;
        ctx.validate()?;
        request.validate()?;
        if request.project_id != ctx.project_id
            || request.requester_ref.as_ref() != ctx.actor.reference()
        {
            return Err(RoutingError::ScopeMismatch);
        }
        root.check_request(request)?;
        let digest = request.digest()?;
        // Keep this check before grant exhaustion: changing the payload of
        // an already consumed action is a conflict, not a second attempted use.
        if self
            .grants
            .values()
            .filter_map(|g| g.consumption.as_ref())
            .any(|used| {
                Some(&used.requester_ref) == request.requester_ref.as_ref()
                    && used.action_id == action_id
                    && (used.request_digest != digest
                        || !match authorization {
                            RouteAuthorization::Grant(id) => id == used.grant_id,
                            RouteAuthorization::ApprovalAndGrant { grant_id, .. } => {
                                grant_id == used.grant_id
                            }
                            _ => false,
                        })
            })
        {
            return Err(RoutingError::ActionPayloadConflict);
        }
        let verdict = self.check_route(ctx, request, root, authorization, now)?;
        if let RouteVerdict::Pass(
            RouteBasis::Grant { grant_id } | RouteBasis::ApprovedGrant { grant_id, .. },
        ) = &verdict
        {
            let requester = request
                .requester_ref
                .clone()
                .ok_or(RoutingError::ScopeMismatch)?;
            let grant = self.grants.get_mut(grant_id).expect("checked grant");
            if let Some(used) = &grant.consumption {
                if used.action_id != action_id || used.request_digest != digest {
                    return Err(RoutingError::GrantExhausted);
                }
                return Ok(verdict);
            }
            let revision = next_revision(grant.revision)?;
            grant.consumption = Some(GrantConsumption {
                grant_id: grant_id.clone(),
                action_id: action_id.into(),
                requester_ref: requester,
                delegation_ref: request.delegation_ref.clone(),
                request_digest: digest,
                consumed_at: now,
            });
            grant.used_count = 1;
            grant.revision = revision;
        }
        Ok(verdict)
    }
}

pub(super) fn validate_id(value: &str) -> RoutingResult<()> {
    if value.is_empty()
        || value.len() > 1024
        || value.trim() != value
        || value.chars().any(char::is_control)
        || value == "*"
    {
        return Err(RoutingError::InvalidIdentifier);
    }
    Ok(())
}

pub(super) fn next_revision(revision: u64) -> RoutingResult<u64> {
    revision
        .checked_add(1)
        .filter(|v| *v <= 9_007_199_254_740_991)
        .ok_or(RoutingError::RevisionOverflow)
}

#[cfg(test)]
mod tests;
