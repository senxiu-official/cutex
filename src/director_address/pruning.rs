//! Count-bounded redundant hot bookkeeping, never history retention.
//!
//! Completed action receipts remain the canonical, permanent replay/conflict
//! fence in AskState. Only copies with independent terminal business proof may
//! leave role_resolutions. reviews are ALL visible history (reviews_for includes
//! settled rows), so no review row is eligible. No age/TTL or cold migration.
//! The v1 format and canonical receipts stay readable by r256; r252 ignores this
//! Director sidecar as before. Pending and unverifiable rows are unbounded.
use super::*;
use std::collections::BTreeSet;

const RESOLVED_ROLE_KEEP: usize = 512;
const RESOLVED_ROLE_PRUNE_BATCH: usize = 128;

fn queue_id(record: &RoleResolutionReceipt) -> Option<&str> {
    let business = record.business.as_ref()?;
    (business["state"] == "committed")
        .then(|| business["queueReceipt"]["id"].as_str())
        .flatten()
        .filter(|id| !id.is_empty())
}

fn redundant_keys(
    state: &OwnershipState,
    asks: &AskState,
    delivered: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut eligible = BTreeSet::new();
    for (action_key, action) in &asks.actions {
        let Some((caller, action_id)) = action_key.split_once('\u{1f}') else {
            continue;
        };
        let Some(result) = action.result.as_ref().filter(|r| r["ok"] == true) else {
            continue;
        };
        let tools: &[&str] = match action.tool.as_str() {
            "agent_tell" => &["agent_tell"],
            "agent_delegate" => &["agent_delegate", "agent_delegate:origin"],
            _ => continue,
        };
        for tool in tools {
            let key = super::resolution::key(caller, action_id, tool);
            let Some(record) = state.role_resolutions.get(&key) else {
                continue;
            };
            if record.raw_digest != action.request_sha256 {
                continue;
            }
            if *tool == "agent_tell" {
                let Some(business) = record.business.as_ref() else {
                    continue;
                };
                if business["state"] == "committed"
                    && business.get("receipt") == Some(result)
                    && result["to"].as_str() == Some(record.resolution.target.as_str())
                    && queue_id(record).is_some_and(|id| delivered.contains(id))
                {
                    eligible.insert(key);
                }
                continue;
            }
            let Some(ask) = result["ask_id"].as_str().and_then(|id| asks.asks.get(id)) else {
                continue;
            };
            if ask.requester.session() != Some(caller)
                || ask.action_id != action_id
                || ask.request_sha256 != action.request_sha256
                || ask.is_open()
                || !ask.execution.is_terminal()
                || ask.remote.is_some()
                || ask.peer_inbound.is_some()
                || ask
                    .current_attempt()
                    .is_some_and(|attempt| !attempt.state.is_terminal())
                || !ask.running_jobs().is_empty()
                || ask.awaiting.is_some()
                || ask.resume.is_some()
                || ask.hold.is_some()
                || matches!(ask.review, Some(crate::asks::model::ReviewState::AwaitingAcceptance | crate::asks::model::ReviewState::ChangesRequested))
                || asks.leases.values().any(|lease| matches!(&lease.holder, crate::asks::model::LeaseHolder::Ask { ask_id, .. } if ask_id == &ask.ask_id))
                || asks.questions.values().any(|q| {
                    q.ask_id == ask.ask_id && q.state == crate::asks::model::QuestionState::Open
                })
                || asks
                    .deliveries
                    .values()
                    .any(|d| d.ask_id == ask.ask_id && d.state != DeliveryState::Acknowledged)
                || state
                    .reviews
                    .values()
                    .any(|r| r.ask_id == ask.ask_id && r.state == "pending")
                || state
                    .asks
                    .get(&ask.ask_id)
                    .is_some_and(|o| o.pending_reconcile || o.owner_hold || o.summary_pending)
            {
                continue;
            }
            let matches_role = if *tool == "agent_delegate" {
                ask.target_cutex_session_id == record.resolution.target
            } else {
                state.asks.get(&ask.ask_id).is_some_and(|o| {
                    o.request_sha256 == ask.request_sha256
                        && o.project_id == record.resolution.project_id
                        && o.origin_epoch == record.resolution.epoch
                        && o.origin_director == record.resolution.target
                        && o.origin_director == caller
                })
            };
            // Delegations do not use tell business receipts. Unknown future
            // payload/provenance is retained rather than interpreted as closed.
            if matches_role && record.business.is_none() {
                eligible.insert(key);
            }
        }
    }
    eligible
}

impl Store {
    /// Caller holds the lifecycle mutation fence. Collect bus proof before any
    /// ask/Director write lock; a failed/absent bus read is never terminal proof.
    pub(super) fn prune_redundant_role_bookkeeping(&self) -> Result<(), AskError> {
        let snapshot = self.snapshot()?;
        if snapshot.role_resolutions.len() <= RESOLVED_ROLE_KEEP {
            return Ok(());
        }
        let mut delivered = BTreeSet::new();
        if let Ok(repository) =
            crate::management::v2::agent_bus_state::agent_bus_message_repository()
        {
            for id in snapshot.role_resolutions.values().filter_map(queue_id) {
                if repository
                    .message_state(id)
                    .ok()
                    .flatten()
                    .is_some_and(|(state, _)| state == "delivered")
                {
                    delivered.insert(id.to_owned());
                }
            }
        }
        let asks = crate::asks::store::AskStore::open_default().map_err(store::unavailable)?;
        // Recheck terminal ask/action proof under its canonical lock. Store::update
        // acquires no other store locks: existing order is ask -> Director.
        asks.with_state(|asks| {
            self.prune_role_copies_at(
                asks,
                &delivered,
                RESOLVED_ROLE_KEEP,
                RESOLVED_ROLE_PRUNE_BATCH,
            )
        })
        .map_err(store::unavailable)??;
        Ok(())
    }

    fn prune_role_copies_at(
        &self,
        asks: &AskState,
        delivered: &BTreeSet<String>,
        keep: usize,
        batch: usize,
    ) -> Result<usize, AskError> {
        // Do not rewrite the file/revision on the common no-op path. Recompute
        // after taking the write lock; concurrent/newer rows cannot inherit proof.
        let current = self.snapshot()?;
        if redundant_keys(&current, asks, delivered).len() <= keep {
            return Ok(0);
        }
        self.update(|state| {
            let keys = redundant_keys(state, asks, delivered);
            let count = keys.len().saturating_sub(keep).min(batch);
            for key in keys.into_iter().take(count) {
                state.role_resolutions.remove(&key);
            }
            Ok(count)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::{ActionLookup, ActionRecord};
    use serde_json::json;

    fn tell(state: &mut OwnershipState, asks: &mut AskState, id: &str) -> String {
        let result = json!({"ok":true,"to":"director","detail":id});
        let key = super::super::resolution::key("caller", id, "agent_tell");
        state.role_resolutions.insert(
            key.clone(),
            RoleResolutionReceipt {
                raw_digest: format!("digest-{id}"),
                resolution: RoleResolution {
                    project_id: "p".into(),
                    target: "director".into(),
                    epoch: 1,
                },
                business: Some(
                    json!({"state":"committed","receipt":result,"queueReceipt":{"id":id}}),
                ),
            },
        );
        asks.actions.insert(
            format!("caller\u{1f}{id}"),
            ActionRecord {
                tool: "agent_tell".into(),
                request_sha256: format!("digest-{id}"),
                result: Some(result),
                at: "2026-10-24T00:00:00Z".into(),
            },
        );
        key
    }

    #[test]
    fn tell_needs_exact_canonical_receipt_and_delivered_not_pending_proof() {
        let mut state = OwnershipState::default();
        let mut asks = AskState::default();
        let good = tell(&mut state, &mut asks, "good");
        let pending = tell(&mut state, &mut asks, "pending");
        let uncertain = tell(&mut state, &mut asks, "uncertain");
        state
            .role_resolutions
            .get_mut(&uncertain)
            .unwrap()
            .business
            .as_mut()
            .unwrap()["state"] = json!("uncertain");
        let mismatched = tell(&mut state, &mut asks, "mismatch");
        asks.actions
            .get_mut("caller\u{1f}mismatch")
            .unwrap()
            .request_sha256 = "another request".into();
        let no_result = tell(&mut state, &mut asks, "no-result");
        asks.actions
            .get_mut("caller\u{1f}no-result")
            .unwrap()
            .result = None;
        let receipt = tell(&mut state, &mut asks, "receipt");
        state
            .role_resolutions
            .get_mut(&receipt)
            .unwrap()
            .business
            .as_mut()
            .unwrap()["receipt"]["detail"] = json!("different");
        let unqueued = tell(&mut state, &mut asks, "unqueued");
        state.role_resolutions.get_mut(&unqueued).unwrap().business = None;
        let ids = [
            "good",
            "uncertain",
            "mismatch",
            "no-result",
            "receipt",
            "unqueued",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(redundant_keys(&state, &asks, &ids), BTreeSet::from([good]));
        assert!(
            redundant_keys(&state, &asks, &BTreeSet::new()).is_empty(),
            "missing/unreadable bus proof retains all rows"
        );
        for key in [pending, uncertain, mismatched, no_result, receipt, unqueued] {
            assert!(state.role_resolutions.contains_key(&key));
        }
    }

    #[test]
    fn count_bound_preserves_history_and_permanent_exact_replay_conflict_fences() {
        let root = std::env::temp_dir().join(format!("director-prune-{}", uuid::Uuid::new_v4()));
        let store = Store::at(&root);
        let mut state = OwnershipState::default();
        let mut asks = AskState::default();
        let mut delivered = BTreeSet::new();
        for n in 0..7 {
            let id = format!("done-{n}");
            tell(&mut state, &mut asks, &id);
            delivered.insert(id);
        }
        let uncertain = tell(&mut state, &mut asks, "uncertain");
        state
            .role_resolutions
            .get_mut(&uncertain)
            .unwrap()
            .business
            .as_mut()
            .unwrap()["state"] = json!("uncertain");
        state
            .extra
            .insert("futureProof".into(), json!({"keep":true}));
        for status in ["pending", "accepted", "discarded", "superseded"] {
            state.reviews.insert(
                status.into(),
                HandoverReview {
                    id: status.into(),
                    project_id: "p".into(),
                    ask_id: "historic".into(),
                    from: "old".into(),
                    to: "director".into(),
                    change_id: "c".into(),
                    revision: 1,
                    state: status.into(),
                    reason: "history".into(),
                    provenance_known: true,
                    actor: None,
                    owner_only: false,
                    resolved_to: None,
                },
            );
        }
        let history = state.reviews.clone();
        store
            .update(|saved| {
                *saved = state;
                Ok(())
            })
            .unwrap();
        let canonical = asks.actions.clone();
        assert_eq!(
            store.prune_role_copies_at(&asks, &delivered, 2, 3).unwrap(),
            3
        );
        assert_eq!(
            store.prune_role_copies_at(&asks, &delivered, 2, 3).unwrap(),
            2
        );
        let stable = std::fs::read(root.join("ownership-v1.json")).unwrap();
        assert_eq!(
            store.prune_role_copies_at(&asks, &delivered, 2, 3).unwrap(),
            0
        );
        assert_eq!(
            std::fs::read(root.join("ownership-v1.json")).unwrap(),
            stable,
            "no-op does not rewrite/revise"
        );
        let saved = store.snapshot().unwrap();
        assert_eq!(
            saved.role_resolutions.len(),
            3,
            "two redundant terminal copies plus all uncertain rows"
        );
        assert!(saved.role_resolutions.contains_key(&uncertain));
        assert_eq!(
            saved.reviews, history,
            "every visible review remains byte-equivalent"
        );
        assert_eq!(saved.extra["futureProof"], json!({"keep":true}));
        assert_eq!(asks.actions, canonical);
        // Exactly the path used after a pruned role cache misses: once() reads
        // this permanent action fence. Both original replay and changed-request
        // conflict survive; a new Director cannot receive a duplicate send.
        for n in 0..7 {
            let id = format!("done-{n}");
            let digest = format!("digest-{id}");
            let receipt = canonical[&format!("caller\u{1f}{id}")]
                .result
                .clone()
                .unwrap();
            assert_eq!(
                asks.begin_action("caller", &id, "agent_tell", &digest, chrono::Utc::now()),
                ActionLookup::Replay(receipt)
            );
            assert_eq!(
                asks.begin_action("caller", &id, "agent_tell", "changed", chrono::Utc::now()),
                ActionLookup::Conflict
            );
            assert_eq!(
                asks.begin_action("caller", &id, "agent_delegate", &digest, chrono::Utc::now()),
                ActionLookup::Conflict
            );
        }
        // No new storage format/export: an r256-shaped deserializer reads this
        // same v1 document and canonical AskState receipts remain unchanged.
        let reread: OwnershipState = serde_json::from_slice(&stable).unwrap();
        assert_eq!(reread.version, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delegation_requires_closed_terminal_matching_provenance_not_just_an_action_result() {
        let mut asks = AskState::default();
        let (ask, _) = asks
            .create_ask(
                crate::asks::store::tests::new_ask("closed", "private"),
                chrono::Utc::now(),
            )
            .unwrap();
        let caller = ask.requester.session().unwrap().to_owned();
        let mut state = OwnershipState::default();
        let key = super::super::resolution::key(&caller, &ask.action_id, "agent_delegate");
        state.role_resolutions.insert(
            key.clone(),
            RoleResolutionReceipt {
                raw_digest: ask.request_sha256.clone(),
                resolution: RoleResolution {
                    project_id: "p".into(),
                    target: ask.target_cutex_session_id.clone(),
                    epoch: 1,
                },
                business: None,
            },
        );
        asks.actions.insert(
            format!("{caller}\u{1f}{}", ask.action_id),
            ActionRecord {
                tool: "agent_delegate".into(),
                request_sha256: ask.request_sha256.clone(),
                result: Some(json!({"ok":true,"ask_id":ask.ask_id})),
                at: ask.created_at.clone(),
            },
        );
        assert!(redundant_keys(&state, &asks, &BTreeSet::new()).is_empty());
        let record = asks.asks.get_mut(&ask.ask_id).unwrap();
        record.closed = Some(crate::asks::model::CloseOutcome::Completed);
        record.execution = crate::asks::model::ExecutionState::Answered;
        assert_eq!(
            redundant_keys(&state, &asks, &BTreeSet::new()),
            BTreeSet::from([key.clone()])
        );
        asks.asks.get_mut(&ask.ask_id).unwrap().request_sha256 = "mismatch".into();
        assert!(redundant_keys(&state, &asks, &BTreeSet::new()).is_empty());
        asks.asks.get_mut(&ask.ask_id).unwrap().request_sha256 = ask.request_sha256.clone();
        asks.asks.get_mut(&ask.ask_id).unwrap().execution =
            crate::asks::model::ExecutionState::Running;
        assert!(redundant_keys(&state, &asks, &BTreeSet::new()).is_empty());
        asks.asks.get_mut(&ask.ask_id).unwrap().execution =
            crate::asks::model::ExecutionState::Answered;
        asks.asks.get_mut(&ask.ask_id).unwrap().review =
            Some(crate::asks::model::ReviewState::AwaitingAcceptance);
        assert!(redundant_keys(&state, &asks, &BTreeSet::new()).is_empty());
        asks.asks.get_mut(&ask.ask_id).unwrap().review = None;
        // Prune both the target and origin copies only when the immutable
        // ownership witness matches, preserving the canonical delegation replay.
        let origin =
            super::super::resolution::key(&caller, &ask.action_id, "agent_delegate:origin");
        state.asks.insert(
            ask.ask_id.clone(),
            Ownership {
                ask_id: ask.ask_id.clone(),
                project_id: "origin-project".into(),
                origin_director: caller.clone(),
                origin_epoch: 7,
                request_sha256: ask.request_sha256.clone(),
                controller: caller.clone(),
                controller_revision: 0,
                history: vec![caller.clone()],
                changes: vec![],
                pending_reconcile: false,
                owner_hold: false,
                summary_pending: false,
                summary_from: None,
            },
        );
        state.role_resolutions.insert(
            origin.clone(),
            RoleResolutionReceipt {
                raw_digest: ask.request_sha256.clone(),
                resolution: RoleResolution {
                    project_id: "origin-project".into(),
                    target: caller.clone(),
                    epoch: 7,
                },
                business: None,
            },
        );
        assert_eq!(
            redundant_keys(&state, &asks, &BTreeSet::new()),
            BTreeSet::from([key.clone(), origin.clone()])
        );
        state.asks.get_mut(&ask.ask_id).unwrap().origin_epoch = 8;
        assert_eq!(
            redundant_keys(&state, &asks, &BTreeSet::new()),
            BTreeSet::from([key.clone()])
        );
        state.asks.get_mut(&ask.ask_id).unwrap().origin_epoch = 7;
        let root =
            std::env::temp_dir().join(format!("director-delegate-prune-{}", uuid::Uuid::new_v4()));
        let store = Store::at(&root);
        store
            .update(|saved| {
                *saved = state;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .prune_role_copies_at(&asks, &BTreeSet::new(), 0, 128)
                .unwrap(),
            2
        );
        let result = asks.actions[&format!("{caller}\u{1f}{}", ask.action_id)]
            .result
            .clone()
            .unwrap();
        assert_eq!(
            asks.begin_action(
                &caller,
                &ask.action_id,
                "agent_delegate",
                &ask.request_sha256,
                chrono::Utc::now()
            ),
            ActionLookup::Replay(result)
        );
        assert_eq!(
            asks.begin_action(
                &caller,
                &ask.action_id,
                "agent_delegate",
                "changed",
                chrono::Utc::now()
            ),
            ActionLookup::Conflict
        );
        assert_eq!(
            store.snapshot().unwrap().asks[&ask.ask_id].origin_epoch,
            7,
            "ownership history remains"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
