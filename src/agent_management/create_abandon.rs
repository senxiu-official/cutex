//! Owner abandon of an unfinished agent_create (docs/design/028 §c, owner
//! 10-22): releases the name reservation, ends the action as NoWrite
//! `create_abandoned` (terminal) and cleans up only what that action created
//! (its durable session if one exists, else its native thread), through a
//! caller-supplied cleanup (the binary owns session delete / native history).
//!
//! - Refused: an action that is not a create, that completed, that already
//!   ended otherwise (its name is already free), or while an Agent
//!   Management action is running (`action_in_progress`, retry).
//! - Idempotent: a second abandon of the same action replays its receipt.
//! - Dry run: the plan only, nothing changes.
//! - Audit: the NoWrite response and a phase event in the store, plus a
//!   receipt file `abandons/<action>.json` next to the store (no new store
//!   map, so older binaries still read the store).
use super::*;
use serde::{Deserialize, Serialize};

pub const CREATE_ABANDONED: &str = "create_abandoned";

/// `POST /v2/agent-management/create-abandon` body.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateAbandonRequest {
    pub action_id: String,
    #[serde(default)]
    pub dry_run: bool,
}

/// What an abandon removes (dry run: would remove).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateAbandonPlan {
    pub action_id: String,
    pub project_id: String,
    pub phase: String,
    /// The reserved name that becomes free.
    pub reserved_name: Option<String>,
    pub reserved_cwd: Option<String>,
    /// The durable cutex session the create made, when it still exists.
    pub durable_session: Option<String>,
    /// The session id the action recorded, also when the record is gone.
    pub successor_session: Option<String>,
    /// The native thread the create made (its history is moved aside).
    pub native_thread: Option<String>,
    /// The agent-management roster row of that session, if present.
    pub roster_entry: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateAbandonReceipt {
    pub schema: String,
    pub plan: CreateAbandonPlan,
    pub dry_run: bool,
    /// A repeated abandon of the same action: the stored receipt.
    pub replayed: bool,
    /// What the cleanup did, one line each (recovery copies included).
    pub removed: Vec<String>,
    pub abandoned_at: Option<String>,
}

/// The binary's effects: delete the durable session (with its recovery
/// copy), or move a session-less native thread's history aside.
pub trait CreateAbandonCleanup {
    /// Whether the durable session exists (and is this host's).
    fn durable_session_exists(&self, cutex_session_id: &str) -> bool;
    fn cleanup(&self, plan: &CreateAbandonPlan) -> Result<Vec<String>, String>;
}

fn phase_name(phase: AgentActionPhase) -> String {
    serde_json::to_value(phase).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

impl AgentManagementProvider {
    fn abandon_receipt_path(&self, action_id: &AgentActionId) -> std::path::PathBuf {
        self.store().root().join("abandons").join(format!("{}.json", action_id.as_str()))
    }

    /// See the module docs. `Ok` is either the plan (dry run), the receipt of
    /// this abandon, or the replayed receipt of an earlier one.
    pub fn abandon_create(
        &self,
        action_id: &str,
        dry_run: bool,
        cleanup: &dyn CreateAbandonCleanup,
    ) -> Result<CreateAbandonReceipt, AgentManagementError> {
        let action_id = AgentActionId::new(action_id.to_string())?;
        let snapshot = self.store().snapshot()?;
        let action = snapshot.actions.get(&action_id).cloned().ok_or(AgentManagementError::NotFound("action_not_found"))?;
        if action.phase == AgentActionPhase::NoWrite {
            let abandoned = matches!(&action.response, Some(AgentManagementResponse { outcome: AgentManagementOutcome::NoWrite { code, .. }, .. }) if code == CREATE_ABANDONED);
            if abandoned {
                let bytes = std::fs::read(self.abandon_receipt_path(&action_id)).map_err(|_| AgentManagementError::InvalidStore)?;
                let mut receipt: CreateAbandonReceipt = serde_json::from_slice(&bytes).map_err(|_| AgentManagementError::InvalidStore)?;
                receipt.replayed = true;
                return Ok(receipt);
            }
            return Err(AgentManagementError::ConflictDetail(
                "action_already_finished: this action already ended without creating an agent; its name is free".into(),
            ));
        }
        if action.operation != AgentOperationKind::Create {
            return Err(AgentManagementError::ConflictDetail("not_a_create: only an unfinished agent_create can be abandoned".into()));
        }
        if action.phase == AgentActionPhase::Complete {
            return Err(AgentManagementError::ConflictDetail(
                "action_completed: this create finished; archive or delete the agent instead".into(),
            ));
        }
        // An action running right now holds the mutation lock for its whole run.
        match self.store().try_lock_delivery_mutations()? {
            Some(lock) => drop(lock),
            None => {
                return Err(AgentManagementError::ConflictDetail(
                    "action_in_progress: an Agent Management action is running right now; retry in a moment".into(),
                ))
            }
        }
        let successor = action.known_successor_cutex_session.as_ref().map(|s| s.as_str().to_owned());
        let plan = CreateAbandonPlan {
            action_id: action_id.as_str().to_owned(),
            project_id: action.project_id.as_str().to_owned(),
            phase: phase_name(action.phase),
            reserved_name: action.reserved_agent_name.clone(),
            reserved_cwd: action.reserved_agent_cwd.clone(),
            durable_session: successor.clone().filter(|id| cleanup.durable_session_exists(id)),
            native_thread: action.known_native_session_id.clone(),
            roster_entry: action.known_successor_cutex_session.as_ref().is_some_and(|id| snapshot.agents.contains_key(id)),
            successor_session: successor,
        };
        if dry_run {
            return Ok(CreateAbandonReceipt { schema: "cutex/agent-management/create-abandon/v1".into(), plan, dry_run: true, replayed: false, removed: Vec::new(), abandoned_at: None });
        }
        let removed = cleanup
            .cleanup(&plan)
            .map_err(|error| AgentManagementError::External(format!("cleanup failed (the action is unchanged; retry): {error}")))?;
        let receipt = CreateAbandonReceipt {
            schema: "cutex/agent-management/create-abandon/v1".into(),
            plan: plan.clone(),
            dry_run: false,
            replayed: false,
            removed,
            abandoned_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        // Audit first: a receipt without the terminal phase is replaced on retry.
        crate::config::atomic::write_private_pretty_json_atomic(&self.abandon_receipt_path(&action_id), &receipt, "create abandon receipt")
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _execution = super::provider::provider_execution_lock().lock().map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        let event = self.store().with_state(true, |mut state| {
            let target = state.actions.get_mut(&action_id).ok_or(AgentManagementError::NotFound("action_not_found"))?;
            if target.phase_sequence != action.phase_sequence || target.phase != action.phase {
                return Err(AgentManagementError::ConflictDetail(
                    "action_changed: the action moved on while it was being abandoned; inspect it and retry".into(),
                ));
            }
            let detail = format!("abandoned by the owner; released name {}", plan.reserved_name.as_deref().unwrap_or("-"));
            target.phase_sequence = target
                .phase_sequence
                .checked_add(1)
                .filter(|value| *value <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                .ok_or(AgentManagementError::Conflict("phase_sequence_overflow"))?;
            target.phase = AgentActionPhase::NoWrite;
            target.response = Some(AgentManagementResponse {
                schema: AgentManagementSchema::V1,
                action_id: action_id.clone(),
                outcome: AgentManagementOutcome::NoWrite { code: CREATE_ABANDONED.into(), detail },
            });
            target.native_bootstrap_retryable = false;
            target.updated_at = now();
            let target = target.clone();
            if let Some(id) = &target.known_successor_cutex_session {
                state.agents.remove(id);
                state.current_project_memberships.remove(id);
            }
            let event_id = format!("agent-management:{}:phase:{}", target.action_id.as_str(), target.phase_sequence);
            let event = AgentManagementPhaseEvent {
                event_id: event_id.clone(),
                action_id: target.action_id.clone(),
                project_id: target.project_id.clone(),
                operation: target.operation,
                phase: AgentActionPhase::NoWrite,
                phase_sequence: target.phase_sequence,
                committed_at: target.updated_at.clone(),
                presentation_owner_cutex_session_id: target.caller_cutex_session.clone(),
                subject_cutex_session_id: None,
                subject_agent_name: target.reserved_agent_name.clone(),
                predecessor_cutex_session_id: None,
                successor_cutex_session_id: None,
                replace_policy: None,
                rotation_mode: None,
                authority_epoch: None,
            };
            if state.phase_events.insert(event_id, event.clone()).is_some() {
                return Err(AgentManagementError::InvalidStore);
            }
            Ok((state, event, true))
        })?;
        self.notify_phase(&event);
        Ok(receipt)
    }
}

#[cfg(test)]
#[path = "create_abandon_tests.rs"]
mod tests;
