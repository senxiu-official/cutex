use super::*;

pub(super) fn key(caller: &str, action: &str, tool: &str) -> String {
    crate::asks::sha256_hex(&serde_json::to_vec(&(caller, action, tool)).expect("string tuple"))
}
pub fn role_target(
    caller: &str,
    action_id: &str,
    tool: &str,
    raw_digest: &str,
) -> Result<Option<RoleResolution>, AskError> {
    Store::open_default()?.role_target_at(caller, action_id, tool, raw_digest)
}
impl Store {
    pub fn role_target_at(
        &self,
        caller: &str,
        action_id: &str,
        tool: &str,
        raw_digest: &str,
    ) -> Result<Option<RoleResolution>, AskError> {
        let state = self.snapshot()?;
        let Some(receipt) = state.role_resolutions.get(&key(caller, action_id, tool)) else {
            return Ok(None);
        };
        if receipt.raw_digest != raw_digest {
            return Err(AskError::new(
                "action_conflict",
                "Role-address action ID already has another request",
            ));
        }
        Ok(Some(receipt.resolution.clone()))
    }
    pub fn pin_role_target(
        &self,
        caller: &str,
        action_id: &str,
        tool: &str,
        raw_digest: &str,
        resolution: RoleResolution,
    ) -> Result<RoleResolution, AskError> {
        self.update(|state| {
            let key = key(caller, action_id, tool);
            if let Some(receipt) = state.role_resolutions.get(&key) {
                if receipt.raw_digest != raw_digest {
                    return Err(AskError::new(
                        "action_conflict",
                        "Role-address action ID already has another request",
                    ));
                }
                return Ok(receipt.resolution.clone());
            }
            state.role_resolutions.insert(
                key,
                RoleResolutionReceipt {
                    raw_digest: raw_digest.into(),
                    resolution: resolution.clone(),
                    business: None,
                },
            );
            Ok(resolution)
        })
    }
}
/// One authority fence covers all target/source checks and local business enqueue.
/// Callbacks may persist local ask/bus queues but must never perform native RPC.
/// In particular, callers must not recursively acquire this mutation lock.
pub fn with_role_action<T>(
    run: impl FnOnce(&crate::agent_management::AgentManagementSnapshot, &Store) -> Result<T, AskError>,
) -> Result<T, AskError> {
    let roster = crate::agent_management::AgentManagementStore::open_default().map_err(store::unavailable)?;
    let _fence = roster.lock_mutations().map_err(store::unavailable)?;
    let snapshot = roster.snapshot().map_err(store::unavailable)?;
    let store = Store::open_default()?;
    // Maintenance is optional: unavailable terminal proof must retain rows, not
    // block or change the business action. No clock or expiry is involved.
    let _ = store.prune_redundant_role_bookkeeping();
    run(&snapshot, &store)
}

/// Validate against the snapshot protected by with_role_action, not a lookup
/// made before its fence. Unqueued retries must call this again.
pub fn validate_role(
    snapshot: &crate::agent_management::AgentManagementSnapshot,
    caller: &str,
    resolution: &RoleResolution,
) -> Result<(), AskError> {
    let config = crate::config::store::load_codez_config_checked().map_err(store::unavailable)?;
    let boundary = crate::agent_bus::boundary::AgentBoundary::from_snapshot(&config.agent_security, snapshot);
    if !boundary.may_message(caller, &resolution.target) {
        return Err(AskError::new("recipient_outside_boundary", "Caller can no longer reach this Director"));
    }
    let authority = snapshot.projects.values().find(|p| p.project_id.as_str() == resolution.project_id
        && crate::memory::project_is_known(snapshot, &p.project_id))
        .ok_or_else(|| AskError::new("director_unassigned", "Project has no current Director"))?;
    if authority.authority_epoch != resolution.epoch || authority.authorized_director_session.as_str() != resolution.target {
        return Err(AskError::new("authority_conflict", "Director changed before this action was queued"));
    }
    Ok(())
}

impl Store {
    /// Keep the immutable request digest, but an intent without a business
    /// receipt is not a delivered message. Its current target may be refreshed
    /// only by a caller holding with_role_action and validating fresh authority.
    pub fn refresh_role_target(&self, caller: &str, action_id: &str, tool: &str, raw_digest: &str,
        resolution: RoleResolution) -> Result<RoleResolution, AskError> {
        self.update(|state| {
            let key = key(caller, action_id, tool);
            if let Some(old) = state.role_resolutions.get_mut(&key) {
                if old.raw_digest != raw_digest { return Err(AskError::new("action_conflict", "Role-address action ID already has another request")); }
                if old.business.is_some() { return Ok(old.resolution.clone()); }
                old.resolution = resolution.clone();
            } else {
                state.role_resolutions.insert(key, RoleResolutionReceipt { raw_digest: raw_digest.into(), resolution: resolution.clone(), business: None });
            }
            Ok(resolution)
        })
    }

    pub fn role_business_at(&self, caller: &str, action_id: &str, tool: &str, raw_digest: &str) -> Result<Option<serde_json::Value>, AskError> {
        self.role_target_at(caller, action_id, tool, raw_digest)?;
        Ok(self.snapshot()?.role_resolutions.get(&key(caller, action_id, tool)).and_then(|record| record.business.clone()))
    }

    /// Persist the exact prepared payload before local send. A crash/error
    /// after this write is uncertain, so it must not be retargeted on retry.
    pub fn set_role_business(&self, caller: &str, action_id: &str, tool: &str, raw_digest: &str,
        business: Option<serde_json::Value>) -> Result<(), AskError> {
        self.update(|state| {
            let record = state.role_resolutions.get_mut(&key(caller, action_id, tool))
                .ok_or_else(|| AskError::new("action_conflict", "No role-address intent exists"))?;
            if record.raw_digest != raw_digest { return Err(AskError::new("action_conflict", "Role-address action ID already has another request")); }
            record.business = business;
            Ok(())
        })
    }

    /// Roll back only this attempt's fresh provisional metadata, never user
    /// history. The caller holds with_role_action and has not published this
    /// random provisional ID. A failed/unknown store read or any business
    /// reference prevents cleanup. The ask lock is released before taking
    /// the extension write lock, preserving their lock order.
    pub fn discard_uncreated_intent(&self, asks: &crate::asks::store::AskStore, ask: &AskRecord,
        project: &str, question_project: Option<&str>, fresh_ownership: bool, fresh_question: bool) -> Result<bool, AskError> {
        if !fresh_ownership && !fresh_question { return Ok(false); }
        let unused = asks.with_records(&[&ask.ask_id],|state| {
            !state.asks.contains_key(&ask.ask_id)
                && !state.answers.values().any(|answer| answer.ask_id == ask.ask_id)
                && !state.questions.values().any(|question| question.ask_id == ask.ask_id)
                && !state.deliveries.values().any(|delivery| delivery.ask_id == ask.ask_id)
        }).map_err(store::unavailable)?;
        if !unused { return Ok(false); }
        self.update(|state| {
            if state.reviews.values().any(|review| review.ask_id == ask.ask_id) { return Ok(false); }
            if fresh_ownership && state.asks.get(&ask.ask_id).is_some_and(|ownership| {
                ownership.ask_id != ask.ask_id || ownership.request_sha256 != ask.request_sha256
                    || Some(ownership.origin_director.as_str()) != ask.requester.session()
                    || ownership.project_id != project || ownership.controller != ownership.origin_director
                    || ownership.controller_revision != 0 || ownership.pending_reconcile || ownership.owner_hold
                    || ownership.summary_pending || ownership.summary_from.is_some()
                    || ownership.history != vec![ownership.origin_director.clone()] || !ownership.changes.is_empty()
            }) { return Ok(false); }
            if fresh_question && state.question_projects.get(&ask.ask_id).is_some_and(|actual| Some(actual.as_str()) != question_project) {
                return Ok(false);
            }
            let mut changed = false;
            if fresh_ownership { changed |= state.asks.remove(&ask.ask_id).is_some(); }
            if fresh_question { changed |= state.question_projects.remove(&ask.ask_id).is_some(); }
            Ok(changed)
        })
    }
}

#[cfg(test)]
mod provisional_cleanup_tests {
    use super::*;

    #[test]
    fn cleanup_retains_committed_mismatched_and_unreadable_intents() {
        let root = std::env::temp_dir().join(format!("provisional-cleanup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let extension = Store::at(root.join("extension"));
        let asks = crate::asks::store::AskStore::at(root.join("asks"));
        let now = chrono::Utc::now();
        let mut new = crate::asks::store::tests::new_ask("committed", "task");
        new.ask_id = Some("ask-committed".into());
        let committed = asks.with_state(|state| state.create_ask(new, now)).unwrap().unwrap().0;
        let mut new = crate::asks::store::tests::new_ask("uncreated", "task");
        new.ask_id = Some("ask-uncreated".into());
        let fresh = crate::asks::store::AskState::provisional(&new, now);
        for ask in [&committed, &fresh] {
            extension.stamp_at(ask, ask.requester.session().unwrap(), "alpha", 1).unwrap();
            extension.update(|state| { state.question_projects.insert(ask.ask_id.clone(), "alpha".into()); Ok(()) }).unwrap();
        }
        assert!(!extension.discard_uncreated_intent(&asks, &committed, "alpha", Some("alpha"), true, true).unwrap());
        assert!(!extension.discard_uncreated_intent(&asks, &fresh, "wrong-project", Some("alpha"), true, true).unwrap());
        let mut mismatch = fresh.clone();
        mismatch.request_sha256 = "wrong-digest".into();
        assert!(!extension.discard_uncreated_intent(&asks, &mismatch, "alpha", Some("alpha"), true, true).unwrap());
        let blocked = root.join("not-a-directory");
        std::fs::write(&blocked, b"not a store").unwrap();
        assert!(extension.discard_uncreated_intent(&crate::asks::store::AskStore::at(blocked), &fresh,
            "alpha", Some("alpha"), true, true).is_err());
        assert_eq!(extension.snapshot().unwrap().asks.len(), 2);
        assert!(extension.discard_uncreated_intent(&asks, &fresh, "alpha", Some("alpha"), true, true).unwrap());
        let state = extension.snapshot().unwrap();
        assert_eq!(state.asks.len(), 1);
        assert_eq!(state.question_projects.len(), 1);
        assert!(state.asks.contains_key(&committed.ask_id));
        assert!(asks.snapshot().unwrap().asks.contains_key(&committed.ask_id));
        std::fs::remove_dir_all(root).unwrap();
    }
}
