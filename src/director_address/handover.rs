use super::*;
use crate::agent_bus::boundary::AgentBoundary;
use crate::asks::caps::{subset_differences, Capabilities};
use crate::asks::store::AskStore;

/// Re-evaluate current reach and execution capabilities at the handover fence.
/// Missing sessions/configuration never grants inherited control.
pub fn successor_may_control(ask: &AskRecord, successor: &str) -> bool {
    successor_may_target(&ask.target_cutex_session_id, successor)
}
pub fn successor_may_target(target: &str, successor: &str) -> bool {
    let Ok(boundary) = AgentBoundary::load() else {
        return false;
    };
    if !boundary.may_message(successor, target) {
        return false;
    }
    crate::session::store::with_cutex_session_store(|sessions| {
        let Some(target) = sessions
            .sessions
            .values()
            .find(|r| r.cutex_session_id == target)
        else {
            return false;
        };
        let Some(controller) = sessions
            .sessions
            .values()
            .find(|r| r.cutex_session_id == successor)
        else {
            return false;
        };
        subset_differences(
            &Capabilities::of_record(target, &boundary),
            &Capabilities::of_record(controller, &boundary),
        )
        .is_empty()
    })
    .unwrap_or(false)
}

pub fn handover_project(
    project: &str,
    from: &str,
    to: &str,
    trusted_retired: bool,
    change_id: &str,
) -> Result<Vec<String>, AskError> {
    let store = Store::open_default()?;
    let asks = AskStore::open_default().map_err(store::unavailable)?;
    store.handover_project_in(&asks, project, from, to, trusted_retired, change_id)?;
    // Recover the second half after a crash between sidecar commit and ask notice.
    let ownership = store.snapshot()?;
    let committed: Vec<_> = ownership
        .asks
        .values()
        .filter(|o| {
            o.project_id == project
                && o.controller == to
                && o.changes.iter().any(|id| id == change_id)
        })
        .cloned()
        .collect();
    if !committed.is_empty() {
        // One ask-store transaction, rather than cloning the complete store
        // once for every inherited delegation.
        asks.with_records(&committed.iter().map(|item|item.ask_id.as_str()).collect::<Vec<_>>(),|state| {
            for item in &committed {
                if state.asks.contains_key(&item.ask_id) {
                    crate::asks::control::apply_handover(
                        state,
                        &item.ask_id,
                        Some(from),
                        &item.controller,
                        item.controller_revision,
                        chrono::Utc::now(),
                    )?;
                }
            }
            Ok::<(), AskError>(())
        })
        .map_err(store::unavailable)??;
        store.update(|state| {
            for item in &committed {
                if let Some(current) = state.asks.get_mut(&item.ask_id) {
                    if current.controller_revision == item.controller_revision
                        && current.controller == item.controller
                    {
                        current.summary_pending = false;
                    }
                }
            }
            Ok(())
        })?;
    }
    Ok(committed.into_iter().map(|o| o.ask_id).collect())
}
/// Recovery cannot infer a previous Director or grant successor authority.
/// Every stamped open project item becomes visible to the owner, even if its
/// controller predates the missing transition or the seat was cleared.
pub fn recover_project(
    project: &str,
    previous: Option<&str>,
    current: Option<&str>,
    change_id: &str,
) -> Result<Vec<String>, AskError> {
    let store = Store::open_default()?;
    let asks = AskStore::open_default()
        .map_err(store::unavailable)?
        .snapshot()
        .map_err(store::unavailable)?;
    store.plan_recovery(&asks, project, previous, current, change_id)?;
    Ok(store
        .snapshot()?
        .reviews
        .values()
        .filter(|r| r.project_id == project && r.change_id == change_id)
        .map(|r| r.ask_id.clone())
        .collect())
}
pub fn repair_handover_summaries() -> Result<usize, AskError> {
    let asks = AskStore::open_default().map_err(store::unavailable)?;
    Store::open_default()?.repair_summaries_for(&asks,|item| {
        authority_for_project(&item.project_id)
            .ok()
            .is_some_and(|(_, current, _)| current == item.controller)
    })
}
impl Store {
    pub(crate) fn repair_summaries_for(&self,asks:&AskStore,authorized:impl Fn(&Ownership)->bool)->Result<usize,AskError> {
        self.repair_summaries_with(|item| {
            if !authorized(item) {return Ok(false);}
            if asks.lookup_ask(&item.ask_id).map_err(store::unavailable)?.is_none() {return Ok(true);}
            apply_committed_handover(asks,item,item.summary_from.as_deref())?;
            Ok(true)
        })
    }
    pub(crate) fn finish_summary(&self, item: &Ownership) -> Result<(), AskError> {
        self.update(|state| {
            if let Some(current) = state.asks.get_mut(&item.ask_id) {
                if current.controller_revision == item.controller_revision
                    && current.controller == item.controller
                {
                    current.summary_pending = false;
                }
            }
            Ok(())
        })
    }
    pub(crate) fn repair_summaries_with(
        &self,
        mut apply: impl FnMut(&Ownership) -> Result<bool, AskError>,
    ) -> Result<usize, AskError> {
        let pending: Vec<_> = self
            .snapshot()?
            .asks
            .values()
            .filter(|o| o.summary_pending && !o.owner_hold && !o.pending_reconcile)
            .cloned()
            .collect();
        let mut repaired = 0;
        let mut error = None;
        for item in pending {
            match apply(&item) {
                Ok(false) => continue,
                Err(failure) => {
                    if error.is_none() {
                        error = Some(failure)
                    }
                    continue;
                }
                Ok(true) => {}
            }
            self.update(|state| {
                if let Some(current) = state.asks.get_mut(&item.ask_id) {
                    if current.controller_revision == item.controller_revision
                        && current.controller == item.controller
                    {
                        current.summary_pending = false;
                    }
                }
                Ok(())
            })?;
            repaired += 1;
        }
        if let Some(error) = error {
            return Err(error);
        }
        Ok(repaired)
    }
}
pub(crate) fn apply_committed_handover(
    asks: &AskStore,
    item: &Ownership,
    from: Option<&str>,
) -> Result<(), AskError> {
    asks.with_records(&[&item.ask_id],|state| {
        crate::asks::control::apply_handover(
            state,
            &item.ask_id,
            from,
            &item.controller,
            item.controller_revision,
            chrono::Utc::now(),
        )
    })
    .map_err(store::unavailable)??;
    Ok(())
}
impl Store {
    pub fn handover_project_in(
        &self,
        asks: &AskStore,
        project: &str,
        from: &str,
        to: &str,
        trusted_retired: bool,
        change_id: &str,
    ) -> Result<Vec<String>, AskError> {
        let snapshot = asks.snapshot().map_err(store::unavailable)?;
        let ownership = self.snapshot()?;
        let mut target_permissions = std::collections::BTreeMap::new();
        let eligible: std::collections::BTreeSet<_> = snapshot
            .asks
            .values()
            .filter(|a| {
                ownership.asks.get(&a.ask_id).is_some_and(|o| {
                    o.project_id == project && (o.controller != to || o.owner_hold)
                })
            })
            .filter(|a| {
                *target_permissions
                    .entry(a.target_cutex_session_id.clone())
                    .or_insert_with(|| successor_may_control(a, to))
            })
            .map(|a| a.ask_id.clone())
            .collect();
        self.plan_handover(
            &snapshot,
            project,
            from,
            to,
            trusted_retired,
            change_id,
            |a| eligible.contains(&a.ask_id),
        )
    }
    /// Pure policy input lets tests use isolated stores without consulting a live home.
    pub fn plan_handover(
        &self,
        asks: &AskState,
        project: &str,
        from: &str,
        to: &str,
        trusted_retired: bool,
        change_id: &str,
        may_control: impl Fn(&AskRecord) -> bool,
    ) -> Result<Vec<String>, AskError> {
        self.plan_handover_mode(
            asks,
            project,
            from,
            to,
            trusted_retired,
            change_id,
            false,
            may_control,
        )
    }
    pub fn plan_recovery(
        &self,
        asks: &AskState,
        project: &str,
        previous: Option<&str>,
        current: Option<&str>,
        change_id: &str,
    ) -> Result<Vec<String>, AskError> {
        self.plan_handover_mode(
            asks,
            project,
            previous.unwrap_or(""),
            current.unwrap_or(""),
            false,
            change_id,
            true,
            |_| false,
        )
    }
    fn plan_handover_mode(
        &self,
        asks: &AskState,
        project: &str,
        from: &str,
        to: &str,
        trusted_retired: bool,
        change_id: &str,
        owner_only: bool,
        may_control: impl Fn(&AskRecord) -> bool,
    ) -> Result<Vec<String>, AskError> {
        self.update(|state| {
            let mut transferred = vec![];
            for ask in asks.asks.values() {
                if !ask.is_open() {
                    continue;
                }
                let Some(ownership) = state.asks.get_mut(&ask.ask_id) else {
                    continue;
                };
                if ownership.project_id != project {
                    continue;
                }
                verify_digest(ask, ownership)?;
                if ownership.changes.iter().any(|id| id == change_id) {
                    continue;
                }
                let id = format!("handover:{change_id}:{}", ask.ask_id);
                if state.reviews.contains_key(&id) {
                    continue;
                }
                // A->B may still be awaiting a decision when C takes over.
                // Preserve the B audit item, but give C (or the owner) an
                // actionable item for the actual remaining controller.
                for prior in state.reviews.values_mut().filter(|r| {
                    r.ask_id == ask.ask_id
                        && r.project_id == project
                        && r.state == "pending"
                        && r.to != to
                }) {
                    prior.state = "superseded".into();
                    prior.revision += 1;
                }
                // A->B->A restores the actual controller without a handover,
                // but the obsolete review for B must still be settled.
                if ownership.controller == to && !ownership.owner_hold && !owner_only {
                    continue;
                }
                let original_controller = ownership.controller.clone();
                let requires_owner = owner_only || ownership.owner_hold || to.is_empty();
                if requires_owner {
                    ownership.owner_hold = true;
                    // Deferred dispatch and a subsequent Recovery can both
                    // describe the same owner decision. Retain one actionable
                    // item and its original provenance rather than duplicate it.
                    if state.reviews.values().any(|review| {
                        review.project_id == project
                            && review.ask_id == ask.ask_id
                            && review.state == "pending"
                            && review.owner_only
                            && review.from == original_controller
                            && review.to == to
                    }) {
                        continue;
                    }
                }
                let permitted = may_control(ask);
                let automatic = !requires_owner
                    && trusted_retired
                    && original_controller == from
                    && permitted
                    && ask.remote.is_none()
                    && ask.peer_inbound.is_none();
                if automatic {
                    ownership.controller = to.into();
                    ownership.controller_revision += 1;
                    ownership.summary_pending = true;
                    ownership.summary_from = Some(original_controller.clone());
                    if !ownership.history.iter().any(|id| id == to) {
                        ownership.history.push(to.into());
                    }
                    ownership.changes.push(change_id.into());
                    transferred.push(ask.ask_id.clone());
                }
                state.reviews.insert(
                    id.clone(),
                    HandoverReview {
                        id,
                        project_id: project.into(),
                        ask_id: ask.ask_id.clone(),
                        from: original_controller,
                        to: to.into(),
                        change_id: change_id.into(),
                        revision: 0,
                        state: if automatic { "accepted" } else { "pending" }.into(),
                        reason: if requires_owner {
                            "ownerResolutionRequired"
                        } else if automatic {
                            "trustedReplacement"
                        } else if !permitted {
                            "capabilityOrBoundaryReview"
                        } else {
                            "replacementEvidenceRequired"
                        }
                        .into(),
                        provenance_known: true,
                        actor: if automatic {
                            Some("trustedProvider".into())
                        } else {
                            None
                        },
                        owner_only: requires_owner,
                        resolved_to: None,
                    },
                );
            }
            Ok(transferred)
        })
    }
    /// Only the authenticated owner route calls this; explicit IDs bound the scope.
    pub fn approve_scope(&self, scope: ApprovedScope, asks: &AskState) -> Result<(), AskError> {
        if scope.id.is_empty() || scope.id.len() > 256 {
            return Err(AskError::new(
                "invalid_scope",
                "Scope ID must be 1–256 bytes",
            ));
        }
        if scope.ask_ids.is_empty() || scope.ask_ids.len() > 500 {
            return Err(AskError::new(
                "invalid_scope",
                "Approve between 1 and 500 explicit delegation IDs",
            ));
        }
        let existing = self.snapshot()?;
        for id in &scope.ask_ids {
            if existing
                .asks
                .get(id)
                .is_some_and(|o| o.project_id != scope.project_id)
            {
                return Err(AskError::new(
                    "authority_conflict",
                    "A stamped delegation cannot be relabelled to another project",
                ));
            }
            let ask = asks.asks.get(id).ok_or_else(|| {
                AskError::new("ask_not_found", "Delegation scope contains an unknown ID")
            })?;
            if ask.requester.session().is_none()
                || ask.remote.is_some()
                || ask.peer_inbound.is_some()
            {
                return Err(AskError::new(
                    "unsupported_scope",
                    "Owner scope only supports local agent delegations",
                ));
            }
        }
        self.update(|state| {
            if let Some(old) = state.approved_scopes.get(&scope.id) {
                if old != &scope {
                    return Err(AskError::new(
                        "action_conflict",
                        "Scope ID already has another payload",
                    ));
                }
                return Ok(());
            }
            for id in &scope.ask_ids {
                let ask = &asks.asks[id];
                let key = format!("scope:{}:{id}", scope.id);
                state.reviews.entry(key.clone()).or_insert(HandoverReview {
                    id: key,
                    project_id: scope.project_id.clone(),
                    ask_id: id.clone(),
                    from: ask.requester.session().unwrap().into(),
                    to: scope.director.clone(),
                    change_id: scope.id.clone(),
                    revision: 0,
                    state: "pending".into(),
                    reason: "ownerApprovedScope".into(),
                    provenance_known: false,
                    actor: None,
                    owner_only: false,
                    resolved_to: None,
                });
            }
            state.approved_scopes.insert(scope.id.clone(), scope);
            Ok(())
        })
    }
    /// CAS protects a concurrent second Director change or a second reviewer.
    pub fn decide_review(
        &self,
        id: &str,
        expected_revision: u64,
        actor: &str,
        accept: bool,
        ask: &AskRecord,
        project: &str,
        current_director: &str,
        epoch: u64,
        owner: bool,
        may_control: bool,
    ) -> Result<(), AskError> {
        if !owner && actor != current_director {
            return Err(AskError::new(
                "not_director",
                "Only the current Director or owner may review",
            ));
        }
        self.update(|state| {
            let mut review = state.reviews.get(id).cloned().ok_or_else(|| {
                AskError::new("review_not_found", "Handover review does not exist")
            })?;
            if review.project_id != project
                || (!owner && review.to != current_director)
                || review.ask_id != ask.ask_id
            {
                return Err(AskError::new(
                    "authority_conflict",
                    "Handover review is for another Director generation",
                ));
            }
            if review.owner_only && !owner{return Err(AskError::new("owner_review_required","Only the owner may resolve this handover"));}
            let desired = if accept { "accepted" } else { "discarded" };
            if review.revision == expected_revision.saturating_add(1)
                && review.state == desired
                && review.actor.as_deref() == Some(actor)
                && (!accept || review.resolved_to.as_deref().unwrap_or(&review.to)==current_director)
            {
                return Ok(());
            }
            let scoped_owner_grant=!review.provenance_known && state.approved_scopes.get(&review.change_id).is_some_and(|scope|scope.project_id==project && scope.director==current_director && scope.expected_epoch==epoch && scope.ask_ids.contains(&ask.ask_id));
            if !owner && !scoped_owner_grant && state.asks.get(&ask.ask_id).is_some_and(|o|o.owner_hold){return Err(AskError::new("owner_review_required","A discarded or recovered handover requires an owner decision"));}
            if review.revision != expected_revision || !(review.state == "pending" || (owner && matches!(review.state.as_str(),"superseded"|"discarded"))) {
                return Err(AskError::new(
                    "revision_conflict",
                    "Handover review changed",
                ));
            }
            if accept {
                if current_director.is_empty(){return Err(AskError::new("director_not_found","Assign a Director before handing over, or use owner stop to close the work"));}
                if !may_control && !owner {
                    return Err(AskError::new(
                        "handover_review_required",
                        "Owner approval is required for this capability or boundary difference",
                    ));
                }
                if !review.provenance_known {
                    let approved =
                        state
                            .approved_scopes
                            .get(&review.change_id)
                            .is_some_and(|scope| {
                                scope.project_id == project
                                    && (owner || (scope.director == current_director && scope.expected_epoch == epoch))
                                    && scope.ask_ids.contains(&ask.ask_id)
                            });
                    if !approved {
                        return Err(AskError::new(
                            "scope_required",
                            "Historical delegation requires a current owner-approved exact scope",
                        ));
                    }
                }
                let ownership = state.asks.entry(ask.ask_id.clone()).or_insert(Ownership {
                    ask_id: ask.ask_id.clone(),
                    project_id: project.into(),
                    origin_director: ask.requester.session().unwrap_or("").into(),
                    origin_epoch: epoch,
                    request_sha256: ask.request_sha256.clone(),
                    controller: review.from.clone(),
                    controller_revision: 0,
                    history: vec![review.from.clone()],
                    changes: vec![],
                    pending_reconcile: false,
                    owner_hold:false,summary_pending:false,summary_from:None,
                });
                verify_digest(ask, ownership)?;
                if ownership.project_id!=project{return Err(AskError::new("authority_conflict","Existing project ownership cannot be relabelled by another scope"));}
                ownership.summary_from=Some(ownership.controller.clone());
                ownership.controller = current_director.into();
                ownership.controller_revision += 1;
                ownership.owner_hold=false;
                ownership.summary_pending=true;
                if !ownership.history.iter().any(|id| id == current_director) {
                    ownership.history.push(current_director.into());
                }
                ownership.changes.push(review.change_id.clone());
                review.resolved_to=Some(current_director.into());
                for other in state.reviews.values_mut().filter(|r|r.ask_id==ask.ask_id && r.project_id==project && r.id!=id && r.state=="pending") {other.state="superseded".into();other.revision+=1;}
            } else if let Some(ownership)=state.asks.get_mut(&ask.ask_id) {
                // Discard declines successor control; it does not abandon the
                // task or silently revive a retired controller. Keep it held
                // with a separate owner-only actionable item.
                ownership.owner_hold=true;
                let fallback_id=format!("owner-resolution:{id}:{}",expected_revision+1);
                state.reviews.entry(fallback_id.clone()).or_insert(HandoverReview {
                    id:fallback_id,project_id:project.into(),ask_id:ask.ask_id.clone(),from:ownership.controller.clone(),to:current_director.into(),change_id:review.change_id.clone(),revision:0,state:"pending".into(),reason:"discardedHandoverRequiresOwner".into(),provenance_known:review.provenance_known,actor:None,owner_only:true,resolved_to:None,
                });
            }
            review.state = if accept { "accepted" } else { "discarded" }.into();
            review.revision += 1;
            review.actor = Some(actor.into());
            state.reviews.insert(id.into(),review);
            Ok(())
        })
    }
}
