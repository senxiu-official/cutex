//! Internal automatic managed-online authority. This is never an agent tool
//! request: the root-authenticated scheduler route supplies a durable wake
//! reservation, and the provider rechecks it under its existing launch fence.
use super::*;
use crate::asks::{model::ExecutionState, store::AskState};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticManagedOnline {
    pub ask_id: String,
    pub target: String,
    pub controller: String,
    pub reserved_at: String,
}

impl AutomaticManagedOnline {
    /// These execution-time refusals leave the delegation waiting. They must
    /// not be turned into a claim that only the owner can repair the runtime.
    pub fn admission_hold(detail: &str) -> bool {
        ["ownerStopped:","ownerPaused:","agentPaused:","autoWakeBudget:","automatic_wake_stale:"]
            .iter().any(|reason|detail.contains(reason))
    }
    /// Pure validation against the protected current authority and AskStore.
    pub fn validate(
        &self, state: &AskState, authority: &AgentManagementSnapshot, budget: usize,
    ) -> anyhow::Result<()> {
        let ask = state.asks.get(&self.ask_id)
            .ok_or_else(|| anyhow::anyhow!("automatic_wake_stale: delegation is absent"))?;
        anyhow::ensure!(ask.remote.is_none()
            && !matches!(ask.requester, crate::asks::model::Requester::Peer { .. })
            && ask.target_cutex_session_id == self.target
            && (ask.execution == ExecutionState::Queued || ask.resume_pending())
            && ask.controller().ok().flatten().as_deref() == Some(self.controller.as_str()),
            "automatic_wake_stale: delegation or controller changed");
        anyhow::ensure!(crate::asks::wake::pending_wake(state,&self.target) == Some(self.reserved_at.as_str()),
            "automatic_wake_stale: reservation changed");
        anyhow::ensure!(!state.owner_offline.contains(&self.target), "ownerStopped: explicit online is required");
        anyhow::ensure!(!state.pauses.contains_key(&self.target)
            && !crate::asks::agent_pause::is_paused(state, &self.target)
            && !crate::asks::pause_admission::is_paused_in(state, authority, &self.target),
            "ownerPaused: automatic online is paused");
        anyhow::ensure!(can_online_managed(authority, &self.controller, &self.target),
            "ownerStartRequired: requester has no current managed-online authority");
        let domain = state.woken.get(&self.target)
            .filter(|(_, at)| at == &self.reserved_at)
            .map(|(domain, _)| domain)
            .ok_or_else(|| anyhow::anyhow!("automatic_wake_stale: wake domain changed"))?;
        let agent = authority.agent_by_session(&self.target).expect("managed authority checked above");
        let project = current_project_id(authority,agent).expect("managed authority checked above");
        anyhow::ensure!(domain == &format!("project:{}",project.as_str()),
            "automatic_wake_stale: target's budget domain changed");
        anyhow::ensure!(crate::asks::wake::runtime_budget_used(state, domain, &|id|
            state.presence.get(id).is_some_and(|p| p.online)) <= budget,
            "autoWakeBudget: automatic online budget is exhausted");
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::role_revision::CutexSessionId;
    use crate::asks::{store::tests::{new_ask, REQUESTER, TARGET}, wake};
    use serde_json::json;

    fn fixture() -> (AskState, AgentManagementSnapshot, AutomaticManagedOnline) {
        let now = chrono::Utc::now();
        let mut state = AskState::default();
        let ask_id = state.create_ask(new_ask("managed-wake", "work"), now).unwrap().0.ask_id;
        wake::record_wake(&mut state, TARGET, "project:alpha", now);
        let context = AutomaticManagedOnline {
            ask_id, target: TARGET.into(), controller: REQUESTER.into(),
            reserved_at: crate::asks::now_rfc3339(now),
        };
        let mut authority = AgentManagementSnapshot::empty();
        let project = ProjectId::new("alpha").unwrap();
        authority.projects.insert(project.clone(), serde_json::from_value(json!({
            "project_id":"alpha", "authorized_director_session":REQUESTER,
            "authority_epoch":1, "updated_at":"2026-11-05T00:00:00Z"
        })).unwrap());
        let agent: ManagedAgentRecord = serde_json::from_value(json!({
            "project_id":"alpha", "created_by_director_session":REQUESTER,
            "cutex_session_id":TARGET, "native_session_id":"synthetic-native",
            "created_at":"2026-11-05T00:00:00Z", "spec": {
                "name":"worker", "cwd":"/synthetic", "profile":null,
                "runtime_backend":"cute_codex", "model":"synthetic", "reasoning":"medium",
                "permissions":"danger-full-access", "approval_policy":"never",
                "sandbox_mode":"danger-full-access", "groups":[], "expose_to_im":false, "pin":false
            }
        })).unwrap();
        authority.agents.insert(agent.cutex_session_id.clone(), agent);
        (state, authority, context)
    }

    #[test]
    fn automatic_managed_authority_does_not_need_a_prior_runtime_review() {
        let (state, authority, context) = fixture();
        assert!(context.validate(&state, &authority, 1).is_ok());
        assert!(authority.actions.is_empty());
    }

    #[test]
    fn automatic_managed_rechecks_owner_stop_and_pause_after_reservation() {
        assert!(AutomaticManagedOnline::admission_hold("start failed: ownerPaused: automatic online is paused"));
        assert!(!AutomaticManagedOnline::admission_hold("ownerStartRequired: no current authority"));
        assert!(!AutomaticManagedOnline::admission_hold("invalid_model: unlisted model"));
        let (mut state, authority, context) = fixture();
        state.owner_offline.insert(TARGET.into());
        assert!(context.validate(&state, &authority, 1).unwrap_err().to_string().starts_with("ownerStopped"));
        state.owner_offline.clear();
        crate::asks::agent_pause::pause(&mut state,TARGET,"owner",None,chrono::Utc::now());
        assert!(context.validate(&state, &authority, 1).unwrap_err().to_string().starts_with("ownerPaused"));
    }

    #[test]
    fn automatic_managed_rechecks_current_authority_and_archive() {
        let (state, mut authority, context) = fixture();
        let project = ProjectId::new("alpha").unwrap();
        authority.projects.get_mut(&project).unwrap().authorized_director_session = CutexSessionId::new("cutex.other").unwrap();
        assert!(context.validate(&state, &authority, 1).is_err());
        authority.projects.get_mut(&project).unwrap().authorized_director_session = CutexSessionId::new(REQUESTER).unwrap();
        authority.agents.get_mut(&CutexSessionId::new(TARGET).unwrap()).unwrap().retired_at =
            Some(crate::role_revision::Rfc3339::new("2026-11-05T00:00:00Z").unwrap());
        assert!(context.validate(&state, &authority, 1).is_err());
    }

    #[test]
    fn automatic_managed_rejects_wrong_controller_target_and_stale_reservation() {
        let (state, authority, context) = fixture();
        let mut wrong = context.clone(); wrong.controller = "cutex.other".into();
        assert!(wrong.validate(&state, &authority, 1).is_err());
        wrong = context.clone(); wrong.target = "cutex.other".into();
        assert!(wrong.validate(&state, &authority, 1).is_err());
        wrong = context; wrong.reserved_at = "old".into();
        assert!(wrong.validate(&state, &authority, 1).is_err());
    }

    #[test]
    fn automatic_managed_operator_has_exactly_typed_online_authority() {
        let (state, mut authority, mut context) = fixture();
        let id = CutexSessionId::new("cutex.operator").unwrap();
        let mut operator = authority.agents[&CutexSessionId::new(TARGET).unwrap()].clone();
        operator.cutex_session_id = id.clone();
        authority.agents.insert(id.clone(), operator);
        context.controller = id.as_str().to_string();
        assert!(!can_online_managed(&authority, &context.controller, TARGET));
        authority.operator_grants.entry(ProjectId::new("alpha").unwrap()).or_default().insert(id,
            serde_json::from_value(json!({"project_id":"alpha", "operator_cutex_session_id":context.controller,
                "grant_revision":1,"granted_at":"2026-11-05T00:00:00Z",
                "granted_by_primary_director_session":REQUESTER,"performed_by_human_management":false})).unwrap());
        assert!(can_online_managed(&authority, &context.controller, TARGET));
        assert!(!can_online_managed(&authority, &context.controller, REQUESTER));
        // Controller mismatch remains a refusal even with Online authority.
        assert!(context.validate(&state, &authority, 1).is_err());
    }

    #[test]
    fn automatic_managed_budget_counts_pending_starts_and_fences_old_completions() {
        let (mut state, authority, context) = fixture();
        wake::record_wake(&mut state,"cutex.other","project:alpha",chrono::Utc::now());
        assert!(context.validate(&state, &authority, 1).unwrap_err().to_string().starts_with("autoWakeBudget"));
        assert!(context.validate(&state, &authority, 2).is_ok());
        wake::finish_wake(&mut state,TARGET,"old-reservation");
        assert!(wake::pending_wake(&state,TARGET).is_some());
        wake::finish_wake(&mut state,TARGET,&context.reserved_at);
        assert!(wake::pending_wake(&state,TARGET).is_none());
        assert!(context.validate(&state,&authority,2).is_err());
    }
}
