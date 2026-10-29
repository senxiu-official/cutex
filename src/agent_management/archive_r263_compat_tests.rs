// r263 be514f28 definitions extracted verbatim (store self:: replaces super:: only).
// These are actual baseline deserialize definitions; unrelated empty map values use current unchanged types.
#![allow(dead_code,unused_imports)]
use crate::agent_management::*;
use crate::role_revision::{CutexSessionId,Sha256,Rfc3339};
use crate::session::model::CutexSessionRecord;
use crate::management::control_plane::{HumanManagementOperatorActionRecord,HumanManagementProjectMutationActionRecord};
use serde::{Deserialize,Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentArchiveOperation {
    Archive,
    Restore,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct AgentArchiveReview {
    pub cutex_session_id: CutexSessionId,
    pub formal_name: String,
    pub operation: AgentArchiveOperation,
    #[serde(default)]
    pub current_project_id: Option<ProjectId>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub runtime_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentArchiveRequest {
    #[serde(default)]
    pub reason: Option<String>,
    pub action_id: AgentActionId,
    pub review: AgentArchiveReview,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentArchiveStage {
    Prepared,
    Stopped,
    Committed,
    StoppedNotArchived,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentArchiveReceipt {
    pub request: AgentArchiveRequest,
    pub request_sha256: Sha256,
    pub stage: AgentArchiveStage,
    pub result: Option<CutexSessionRecord>,
    pub error: Option<String>,
    pub committed_at: crate::role_revision::Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentManagementSnapshot {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bootstrap_intents: BTreeMap<AgentActionId, self::BootstrapIntentReview>,
    /// Read-only durable projection, never persisted into roster history.
    #[serde(skip)]
    pub reversible_archive_projection: BTreeMap<crate::role_revision::CutexSessionId, bool>,
    #[serde(default)]
    pub agent_archive_actions: BTreeMap<AgentActionId, self::AgentArchiveReceipt>,
    #[serde(default)]
    pub agent_archive_audit: BTreeMap<String, self::AgentArchiveReceipt>,
    pub schema: AgentManagementStoreSchema,
    pub store_revision: u64,
    #[serde(default)]
    pub durable_import_actions: BTreeMap<AgentActionId, self::DurableImportReceipt>,
    #[serde(default)]
    pub durable_import_audit: BTreeMap<String, self::DurableImportAuditEvent>,
    pub projects: BTreeMap<ProjectId, ProjectAuthority>,
    /// Exact project/session keyed Agent Operator grants. Missing on legacy v1
    /// stores and therefore migrated as an empty grant set.
    #[serde(default)]
    pub operator_grants:
        BTreeMap<ProjectId, BTreeMap<crate::role_revision::CutexSessionId, AgentOperatorGrant>>,
    /// Per-project CAS revision for the complete Operator grant set.
    #[serde(default)]
    pub operator_grant_revisions: BTreeMap<ProjectId, u64>,
    #[serde(default)]
    pub operator_audit_events: BTreeMap<String, AgentOperatorAuditEvent>,
    /// Idempotency receipts for the separate Human/Management action domain.
    /// These records never impersonate an Agent Management action caller.
    #[serde(default)]
    pub human_management_operator_actions:
        BTreeMap<AgentActionId, HumanManagementOperatorActionRecord>,
    /// Idempotent Human/Management Project structure mutations.
    #[serde(default)]
    pub human_management_project_mutations:
        BTreeMap<AgentActionId, HumanManagementProjectMutationActionRecord>,
    /// Additive current membership overrides. Missing entries retain the
    /// historical ManagedAgentRecord.project_id interpretation for active
    /// legacy Agents; writes never rewrite that historical provenance.
    #[serde(default)]
    pub current_project_memberships:
        BTreeMap<crate::role_revision::CutexSessionId, CurrentProjectMembership>,
    /// Lifecycle/CAS records for Project structure. Missing legacy records are
    /// projected as active at revision zero without forcing a migration write.
    #[serde(default)]
    pub project_states: BTreeMap<ProjectId, ProjectStateRecord>,
    #[serde(default)]
    pub project_tombstones: BTreeMap<ProjectId, ProjectTombstone>,
    #[serde(default)]
    pub project_audit_events: BTreeMap<String, ProjectAuditEvent>,
    /// Non-authoritative UI metadata. Its map key is always the canonical
    /// provider-owned project identity; none of these values participate in
    /// authorization.
    #[serde(default)]
    pub project_presentations: BTreeMap<ProjectId, self::ProjectPresentationSettings>,
    pub agents: BTreeMap<crate::role_revision::CutexSessionId, ManagedAgentRecord>,
    pub actions: BTreeMap<AgentActionId, AgentActionRecord>,
    #[serde(default)]
    pub phase_events: BTreeMap<String, AgentManagementPhaseEvent>,
    pub authority_receipts: BTreeMap<AgentActionId, ProjectAuthorityReceipt>,
    #[serde(default)]
    pub legacy_director_ownership_import_receipts:
        BTreeMap<AgentActionId, LegacyDirectorOwnershipImportReceipt>,
    #[serde(default)]
    pub reservation_reconciliation_receipts:
        BTreeMap<AgentActionId, AgentReservationReconciliationReceipt>,
    #[serde(default)]
    pub reservation_reconciliation_events: BTreeMap<String, AgentReservationReconciliationEvent>,
    pub failure_events: BTreeMap<String, AgentManagementFailureEvent>,
    /// Sparse `agent_create` / `member_replace` calls and the full request
    /// each resolved to (`sparse.rs`), keyed by action id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sparse_requests: BTreeMap<AgentActionId, self::SparseRequestRecord>,
    /// Preserve additive provider fields across presentation-only writes so a
    /// newer store is not silently downgraded by this reader.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}
use crate::session::model::{NativeStopReceipt,FormalAgentNameReceipt,StoreRevisionCell};
use std::collections::HashMap;
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CutexSessionStore {
    /// Latest proven stopped native occurrence, saved with runtime clearing.
    #[serde(default)]
    pub native_stop_receipts: std::collections::BTreeMap<String, NativeStopReceipt>,
    #[serde(default)]
    pub explicit_launch_receipts:
        std::collections::BTreeMap<String, crate::agent_management::ExplicitLaunchActionReceipt>,
    #[serde(default)]
    pub human_adoption_receipts:
        std::collections::BTreeMap<String, crate::agent_management::HumanAdoptReceipt>,
    /// Archive transition receipt committed atomically with its durable state.
    #[serde(default)]
    pub agent_archive_receipts:
        std::collections::BTreeMap<String, AgentArchiveReceipt>,
    /// Exact receipts for Human import naming; committed atomically with the name.
    #[serde(default)]
    pub formal_name_receipts: std::collections::BTreeMap<String, FormalAgentNameReceipt>,
    /// Explicit-launch actions removed by receipt retention
    /// (`crate::session::receipt_retention`): a replay or status query of one
    /// of these keys fails with `explicit_launch_action_pruned` instead of
    /// starting a new action. Older cutex ignores this map (and drops it on
    /// its next save).
    #[serde(
        default,
        rename = "explicitLaunchTombstones",
        skip_serializing_if = "std::collections::BTreeMap::is_empty"
    )]
    pub explicit_launch_tombstones: std::collections::BTreeMap<
        String,
        crate::session::receipt_retention::ExplicitLaunchTombstone,
    >,
    /// Internal compare-and-swap generation for cross-process writers.
    #[serde(default, rename = "storeRevision")]
    pub store_revision: StoreRevisionCell,
    #[serde(default)]
    pub sessions: HashMap<String, CutexSessionRecord>,
}

#[test]
fn r264_r263_baseline_store_parser_rollback_boundary() {
 let empty=serde_json::to_value(crate::agent_management::AgentManagementStore::open(std::env::temp_dir().join(format!("r263-parser-{}",uuid::Uuid::new_v4()))).unwrap().snapshot().unwrap()).unwrap();
 let receipt=serde_json::json!({"request":{"reason":null,"action_id":"baseline-fixture","review":{"cutex_session_id":"cutex.fixture","formal_name":"Fixture","operation":"archive","current_project_id":null,"revision":1,"runtime_generation":0}},"request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stage":"committed","result":null,"error":null,"committed_at":"2026-10-26T00:00:00Z"});
 let mut session=serde_json::to_value(crate::session::model::CutexSessionStore::default()).unwrap();
 session["agent_archive_receipts"]["baseline-fixture"]=receipt.clone();
 serde_json::from_value::<CutexSessionStore>(session.clone()).unwrap();
 session["agent_archive_receipts"]["baseline-fixture"]["restorable"]=serde_json::json!(true);
 let error=serde_json::from_value::<CutexSessionStore>(session).unwrap_err().to_string();
 assert!(error.contains("unknown field"));
 println!("r263 be514f28: session embedded receipt => WHOLE SESSION STORE REJECTED: {error}");
 let mut compatible=empty.clone();compatible["permanent_retirement_confirmations_v1"]=serde_json::json!({"hash":{"target":"cutex.fixture"}});
 let parsed:AgentManagementSnapshot=serde_json::from_value(compatible.clone()).unwrap();
 assert_eq!(serde_json::to_value(parsed).unwrap()["permanent_retirement_confirmations_v1"],compatible["permanent_retirement_confirmations_v1"]);
 println!("r263 be514f28: confirmation extension survives old reader/writer: PASS");
 for bucket in ["agent_archive_actions","agent_archive_audit"] {
  let mut old=empty.clone();old[bucket]["baseline-fixture"]=receipt.clone();
  serde_json::from_value::<AgentManagementSnapshot>(old.clone()).unwrap();
  for (label,field,value) in [("soft archive additive receipt fields","archiveKind","soft"),("permanent operation","operation","permanent_retire"),("permanent stage","stage","permanent_retirement_committed")] {
   let mut new=old.clone();
   if field=="operation" {new[bucket]["baseline-fixture"]["request"]["review"][field]=serde_json::json!(value);}
   else {new[bucket]["baseline-fixture"][field]=serde_json::json!(value);}
   let error=serde_json::from_value::<AgentManagementSnapshot>(new).unwrap_err().to_string();
   println!("r263 be514f28: {bucket} {label} => WHOLE STORE REJECTED: {error}");
   assert!(error.contains("unknown field")||error.contains("unknown variant"));
  }
 }
}
