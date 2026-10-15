//! `project-optional-director`: a Project exists without a Director.
//!
//! A Project exists through its `project_states` record (legacy stores are
//! migrated on load, `migrate_project_states`); `projects` (the authority map)
//! only holds the Projects that currently have a Director. Human/Management
//! can create a Project with or without a Director (`create_project`), seat a
//! Director later (`assign_director`) and vacate the seat (`clear_director`).
//! The epoch of a cleared seat is kept in `ProjectStateRecord.last_director_epoch`
//! so a later assignment continues it.

use super::*;
use crate::management::control_plane::HumanManagementProjectMutationActionRecord;
use crate::role_revision::Sha256;

/// Whether `candidate` may take the Director seat of `project_id`. Returns
/// `true` when the candidate is independent and joins the Project.
fn director_candidate_joins(
    snapshot: &super::super::AgentManagementSnapshot,
    project_id: &ProjectId,
    candidate: &CutexSessionId,
) -> Result<bool, AgentManagementError> {
    let agent = snapshot
        .agents
        .get(candidate)
        .filter(|agent| agent.retired_at.is_none())
        .ok_or(AgentManagementError::NotFound("active_managed_director"))?;
    if snapshot
        .reversible_archive_projection
        .get(candidate)
        .copied()
        .unwrap_or(false)
    {
        return Err(AgentManagementError::ConflictDetail(format!(
            "agent_archived: {} is archived; restore it before making it a Director",
            agent.spec.name
        )));
    }
    if snapshot.projects.values().any(|authority| {
        &authority.authorized_director_session == candidate && &authority.project_id != project_id
    }) {
        return Err(AgentManagementError::Conflict(
            "session_already_directs_another_project",
        ));
    }
    if snapshot
        .operator_grants
        .get(project_id)
        .is_some_and(|grants| grants.contains_key(candidate))
    {
        return Err(AgentManagementError::Conflict(
            "primary_director_cannot_be_operator",
        ));
    }
    match current_project_id(snapshot, agent) {
        None => Ok(true),
        Some(current) if &current == project_id => Ok(false),
        Some(current) => Err(AgentManagementError::ConflictDetail(format!(
            "director_already_has_active_project: {} is a member of Project {current}; \
             detach it from that Project first or choose an independent Agent",
            agent.spec.name
        ))),
    }
}

/// Seat `director` in the authority map (and its membership when it joins).
fn commit_director(
    state: &mut super::super::AgentManagementSnapshot,
    project_id: &ProjectId,
    director: &CutexSessionId,
    epoch: u64,
    joins: bool,
    committed_at: &Rfc3339,
) -> Result<Option<CurrentProjectMembership>, AgentManagementError> {
    state.projects.insert(
        project_id.clone(),
        ProjectAuthority {
            project_id: project_id.clone(),
            authorized_director_session: director.clone(),
            authority_epoch: epoch,
            updated_at: committed_at.clone(),
        },
    );
    if !joins {
        return Ok(None);
    }
    let membership = next_membership(state, director, Some(project_id.clone()), committed_at.clone())?;
    state
        .current_project_memberships
        .insert(director.clone(), membership.clone());
    Ok(Some(membership))
}

struct DirectorCommit {
    kind: ProjectAuditKind,
    member: Option<CutexSessionId>,
    previous_revision: u64,
    project_state: ProjectStateRecord,
    membership: Option<CurrentProjectMembership>,
    committed_at: Rfc3339,
}

/// Record the audit event and receipt; `project_state` is written too.
fn commit_receipt(
    state: &mut super::super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    digest: &Sha256,
    commit: DirectorCommit,
) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
    let revision = commit.project_state.revision;
    state
        .project_states
        .insert(request.project_id.clone(), commit.project_state.clone());
    let event_id = format!(
        "human-management:{}:project:{}",
        request.action_id, revision
    );
    let audit_event = ProjectAuditEvent {
        event_id: event_id.clone(),
        action_id: request.action_id.clone(),
        project_id: request.project_id.clone(),
        kind: commit.kind,
        previous_project_revision: commit.previous_revision,
        project_revision: revision,
        member_cutex_session_id: commit.member,
        performed_by_human_management: true,
        committed_at: commit.committed_at.clone(),
    };
    if state
        .project_audit_events
        .insert(event_id, audit_event.clone())
        .is_some()
    {
        return Err(AgentManagementError::InvalidStore);
    }
    let receipt = HumanManagementProjectMutationReceipt {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: request.action_id.clone(),
        request_sha256: digest.clone(),
        project_id: request.project_id.clone(),
        operation: request.operation.clone(),
        previous_project_revision: commit.previous_revision,
        project_revision: revision,
        project_state: Some(commit.project_state),
        membership: commit.membership,
        tombstone: None,
        audit_event,
        committed_at: commit.committed_at,
    };
    state.human_management_project_mutations.insert(
        request.action_id.clone(),
        HumanManagementProjectMutationActionRecord {
            request_sha256: digest.clone(),
            receipt: receipt.clone(),
        },
    );
    Ok(receipt)
}

fn check_cas(
    snapshot: &super::super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    authority: Option<&ProjectAuthority>,
    project_state: &ProjectStateRecord,
) -> Result<(), AgentManagementError> {
    let epoch = effective_authority_epoch(snapshot, &request.project_id, authority);
    if request
        .expected_authority_epoch
        .is_some_and(|expected| expected != epoch)
    {
        return Err(AgentManagementError::Conflict("stale_project_authority"));
    }
    if request
        .expected_project_revision
        .is_some_and(|expected| expected != project_state.revision)
    {
        return Err(AgentManagementError::Conflict("project_revision_conflict"));
    }
    Ok(())
}

/// An exact replay of this action, or its payload conflict.
fn existing_receipt(
    state: &super::super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    digest: &Sha256,
) -> Option<Result<HumanManagementProjectMutationReceipt, AgentManagementError>> {
    state
        .human_management_project_mutations
        .get(&request.action_id)
        .map(|record| {
            if &record.request_sha256 == digest {
                Ok(record.receipt.clone())
            } else {
                Err(AgentManagementError::Conflict("action_id_payload_conflict"))
            }
        })
}

impl AgentManagementProvider {
    /// `create_project` / `assign_director` / `clear_director`. Called with the
    /// provider execution and mutation locks held. These change Agent
    /// Management only (r219: the Task Service Director seat copy is gone).
    pub(in crate::agent_management) fn execute_optional_director_mutation(
        &self,
        request: &HumanManagementProjectMutationRequest,
    ) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
        if request
            .expected_authority_epoch
            .is_some_and(|epoch| epoch > MAX_JSON_SAFE_INTEGER)
            || request
                .expected_project_revision
                .is_some_and(|revision| revision > MAX_JSON_SAFE_INTEGER)
        {
            return Err(AgentManagementError::InvalidRequest(
                "invalid_project_mutation_cas",
            ));
        }
        let digest = super::super::store::request_sha256(request)?;
        if let HumanManagementProjectMutationKind::CreateProject {
            presentation,
            workspace_root,
            create_workspace_root,
            ..
        } = &request.operation
        {
            if request.expected_authority_epoch.unwrap_or(0) != 0
                || request.expected_project_revision.unwrap_or(0) != 0
            {
                return Err(AgentManagementError::InvalidRequest(
                    "project_create_requires_zero_cas",
                ));
            }
            presentation.validate()?;
            if let Some(root) = workspace_root {
                if *create_workspace_root {
                    workspace_root_creatable(root)?;
                } else {
                    canonical_workspace_root(root)?;
                }
            }
        }
        match &request.operation {
            HumanManagementProjectMutationKind::AssignDirector { cutex_session_id }
            | HumanManagementProjectMutationKind::RotateDirector {
                successor_cutex_session_id: cutex_session_id,
            }
            | HumanManagementProjectMutationKind::CreateProject {
                director_cutex_session_id: Some(cutex_session_id),
                ..
            } => self.require_not_reversibly_archived(cutex_session_id)?,
            _ => {}
        }
        if matches!(
            request.operation,
            HumanManagementProjectMutationKind::RotateDirector { .. }
        ) && request.expected_authority_epoch.is_none()
        {
            return Err(AgentManagementError::InvalidRequest(
                "rotate_director_requires_authority_epoch",
            ));
        }
        self.store().with_state(true, |mut state| {
            if let Some(replay) = existing_receipt(&state, request, &digest) {
                return replay.map(|receipt| (state, receipt, false));
            }
            reject_project_action_id_domain_collision(&state, &request.action_id)?;
            let receipt = match &request.operation {
                HumanManagementProjectMutationKind::CreateProject {
                    presentation,
                    workspace_root,
                    director_cutex_session_id,
                    create_workspace_root,
                } => create_project(
                    &mut state,
                    request,
                    &digest,
                    presentation,
                    workspace_root.as_deref(),
                    director_cutex_session_id.as_ref(),
                    *create_workspace_root,
                )?,
                HumanManagementProjectMutationKind::AssignDirector { cutex_session_id } => {
                    assign_director(&mut state, request, &digest, cutex_session_id)?
                }
                HumanManagementProjectMutationKind::ClearDirector => {
                    clear_director(&mut state, request, &digest)?
                }
                HumanManagementProjectMutationKind::RotateDirector {
                    successor_cutex_session_id,
                } => rotate_director(&mut state, request, &digest, successor_cutex_session_id)?,
                _ => return Err(AgentManagementError::InvalidStore),
            };
            Ok((state, receipt, true))
        })
    }
}

fn create_project(
    state: &mut super::super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    digest: &Sha256,
    presentation: &ProjectPresentationInput,
    workspace_root: Option<&str>,
    director: Option<&CutexSessionId>,
    create_workspace_root: bool,
) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
    if project_id_used(state, &request.project_id) {
        return Err(AgentManagementError::Conflict("project_id_already_used"));
    }
    // Validated before the directory is created, so a refused create (Director
    // conflict) leaves at most an empty directory behind, never a project.
    if let Some(director) = director {
        director_candidate_joins(state, &request.project_id, director)?;
    }
    let workspace_root = match workspace_root {
        Some(root) if create_workspace_root => Some(ensure_workspace_root(root)?),
        Some(root) => Some(canonical_workspace_root(root)?),
        None => None,
    };
    let committed_at = now();
    let membership = match director {
        Some(director) => {
            let joins = director_candidate_joins(state, &request.project_id, director)?;
            commit_director(state, &request.project_id, director, 1, joins, &committed_at)?
        }
        None => None,
    };
    state.project_presentations.insert(
        request.project_id.clone(),
        ProjectPresentationSettings {
            display_name: presentation.display_name.trim().to_string(),
            badge_label: presentation.badge_label.trim().to_string(),
            color: presentation.color,
            revision: 1,
            updated_at: committed_at.clone(),
            updated_by_director_session: None,
            updated_by_human_management: true,
            extra: BTreeMap::new(),
        },
    );
    let project_state = ProjectStateRecord {
        project_id: request.project_id.clone(),
        lifecycle: ProjectLifecycle::Active,
        revision: 1,
        created_at: committed_at.clone(),
        updated_at: committed_at.clone(),
        archived_at: None,
        workspace_root,
        last_director_epoch: None,
    };
    commit_receipt(
        state,
        request,
        digest,
        DirectorCommit {
            kind: ProjectAuditKind::Created,
            member: director.cloned(),
            previous_revision: 0,
            project_state,
            membership,
            committed_at,
        },
    )
}

fn assign_director(
    state: &mut super::super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    digest: &Sha256,
    candidate: &CutexSessionId,
) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
    let (authority, mut project_state) = project_entry(state, &request.project_id)
        .map(|(authority, project_state)| (authority.cloned(), project_state))
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
    if let Some(authority) = authority {
        return Err(AgentManagementError::ConflictDetail(format!(
            "project_has_director: Project {} already has Director {}; clear it first \
             (clear_director)",
            request.project_id,
            authority.authorized_director_session.as_str()
        )));
    }
    check_cas(state, request, None, &project_state)?;
    let joins = director_candidate_joins(state, &request.project_id, candidate)?;
    let epoch = effective_authority_epoch(state, &request.project_id, None)
        .checked_add(1)
        .filter(|epoch| *epoch <= MAX_JSON_SAFE_INTEGER)
        .ok_or(AgentManagementError::Conflict("authority_epoch_overflow"))?;
    let committed_at = now();
    let membership = commit_director(
        state,
        &request.project_id,
        candidate,
        epoch,
        joins,
        &committed_at,
    )?;
    let previous_revision = project_state.revision;
    project_state.last_director_epoch = None;
    project_state.revision = next_project_revision(previous_revision)?;
    project_state.updated_at = committed_at.clone();
    commit_receipt(
        state,
        request,
        digest,
        DirectorCommit {
            kind: ProjectAuditKind::DirectorAssigned,
            member: Some(candidate.clone()),
            previous_revision,
            project_state,
            membership,
            committed_at,
        },
    )
}

fn clear_director(
    state: &mut super::super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    digest: &Sha256,
) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
    let (authority, mut project_state) = project_entry(state, &request.project_id)
        .map(|(authority, project_state)| (authority.cloned(), project_state))
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
    let authority = authority.ok_or(AgentManagementError::Conflict("project_has_no_director"))?;
    check_cas(state, request, Some(&authority), &project_state)?;
    // Preconditions, each naming the step that unblocks it.
    let operators = state
        .operator_grants
        .get(&request.project_id)
        .map_or(0, BTreeMap::len);
    if operators > 0 {
        return Err(AgentManagementError::ConflictDetail(format!(
            "project_has_agent_operators: revoke the Project's {operators} Agent Operator \
             grant(s) first; Operators act under the Director's authority"
        )));
    }
    let committed_at = now();
    state.projects.remove(&request.project_id);
    let previous_revision = project_state.revision;
    project_state.last_director_epoch = Some(authority.authority_epoch);
    project_state.revision = next_project_revision(previous_revision)?;
    project_state.updated_at = committed_at.clone();
    commit_receipt(
        state,
        request,
        digest,
        DirectorCommit {
            kind: ProjectAuditKind::DirectorCleared,
            member: Some(authority.authorized_director_session.clone()),
            previous_revision,
            project_state,
            membership: None,
            committed_at,
        },
    )
}

/// `rotate_director` (owner-rotation): move the seat from the current Director
/// to `successor` in one commit. The epoch fence is required. The successor is
/// a current member or an independent Agent (it joins); the former Director
/// keeps its membership and Operator grants are untouched.
fn rotate_director(
    state: &mut super::super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    digest: &Sha256,
    successor: &CutexSessionId,
) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
    let (authority, mut project_state) = project_entry(state, &request.project_id)
        .map(|(authority, project_state)| (authority.cloned(), project_state))
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
    let authority = authority.ok_or(AgentManagementError::Conflict("project_has_no_director"))?;
    check_cas(state, request, Some(&authority), &project_state)?;
    if &authority.authorized_director_session == successor {
        return Err(AgentManagementError::Conflict("successor_is_current_director"));
    }
    let joins = director_candidate_joins(state, &request.project_id, successor)?;
    let epoch = authority
        .authority_epoch
        .checked_add(1)
        .filter(|epoch| *epoch <= MAX_JSON_SAFE_INTEGER)
        .ok_or(AgentManagementError::Conflict("authority_epoch_overflow"))?;
    let committed_at = now();
    let membership = commit_director(
        state,
        &request.project_id,
        successor,
        epoch,
        joins,
        &committed_at,
    )?;
    let previous_revision = project_state.revision;
    project_state.last_director_epoch = None;
    project_state.revision = next_project_revision(previous_revision)?;
    project_state.updated_at = committed_at.clone();
    commit_receipt(
        state,
        request,
        digest,
        DirectorCommit {
            kind: ProjectAuditKind::DirectorRotated,
            member: Some(successor.clone()),
            previous_revision,
            project_state,
            membership,
            committed_at,
        },
    )
}

#[cfg(test)]
#[path = "director_tests.rs"]
mod tests;
