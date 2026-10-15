//! Authenticated local Human/Management control-plane wire contract.
//!
//! The principal is minted only after the Management HTTP server validates the
//! dedicated root bearer.  It is deliberately absent from every request body:
//! a local client can request an operation, but cannot claim an Agent, Director,
//! seat, or durable session identity.

use serde::{Deserialize, Serialize};

use crate::agent_management::{
    AgentActionId, AgentOperatorAuditEvent, AgentOperatorGrant, AgentOperatorRosterProjection,
    CurrentProjectMembership, CutexProjectSummary, CutexProjectWorkspace,
    ProjectAgentChoice, ProjectAuditEvent, ProjectId,
    ProjectPresentationInput, ProjectStateRecord, ProjectTombstone,
};
use crate::role_revision::{CutexSessionId, Rfc3339, Sha256};

/// Proof that a request crossed the dedicated Human/Management bearer boundary.
///
/// There is intentionally no public constructor and no serde implementation.
/// Agent Bus credentials and request payloads therefore cannot manufacture it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HumanManagementPrincipal {
    _private: (),
}

impl HumanManagementPrincipal {
    pub(crate) fn authenticated() -> Self {
        Self { _private: () }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HumanManagementProjectSchema {
    #[serde(rename = "cutex/human-management-projects/v1")]
    V1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanManagementProjectCollection {
    pub schema: HumanManagementProjectSchema,
    pub projects: Vec<CutexProjectSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archived_projects: Vec<CutexProjectSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_agents: Vec<ProjectAgentChoice>,
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
pub struct HumanManagementProjectMutationRequest {
    pub schema: HumanManagementProjectMutationSchema,
    /// Retry key; generated by the server when omitted.
    #[serde(default = "generated_agent_action_id")]
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    /// Optional fences: checked when supplied, last-write-wins when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_project_revision: Option<u64>,
    pub operation: HumanManagementProjectMutationKind,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HumanManagementPresentationSchema {
    #[serde(rename = "cutex/human-management-project-presentation/v1")]
    V1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanManagementPresentationUpdateRequest {
    pub schema: HumanManagementPresentationSchema,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_presentation_revision: Option<u64>,
    pub presentation: ProjectPresentationInput,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HumanManagementOperatorSchema {
    #[serde(rename = "cutex/human-management-operator-action/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanManagementOperatorKind {
    Grant,
    Revoke,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanManagementOperatorActionRequest {
    pub schema: HumanManagementOperatorSchema,
    #[serde(default = "generated_agent_action_id")]
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_grant_revision: Option<u64>,
    pub operation: HumanManagementOperatorKind,
    pub operator_cutex_session_id: CutexSessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanManagementOperatorReceipt {
    pub schema: HumanManagementOperatorSchema,
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub operation: HumanManagementOperatorKind,
    pub project_id: ProjectId,
    pub authority_epoch: u64,
    pub primary_director_cutex_session_id: CutexSessionId,
    pub operator_cutex_session_id: CutexSessionId,
    pub previous_grant_revision: u64,
    pub grant_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<AgentOperatorGrant>,
    pub roster: AgentOperatorRosterProjection,
    pub audit_event: AgentOperatorAuditEvent,
    pub committed_at: Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanManagementOperatorActionRecord {
    pub request_sha256: Sha256,
    pub receipt: HumanManagementOperatorReceipt,
}

/// Complete read projection returned by the authenticated Project detail route.
pub type HumanManagementProjectWorkspace = CutexProjectWorkspace;

fn generated_agent_action_id() -> AgentActionId {
    AgentActionId::new(format!("human-{}", uuid::Uuid::new_v4())).expect("generated action id is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_requests_cannot_claim_an_agent_or_director_identity() {
        fn rejects_identity_claims<T: serde::de::DeserializeOwned>(base: serde_json::Value) {
            assert!(serde_json::from_value::<T>(base.clone()).is_ok(), "{base}");
            for forbidden in [
                "caller_cutex_session",
                "caller_runtime_agent_id",
                "director_cutex_session",
                "seat_id",
                "seat_epoch",
            ] {
                let mut forged = base.clone();
                forged[forbidden] = serde_json::json!("forged");
                assert!(serde_json::from_value::<T>(forged).is_err(), "{forbidden}");
            }
        }
        rejects_identity_claims::<HumanManagementProjectMutationRequest>(serde_json::json!({
            "schema": "cutex/human-management-project-mutation/v1",
            "action_id": "assign-director-1",
            "project_id": "project-alpha",
            "operation": { "kind": "assign_director", "cutex_session_id": "cutex.director" }
        }));
        rejects_identity_claims::<HumanManagementOperatorActionRequest>(serde_json::json!({
            "schema": "cutex/human-management-operator-action/v1",
            "action_id": "grant-operator-1",
            "project_id": "project-alpha",
            "operation": "grant",
            "operator_cutex_session_id": "cutex.operator"
        }));
    }

    #[test]
    fn project_mutation_contract_is_strict_and_round_trips_custom_rgb() {
        let base = serde_json::json!({
            "schema": "cutex/human-management-project-mutation/v1",
            "action_id": "create-project-1",
            "project_id": "project-rgb",
            "expected_authority_epoch": 0,
            "expected_project_revision": 0,
            "operation": {
                "kind": "create",
                "director_cutex_session_id": "cutex.director-rgb",
                "presentation": {
                    "display_name": "RGB Project",
                    "badge_label": "CX",
                    "color": "#12AB34"
                }
            }
        });
        let request = serde_json::from_value::<HumanManagementProjectMutationRequest>(base.clone())
            .expect("strict create request");
        assert_eq!(serde_json::to_value(&request).unwrap(), base);

        for forbidden in ["caller_cutex_session", "seat_id", "authority_epoch"] {
            let mut forged = base.clone();
            forged[forbidden] = serde_json::json!("forged");
            assert!(
                serde_json::from_value::<HumanManagementProjectMutationRequest>(forged).is_err()
            );
        }
        let mut invalid = base;
        invalid["operation"]["presentation"]["color"] = serde_json::json!("#123");
        assert!(serde_json::from_value::<HumanManagementProjectMutationRequest>(invalid).is_err());
    }
}
