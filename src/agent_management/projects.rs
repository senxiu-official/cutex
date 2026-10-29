//! Provider-authoritative Cutex Project read model and presentation settings.
//!
//! A Cutex Project is an Agent Management ownership boundary. Its identity is
//! only the canonical [`ProjectId`] stored by the provider. Display metadata is
//! deliberately non-authoritative and can never select or grant authority.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use unicode_width::UnicodeWidthStr;

use crate::management::control_plane::{
    HumanManagementOperatorActionRecord, HumanManagementOperatorActionRequest,
    HumanManagementOperatorKind, HumanManagementOperatorReceipt,
    HumanManagementPresentationUpdateRequest, HumanManagementPrincipal,
    HumanManagementProjectCollection, HumanManagementProjectMutationKind,
    HumanManagementProjectMutationReceipt, HumanManagementProjectMutationRequest,
    HumanManagementProjectMutationSchema, HumanManagementProjectSchema,
};
use crate::role_revision::{CutexSessionId, Rfc3339, MAX_JSON_SAFE_INTEGER};

use super::{
    now, AgentManagementError, AgentManagementInvocation, AgentManagementProvider,
    AgentOperatorGrant, AgentRuntimeObservation, ManagedAgentRecord, ProjectAuthority, ProjectId,
};

mod director;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectPaletteColor {
    Cyan,
    Blue,
    Green,
    Magenta,
    Yellow,
    Red,
    Rgb(u8, u8, u8),
}

impl ProjectPaletteColor {
    pub const ALL: [Self; 6] = [
        Self::Cyan,
        Self::Blue,
        Self::Green,
        Self::Magenta,
        Self::Yellow,
        Self::Red,
    ];

    pub fn token(self) -> String {
        match self {
            Self::Cyan => "cyan".to_string(),
            Self::Blue => "blue".to_string(),
            Self::Green => "green".to_string(),
            Self::Magenta => "magenta".to_string(),
            Self::Yellow => "yellow".to_string(),
            Self::Red => "red".to_string(),
            Self::Rgb(red, green, blue) => format!("#{red:02X}{green:02X}{blue:02X}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectColorParseError;

impl fmt::Display for ProjectColorParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "invalid project badge color; expected cyan, blue, green, magenta, yellow, red, or #RRGGBB",
        )
    }
}

impl std::error::Error for ProjectColorParseError {}

impl FromStr for ProjectPaletteColor {
    type Err = ProjectColorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cyan" => Ok(Self::Cyan),
            "blue" => Ok(Self::Blue),
            "green" => Ok(Self::Green),
            "magenta" => Ok(Self::Magenta),
            "yellow" => Ok(Self::Yellow),
            "red" => Ok(Self::Red),
            value
                if value.strip_prefix('#').is_some_and(|hex| {
                    hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) =>
            {
                let parse_component = |start| {
                    u8::from_str_radix(&value[start..start + 2], 16)
                        .map_err(|_| ProjectColorParseError)
                };
                Ok(Self::Rgb(
                    parse_component(1)?,
                    parse_component(3)?,
                    parse_component(5)?,
                ))
            }
            _ => Err(ProjectColorParseError),
        }
    }
}

impl Serialize for ProjectPaletteColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.token())
    }
}

impl<'de> Deserialize<'de> for ProjectPaletteColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectPresentationSettings {
    pub display_name: String,
    pub badge_label: String,
    pub color: ProjectPaletteColor,
    pub revision: u64,
    pub updated_at: Rfc3339,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by_director_session: Option<CutexSessionId>,
    #[serde(default)]
    pub updated_by_human_management: bool,
    /// Presentation records are intentionally forward-compatible. Unknown
    /// fields survive a read/change/write cycle.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPresentationInput {
    pub display_name: String,
    pub badge_label: String,
    pub color: ProjectPaletteColor,
}

impl ProjectPresentationInput {
    pub fn validate(&self) -> Result<(), AgentManagementError> {
        let display_name = self.display_name.trim();
        if display_name.is_empty()
            || display_name.chars().count() > 80
            || display_name.chars().any(char::is_control)
        {
            return Err(AgentManagementError::InvalidRequest(
                "invalid_project_display_name",
            ));
        }
        let badge = self.badge_label.trim();
        if !crate::session::model::valid_badge_text(badge) {
            return Err(AgentManagementError::InvalidRequest(
                "invalid_project_badge_label",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPresentationUpdateRequest {
    pub project_id: ProjectId,
    pub expected_presentation_revision: u64,
    pub presentation: ProjectPresentationInput,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveProjectPresentation {
    pub display_name: String,
    pub badge_label: String,
    pub color: ProjectPaletteColor,
    pub revision: u64,
    pub stored: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectMemberLifecycle {
    Online,
    Offline,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLifecycle {
    #[default]
    Active,
    Archived,
    Removed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectStateRecord {
    pub project_id: ProjectId,
    pub lifecycle: ProjectLifecycle,
    pub revision: u64,
    pub created_at: Rfc3339,
    pub updated_at: Rfc3339,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Rfc3339>,
    /// Canonical absolute project directory (Human-set via the
    /// `set_workspace_root` project mutation). Member agents created for the
    /// project default their cwd to it; project memory is keyed by project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    /// Authority epoch of the last Director after `clear_director`; a later
    /// `assign_director` continues from it so epochs stay monotonic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_director_epoch: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentProjectMembership {
    pub cutex_session_id: CutexSessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub revision: u64,
    pub updated_at: Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTombstone {
    pub project_id: ProjectId,
    pub final_revision: u64,
    pub final_authority_epoch: u64,
    pub removed_at: Rfc3339,
    pub removed_by_human_management: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectAuditKind {
    Created,
    /// r219: legacy, decode-only; never produced.
    DirectorSeatRepaired,
    MemberAdded,
    MemberDetached,
    Archived,
    Restored,
    Removed,
    WorkspaceRootUpdated,
    DirectorAssigned,
    DirectorCleared,
    /// `rotate_director` (owner-rotation): the seat moved to a successor.
    DirectorRotated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAuditEvent {
    pub event_id: String,
    pub action_id: super::AgentActionId,
    pub project_id: ProjectId,
    pub kind: ProjectAuditKind,
    pub previous_project_revision: u64,
    pub project_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_cutex_session_id: Option<CutexSessionId>,
    pub performed_by_human_management: bool,
    pub committed_at: Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAgentChoice {
    pub cutex_session_id: CutexSessionId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_project_id: Option<ProjectId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemberProjection {
    pub agent: ManagedAgentRecord,
    pub lifecycle: ProjectMemberLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<AgentRuntimeObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDirectorProjection {
    pub cutex_session_id: CutexSessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<ProjectMemberProjection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAgentOperatorProjection {
    pub grant: AgentOperatorGrant,
    pub member: ProjectMemberProjection,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectAccessRole {
    PrimaryDirector,
    AgentOperator,
    HumanManagement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CutexProjectSummary {
    pub project_id: ProjectId,
    pub authority_epoch: u64,
    /// Absent for a Project without a Director (`project-optional-director`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub director_cutex_session_id: Option<CutexSessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub director_name: Option<String>,
    pub access_role: ProjectAccessRole,
    pub operator_count: usize,
    pub presentation: EffectiveProjectPresentation,
    pub active_member_count: usize,
    #[serde(default)]
    pub archived_member_count: usize,
    pub retired_member_count: usize,
    #[serde(default)]
    pub lifecycle: ProjectLifecycle,
    #[serde(default)]
    pub project_revision: u64,
    #[serde(default)]
    pub workspace_root: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CutexProjectWorkspace {
    pub project_id: ProjectId,
    pub authority_epoch: u64,
    #[serde(default)]
    pub lifecycle: ProjectLifecycle,
    #[serde(default)]
    pub project_revision: u64,
    /// Absent for a Project without a Director (`project-optional-director`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub director: Option<ProjectDirectorProjection>,
    pub access_role: ProjectAccessRole,
    pub operator_grant_revision: u64,
    pub agent_operators: Vec<ProjectAgentOperatorProjection>,
    pub presentation: EffectiveProjectPresentation,
    pub active_agents: Vec<ProjectMemberProjection>,
    #[serde(default)]
    pub archived_agents: Vec<ProjectMemberProjection>,
    pub retired_agents: Vec<ProjectMemberProjection>,
    #[serde(default)]
    pub workspace_root: Option<String>,
}

pub trait ProjectRuntimeObserver {
    fn observe(
        &self,
        cutex_session_id: &CutexSessionId,
    ) -> Result<AgentRuntimeObservation, AgentManagementError>;
}

impl<F> ProjectRuntimeObserver for F
where
    F: Fn(&CutexSessionId) -> Result<AgentRuntimeObservation, AgentManagementError>,
{
    fn observe(
        &self,
        cutex_session_id: &CutexSessionId,
    ) -> Result<AgentRuntimeObservation, AgentManagementError> {
        self(cutex_session_id)
    }
}

impl AgentManagementProvider {
    /// Lists only the active projects for which the authenticated caller is
    /// the authorized Director (the Agent Management authority) or an Agent
    /// Operator. No group, cwd, display name, or native Codex project record
    /// participates in selection.
    pub fn list_cutex_projects(
        &self,
        invocation: &AgentManagementInvocation,
    ) -> Result<Vec<CutexProjectSummary>, AgentManagementError> {
        let snapshot = self.current_name_snapshot()?;
        let mut projects = snapshot
            .projects
            .values()
            .filter(|authority| {
                effective_project_state(&snapshot, authority).lifecycle == ProjectLifecycle::Active
            })
            .filter_map(|authority| {
                project_access_role(&snapshot, invocation, &authority.project_id)
                    .ok()
                    .map(|role| {
                        summary(
                            &snapshot,
                            &authority.project_id,
                            Some(authority),
                            effective_project_state(&snapshot, authority),
                            role,
                        )
                    })
            })
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        if projects.is_empty() {
            return Err(AgentManagementError::NotAuthorizedDirector);
        }
        Ok(projects)
    }

    pub fn read_cutex_project(
        &self,
        invocation: &AgentManagementInvocation,
        project_id: &ProjectId,
        observer: &dyn ProjectRuntimeObserver,
    ) -> Result<CutexProjectWorkspace, AgentManagementError> {
        let snapshot = self.current_name_snapshot()?;
        let (authority, access_role) = authorized_project(&snapshot, invocation, project_id)?;
        if effective_project_state(&snapshot, authority).lifecycle != ProjectLifecycle::Active {
            return Err(AgentManagementError::ProjectNotAuthorized);
        }
        project_workspace(
            &snapshot,
            project_id,
            Some(authority),
            effective_project_state(&snapshot, authority),
            access_role,
            observer,
        )
    }

    pub fn update_project_presentation(
        &self,
        invocation: &AgentManagementInvocation,
        request: &ProjectPresentationUpdateRequest,
    ) -> Result<ProjectPresentationSettings, AgentManagementError> {
        request.presentation.validate()?;
        self.store().with_state(true, |mut state| {
            require_active_project(&state, &request.project_id)?;
            authorized_primary_authority(&state, invocation, &request.project_id)?;
            let current_revision = state
                .project_presentations
                .get(&request.project_id)
                .map(|settings| settings.revision)
                .unwrap_or(0);
            if current_revision != request.expected_presentation_revision {
                return Err(AgentManagementError::Conflict(
                    "project_presentation_revision_conflict",
                ));
            }
            if let Some(current) = state.project_presentations.get(&request.project_id) {
                if current.display_name == request.presentation.display_name.trim()
                    && current.badge_label == request.presentation.badge_label.trim()
                    && current.color == request.presentation.color
                {
                    let current = current.clone();
                    return Ok((state, current, false));
                }
            }
            let revision =
                current_revision
                    .checked_add(1)
                    .ok_or(AgentManagementError::Conflict(
                        "project_presentation_revision_overflow",
                    ))?;
            let extra = state
                .project_presentations
                .get(&request.project_id)
                .map(|settings| settings.extra.clone())
                .unwrap_or_default();
            let settings = ProjectPresentationSettings {
                display_name: request.presentation.display_name.trim().to_string(),
                badge_label: request.presentation.badge_label.trim().to_string(),
                color: request.presentation.color,
                revision,
                updated_at: now(),
                updated_by_director_session: Some(invocation.caller_cutex_session.clone()),
                updated_by_human_management: false,
                extra,
            };
            state
                .project_presentations
                .insert(request.project_id.clone(), settings.clone());
            Ok((state, settings, true))
        })
    }

    /// Lists every canonical Project after the dedicated Management server
    /// has authenticated the local Human principal. Agent identities are not
    /// accepted by this boundary and are not synthesized here.
    pub fn list_cutex_projects_for_management(
        &self,
        _principal: &HumanManagementPrincipal,
    ) -> Result<HumanManagementProjectCollection, AgentManagementError> {
        let snapshot = self.current_name_snapshot()?;
        let mut projects = Vec::new();
        let mut archived_projects = Vec::new();
        for (project_id, state) in directorless_projects(&snapshot) {
            let project = summary(
                &snapshot,
                project_id,
                None,
                state.clone(),
                ProjectAccessRole::HumanManagement,
            );
            match project.lifecycle {
                ProjectLifecycle::Active => projects.push(project),
                ProjectLifecycle::Archived => archived_projects.push(project),
                ProjectLifecycle::Removed => {}
            }
        }
        for authority in snapshot.projects.values() {
            let project = summary(
                &snapshot,
                &authority.project_id,
                Some(authority),
                effective_project_state(&snapshot, authority),
                ProjectAccessRole::HumanManagement,
            );
            match project.lifecycle {
                ProjectLifecycle::Active => projects.push(project),
                ProjectLifecycle::Archived => archived_projects.push(project),
                ProjectLifecycle::Removed => {}
            }
        }
        projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        archived_projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        let mut available_agents = snapshot
            .agents
            .values()
            .filter(|agent| {
                agent.retired_at.is_none()
                    && !snapshot
                        .reversible_archive_projection
                        .get(&agent.cutex_session_id)
                        .copied()
                        .unwrap_or(false)
            })
            .map(|agent| ProjectAgentChoice {
                cutex_session_id: agent.cutex_session_id.clone(),
                name: agent.spec.name.clone(),
                current_project_id: current_project_id(&snapshot, agent),
            })
            .collect::<Vec<_>>();
        available_agents.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.cutex_session_id.cmp(&right.cutex_session_id))
        });
        Ok(HumanManagementProjectCollection {
            schema: HumanManagementProjectSchema::V1,
            projects,
            archived_projects,
            available_agents,
        })
    }

    pub fn read_cutex_project_for_management(
        &self,
        _principal: &HumanManagementPrincipal,
        project_id: &ProjectId,
        observer: &dyn ProjectRuntimeObserver,
    ) -> Result<CutexProjectWorkspace, AgentManagementError> {
        let snapshot = self.current_name_snapshot()?;
        let (authority, state) =
            project_entry(&snapshot, project_id).ok_or(AgentManagementError::ProjectNotAuthorized)?;
        project_workspace(
            &snapshot,
            project_id,
            authority,
            state,
            ProjectAccessRole::HumanManagement,
            observer,
        )
    }

    pub fn update_project_presentation_for_management(
        &self,
        _principal: &HumanManagementPrincipal,
        request: &HumanManagementPresentationUpdateRequest,
    ) -> Result<ProjectPresentationSettings, AgentManagementError> {
        request.presentation.validate()?;
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        self.store().with_state(true, |mut state| {
            require_active_project(&state, &request.project_id)?;
            let (authority, _) = project_entry(&state, &request.project_id)
                .ok_or(AgentManagementError::ProjectNotAuthorized)?;
            let epoch = effective_authority_epoch(&state, &request.project_id, authority);
            if request.expected_authority_epoch.is_some_and(|expected| expected != epoch) {
                return Err(AgentManagementError::Conflict("stale_project_authority"));
            }
            let current_revision = state
                .project_presentations
                .get(&request.project_id)
                .map(|settings| settings.revision)
                .unwrap_or(0);
            if request.expected_presentation_revision.is_some_and(|expected| expected != current_revision) {
                return Err(AgentManagementError::Conflict(
                    "project_presentation_revision_conflict",
                ));
            }
            if let Some(current) = state.project_presentations.get(&request.project_id) {
                if current.display_name == request.presentation.display_name.trim()
                    && current.badge_label == request.presentation.badge_label.trim()
                    && current.color == request.presentation.color
                {
                    let current = current.clone();
                    return Ok((state, current, false));
                }
            }
            let revision = current_revision
                .checked_add(1)
                .filter(|value| *value <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                .ok_or(AgentManagementError::Conflict(
                    "project_presentation_revision_overflow",
                ))?;
            let extra = state
                .project_presentations
                .get(&request.project_id)
                .map(|settings| settings.extra.clone())
                .unwrap_or_default();
            let settings = ProjectPresentationSettings {
                display_name: request.presentation.display_name.trim().to_string(),
                badge_label: request.presentation.badge_label.trim().to_string(),
                color: request.presentation.color,
                revision,
                updated_at: now(),
                updated_by_director_session: None,
                updated_by_human_management: true,
                extra,
            };
            state
                .project_presentations
                .insert(request.project_id.clone(), settings.clone());
            Ok((state, settings, true))
        })
    }

    /// Grant or revoke an Operator as a distinct Human/Management action.
    /// Both project authority and the complete Operator set are CAS-fenced.
    pub fn execute_operator_action_for_management(
        &self,
        _principal: &HumanManagementPrincipal,
        request: &HumanManagementOperatorActionRequest,
    ) -> Result<HumanManagementOperatorReceipt, AgentManagementError> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        let digest = super::store::request_sha256(request)?;
        self.store().with_state(true, |mut state| {
            if let Some(record) = state
                .human_management_operator_actions
                .get(&request.action_id)
                .cloned()
            {
                return if record.request_sha256 == digest {
                    Ok((state, record.receipt, false))
                } else {
                    Err(AgentManagementError::Conflict("action_id_payload_conflict"))
                };
            }
            if state
                .durable_import_actions
                .contains_key(&request.action_id)
                || state.actions.contains_key(&request.action_id)
                || state.authority_receipts.contains_key(&request.action_id)
                || state
                    .human_management_project_mutations
                    .contains_key(&request.action_id)
                || state
                    .legacy_director_ownership_import_receipts
                    .contains_key(&request.action_id)
                || state
                    .reservation_reconciliation_receipts
                    .contains_key(&request.action_id)
            {
                return Err(AgentManagementError::Conflict("action_id_domain_conflict"));
            }
            let authority = state
                .projects
                .get(&request.project_id)
                .cloned()
                .ok_or(AgentManagementError::ProjectNotAuthorized)?;
            require_active_project(&state, &request.project_id)?;
            if request.expected_authority_epoch.is_some_and(|expected| expected != authority.authority_epoch) {
                return Err(AgentManagementError::Conflict("stale_project_authority"));
            }
            if request.operator_cutex_session_id == authority.authorized_director_session {
                return Err(AgentManagementError::Conflict(
                    "primary_director_cannot_be_operator",
                ));
            }
            let current_revision = management_operator_grant_revision(&state, &request.project_id);
            if request.expected_grant_revision.is_some_and(|expected| expected != current_revision) {
                return Err(AgentManagementError::Conflict(
                    "operator_grant_revision_conflict",
                ));
            }
            let revision = management_next_operator_grant_revision(current_revision)?;
            let committed_at = now();
            let grant = match request.operation {
                HumanManagementOperatorKind::Grant => {
                    let agent = state
                        .agents
                        .get(&request.operator_cutex_session_id)
                        .filter(|agent| {
                            current_project_id(&state, agent).as_ref() == Some(&request.project_id)
                                && agent.retired_at.is_none()
                        })
                        .ok_or(AgentManagementError::Conflict(
                            "operator_must_be_active_managed_agent",
                        ))?;
                    if agent.cutex_session_id != request.operator_cutex_session_id {
                        return Err(AgentManagementError::InvalidStore);
                    }
                    if state
                        .operator_grants
                        .get(&request.project_id)
                        .is_some_and(|grants| {
                            grants.contains_key(&request.operator_cutex_session_id)
                        })
                    {
                        return Err(AgentManagementError::Conflict("operator_already_granted"));
                    }
                    let grant = super::AgentOperatorGrant {
                        project_id: request.project_id.clone(),
                        operator_cutex_session_id: request.operator_cutex_session_id.clone(),
                        grant_revision: revision,
                        granted_at: committed_at.clone(),
                        granted_by_primary_director_session: authority
                            .authorized_director_session
                            .clone(),
                        performed_by_human_management: true,
                    };
                    state
                        .operator_grants
                        .entry(request.project_id.clone())
                        .or_default()
                        .insert(request.operator_cutex_session_id.clone(), grant.clone());
                    Some(grant)
                }
                HumanManagementOperatorKind::Revoke => {
                    let removed = state
                        .operator_grants
                        .get_mut(&request.project_id)
                        .and_then(|grants| grants.remove(&request.operator_cutex_session_id))
                        .ok_or(AgentManagementError::Conflict("operator_not_granted"))?;
                    if removed.project_id != request.project_id
                        || removed.operator_cutex_session_id != request.operator_cutex_session_id
                    {
                        return Err(AgentManagementError::InvalidStore);
                    }
                    None
                }
            };
            state
                .operator_grant_revisions
                .insert(request.project_id.clone(), revision);
            let event_id = format!(
                "human-management:{}:operator-grant:{}",
                request.action_id, revision
            );
            let audit_event = super::AgentOperatorAuditEvent {
                event_id: event_id.clone(),
                action_id: request.action_id.clone(),
                project_id: request.project_id.clone(),
                operator_cutex_session_id: request.operator_cutex_session_id.clone(),
                kind: match request.operation {
                    HumanManagementOperatorKind::Grant => super::AgentOperatorAuditKind::Granted,
                    HumanManagementOperatorKind::Revoke => super::AgentOperatorAuditKind::Revoked,
                },
                previous_grant_revision: current_revision,
                grant_revision: revision,
                primary_director_cutex_session_id: authority.authorized_director_session.clone(),
                performed_by_human_management: true,
                committed_at: committed_at.clone(),
            };
            if state
                .operator_audit_events
                .insert(event_id, audit_event.clone())
                .is_some()
            {
                return Err(AgentManagementError::InvalidStore);
            }
            let roster = management_operator_roster(&state, &request.project_id);
            let receipt = HumanManagementOperatorReceipt {
                schema: request.schema,
                action_id: request.action_id.clone(),
                request_sha256: digest.clone(),
                operation: request.operation,
                project_id: request.project_id.clone(),
                authority_epoch: authority.authority_epoch,
                primary_director_cutex_session_id: authority.authorized_director_session,
                operator_cutex_session_id: request.operator_cutex_session_id.clone(),
                previous_grant_revision: current_revision,
                grant_revision: revision,
                grant,
                roster,
                audit_event,
                committed_at,
            };
            state.human_management_operator_actions.insert(
                request.action_id.clone(),
                HumanManagementOperatorActionRecord {
                    request_sha256: digest.clone(),
                    receipt: receipt.clone(),
                },
            );
            Ok((state, receipt, true))
        })
    }

    /// Mutates only Project structure. Runtime lifecycle and historical Agent
    /// ownership records are deliberately outside this boundary.
    pub fn execute_project_mutation_for_management(
        &self,
        _principal: &HumanManagementPrincipal,
        request: &HumanManagementProjectMutationRequest,
    ) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        self.execute_project_mutation_locked(request)
    }

    pub(super) fn execute_project_mutation_locked(
        &self,
        request: &HumanManagementProjectMutationRequest,
    ) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
        // r219: the Director seat copy was removed. A recorded repair still
        // decodes, but neither a new request nor a replay of one executes.
        if matches!(
            request.operation,
            HumanManagementProjectMutationKind::RepairDirectorSeat { .. }
        ) {
            return Err(AgentManagementError::Removed(
                super::DIRECTOR_SEAT_REPAIR_REMOVED,
            ));
        }
        if let HumanManagementProjectMutationKind::Create {
            director_cutex_session_id,
            presentation,
        } = &request.operation
        {
            return self.create_project_for_management(
                request,
                director_cutex_session_id,
                presentation,
            );
        }
        if request.operation.is_optional_director_operation() {
            return self.execute_optional_director_mutation(request);
        }
        // Epoch 0 is the fence of a Project that never had a Director.
        if request.expected_authority_epoch.is_some_and(|epoch| epoch > MAX_JSON_SAFE_INTEGER)
            || request.expected_project_revision.is_some_and(|revision| revision > MAX_JSON_SAFE_INTEGER)
        {
            return Err(AgentManagementError::InvalidRequest(
                "invalid_project_mutation_cas",
            ));
        }
        let digest = super::store::request_sha256(request)?;
        let before = self.store().snapshot()?;
        if !before
            .human_management_project_mutations
            .contains_key(&request.action_id)
        {
            reject_project_action_id_domain_collision(&before, &request.action_id)?;
            if let HumanManagementProjectMutationKind::AddMember { cutex_session_id } =
                &request.operation
            {
                self.require_not_reversibly_archived(cutex_session_id)?;
            }
            let (authority, project_state) = project_entry(&before, &request.project_id)
                .ok_or(AgentManagementError::ProjectNotAuthorized)?;
            let epoch = effective_authority_epoch(&before, &request.project_id, authority);
            if request.expected_authority_epoch.is_some_and(|expected| expected != epoch) {
                return Err(AgentManagementError::Conflict("stale_project_authority"));
            }
            if request.expected_project_revision.is_some_and(|expected| expected != project_state.revision) {
                return Err(AgentManagementError::Conflict("project_revision_conflict"));
            }
            match &request.operation {
                HumanManagementProjectMutationKind::Archive => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?
                }
                HumanManagementProjectMutationKind::Restore
                | HumanManagementProjectMutationKind::Remove => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Archived)?
                }
                HumanManagementProjectMutationKind::SetWorkspaceRoot { workspace_root } => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
                    if let Some(root) = workspace_root {
                        canonical_workspace_root(root)?;
                    }
                }
                HumanManagementProjectMutationKind::AddMember { .. }
                | HumanManagementProjectMutationKind::DetachMember { .. }
                | HumanManagementProjectMutationKind::Create { .. }
                | HumanManagementProjectMutationKind::CreateProject { .. }
                | HumanManagementProjectMutationKind::AssignDirector { .. }
                | HumanManagementProjectMutationKind::ClearDirector
                | HumanManagementProjectMutationKind::RotateDirector { .. }
                | HumanManagementProjectMutationKind::RepairDirectorSeat { .. } => {}
            }
        }
        self.store().with_state(true, |mut state| {
            if let Some(record) = state
                .human_management_project_mutations
                .get(&request.action_id)
                .cloned()
            {
                return if record.request_sha256 == digest {
                    Ok((state, record.receipt, false))
                } else {
                    Err(AgentManagementError::Conflict("action_id_payload_conflict"))
                };
            }
            reject_project_action_id_domain_collision(&state, &request.action_id)?;
            let (authority, mut project_state) = project_entry(&state, &request.project_id)
                .map(|(authority, project_state)| (authority.cloned(), project_state))
                .ok_or(AgentManagementError::ProjectNotAuthorized)?;
            let authority_epoch =
                effective_authority_epoch(&state, &request.project_id, authority.as_ref());
            if request.expected_authority_epoch.is_some_and(|expected| expected != authority_epoch) {
                return Err(AgentManagementError::Conflict("stale_project_authority"));
            }
            if request.expected_project_revision.is_some_and(|expected| expected != project_state.revision) {
                return Err(AgentManagementError::Conflict("project_revision_conflict"));
            }
            let previous_revision = project_state.revision;
            let revision = next_project_revision(previous_revision)?;
            let committed_at = now();
            let mut membership = None;
            let mut tombstone = None;
            let (kind, member_cutex_session_id) = match &request.operation {
                HumanManagementProjectMutationKind::Create { .. }
                | HumanManagementProjectMutationKind::CreateProject { .. }
                | HumanManagementProjectMutationKind::AssignDirector { .. }
                | HumanManagementProjectMutationKind::ClearDirector
                | HumanManagementProjectMutationKind::RotateDirector { .. } => {
                    return Err(AgentManagementError::InvalidStore)
                }
                HumanManagementProjectMutationKind::RepairDirectorSeat { .. } => {
                    return Err(AgentManagementError::Removed(
                        super::DIRECTOR_SEAT_REPAIR_REMOVED,
                    ))
                }
                HumanManagementProjectMutationKind::AddMember { cutex_session_id } => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
                    let agent = state
                        .agents
                        .get(cutex_session_id)
                        .cloned()
                        .filter(|agent| agent.retired_at.is_none())
                        .ok_or(AgentManagementError::NotFound("active_managed_agent"))?;
                    match current_project_id(&state, &agent) {
                        Some(current) if current == request.project_id => {
                            return Err(AgentManagementError::Conflict(
                                "agent_already_project_member",
                            ))
                        }
                        Some(_) => {
                            return Err(AgentManagementError::Conflict(
                                "agent_already_has_active_project",
                            ))
                        }
                        None => {}
                    }
                    let record = next_membership(
                        &state,
                        cutex_session_id,
                        Some(request.project_id.clone()),
                        committed_at.clone(),
                    )?;
                    state
                        .current_project_memberships
                        .insert(cutex_session_id.clone(), record.clone());
                    membership = Some(record);
                    (
                        ProjectAuditKind::MemberAdded,
                        Some(cutex_session_id.clone()),
                    )
                }
                HumanManagementProjectMutationKind::DetachMember { cutex_session_id } => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
                    if authority
                        .as_ref()
                        .is_some_and(|authority| cutex_session_id == &authority.authorized_director_session)
                    {
                        return Err(AgentManagementError::Conflict(
                            "primary_director_requires_rotation",
                        ));
                    }
                    let agent = state
                        .agents
                        .get(cutex_session_id)
                        .cloned()
                        .filter(|agent| agent.retired_at.is_none())
                        .ok_or(AgentManagementError::NotFound("active_managed_agent"))?;
                    if current_project_id(&state, &agent).as_ref() != Some(&request.project_id) {
                        return Err(AgentManagementError::Conflict(
                            "agent_not_current_project_member",
                        ));
                    }
                    // Operator grants only exist under a Director.
                    if let Some(authority) = &authority {
                        revoke_operator_for_detach(
                            &mut state,
                            request,
                            authority,
                            cutex_session_id,
                            &committed_at,
                        )?;
                    }
                    let record =
                        next_membership(&state, cutex_session_id, None, committed_at.clone())?;
                    state
                        .current_project_memberships
                        .insert(cutex_session_id.clone(), record.clone());
                    membership = Some(record);
                    (
                        ProjectAuditKind::MemberDetached,
                        Some(cutex_session_id.clone()),
                    )
                }
                HumanManagementProjectMutationKind::SetWorkspaceRoot { workspace_root } => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
                    project_state.workspace_root = workspace_root
                        .as_deref()
                        .map(canonical_workspace_root)
                        .transpose()?;
                    (ProjectAuditKind::WorkspaceRootUpdated, None)
                }
                HumanManagementProjectMutationKind::Archive => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Active)?;
                    project_state.lifecycle = ProjectLifecycle::Archived;
                    project_state.archived_at = Some(committed_at.clone());
                    (ProjectAuditKind::Archived, None)
                }
                HumanManagementProjectMutationKind::Restore => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Archived)?;
                    project_state.lifecycle = ProjectLifecycle::Active;
                    project_state.archived_at = None;
                    (ProjectAuditKind::Restored, None)
                }
                HumanManagementProjectMutationKind::Remove => {
                    require_state_lifecycle(&project_state, ProjectLifecycle::Archived)?;
                    let members = state
                        .agents
                        .values()
                        .filter(|agent| {
                            current_project_id(&state, agent).as_ref() == Some(&request.project_id)
                        })
                        .map(|agent| agent.cutex_session_id.clone())
                        .collect::<Vec<_>>();
                    for member_id in members {
                        let record =
                            next_membership(&state, &member_id, None, committed_at.clone())?;
                        state.current_project_memberships.insert(member_id, record);
                    }
                    if state.operator_grants.remove(&request.project_id).is_some() {
                        let next_grant_revision = state
                            .operator_grant_revisions
                            .get(&request.project_id)
                            .copied()
                            .unwrap_or(0)
                            .checked_add(1)
                            .filter(|value| *value <= MAX_JSON_SAFE_INTEGER)
                            .ok_or(AgentManagementError::Conflict(
                                "operator_grant_revision_overflow",
                            ))?;
                        state
                            .operator_grant_revisions
                            .insert(request.project_id.clone(), next_grant_revision);
                    }
                    state.project_presentations.remove(&request.project_id);
                    state.projects.remove(&request.project_id);
                    project_state.lifecycle = ProjectLifecycle::Removed;
                    let removed = ProjectTombstone {
                        project_id: request.project_id.clone(),
                        final_revision: revision,
                        final_authority_epoch: authority_epoch,
                        removed_at: committed_at.clone(),
                        removed_by_human_management: true,
                    };
                    state
                        .project_tombstones
                        .insert(request.project_id.clone(), removed.clone());
                    tombstone = Some(removed);
                    (ProjectAuditKind::Removed, None)
                }
            };
            project_state.revision = revision;
            project_state.updated_at = committed_at.clone();
            state
                .project_states
                .insert(request.project_id.clone(), project_state.clone());
            let event_id = format!(
                "human-management:{}:project:{}",
                request.action_id, revision
            );
            let audit_event = ProjectAuditEvent {
                event_id: event_id.clone(),
                action_id: request.action_id.clone(),
                project_id: request.project_id.clone(),
                kind,
                previous_project_revision: previous_revision,
                project_revision: revision,
                member_cutex_session_id,
                performed_by_human_management: true,
                committed_at: committed_at.clone(),
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
                previous_project_revision: previous_revision,
                project_revision: revision,
                project_state: Some(project_state),
                membership,
                tombstone,
                audit_event,
                committed_at,
            };
            state.human_management_project_mutations.insert(
                request.action_id.clone(),
                crate::management::control_plane::HumanManagementProjectMutationActionRecord {
                    request_sha256: digest.clone(),
                    receipt: receipt.clone(),
                },
            );
            Ok((state, receipt, true))
        })
    }
}

impl AgentManagementProvider {
    fn create_project_for_management(
        &self,
        request: &HumanManagementProjectMutationRequest,
        director_cutex_session_id: &CutexSessionId,
        presentation: &ProjectPresentationInput,
    ) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
        if request.expected_authority_epoch.unwrap_or(0) != 0 || request.expected_project_revision.unwrap_or(0) != 0 {
            return Err(AgentManagementError::InvalidRequest(
                "project_create_requires_zero_cas",
            ));
        }
        presentation.validate()?;
        let digest = super::store::request_sha256(request)?;
        let before = self.store().snapshot()?;
        if let Some(record) = before
            .human_management_project_mutations
            .get(&request.action_id)
        {
            if record.request_sha256 != digest {
                return Err(AgentManagementError::Conflict("action_id_payload_conflict"));
            }
            return Ok(record.receipt.clone());
        }
        reject_project_action_id_domain_collision(&before, &request.action_id)?;
        self.require_not_reversibly_archived(director_cutex_session_id)?;
        if project_id_used(&before, &request.project_id)
        {
            return Err(AgentManagementError::Conflict("project_id_already_used"));
        }
        let director = before
            .agents
            .get(director_cutex_session_id)
            .filter(|agent| agent.retired_at.is_none())
            .ok_or(AgentManagementError::NotFound("active_managed_director"))?;
        if current_project_id(&before, director).is_some() {
            return Err(AgentManagementError::Conflict(
                "director_already_has_active_project",
            ));
        }
        self.store().with_state(true, |mut state| {
            if project_id_used(&state, &request.project_id)
            {
                return Err(AgentManagementError::Conflict("project_id_already_used"));
            }
            let director = state
                .agents
                .get(director_cutex_session_id)
                .cloned()
                .filter(|agent| agent.retired_at.is_none())
                .ok_or(AgentManagementError::NotFound("active_managed_director"))?;
            if current_project_id(&state, &director).is_some() {
                return Err(AgentManagementError::Conflict(
                    "director_already_has_active_project",
                ));
            }
            let committed_at = now();
            let authority = ProjectAuthority {
                project_id: request.project_id.clone(),
                authorized_director_session: director_cutex_session_id.clone(),
                authority_epoch: 1,
                updated_at: committed_at.clone(),
            };
            let project_state = ProjectStateRecord {
                project_id: request.project_id.clone(),
                lifecycle: ProjectLifecycle::Active,
                revision: 1,
                created_at: committed_at.clone(),
                updated_at: committed_at.clone(),
                archived_at: None,
                workspace_root: None,
                last_director_epoch: None,
            };
            let membership = next_membership(
                &state,
                director_cutex_session_id,
                Some(request.project_id.clone()),
                committed_at.clone(),
            )?;
            let stored_presentation = ProjectPresentationSettings {
                display_name: presentation.display_name.trim().to_string(),
                badge_label: presentation.badge_label.trim().to_string(),
                color: presentation.color,
                revision: 1,
                updated_at: committed_at.clone(),
                updated_by_director_session: None,
                updated_by_human_management: true,
                extra: BTreeMap::new(),
            };
            let event_id = format!("human-management:{}:project:1", request.action_id);
            let audit_event = ProjectAuditEvent {
                event_id: event_id.clone(),
                action_id: request.action_id.clone(),
                project_id: request.project_id.clone(),
                kind: ProjectAuditKind::Created,
                previous_project_revision: 0,
                project_revision: 1,
                member_cutex_session_id: Some(director_cutex_session_id.clone()),
                performed_by_human_management: true,
                committed_at: committed_at.clone(),
            };
            let receipt = HumanManagementProjectMutationReceipt {
                schema: HumanManagementProjectMutationSchema::V1,
                action_id: request.action_id.clone(),
                request_sha256: digest.clone(),
                project_id: request.project_id.clone(),
                operation: request.operation.clone(),
                previous_project_revision: 0,
                project_revision: 1,
                project_state: Some(project_state.clone()),
                membership: Some(membership.clone()),
                tombstone: None,
                audit_event: audit_event.clone(),
                committed_at,
            };
            state.projects.insert(request.project_id.clone(), authority);
            state
                .project_states
                .insert(request.project_id.clone(), project_state);
            state
                .current_project_memberships
                .insert(director_cutex_session_id.clone(), membership);
            state
                .project_presentations
                .insert(request.project_id.clone(), stored_presentation);
            state.project_audit_events.insert(event_id, audit_event);
            state.human_management_project_mutations.insert(
                request.action_id.clone(),
                crate::management::control_plane::HumanManagementProjectMutationActionRecord {
                    request_sha256: digest.clone(),
                    receipt: receipt.clone(),
                },
            );
            Ok((state, receipt, true))
        })
    }
}

fn management_operator_grant_revision(
    snapshot: &super::AgentManagementSnapshot,
    project_id: &ProjectId,
) -> u64 {
    snapshot
        .operator_grant_revisions
        .get(project_id)
        .copied()
        .unwrap_or(0)
}

fn management_next_operator_grant_revision(current: u64) -> Result<u64, AgentManagementError> {
    current
        .checked_add(1)
        .filter(|value| *value <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
        .ok_or(AgentManagementError::Conflict(
            "operator_grant_revision_overflow",
        ))
}

fn management_operator_roster(
    snapshot: &super::AgentManagementSnapshot,
    project_id: &ProjectId,
) -> super::AgentOperatorRosterProjection {
    let mut operators = snapshot
        .operator_grants
        .get(project_id)
        .into_iter()
        .flat_map(|grants| grants.values().cloned())
        .collect::<Vec<_>>();
    operators.sort_by(|left, right| {
        left.operator_cutex_session_id
            .cmp(&right.operator_cutex_session_id)
    });
    super::AgentOperatorRosterProjection {
        grant_revision: management_operator_grant_revision(snapshot, project_id),
        operators,
    }
}

fn reject_project_action_id_domain_collision(
    state: &super::AgentManagementSnapshot,
    action_id: &super::AgentActionId,
) -> Result<(), AgentManagementError> {
    if state.durable_import_actions.contains_key(action_id)
        || state.actions.contains_key(action_id)
        || state.authority_receipts.contains_key(action_id)
        || state
            .human_management_operator_actions
            .contains_key(action_id)
        || state
            .legacy_director_ownership_import_receipts
            .contains_key(action_id)
        || state
            .reservation_reconciliation_receipts
            .contains_key(action_id)
    {
        Err(AgentManagementError::Conflict("action_id_domain_conflict"))
    } else {
        Ok(())
    }
}

/// Canonicalize a Human-supplied project workspace root: it must be an
/// absolute path to an existing directory. Windows verbatim prefixes are
/// removed so the stored value matches ordinary agent cwds.
pub fn canonical_workspace_root(input: &str) -> Result<String, AgentManagementError> {
    let trimmed = input.trim();
    if trimmed.is_empty()
        || trimmed != input
        || input.chars().any(char::is_control)
        || input.chars().count() > 4096
        || !std::path::Path::new(input).is_absolute()
    {
        return Err(AgentManagementError::InvalidRequest(
            "invalid_workspace_root",
        ));
    }
    let canonical = std::path::Path::new(input)
        .canonicalize()
        .map_err(|_| AgentManagementError::InvalidRequest("workspace_root_not_found"))?;
    if !canonical.is_dir() {
        return Err(AgentManagementError::InvalidRequest(
            "workspace_root_not_directory",
        ));
    }
    let text = canonical
        .to_str()
        .ok_or(AgentManagementError::InvalidRequest("invalid_workspace_root"))?;
    Ok(strip_verbatim_prefix(text))
}

/// Syntax checks of a workspace root that may still have to be created
/// (`create_project {create_workspace_root: true}`): what
/// [`canonical_workspace_root`] requires of the text, no `.` / `..`
/// components (the created path is the one the owner typed), and an existing
/// path must be a directory.
pub fn workspace_root_creatable(input: &str) -> Result<(), AgentManagementError> {
    let invalid = AgentManagementError::InvalidRequest("invalid_workspace_root");
    let trimmed = input.trim();
    let path = std::path::Path::new(input);
    if trimmed.is_empty()
        || trimmed != input
        || input.chars().any(char::is_control)
        || input.chars().count() > 4096
        || !path.is_absolute()
        || path.components().any(|part| {
            matches!(part, std::path::Component::CurDir | std::path::Component::ParentDir)
        })
    {
        return Err(invalid);
    }
    match std::fs::metadata(path) {
        Ok(metadata) if !metadata.is_dir() => Err(AgentManagementError::InvalidRequest(
            "workspace_root_not_directory",
        )),
        _ => Ok(()),
    }
}

/// [`canonical_workspace_root`] of a root that is created first when it is
/// missing (the missing tail of the path, as the daemon user: only the
/// owner's Human Management credential reaches project mutations). A
/// component that exists as a file is `workspace_root_not_directory`; any
/// other failure to create it `workspace_root_create_failed`.
pub fn ensure_workspace_root(input: &str) -> Result<String, AgentManagementError> {
    workspace_root_creatable(input)?;
    let path = std::path::Path::new(input);
    if !path.exists() {
        if path.ancestors().skip(1).any(|ancestor| ancestor.exists() && !ancestor.is_dir()) {
            return Err(AgentManagementError::InvalidRequest(
                "workspace_root_not_directory",
            ));
        }
        std::fs::create_dir_all(path).map_err(|error| {
            eprintln!("project workspace root {input}: create failed: {error}");
            AgentManagementError::InvalidRequest("workspace_root_create_failed")
        })?;
    }
    canonical_workspace_root(input)
}

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\h\s` -> `\\h\s`; other paths unchanged.
pub fn strip_verbatim_prefix(text: &str) -> String {
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(text).to_string()
    }
}

fn next_project_revision(current: u64) -> Result<u64, AgentManagementError> {
    current
        .checked_add(1)
        .filter(|revision| *revision <= MAX_JSON_SAFE_INTEGER)
        .ok_or(AgentManagementError::Conflict("project_revision_overflow"))
}

/// Load-time migration (`project-optional-director`): every Project owns a
/// `project_states` record, which is what makes it exist; `projects` only holds
/// the Director authority of Projects that have one. Legacy Projects without a
/// state record get their effective state (active, revision 0) materialized.
pub(crate) fn migrate_project_states(snapshot: &mut super::AgentManagementSnapshot) {
    let missing = snapshot
        .projects
        .values()
        .filter(|authority| !snapshot.project_states.contains_key(&authority.project_id))
        .map(|authority| effective_project_state(snapshot, authority))
        .collect::<Vec<_>>();
    for state in missing {
        snapshot.project_states.insert(state.project_id.clone(), state);
    }
}

fn effective_project_state(
    snapshot: &super::AgentManagementSnapshot,
    authority: &ProjectAuthority,
) -> ProjectStateRecord {
    snapshot
        .project_states
        .get(&authority.project_id)
        .cloned()
        .unwrap_or_else(|| ProjectStateRecord {
            project_id: authority.project_id.clone(),
            lifecycle: ProjectLifecycle::Active,
            revision: 0,
            created_at: authority.updated_at.clone(),
            updated_at: authority.updated_at.clone(),
            archived_at: None,
            workspace_root: None,
            last_director_epoch: None,
        })
}

fn require_active_project(
    snapshot: &super::AgentManagementSnapshot,
    project_id: &ProjectId,
) -> Result<(), AgentManagementError> {
    let (_, state) =
        project_entry(snapshot, project_id).ok_or(AgentManagementError::ProjectNotAuthorized)?;
    require_state_lifecycle(&state, ProjectLifecycle::Active)
}

/// A Project that exists (active or archived, not removed): its Director
/// authority when it has one, and its effective lifecycle state. A Project
/// without a Director exists only through its `project_states` record.
pub fn project_entry<'a>(
    snapshot: &'a super::AgentManagementSnapshot,
    project_id: &ProjectId,
) -> Option<(Option<&'a ProjectAuthority>, ProjectStateRecord)> {
    if snapshot.project_tombstones.contains_key(project_id) {
        return None;
    }
    if let Some(authority) = snapshot.projects.get(project_id) {
        return Some((Some(authority), effective_project_state(snapshot, authority)));
    }
    snapshot
        .project_states
        .get(project_id)
        .filter(|state| state.lifecycle != ProjectLifecycle::Removed)
        .map(|state| (None, state.clone()))
}

/// Whether a Project exists (with or without a Director) and is not removed.
pub fn project_exists(snapshot: &super::AgentManagementSnapshot, project_id: &ProjectId) -> bool {
    project_entry(snapshot, project_id).is_some()
}

/// Projects without a Director (never had one, or the seat was cleared).
pub fn directorless_projects(
    snapshot: &super::AgentManagementSnapshot,
) -> impl Iterator<Item = (&ProjectId, &ProjectStateRecord)> {
    snapshot.project_states.iter().filter(|(project_id, state)| {
        state.lifecycle != ProjectLifecycle::Removed
            && !snapshot.projects.contains_key(*project_id)
            && !snapshot.project_tombstones.contains_key(*project_id)
    })
}

/// The authority epoch CAS fence of a Project: the Director's epoch, the last
/// epoch of a cleared seat, or 0 for a Project that never had a Director.
pub fn effective_authority_epoch(
    snapshot: &super::AgentManagementSnapshot,
    project_id: &ProjectId,
    authority: Option<&ProjectAuthority>,
) -> u64 {
    match authority {
        Some(authority) => authority.authority_epoch,
        None => snapshot
            .project_states
            .get(project_id)
            .and_then(|state| state.last_director_epoch)
            .unwrap_or(0),
    }
}

fn project_id_used(snapshot: &super::AgentManagementSnapshot, project_id: &ProjectId) -> bool {
    snapshot.projects.contains_key(project_id)
        || snapshot.project_states.contains_key(project_id)
        || snapshot.project_tombstones.contains_key(project_id)
}

fn require_state_lifecycle(
    state: &ProjectStateRecord,
    expected: ProjectLifecycle,
) -> Result<(), AgentManagementError> {
    if state.lifecycle == expected {
        Ok(())
    } else {
        Err(AgentManagementError::Conflict(match state.lifecycle {
            ProjectLifecycle::Active => "project_is_active",
            ProjectLifecycle::Archived => "project_is_archived",
            ProjectLifecycle::Removed => "project_is_removed",
        }))
    }
}

pub fn current_project_id(
    snapshot: &super::AgentManagementSnapshot,
    agent: &ManagedAgentRecord,
) -> Option<ProjectId> {
    match snapshot
        .current_project_memberships
        .get(&agent.cutex_session_id)
    {
        Some(membership) => membership.project_id.clone(),
        None if agent.retired_at.is_none() => agent.project_id.clone(),
        None => None,
    }
}

fn next_membership(
    snapshot: &super::AgentManagementSnapshot,
    cutex_session_id: &CutexSessionId,
    project_id: Option<ProjectId>,
    updated_at: Rfc3339,
) -> Result<CurrentProjectMembership, AgentManagementError> {
    let revision = snapshot
        .current_project_memberships
        .get(cutex_session_id)
        .map_or(Ok(1), |current| next_project_revision(current.revision))?;
    Ok(CurrentProjectMembership {
        cutex_session_id: cutex_session_id.clone(),
        project_id,
        revision,
        updated_at,
    })
}

fn revoke_operator_for_detach(
    state: &mut super::AgentManagementSnapshot,
    request: &HumanManagementProjectMutationRequest,
    authority: &ProjectAuthority,
    cutex_session_id: &CutexSessionId,
    committed_at: &Rfc3339,
) -> Result<(), AgentManagementError> {
    let removed = state
        .operator_grants
        .get_mut(&request.project_id)
        .and_then(|grants| grants.remove(cutex_session_id));
    if removed.is_none() {
        return Ok(());
    }
    let previous = management_operator_grant_revision(state, &request.project_id);
    let revision = management_next_operator_grant_revision(previous)?;
    state
        .operator_grant_revisions
        .insert(request.project_id.clone(), revision);
    let event_id = format!(
        "human-management:{}:member-detach-operator:{}",
        request.action_id, revision
    );
    let event = super::AgentOperatorAuditEvent {
        event_id: event_id.clone(),
        action_id: request.action_id.clone(),
        project_id: request.project_id.clone(),
        operator_cutex_session_id: cutex_session_id.clone(),
        kind: super::AgentOperatorAuditKind::Revoked,
        previous_grant_revision: previous,
        grant_revision: revision,
        primary_director_cutex_session_id: authority.authorized_director_session.clone(),
        performed_by_human_management: true,
        committed_at: committed_at.clone(),
    };
    if state
        .operator_audit_events
        .insert(event_id, event)
        .is_some()
    {
        return Err(AgentManagementError::InvalidStore);
    }
    Ok(())
}

fn project_workspace(
    snapshot: &super::AgentManagementSnapshot,
    project_id: &ProjectId,
    authority: Option<&ProjectAuthority>,
    state: ProjectStateRecord,
    access_role: ProjectAccessRole,
    observer: &dyn ProjectRuntimeObserver,
) -> Result<CutexProjectWorkspace, AgentManagementError> {
    let mut active_agents = Vec::new();
    let mut archived_agents = Vec::new();
    let mut retired_agents = Vec::new();
    let mut agent_operators = Vec::new();
    let mut director_member = None;
    for agent in snapshot.agents.values().filter(|agent| {
        agent.retired_at.is_some() && agent.project_id.as_ref() == Some(project_id)
            || current_project_id(snapshot, agent).as_ref() == Some(project_id)
    }) {
        let member = project_member(agent.clone(), observer);
        if agent.retired_at.is_none()
            && snapshot
                .reversible_archive_projection
                .get(&agent.cutex_session_id)
                .copied()
                .unwrap_or(false)
        {
            archived_agents.push(member);
            continue;
        }
        if authority.is_some_and(|authority| agent.cutex_session_id == authority.authorized_director_session) {
            director_member = Some(member);
        } else if let Some(grant) = snapshot
            .operator_grants
            .get(project_id)
            .and_then(|grants| grants.get(&agent.cutex_session_id))
        {
            agent_operators.push(ProjectAgentOperatorProjection {
                grant: grant.clone(),
                member,
            });
        } else if agent.retired_at.is_some() {
            retired_agents.push(member);
        } else {
            active_agents.push(member);
        }
    }
    active_agents.sort_by(|left, right| {
        left.agent
            .cutex_session_id
            .cmp(&right.agent.cutex_session_id)
    });
    retired_agents.sort_by(|left, right| {
        left.agent
            .cutex_session_id
            .cmp(&right.agent.cutex_session_id)
    });
    agent_operators.sort_by(|left, right| {
        left.grant
            .operator_cutex_session_id
            .cmp(&right.grant.operator_cutex_session_id)
    });
    Ok(CutexProjectWorkspace {
        project_id: project_id.clone(),
        authority_epoch: effective_authority_epoch(snapshot, project_id, authority),
        lifecycle: state.lifecycle,
        project_revision: state.revision,
        director: authority.map(|authority| ProjectDirectorProjection {
            cutex_session_id: authority.authorized_director_session.clone(),
            member: director_member,
        }),
        access_role,
        operator_grant_revision: management_operator_grant_revision(snapshot, project_id),
        agent_operators,
        presentation: effective_presentation(
            project_id,
            snapshot.project_presentations.get(project_id),
        ),
        active_agents,
        archived_agents,
        retired_agents,
        workspace_root: state.workspace_root,
    })
}

fn authorized_primary_authority<'a>(
    snapshot: &'a super::AgentManagementSnapshot,
    invocation: &AgentManagementInvocation,
    project_id: &ProjectId,
) -> Result<&'a ProjectAuthority, AgentManagementError> {
    let authority = snapshot
        .projects
        .get(project_id)
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    if authority.authorized_director_session != invocation.caller_cutex_session {
        return Err(AgentManagementError::ProjectNotAuthorized);
    }
    Ok(authority)
}

fn authorized_project<'a>(
    snapshot: &'a super::AgentManagementSnapshot,
    invocation: &AgentManagementInvocation,
    project_id: &ProjectId,
) -> Result<(&'a ProjectAuthority, ProjectAccessRole), AgentManagementError> {
    let authority = snapshot
        .projects
        .get(project_id)
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    let role = project_access_role(snapshot, invocation, project_id)?;
    Ok((authority, role))
}

fn project_access_role(
    snapshot: &super::AgentManagementSnapshot,
    invocation: &AgentManagementInvocation,
    project_id: &ProjectId,
) -> Result<ProjectAccessRole, AgentManagementError> {
    let authority = snapshot
        .projects
        .get(project_id)
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    if authority.authorized_director_session == invocation.caller_cutex_session {
        return Ok(ProjectAccessRole::PrimaryDirector);
    }
    let has_grant = snapshot
        .operator_grants
        .get(project_id)
        .is_some_and(|grants| grants.contains_key(&invocation.caller_cutex_session));
    let active_owned = snapshot
        .agents
        .get(&invocation.caller_cutex_session)
        .is_some_and(|agent| {
            current_project_id(snapshot, agent).as_ref() == Some(project_id)
                && agent.retired_at.is_none()
        });
    if has_grant && active_owned {
        Ok(ProjectAccessRole::AgentOperator)
    } else {
        Err(AgentManagementError::ProjectNotAuthorized)
    }
}

fn summary(
    snapshot: &super::AgentManagementSnapshot,
    project_id: &ProjectId,
    authority: Option<&ProjectAuthority>,
    state: ProjectStateRecord,
    access_role: ProjectAccessRole,
) -> CutexProjectSummary {
    let mut active_member_count = 0;
    let mut archived_member_count = 0;
    let mut retired_member_count = 0;
    for agent in snapshot.agents.values().filter(|agent| {
        agent.retired_at.is_some() && agent.project_id.as_ref() == Some(project_id)
            || current_project_id(snapshot, agent).as_ref() == Some(project_id)
    }) {
        if agent.retired_at.is_some() {
            retired_member_count += 1;
        } else if snapshot
            .reversible_archive_projection
            .get(&agent.cutex_session_id)
            .copied()
            .unwrap_or(false)
        {
            archived_member_count += 1;
        } else {
            active_member_count += 1;
        }
    }
    CutexProjectSummary {
        project_id: project_id.clone(),
        authority_epoch: effective_authority_epoch(snapshot, project_id, authority),
        director_cutex_session_id: authority
            .map(|authority| authority.authorized_director_session.clone()),
        director_name: authority
            .and_then(|authority| snapshot.agents.get(&authority.authorized_director_session))
            .map(|agent| agent.spec.name.clone()),
        access_role,
        operator_count: snapshot
            .operator_grants
            .get(project_id)
            .map_or(0, BTreeMap::len),
        presentation: effective_presentation(
            project_id,
            snapshot.project_presentations.get(project_id),
        ),
        active_member_count,
        archived_member_count,
        retired_member_count,
        lifecycle: state.lifecycle,
        project_revision: state.revision,
        workspace_root: state.workspace_root,
    }
}

fn project_member(
    agent: ManagedAgentRecord,
    observer: &dyn ProjectRuntimeObserver,
) -> ProjectMemberProjection {
    match observer.observe(&agent.cutex_session_id) {
        Ok(runtime) => {
            let online = runtime.active
                && (!runtime.runtime_agent_ids.is_empty() || runtime.app_server_runtime);
            ProjectMemberProjection {
                agent,
                lifecycle: if online {
                    ProjectMemberLifecycle::Online
                } else {
                    ProjectMemberLifecycle::Offline
                },
                runtime: Some(runtime),
                observation_error: None,
            }
        }
        Err(error) => ProjectMemberProjection {
            agent,
            lifecycle: ProjectMemberLifecycle::Unavailable,
            runtime: None,
            observation_error: Some(error.to_string().chars().take(512).collect()),
        },
    }
}

pub fn effective_presentation(
    project_id: &ProjectId,
    stored: Option<&ProjectPresentationSettings>,
) -> EffectiveProjectPresentation {
    match stored {
        Some(settings) => EffectiveProjectPresentation {
            display_name: settings.display_name.clone(),
            badge_label: settings.badge_label.clone(),
            color: settings.color,
            revision: settings.revision,
            stored: true,
        },
        None => {
            let display_name = project_id.as_str().to_string();
            EffectiveProjectPresentation {
                badge_label: default_badge(&display_name),
                color: default_color(project_id),
                display_name,
                revision: 0,
                stored: false,
            }
        }
    }
}

fn default_badge(name: &str) -> String {
    let words = name
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let badge = if words.len() >= 2 {
        words
            .iter()
            .take(2)
            .filter_map(|word| word.chars().next())
            .collect::<String>()
    } else {
        words
            .first()
            .copied()
            .unwrap_or("P")
            .chars()
            .take(2)
            .collect::<String>()
    };
    badge.to_ascii_uppercase()
}

fn default_color(project_id: &ProjectId) -> ProjectPaletteColor {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in project_id.as_str().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    ProjectPaletteColor::ALL[(hash % ProjectPaletteColor::ALL.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_management::{
        AgentActionId, AgentActionPhase, AgentManagementPhaseEvent, AgentOperationKind,
        DirectorRotateMode, ManagedAgentSpec, ProjectAuthority,
    };

    fn session(value: &str) -> CutexSessionId {
        CutexSessionId::new(value).unwrap()
    }

    fn project(value: &str) -> ProjectId {
        ProjectId::new(value).unwrap()
    }

    fn timestamp() -> Rfc3339 {
        Rfc3339::new("2026-09-03T00:00:00Z").unwrap()
    }

    fn invocation(value: &str) -> AgentManagementInvocation {
        AgentManagementInvocation {
            caller_cutex_session: session(value),
            caller_runtime_agent_id: format!("runtime-{value}"),
        }
    }

    fn agent(project_id: &ProjectId, value: &str, retired: bool) -> ManagedAgentRecord {
        ManagedAgentRecord {
            project_id: Some(project_id.clone()),
            created_by_director_session: Some(session("cutex.director")),
            created_by_operator_session: None,
            cutex_session_id: session(value),
            native_session_id: format!("native-{value}"),
            spec: ManagedAgentSpec {
                name: value.to_string(),
                cwd: format!("/tmp/{value}"),
                profile: Some("default".to_string()),
                runtime_backend: "app_server".to_string(),
                model: "gpt-test".to_string(),
                reasoning: "medium".to_string(),
                permissions: "default".to_string(),
                approval_policy: "never".to_string(),
                sandbox_mode: "workspace-write".to_string(),
                groups: vec!["workers".to_string()],
                expose_to_im: false,
                pin: false,
            },
            created_at: timestamp(),
            retired_at: retired.then(timestamp),
        }
    }

    fn provider_with_project() -> (AgentManagementProvider, std::path::PathBuf, ProjectId) {
        let root =
            std::env::temp_dir().join(format!("cutex-project-projection-{}", uuid::Uuid::new_v4()));
        let provider = AgentManagementProvider::open(&root).unwrap();
        let project_id = project("project-alpha");
        provider
            .store()
            .with_state(true, |mut state| {
                state.projects.insert(
                    project_id.clone(),
                    ProjectAuthority {
                        project_id: project_id.clone(),
                        authorized_director_session: session("cutex.director"),
                        authority_epoch: 7,
                        updated_at: timestamp(),
                    },
                );
                for record in [
                    agent(&project_id, "cutex.director", false),
                    agent(&project_id, "cutex.worker-online", false),
                    agent(&project_id, "cutex.worker-retired", true),
                ] {
                    state.agents.insert(record.cutex_session_id.clone(), record);
                }
                Ok((state, (), true))
            })
            .unwrap();
        (provider, root, project_id)
    }

    fn observation(id: &CutexSessionId) -> Result<AgentRuntimeObservation, AgentManagementError> {
        Ok(AgentRuntimeObservation {
            cutex_session_id: id.clone(),
            native_session_id: format!("native-{}", id.as_str()),
            active: true,
            cwd: "/tmp/exact".to_string(),
            profile: "default".to_string(),
            runtime_backend: "app_server".to_string(),
            model: "gpt-test".to_string(),
            reasoning: "medium".to_string(),
            permissions: "default".to_string(),
            approval_policy: "never".to_string(),
            sandbox_mode: "workspace-write".to_string(),
            groups: vec!["project:forged".to_string()],
            runtime_generation: 1,
            runtime_version: None,
            runtime_agent_ids: vec![format!("runtime-{}", id.as_str())],
            app_server_runtime: true,
            agent_bus_endpoint_ids: Vec::new(),
        })
    }

    #[test]
    fn defaults_are_stable_and_require_no_store_write() {
        let (provider, root, project_id) = provider_with_project();
        let before = provider.store().snapshot().unwrap().store_revision;
        let first = provider
            .list_cutex_projects(&invocation("cutex.director"))
            .unwrap();
        let second = provider
            .list_cutex_projects(&invocation("cutex.director"))
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first[0].presentation.display_name, "project-alpha");
        assert_eq!(first[0].presentation.badge_label, "PA");
        assert!(!first[0].presentation.stored);
        assert_eq!(
            effective_presentation(&project("cutex-stack-main"), None).badge_label,
            "CS"
        );
        assert_eq!(provider.store().snapshot().unwrap().store_revision, before);
        std::fs::remove_dir_all(root).unwrap();
        let _ = project_id;
    }

    #[test]
    fn project_colors_keep_legacy_json_tokens_and_round_trip_rgb() {
        for (token, color) in [
            ("cyan", ProjectPaletteColor::Cyan),
            ("blue", ProjectPaletteColor::Blue),
            ("green", ProjectPaletteColor::Green),
            ("magenta", ProjectPaletteColor::Magenta),
            ("yellow", ProjectPaletteColor::Yellow),
            ("red", ProjectPaletteColor::Red),
        ] {
            assert_eq!(
                serde_json::from_str::<ProjectPaletteColor>(&format!("\"{token}\"")).unwrap(),
                color
            );
            assert_eq!(
                serde_json::to_string(&color).unwrap(),
                format!("\"{token}\"")
            );
        }

        let rgb = serde_json::from_str::<ProjectPaletteColor>("\"#12aBcF\"").unwrap();
        assert_eq!(rgb, ProjectPaletteColor::Rgb(0x12, 0xab, 0xcf));
        assert_eq!(serde_json::to_string(&rgb).unwrap(), "\"#12ABCF\"");
        assert_eq!(
            serde_json::from_str::<ProjectPaletteColor>(&serde_json::to_string(&rgb).unwrap())
                .unwrap(),
            rgb
        );

        let presentation: ProjectPresentationInput = serde_json::from_value(serde_json::json!({
            "display_name": "RGB Project",
            "badge_label": "CX",
            "color": "#102A4F"
        }))
        .unwrap();
        assert_eq!(
            presentation.color,
            ProjectPaletteColor::Rgb(0x10, 0x2a, 0x4f)
        );
        let encoded = serde_json::to_value(&presentation).unwrap();
        assert_eq!(encoded["color"], "#102A4F");
        assert_eq!(
            serde_json::from_value::<ProjectPresentationInput>(encoded).unwrap(),
            presentation
        );
    }

    #[test]
    fn project_color_rejects_non_rgb_hex_forms_with_a_clear_error() {
        for invalid in [
            "#123", "#12345", "#1234567", "123456", "#12GG56", "cyan ", "#12é45",
        ] {
            let error = invalid.parse::<ProjectPaletteColor>().unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid project badge color; expected cyan, blue, green, magenta, yellow, red, or #RRGGBB"
            );
            assert!(
                serde_json::from_str::<ProjectPaletteColor>(&format!("\"{invalid}\""))
                    .unwrap_err()
                    .to_string()
                    .contains("expected cyan, blue, green, magenta, yellow, red, or #RRGGBB")
            );
        }
    }

    #[test]
    fn exact_owning_project_identity_controls_reads_not_groups_or_names() {
        let (provider, root, project_id) = provider_with_project();
        assert_eq!(
            provider
                .read_cutex_project(
                    &invocation("cutex.worker-online"),
                    &project_id,
                    &observation
                )
                .unwrap_err(),
            AgentManagementError::ProjectNotAuthorized
        );
        assert_eq!(
            provider
                .read_cutex_project(
                    &invocation("cutex.director"),
                    &project("project-lookalike"),
                    &observation,
                )
                .unwrap_err(),
            AgentManagementError::ProjectNotAuthorized
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn details_keep_director_active_and_retired_members_separate() {
        let (provider, root, project_id) = provider_with_project();
        let workspace = provider
            .read_cutex_project(&invocation("cutex.director"), &project_id, &observation)
            .unwrap();
        assert_eq!(workspace.project_id, project_id);
        assert_eq!(workspace.authority_epoch, 7);
        assert_eq!(
            workspace.director.unwrap().member.unwrap().agent.cutex_session_id,
            session("cutex.director")
        );
        assert_eq!(workspace.active_agents.len(), 1);
        assert_eq!(workspace.retired_agents.len(), 1);
        assert_eq!(
            workspace.retired_agents[0].agent.cutex_session_id,
            session("cutex.worker-retired")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_validates_badge_and_authorized_director() {
        let (provider, root, project_id) = provider_with_project();
        let request = ProjectPresentationUpdateRequest {
            project_id: project_id.clone(),
            expected_presentation_revision: 0,
            presentation: ProjectPresentationInput {
                display_name: "Alpha Team".to_string(),
                badge_label: "AT".to_string(),
                color: ProjectPaletteColor::Green,
            },
        };
        assert_eq!(
            provider
                .update_project_presentation(&invocation("cutex.worker-online"), &request)
                .unwrap_err(),
            AgentManagementError::ProjectNotAuthorized
        );
        let saved = provider
            .update_project_presentation(&invocation("cutex.director"), &request)
            .unwrap();
        assert_eq!(saved.revision, 1);

        let mut invalid = request;
        invalid.expected_presentation_revision = 1;
        invalid.presentation.badge_label = "ABCDE".to_string();
        assert_eq!(
            provider.update_project_presentation(&invocation("cutex.director"), &invalid),
            Err(AgentManagementError::InvalidRequest(
                "invalid_project_badge_label"
            ))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_validation_enforces_bounded_name_and_terminal_cell_badge() {
        let valid = |display_name: &str, badge_label: &str| ProjectPresentationInput {
            display_name: display_name.to_string(),
            badge_label: badge_label.to_string(),
            color: ProjectPaletteColor::Magenta,
        };
        assert!(valid("Alpha", "A").validate().is_ok());
        assert!(valid("Alpha", "界").validate().is_ok());
        assert!(valid("Alpha", "CX").validate().is_ok());
        assert!(valid("Alpha", "ABCD").validate().is_ok());
        assert!(valid("Alpha", "中文").validate().is_ok());
        assert!(valid("Alpha", "A\u{0301}").validate().is_ok());
        assert!(valid("Alpha", "❤️").validate().is_ok());
        assert!(valid("Alpha", "👨‍👩‍👧‍👦").validate().is_ok());
        for bad in [format!("A{}","\u{200b}".repeat(100_000)),"A\u{202e}".into(),"A\u{200d}".into(),"A\u{3164}".into()] {
            assert!(valid("Alpha",&bad).validate().is_err());
        }
        for invalid in [
            valid("", "A"),
            valid("bad\nname", "A"),
            valid(&"x".repeat(81), "A"),
            valid("Alpha", ""),
            valid("Alpha", "ABCDE"),
            valid("Alpha", "A B"),
            valid("Alpha", "界界A"),
        ] {
            assert!(matches!(
                invalid.validate(),
                Err(AgentManagementError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn management_project_reads_need_no_agent_invocation_or_environment_identity() {
        let (provider, root, project_id) = provider_with_project();
        let principal = HumanManagementPrincipal::authenticated();
        let collection = provider
            .list_cutex_projects_for_management(&principal)
            .unwrap();
        assert_eq!(collection.projects.len(), 1);
        assert_eq!(
            collection.projects[0].access_role,
            ProjectAccessRole::HumanManagement
        );
        let workspace = provider
            .read_cutex_project_for_management(&principal, &project_id, &observation)
            .unwrap();
        assert_eq!(workspace.project_id, project_id);
        assert_eq!(workspace.access_role, ProjectAccessRole::HumanManagement);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn management_presentation_write_fences_authority_and_records_human_actor() {
        let (provider, root, project_id) = provider_with_project();
        let principal = HumanManagementPrincipal::authenticated();
        let mut request = HumanManagementPresentationUpdateRequest {
            schema: crate::management::control_plane::HumanManagementPresentationSchema::V1,
            project_id: project_id.clone(),
            expected_authority_epoch: Some(6),
            expected_presentation_revision: Some(0),
            presentation: ProjectPresentationInput {
                display_name: "Control Plane".to_string(),
                badge_label: "CX".to_string(),
                color: ProjectPaletteColor::Magenta,
            },
        };
        assert_eq!(
            provider.update_project_presentation_for_management(&principal, &request),
            Err(AgentManagementError::Conflict("stale_project_authority"))
        );
        request.expected_authority_epoch = Some(7);
        let saved = provider
            .update_project_presentation_for_management(&principal, &request)
            .unwrap();
        assert!(saved.updated_by_human_management);
        assert!(saved.updated_by_director_session.is_none());
        assert_eq!(saved.badge_label, "CX");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn management_operator_grant_revoke_are_idempotent_cas_and_fully_audited() {
        let (provider, root, project_id) = provider_with_project();
        let principal = HumanManagementPrincipal::authenticated();
        let operator = session("cutex.worker-online");
        let grant = HumanManagementOperatorActionRequest {
            schema: crate::management::control_plane::HumanManagementOperatorSchema::V1,
            action_id: AgentActionId::new("human-grant-1").unwrap(),
            project_id: project_id.clone(),
            expected_authority_epoch: Some(7),
            expected_grant_revision: Some(0),
            operation: HumanManagementOperatorKind::Grant,
            operator_cutex_session_id: operator.clone(),
        };
        let receipt = provider
            .execute_operator_action_for_management(&principal, &grant)
            .unwrap();
        assert_eq!(receipt.grant_revision, 1);
        assert!(receipt.audit_event.performed_by_human_management);
        assert!(
            receipt
                .grant
                .as_ref()
                .unwrap()
                .performed_by_human_management
        );
        let after_grant = provider.store().snapshot().unwrap();
        assert_eq!(after_grant.operator_audit_events.len(), 1);
        assert_eq!(after_grant.human_management_operator_actions.len(), 1);
        let revision = after_grant.store_revision;
        assert_eq!(
            provider
                .execute_operator_action_for_management(&principal, &grant)
                .unwrap(),
            receipt
        );
        assert_eq!(
            provider.store().snapshot().unwrap().store_revision,
            revision
        );
        assert_eq!(
            provider.bind_project_authority(&crate::agent_management::ProjectAuthorityRequest {
                schema: crate::agent_management::AgentManagementSchema::V1,
                action_id: AgentActionId::new("human-grant-1").unwrap(),
                project_id: project_id.clone(),
                authorized_director_session: session("cutex.director"),
                expected_authorized_director_session: Some(session("cutex.director")),
                expected_authority_epoch: Some(7),
            }),
            Err(AgentManagementError::Conflict("action_id_domain_conflict"))
        );

        let stale = HumanManagementOperatorActionRequest {
            action_id: AgentActionId::new("human-grant-stale").unwrap(),
            ..grant.clone()
        };
        assert_eq!(
            provider.execute_operator_action_for_management(&principal, &stale),
            Err(AgentManagementError::Conflict(
                "operator_grant_revision_conflict"
            ))
        );
        let revoke = HumanManagementOperatorActionRequest {
            schema: crate::management::control_plane::HumanManagementOperatorSchema::V1,
            action_id: AgentActionId::new("human-revoke-1").unwrap(),
            project_id: project_id.clone(),
            expected_authority_epoch: Some(7),
            expected_grant_revision: Some(1),
            operation: HumanManagementOperatorKind::Revoke,
            operator_cutex_session_id: operator,
        };
        let revoked = provider
            .execute_operator_action_for_management(&principal, &revoke)
            .unwrap();
        assert_eq!(revoked.grant_revision, 2);
        assert!(revoked.grant.is_none());
        assert!(provider.store().snapshot().unwrap().operator_grants[&project_id].is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_director_rotation_history_still_loads_and_reads_without_writes() {
        let (provider, root, project_id) = provider_with_project();
        provider
            .store()
            .with_state(true, |mut state| {
                let action_id = AgentActionId::new("legacy-r11-rotation").unwrap();
                state.phase_events.insert(
                    "legacy-r11-rotation:complete".to_string(),
                    AgentManagementPhaseEvent {
                        event_id: "legacy-r11-rotation:complete".to_string(),
                        action_id,
                        project_id: project_id.clone(),
                        operation: AgentOperationKind::DirectorRotate,
                        phase: AgentActionPhase::Complete,
                        phase_sequence: 4,
                        committed_at: timestamp(),
                        presentation_owner_cutex_session_id: session("cutex.director"),
                        subject_cutex_session_id: None,
                        subject_agent_name: None,
                        predecessor_cutex_session_id: Some(session("cutex.worker-online")),
                        successor_cutex_session_id: Some(session("cutex.director")),
                        replace_policy: None,
                        rotation_mode: Some(DirectorRotateMode::RetainPredecessorWithMessage),
                        authority_epoch: Some(7),
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        let before = provider.store().snapshot().unwrap().store_revision;
        let workspace = provider
            .read_cutex_project_for_management(
                &HumanManagementPrincipal::authenticated(),
                &project_id,
                &observation,
            )
            .unwrap();
        assert_eq!(workspace.project_id, project_id);
        let after = provider.store().snapshot().unwrap();
        assert!(!after.operator_grants.contains_key(&project_id));
        assert_eq!(after.store_revision, before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_write_preserves_unknown_additive_fields() {
        let (provider, root, project_id) = provider_with_project();
        provider
            .store()
            .with_state(true, |mut state| {
                state
                    .extra
                    .insert("future_root".to_string(), serde_json::json!({"kept": true}));
                state.project_presentations.insert(
                    project_id.clone(),
                    ProjectPresentationSettings {
                        display_name: "Old".to_string(),
                        badge_label: "O".to_string(),
                        color: ProjectPaletteColor::Cyan,
                        revision: 3,
                        updated_at: timestamp(),
                        updated_by_director_session: Some(session("cutex.director")),
                        updated_by_human_management: false,
                        extra: BTreeMap::from([(
                            "future_setting".to_string(),
                            serde_json::json!([1, 2, 3]),
                        )]),
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        serde_json::from_slice::<super::super::AgentManagementSnapshot>(
            &std::fs::read(root.join("agent-management-v1.json")).unwrap(),
        )
        .expect("snapshot with additive fields remains readable");
        provider
            .update_project_presentation(
                &invocation("cutex.director"),
                &ProjectPresentationUpdateRequest {
                    project_id: project_id.clone(),
                    expected_presentation_revision: 3,
                    presentation: ProjectPresentationInput {
                        display_name: "New".to_string(),
                        badge_label: "N".to_string(),
                        color: ProjectPaletteColor::Yellow,
                    },
                },
            )
            .unwrap();
        let snapshot = provider.store().snapshot().unwrap();
        assert_eq!(snapshot.extra["future_root"]["kept"], true);
        assert_eq!(
            snapshot.project_presentations[&project_id].extra["future_setting"],
            serde_json::json!([1, 2, 3])
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_provider_is_an_authorization_error_not_a_native_workspace_fallback() {
        let root =
            std::env::temp_dir().join(format!("cutex-project-empty-{}", uuid::Uuid::new_v4()));
        let provider = AgentManagementProvider::open(&root).unwrap();
        assert_eq!(
            provider.list_cutex_projects(&invocation("cutex.director")),
            Err(AgentManagementError::NotAuthorizedDirector)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operator_projection_is_distinct_and_cannot_mutate_presentation() {
        let (provider, root, project_id) = provider_with_project();
        let operator = session("cutex.worker-online");
        provider
            .store()
            .with_state(true, |mut state| {
                state
                    .operator_grants
                    .entry(project_id.clone())
                    .or_default()
                    .insert(
                        operator.clone(),
                        AgentOperatorGrant {
                            project_id: project_id.clone(),
                            operator_cutex_session_id: operator.clone(),
                            grant_revision: 1,
                            granted_at: timestamp(),
                            granted_by_primary_director_session: session("cutex.director"),
                            performed_by_human_management: false,
                        },
                    );
                state.operator_grant_revisions.insert(project_id.clone(), 1);
                Ok((state, (), true))
            })
            .unwrap();

        let summaries = provider
            .list_cutex_projects(&invocation(operator.as_str()))
            .unwrap();
        assert_eq!(summaries[0].access_role, ProjectAccessRole::AgentOperator);
        assert_eq!(summaries[0].operator_count, 1);
        let workspace = provider
            .read_cutex_project(&invocation(operator.as_str()), &project_id, &observation)
            .unwrap();
        assert_eq!(workspace.access_role, ProjectAccessRole::AgentOperator);
        assert_eq!(
            workspace.director.unwrap().cutex_session_id,
            session("cutex.director")
        );
        assert_eq!(workspace.agent_operators.len(), 1);
        assert!(workspace.active_agents.is_empty());
        assert!(matches!(
            provider.update_project_presentation(
                &invocation(operator.as_str()),
                &ProjectPresentationUpdateRequest {
                    project_id: project_id.clone(),
                    expected_presentation_revision: 0,
                    presentation: ProjectPresentationInput {
                        display_name: "Forbidden".to_string(),
                        badge_label: "F".to_string(),
                        color: ProjectPaletteColor::Red,
                    },
                }
            ),
            Err(AgentManagementError::ProjectNotAuthorized)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// r219: the project Director is the Agent Management authority only; no
    /// Agent Management path opens or writes the removed seat store copy.
    fn assert_no_seat_store(root: &std::path::Path) {
        assert!(
            !root.join("task-service-seat-authority-v1").exists(),
            "Agent Management must not create the removed Director seat store"
        );
    }

    #[test]
    fn human_project_membership_archive_restore_remove_are_fenced_and_replayable() {
        let (provider, root, project_id) = provider_with_project();
        let principal = HumanManagementPrincipal::authenticated();
        let detach = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("human-detach-worker").unwrap(),
            project_id: project_id.clone(),
            expected_authority_epoch: Some(7),
            expected_project_revision: Some(0),
            operation: HumanManagementProjectMutationKind::DetachMember {
                cutex_session_id: session("cutex.worker-online"),
            },
        };
        let first = provider
            .execute_project_mutation_for_management(&principal, &detach)
            .unwrap();
        assert_eq!(first.project_revision, 1);
        assert_eq!(
            provider
                .execute_project_mutation_for_management(&principal, &detach)
                .unwrap(),
            first
        );
        let snapshot = provider.store().snapshot().unwrap();
        assert_eq!(
            snapshot.agents[&session("cutex.worker-online")].project_id,
            Some(project_id.clone())
        );
        assert_eq!(
            snapshot.current_project_memberships[&session("cutex.worker-online")].project_id,
            None
        );

        let archive = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("human-archive-project").unwrap(),
            project_id: project_id.clone(),
            expected_authority_epoch: Some(7),
            expected_project_revision: Some(1),
            operation: HumanManagementProjectMutationKind::Archive,
        };
        provider
            .execute_project_mutation_for_management(&principal, &archive)
            .unwrap();
        let collection = provider
            .list_cutex_projects_for_management(&principal)
            .unwrap();
        assert!(collection.projects.is_empty());
        assert_eq!(collection.archived_projects.len(), 1);
        // An archived project is not offered to its Director's agent reads.
        assert_eq!(
            provider.list_cutex_projects(&invocation("cutex.director")),
            Err(AgentManagementError::NotAuthorizedDirector)
        );
        assert_eq!(
            provider.update_project_presentation_for_management(
                &principal,
                &HumanManagementPresentationUpdateRequest {
                    schema: crate::management::control_plane::HumanManagementPresentationSchema::V1,
                    project_id: project_id.clone(),
                    expected_authority_epoch: Some(7),
                    expected_presentation_revision: Some(0),
                    presentation: ProjectPresentationInput {
                        display_name: "Archived write".to_string(),
                        badge_label: "AW".to_string(),
                        color: ProjectPaletteColor::Green,
                    },
                },
            ),
            Err(AgentManagementError::Conflict("project_is_archived"))
        );

        let restore = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("human-restore-project").unwrap(),
            project_id: project_id.clone(),
            expected_authority_epoch: Some(7),
            expected_project_revision: Some(2),
            operation: HumanManagementProjectMutationKind::Restore,
        };
        provider
            .execute_project_mutation_for_management(&principal, &restore)
            .unwrap();
        let archive_again = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("human-archive-project-again").unwrap(),
            project_id: project_id.clone(),
            expected_authority_epoch: Some(7),
            expected_project_revision: Some(3),
            operation: HumanManagementProjectMutationKind::Archive,
        };
        provider
            .execute_project_mutation_for_management(&principal, &archive_again)
            .unwrap();

        // r219: Remove no longer consults the removed Task Service.
        let remove = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("human-remove-project").unwrap(),
            project_id: project_id.clone(),
            expected_authority_epoch: Some(7),
            expected_project_revision: Some(4),
            operation: HumanManagementProjectMutationKind::Remove,
        };
        provider
            .execute_project_mutation_for_management(&principal, &remove)
            .unwrap();
        let snapshot = provider.store().snapshot().unwrap();
        assert!(!snapshot.projects.contains_key(&project_id));
        assert!(snapshot.project_tombstones.contains_key(&project_id));
        assert_eq!(
            snapshot.agents[&session("cutex.director")].project_id,
            Some(project_id)
        );
        assert_no_seat_store(&root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn distinct_legacy_projects_archive_from_their_own_authorities() {
        let root = std::env::temp_dir().join(format!(
            "cutex-project-legacy-multi-archive-{}",
            uuid::Uuid::new_v4()
        ));
        let provider = AgentManagementProvider::open(&root).unwrap();
        let projects = [
            (project("legacy-vce"), session("cutex.vce-director")),
            (project("legacy-ops"), session("cutex.ops-director")),
        ];
        provider
            .store()
            .with_state(true, |mut state| {
                for (project_id, director) in &projects {
                    state.projects.insert(
                        project_id.clone(),
                        ProjectAuthority {
                            project_id: project_id.clone(),
                            authorized_director_session: director.clone(),
                            authority_epoch: 1,
                            updated_at: timestamp(),
                        },
                    );
                    let record = agent(project_id, director.as_str(), false);
                    state.agents.insert(director.clone(), record);
                }
                Ok((state, (), true))
            })
            .unwrap();
        let principal = HumanManagementPrincipal::authenticated();
        let mut receipts = Vec::new();
        for (index, (project_id, _)) in projects.iter().enumerate() {
            let request = HumanManagementProjectMutationRequest {
                schema: HumanManagementProjectMutationSchema::V1,
                action_id: AgentActionId::new(format!("archive-legacy-{index}")).unwrap(),
                project_id: project_id.clone(),
                expected_authority_epoch: Some(1),
                expected_project_revision: Some(0),
                operation: HumanManagementProjectMutationKind::Archive,
            };
            let receipt = provider
                .execute_project_mutation_for_management(&principal, &request)
                .unwrap();
            assert_eq!(
                provider
                    .execute_project_mutation_for_management(&principal, &request)
                    .unwrap(),
                receipt
            );
            receipts.push(receipt);
        }
        assert_eq!(receipts.len(), 2);
        let management = provider.store().snapshot().unwrap();
        for (project_id, director) in &projects {
            assert_eq!(
                management.project_states[project_id].lifecycle,
                ProjectLifecycle::Archived
            );
            assert_eq!(
                management.projects[project_id].authorized_director_session,
                *director
            );
        }
        let collection = provider
            .list_cutex_projects_for_management(&principal)
            .unwrap();
        assert!(collection.projects.is_empty());
        assert_eq!(collection.archived_projects.len(), 2);
        assert_no_seat_store(&root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_director_authorizing_two_legacy_projects_can_archive_one() {
        // r219: there is no seat to materialize, so a legacy store where one
        // session authorizes two active projects can be cleaned up by archive.
        let root = std::env::temp_dir().join(format!(
            "cutex-project-legacy-director-conflict-{}",
            uuid::Uuid::new_v4()
        ));
        let provider = AgentManagementProvider::open(&root).unwrap();
        let director = session("cutex.shared-director");
        let first = project("legacy-first");
        let second = project("legacy-second");
        provider
            .store()
            .with_state(true, |mut state| {
                for project_id in [&first, &second] {
                    state.projects.insert(
                        project_id.clone(),
                        ProjectAuthority {
                            project_id: project_id.clone(),
                            authorized_director_session: director.clone(),
                            authority_epoch: 1,
                            updated_at: timestamp(),
                        },
                    );
                }
                Ok((state, (), true))
            })
            .unwrap();
        let request = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("archive-shared-director").unwrap(),
            project_id: first.clone(),
            expected_authority_epoch: Some(1),
            expected_project_revision: Some(0),
            operation: HumanManagementProjectMutationKind::Archive,
        };
        let receipt = provider
            .execute_project_mutation_for_management(
                &HumanManagementPrincipal::authenticated(),
                &request,
            )
            .unwrap();
        assert_eq!(receipt.audit_event.kind, ProjectAuditKind::Archived);
        let snapshot = provider.store().snapshot().unwrap();
        assert_eq!(
            snapshot.project_states[&first].lifecycle,
            ProjectLifecycle::Archived
        );
        assert_eq!(
            snapshot.project_states[&second].lifecycle,
            ProjectLifecycle::Active
        );
        assert_no_seat_store(&root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn human_project_create_is_agent_management_only_and_replays_exactly() {
        let root =
            std::env::temp_dir().join(format!("cutex-project-create-{}", uuid::Uuid::new_v4()));
        let provider = AgentManagementProvider::open(&root).unwrap();
        let historic = project("historic-project");
        let director = agent(&historic, "cutex.new-director", false);
        provider
            .store()
            .with_state(true, |mut state| {
                state.current_project_memberships.insert(
                    director.cutex_session_id.clone(),
                    CurrentProjectMembership {
                        cutex_session_id: director.cutex_session_id.clone(),
                        project_id: None,
                        revision: 1,
                        updated_at: timestamp(),
                    },
                );
                state
                    .agents
                    .insert(director.cutex_session_id.clone(), director);
                Ok((state, (), true))
            })
            .unwrap();
        let request = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("human-create-project").unwrap(),
            project_id: project("new-project"),
            expected_authority_epoch: Some(0),
            expected_project_revision: Some(0),
            operation: HumanManagementProjectMutationKind::Create {
                director_cutex_session_id: session("cutex.new-director"),
                presentation: ProjectPresentationInput {
                    display_name: "New Project".to_string(),
                    badge_label: "NP".to_string(),
                    color: ProjectPaletteColor::Rgb(0x12, 0x34, 0x56),
                },
            },
        };
        let principal = HumanManagementPrincipal::authenticated();
        let first = provider
            .execute_project_mutation_for_management(&principal, &request)
            .unwrap();
        assert_eq!(first.project_revision, 1);
        assert_eq!(
            provider
                .execute_project_mutation_for_management(&principal, &request)
                .unwrap(),
            first
        );
        let snapshot = provider.store().snapshot().unwrap();
        assert_eq!(
            snapshot.projects[&project("new-project")].authorized_director_session,
            session("cutex.new-director")
        );
        assert_eq!(
            snapshot.project_presentations[&project("new-project")].color,
            ProjectPaletteColor::Rgb(0x12, 0x34, 0x56)
        );
        let listed = provider
            .list_cutex_projects_for_management(&principal)
            .unwrap();
        assert_eq!(listed.projects.len(), 1);
        // The Director sees its project through the authority alone.
        assert_eq!(
            provider
                .list_cutex_projects(&invocation("cutex.new-director"))
                .unwrap()[0]
                .project_id,
            project("new-project")
        );
        assert_no_seat_store(&root);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// r219: an r218 store that recorded a `repair_director_seat` mutation and
    /// its `director_seat_repaired` audit still loads and round-trips; neither
    /// a new repair nor an exact replay of the recorded one runs
    /// (`tool_removed`), and nothing creates the removed seat store.
    #[test]
    fn r218_director_seat_repair_history_loads_and_repairs_are_refused() {
        use serde_json::json;
        let root = std::env::temp_dir().join(format!(
            "cutex-project-r218-repair-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let digest = "a".repeat(64);
        let operation = json!({
            "kind": "repair_director_seat",
            "expected_legacy_occupant": "cutex.director",
            "expected_legacy_epoch": 7
        });
        let project_state = json!({
            "project_id": "project-alpha",
            "lifecycle": "active",
            "revision": 1,
            "created_at": "2026-09-01T00:00:00Z",
            "updated_at": "2026-10-01T00:00:00Z"
        });
        let audit = json!({
            "event_id": "human-management:repair-1:project:1",
            "action_id": "repair-1",
            "project_id": "project-alpha",
            "kind": "director_seat_repaired",
            "previous_project_revision": 0,
            "project_revision": 1,
            "performed_by_human_management": true,
            "committed_at": "2026-10-01T00:00:00Z"
        });
        let receipt = json!({
            "schema": "cutex/human-management-project-mutation/v1",
            "action_id": "repair-1",
            "request_sha256": digest,
            "project_id": "project-alpha",
            "operation": operation,
            "previous_project_revision": 0,
            "project_revision": 1,
            "project_state": project_state,
            "audit_event": audit,
            "committed_at": "2026-10-01T00:00:00Z"
        });
        let store = json!({
            "schema": "cutex/agent-management-store/v1",
            "store_revision": 3,
            "projects": {"project-alpha": {
                "project_id": "project-alpha",
                "authorized_director_session": "cutex.director",
                "authority_epoch": 7,
                "updated_at": "2026-10-01T00:00:00Z"
            }},
            "project_states": {"project-alpha": project_state},
            "human_management_project_mutations": {
                "repair-1": {"request_sha256": digest, "receipt": receipt}
            },
            "project_audit_events": {"human-management:repair-1:project:1": audit},
            "durable_import_actions": {},
            "agents": {},
            "actions": {},
            "authority_receipts": {},
            "failure_events": {}
        });
        std::fs::write(
            root.join("agent-management-v1.json"),
            serde_json::to_vec_pretty(&store).unwrap(),
        )
        .unwrap();
        let provider = AgentManagementProvider::open(&root).unwrap();
        // The store lock file exists once any store has been opened for writes.
        provider
            .store()
            .with_state(true, |state| Ok((state, (), false)))
            .unwrap();
        let snapshot = provider.store().snapshot().unwrap();
        let record =
            &snapshot.human_management_project_mutations[&AgentActionId::new("repair-1").unwrap()];
        assert!(matches!(
            record.receipt.operation,
            HumanManagementProjectMutationKind::RepairDirectorSeat {
                expected_legacy_epoch: 7,
                ..
            }
        ));
        assert_eq!(
            record.receipt.audit_event.kind,
            ProjectAuditKind::DirectorSeatRepaired
        );
        assert_eq!(
            snapshot.project_audit_events["human-management:repair-1:project:1"].kind,
            ProjectAuditKind::DirectorSeatRepaired
        );
        // The decode-only names survive a re-encode (the next store write).
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            encoded["human_management_project_mutations"]["repair-1"]["receipt"],
            receipt
        );
        assert_eq!(
            encoded["project_audit_events"]["human-management:repair-1:project:1"],
            audit
        );

        // Reads come from the Agent Management authority alone.
        let principal = HumanManagementPrincipal::authenticated();
        assert_eq!(
            provider
                .list_cutex_projects_for_management(&principal)
                .unwrap()
                .projects
                .len(),
            1
        );
        provider
            .read_cutex_project_for_management(&principal, &project("project-alpha"), &observation)
            .unwrap();
        assert_eq!(
            provider
                .list_cutex_projects(&invocation("cutex.director"))
                .unwrap()[0]
                .authority_epoch,
            7
        );

        // Neither an exact replay of the recorded repair nor a new one runs.
        let replay: HumanManagementProjectMutationRequest = serde_json::from_value(json!({
            "schema": "cutex/human-management-project-mutation/v1",
            "action_id": "repair-1",
            "project_id": "project-alpha",
            "operation": operation
        }))
        .unwrap();
        let fresh = HumanManagementProjectMutationRequest {
            action_id: AgentActionId::new("repair-2").unwrap(),
            expected_authority_epoch: Some(7),
            expected_project_revision: Some(1),
            ..replay.clone()
        };
        let revision = snapshot.store_revision;
        for request in [&replay, &fresh] {
            let error = provider
                .execute_project_mutation_for_management(&principal, request)
                .unwrap_err();
            assert_eq!(
                error,
                AgentManagementError::Removed(crate::agent_management::DIRECTOR_SEAT_REPAIR_REMOVED)
            );
            assert_eq!(error.code(), "tool_removed");
        }
        assert_eq!(provider.store().snapshot().unwrap().store_revision, revision);

        // Ordinary mutations keep the recorded history.
        provider
            .execute_project_mutation_for_management(
                &principal,
                &HumanManagementProjectMutationRequest {
                    schema: HumanManagementProjectMutationSchema::V1,
                    action_id: AgentActionId::new("archive-after-repair").unwrap(),
                    project_id: project("project-alpha"),
                    expected_authority_epoch: Some(7),
                    expected_project_revision: Some(1),
                    operation: HumanManagementProjectMutationKind::Archive,
                },
            )
            .unwrap();
        let reloaded: crate::agent_management::AgentManagementSnapshot = serde_json::from_slice(
            &std::fs::read(root.join("agent-management-v1.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            reloaded.human_management_project_mutations
                [&AgentActionId::new("repair-1").unwrap()],
            *record
        );
        assert_no_seat_store(&root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn human_project_workspace_root_is_canonical_cas_fenced_and_projected() {
        let root =
            std::env::temp_dir().join(format!("cutex-project-workspace-{}", uuid::Uuid::new_v4()));
        let provider = AgentManagementProvider::open(&root).unwrap();
        let historic = project("historic-project");
        let director = agent(&historic, "cutex.ws-director", false);
        provider
            .store()
            .with_state(true, |mut state| {
                state.current_project_memberships.insert(
                    director.cutex_session_id.clone(),
                    CurrentProjectMembership {
                        cutex_session_id: director.cutex_session_id.clone(),
                        project_id: None,
                        revision: 1,
                        updated_at: timestamp(),
                    },
                );
                state
                    .agents
                    .insert(director.cutex_session_id.clone(), director);
                Ok((state, (), true))
            })
            .unwrap();
        let principal = HumanManagementPrincipal::authenticated();
        provider
            .execute_project_mutation_for_management(
                &principal,
                &HumanManagementProjectMutationRequest {
                    schema: HumanManagementProjectMutationSchema::V1,
                    action_id: AgentActionId::new("ws-create").unwrap(),
                    project_id: project("ws-project"),
                    expected_authority_epoch: Some(0),
                    expected_project_revision: Some(0),
                    operation: HumanManagementProjectMutationKind::Create {
                        director_cutex_session_id: session("cutex.ws-director"),
                        presentation: ProjectPresentationInput {
                            display_name: "WS".to_string(),
                            badge_label: "WS".to_string(),
                            color: ProjectPaletteColor::Rgb(1, 2, 3),
                        },
                    },
                },
            )
            .unwrap();
        let workspace_dir = root.join("ws dir");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        let set = |action: &str, revision: u64, value: Option<String>| {
            provider.execute_project_mutation_for_management(
                &principal,
                &HumanManagementProjectMutationRequest {
                    schema: HumanManagementProjectMutationSchema::V1,
                    action_id: AgentActionId::new(action).unwrap(),
                    project_id: project("ws-project"),
                    expected_authority_epoch: Some(1),
                    expected_project_revision: Some(revision),
                    operation: HumanManagementProjectMutationKind::SetWorkspaceRoot {
                        workspace_root: value,
                    },
                },
            )
        };
        assert_eq!(
            set("ws-relative", 1, Some("relative/dir".to_string())),
            Err(AgentManagementError::InvalidRequest("invalid_workspace_root"))
        );
        assert_eq!(
            set(
                "ws-missing",
                1,
                Some(root.join("missing").to_string_lossy().into_owned())
            ),
            Err(AgentManagementError::InvalidRequest("workspace_root_not_found"))
        );
        assert_eq!(
            set(
                "ws-stale",
                0,
                Some(workspace_dir.to_string_lossy().into_owned())
            ),
            Err(AgentManagementError::Conflict("project_revision_conflict"))
        );
        let receipt = set(
            "ws-set",
            1,
            Some(workspace_dir.to_string_lossy().into_owned()),
        )
        .unwrap();
        assert_eq!(receipt.project_revision, 2);
        assert_eq!(receipt.audit_event.kind, ProjectAuditKind::WorkspaceRootUpdated);
        let stored = receipt
            .project_state
            .as_ref()
            .and_then(|state| state.workspace_root.clone())
            .unwrap();
        assert!(!stored.starts_with(r"\\?\"), "{stored}");
        assert_eq!(
            std::path::Path::new(&stored).canonicalize().unwrap(),
            workspace_dir.canonicalize().unwrap()
        );
        let summaries = provider.list_cutex_projects_for_management(&principal).unwrap();
        assert_eq!(summaries.projects[0].workspace_root.as_deref(), Some(stored.as_str()));
        let json = serde_json::to_value(&summaries.projects[0]).unwrap();
        assert_eq!(json["workspace_root"], stored.as_str());
        let cleared = set("ws-clear", 2, None).unwrap();
        assert_eq!(
            cleared.project_state.and_then(|state| state.workspace_root),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
