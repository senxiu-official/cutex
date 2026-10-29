//! Frozen ownership and authority for one request. For mutations, load fresh
//! under the authority fence before AskStore, and retain the fence through commit.
use super::{AskError, AskRecord, OwnershipState, Store};
use crate::asks::model::{DeliveryKind, DeliveryRecord, DeliveryState};
use crate::agent_management::AgentManagementSnapshot;
use std::sync::Arc;

pub(crate) struct ReadView<'a> {
    ownership: Arc<OwnershipState>,
    authority: &'a AgentManagementSnapshot,
}

impl<'a> ReadView<'a> {
    pub(crate) fn load(authority: &'a AgentManagementSnapshot) -> Result<Self, AskError> {
        Ok(Self { ownership: Store::open_default()?.snapshot()?, authority })
    }

    fn director(&self, project: &str) -> Result<String, AskError> {
        super::project_director_state(self.authority, project)?.0
            .ok_or_else(|| AskError::new("director_not_found", "Project has no active Director"))
    }

    /// Effective current controller, including the immutable requester when
    /// there is no role ownership. A failed read never falls back to requester.
    pub(crate) fn controller(&self, ask: &AskRecord) -> Result<Option<String>, AskError> {
        Ok(super::controller_in(ask, &self.ownership, |project| self.director(project))?
            .or_else(|| ask.requester.session().map(str::to_owned)))
    }

    /// Same requester/history visibility as AskRecord::readable_by, using only
    /// this frozen ownership+roster snapshot. History requires valid immutable
    /// provenance and grants reading only, never current control.
    pub(crate) fn readable_by(&self, ask: &AskRecord, caller: &str) -> bool {
        ask.requester.session() == Some(caller)
            || matches!(self.controller(ask),Ok(Some(ref current)) if current==caller)
            || self.ownership.asks.get(&ask.ask_id).is_some_and(|ownership|
                super::verify_digest(ask,ownership).is_ok() && ownership.history.iter().any(|id|id==caller))
    }

    pub(crate) fn question_recipient(&self, ask: &AskRecord) -> Result<Option<String>, AskError> {
        if let Some(project) = self.ownership.question_projects.get(&ask.ask_id) {
            return self.director(project).map(Some);
        }
        self.controller(ask)
    }

    pub(crate) fn pending_recipient(&self, ask: &AskRecord, delivery: &DeliveryRecord) -> Option<String> {
        debug_assert_eq!(delivery.state, DeliveryState::Pending);
        if delivery.kind == DeliveryKind::Question {
            return self.question_recipient(ask).ok().flatten();
        }
        super::controller_in(ask, &self.ownership, |project| self.director(project)).ok()
            .map(|controller| controller.unwrap_or_else(|| delivery.requester_cutex_session_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::{store::{tests::{new_ask, REQUESTER}, AskState}, model::CloseOutcome};
    use serde_json::json;

    #[test]
    fn read_view_preserves_provenance_holds_and_handover_authority() {
        let mut asks = AskState::default();
        let (mut ask, _) = asks.create_ask(new_ask("read-view", "brief"), chrono::Utc::now()).unwrap();
        let root = std::env::temp_dir().join(format!("ownership-read-{}", uuid::Uuid::new_v4()));
        let store = Store::at(&root);
        store.stamp_at(&ask, REQUESTER, "alpha", 1).unwrap();
        let mut authority = AgentManagementSnapshot::empty();
        let project = crate::agent_management::ProjectId::new("alpha").unwrap();
        authority.projects.insert(project.clone(), serde_json::from_value(json!({
            "project_id":"alpha", "authorized_director_session":REQUESTER,
            "authority_epoch":1, "updated_at":"2026-10-27T00:00:00Z"
        })).unwrap());
        let view = ReadView { ownership: store.snapshot().unwrap(), authority: &authority };
        assert_eq!(view.controller(&ask).unwrap().as_deref(), Some(REQUESTER));
        assert!(view.readable_by(&ask,REQUESTER));
        assert!(!view.readable_by(&ask,"new"));
        let mut same_director_epoch = authority.clone();
        same_director_epoch.projects.get_mut(&project).unwrap().authority_epoch += 1;
        let same_director = ReadView { ownership: view.ownership.clone(), authority: &same_director_epoch };
        // Preserve existing A->B->A semantics: origin_epoch is provenance,
        // not a requirement that an accepted current controller never changes epoch.
        assert_eq!(same_director.controller(&ask).unwrap().as_deref(), Some(REQUESTER));
        // A new current epoch/Director cannot be inferred to own old work.
        let mut changed = authority.clone();
        let current = changed.projects.get_mut(&project).unwrap();
        current.authorized_director_session = crate::role_revision::CutexSessionId::new("new").unwrap();
        current.authority_epoch = 2;
        let after = ReadView { ownership: view.ownership.clone(), authority: &changed };
        assert_eq!(after.controller(&ask).unwrap_err().code, "handover_pending_review");
        assert!(!after.readable_by(&ask,"new"),"current seat alone grants no historical read");
        let mut question_ownership = (*view.ownership).clone();
        question_ownership.question_projects.insert(ask.ask_id.clone(), "alpha".into());
        let question_view = ReadView { ownership: Arc::new(question_ownership), authority: &changed };
        let question_id = crate::asks::delivery::record_question_delivery(&mut asks, &ask, "q", chrono::Utc::now()).unwrap();
        let answer_id = crate::asks::delivery::record_answer_delivery(&mut asks, &ask, "a", chrono::Utc::now()).unwrap();
        let question = &asks.deliveries[&question_id];
        let answer = &asks.deliveries[&answer_id];
        // Director questions may move before the old controller accepts handover;
        // answers remain held. Never route a held answer back to the origin.
        assert_eq!(question_view.question_recipient(&ask).unwrap().as_deref(), Some("new"));
        assert_eq!(question_view.pending_recipient(&ask, question).as_deref(), Some("new"));
        assert_eq!(question_view.pending_recipient(&ask, answer), None);
        let unavailable = AgentManagementSnapshot::empty();
        let no_director = ReadView { ownership: question_view.ownership.clone(), authority: &unavailable };
        assert!(no_director.question_recipient(&ask).is_err());
        assert_eq!(no_director.pending_recipient(&ask, question), None);
        let mut accepted_ownership = (*question_view.ownership).clone();
        let accepted = accepted_ownership.asks.get_mut(&ask.ask_id).unwrap();
        accepted.controller = "new".into(); accepted.history.push("new".into());
        let accepted_view = ReadView { ownership: Arc::new(accepted_ownership), authority: &changed };
        assert_eq!(accepted_view.pending_recipient(&ask, answer).as_deref(), Some("new"));
        assert!(accepted_view.readable_by(&ask,"new"));
        assert!(accepted_view.readable_by(&ask,REQUESTER));
        // The first read keeps its original authority, without refreshing mid-list.
        assert_eq!(view.controller(&ask).unwrap().as_deref(), Some(REQUESTER));
        for (hold, pending, code) in [(true, false, "handover_owner_review"), (false, true, "handover_pending_reconcile")] {
            let mut ownership = (*view.ownership).clone();
            let item = ownership.asks.get_mut(&ask.ask_id).unwrap();
            item.owner_hold = hold; item.pending_reconcile = pending;
            let blocked = ReadView { ownership: Arc::new(ownership), authority: &authority };
            assert_eq!(blocked.controller(&ask).unwrap_err().code, code);
        }
        ask.request_sha256.push('x');
        assert_eq!(view.controller(&ask).unwrap_err().code, "director_provenance_invalid");
        assert!(!accepted_view.readable_by(&ask,"new"),"invalid provenance cannot authorize history");
        // Existing closed/historical read APIs retain their distinct semantics.
        ask.request_sha256.pop(); ask.closed = Some(CloseOutcome::Accepted);
        assert_eq!(store.closed_controller(&ask).unwrap().as_deref(), Some(REQUESTER));
        std::fs::remove_dir_all(root).unwrap();
    }
}
