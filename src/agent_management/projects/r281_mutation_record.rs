//! Frozen r281 (7f9b6a2b) mutation-record wire types for rollback decoding.
//! The presentation input, mutation schema/kind, receipt and record below are
//! copied from that revision; dependent identity/state types are unchanged.
#![allow(dead_code)]
use crate::agent_management::{
    AgentActionId, CurrentProjectMembership, ProjectAuditEvent, ProjectId, ProjectPaletteColor,
    ProjectStateRecord, ProjectTombstone,
};
use crate::role_revision::{CutexSessionId, Rfc3339, Sha256};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPresentationInput {
    pub display_name: String,
    pub badge_label: String,
    pub color: ProjectPaletteColor,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HumanManagementProjectMutationSchema {
    #[serde(rename = "cutex/human-management-project-mutation/v1")]
    V1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HumanManagementProjectMutationKind {
    Create {
        director_cutex_session_id: CutexSessionId,
        presentation: ProjectPresentationInput,
    },
    /// r219: legacy, decode-only; never produced. Recorded receipts still
    /// load; a request with it is refused with `tool_removed`.
    RepairDirectorSeat {
        expected_legacy_occupant: CutexSessionId,
        expected_legacy_epoch: u64,
    },
    AddMember {
        cutex_session_id: CutexSessionId,
    },
    DetachMember {
        cutex_session_id: CutexSessionId,
    },
    Archive,
    Restore,
    Remove,
    /// Set (absolute path; canonicalized and required to be an existing
    /// directory) or clear (`null`) the project's workspace root.
    SetWorkspaceRoot {
        workspace_root: Option<String>,
    },
    /// Create a Project whose Director is optional (`project-optional-director`),
    /// optionally with its workspace root.
    CreateProject {
        presentation: ProjectPresentationInput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_root: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        director_cutex_session_id: Option<CutexSessionId>,
        /// `project-create-workspace-root`: a missing `workspace_root` is
        /// created (the missing tail, as the daemon user) instead of refused
        /// with `workspace_root_not_found`. Omitted when false, so requests
        /// without it keep their idempotency digest.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        create_workspace_root: bool,
    },
    /// Seat a Director in a Project that has none. An independent Agent joins
    /// the Project; a current member keeps its membership.
    AssignDirector {
        cutex_session_id: CutexSessionId,
    },
    /// Vacate the Director seat. The former Director stays a member.
    ClearDirector,
    /// Replace the current Director with `successor_cutex_session_id` in one
    /// commit (`owner-rotation`; `expected_authority_epoch` is required). The
    /// former Director stays a member; Operator grants are kept.
    RotateDirector {
        successor_cutex_session_id: CutexSessionId,
    },
}

impl HumanManagementProjectMutationKind {
    /// The `project-optional-director` operations (projects/director.rs).
    pub fn is_optional_director_operation(&self) -> bool {
        matches!(
            self,
            Self::CreateProject { .. }
                | Self::AssignDirector { .. }
                | Self::ClearDirector
                | Self::RotateDirector { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanManagementProjectMutationReceipt {
    pub schema: HumanManagementProjectMutationSchema,
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub project_id: ProjectId,
    pub operation: HumanManagementProjectMutationKind,
    pub previous_project_revision: u64,
    pub project_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_state: Option<ProjectStateRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub membership: Option<CurrentProjectMembership>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<ProjectTombstone>,
    pub audit_event: ProjectAuditEvent,
    pub committed_at: Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanManagementProjectMutationActionRecord {
    pub request_sha256: Sha256,
    pub receipt: HumanManagementProjectMutationReceipt,
}
