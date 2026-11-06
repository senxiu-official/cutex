//! Count-bounded redundant hot bookkeeping, never history retention.
//!
//! Completed action receipts remain the canonical, permanent replay/conflict
//! fence in AskState. Only copies with independent terminal business proof may
//! leave role_resolutions. reviews are ALL visible history (reviews_for includes
//! settled rows), so no review row is eligible. No age/TTL or cold migration.
//! The v1 format and canonical receipts stay readable by r256; r252 ignores this
//! Director sidecar as before. Pending and unverifiable rows are unbounded.
use super::*;
use crate::asks::store::AskStore;
use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

const RESOLVED_ROLE_KEEP: usize = 512;
const RESOLVED_ROLE_PRUNE_BATCH: usize = 128;
const ACTIONS_PER_PASS: usize = 128;
static REQUESTED: AtomicBool = AtomicBool::new(true);
#[derive(Default)]
struct Scan {
    root: std::path::PathBuf,
    last: Option<String>,
    retained: Vec<String>,
}
static SCAN: Mutex<Option<Scan>> = Mutex::new(None);
pub(super) fn request_maintenance() {
    REQUESTED.store(true, Ordering::Relaxed);
}
pub(super) fn maintenance() {
    if !REQUESTED.swap(false, Ordering::Relaxed) {
        return;
    }
    let result = (|| -> Result<(), AskError> {
        let roster = crate::agent_management::AgentManagementStore::open_default()
            .map_err(store::unavailable)?;
        let Some(_fence) = roster
            .try_lock_delivery_mutations()
            .map_err(store::unavailable)?
        else {
            request_maintenance();
            return Ok(());
        };
        Store::open_default()?.prune_redundant_role_bookkeeping()
    })();
    if result.is_err() {
        request_maintenance();
    }
}

fn queue_id(record: &RoleResolutionReceipt) -> Option<&str> {
    let business = record.business.as_ref()?;
    (business["state"] == "committed")
        .then(|| business["queueReceipt"]["id"].as_str())
        .flatten()
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
fn redundant_keys(
    state: &OwnershipState,
    asks: &AskState,
    delivered: &BTreeSet<String>,
) -> BTreeSet<String> {
    redundant_keys_in(state, asks, delivered, asks.actions.iter())
}
fn redundant_keys_in<'a>(
    state: &OwnershipState,
    asks: &AskState,
    delivered: &BTreeSet<String>,
    actions: impl Iterator<Item = (&'a String, &'a crate::asks::store::ActionRecord)>,
) -> BTreeSet<String> {
    let mut eligible = BTreeSet::new();
    for (action_key, action) in actions {
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
        let asks = crate::asks::store::AskStore::open_default().map_err(store::unavailable)?;
        let repository = std::cell::OnceCell::new();
        self.prune_redundant_in(&asks, |id| {
            repository
                .get_or_init(crate::management::v2::agent_bus_state::agent_bus_message_repository)
                .as_ref()
                .ok()
                .and_then(|repository| repository.message_state(id).ok().flatten())
                .is_some_and(|(state, _)| state == "delivered")
        })
    }
    fn prune_redundant_in(
        &self,
        asks: &AskStore,
        delivered_proof: impl Fn(&str) -> bool,
    ) -> Result<(), AskError> {
        let snapshot = self.snapshot()?;
        if snapshot.role_resolutions.len() <= RESOLVED_ROLE_KEEP {
            return Ok(());
        }
        let shared = asks.shared().map_err(store::unavailable)?;
        // A moving cursor bounds fresh canonical action inspection. Carry at
        // most KEEP previously proven candidates across passes so sparse
        // terminal records can still reach the pruning threshold. Every one
        // is re-proved below; the cursor/pool never authorizes a deletion.
        let mut scan = {
            let mut slot = SCAN.lock().unwrap_or_else(|e| e.into_inner());
            slot.take()
                .filter(|s| s.root == self.root())
                .unwrap_or_else(|| Scan {
                    root: self.root().into(),
                    ..Default::default()
                })
        };
        let fresh: Vec<String> = match &scan.last {
            Some(last) => shared
                .actions
                .range((
                    std::ops::Bound::Excluded(last.clone()),
                    std::ops::Bound::Unbounded,
                ))
                .take(ACTIONS_PER_PASS)
                .map(|(k, _)| k.clone())
                .collect(),
            None => shared
                .actions
                .keys()
                .take(ACTIONS_PER_PASS)
                .cloned()
                .collect(),
        };
        scan.last = if fresh.len() < ACTIONS_PER_PASS {
            None
        } else {
            fresh.last().cloned()
        };
        let candidates: Vec<String> = scan
            .retained
            .iter()
            .chain(fresh.iter())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let mut role_keys = BTreeSet::new();
        for key in &candidates {
            if let Some((caller, action_id)) = key.split_once('\u{1f}') {
                if let Some(action) = shared.actions.get(key) {
                    role_keys.insert(super::resolution::key(caller, action_id, &action.tool));
                    if action.tool == "agent_delegate" {
                        role_keys.insert(super::resolution::key(
                            caller,
                            action_id,
                            "agent_delegate:origin",
                        ));
                    }
                }
            }
        }
        let mut delivered = BTreeSet::new();
        for id in role_keys
            .iter()
            .filter_map(|key| snapshot.role_resolutions.get(key))
            .filter_map(queue_id)
            .take(RESOLVED_ROLE_KEEP + ACTIONS_PER_PASS)
        {
            if delivered_proof(id) {
                delivered.insert(id.to_owned());
            }
        }
        // Recheck terminal ask/action proof under its canonical lock. Store::update
        // acquires no other store locks: existing order is ask -> Director.
        let remaining = asks
            .inspect(|asks| {
                self.update_if(|state| {
                    let eligible = redundant_keys_in(
                        state,
                        asks,
                        &delivered,
                        candidates
                            .iter()
                            .filter_map(|key| asks.actions.get_key_value(key)),
                    );
                    let count = eligible
                        .len()
                        .saturating_sub(RESOLVED_ROLE_KEEP)
                        .min(RESOLVED_ROLE_PRUNE_BATCH);
                    for key in eligible.iter().take(count) {
                        state.role_resolutions.remove(key);
                    }
                    let remaining: Vec<String> = candidates
                        .iter()
                        .filter(|key| {
                            let Some((caller, action_id)) = key.split_once('\u{1f}') else {
                                return false;
                            };
                            let Some(action) = asks.actions.get(*key) else {
                                return false;
                            };
                            [action.tool.as_str(), "agent_delegate:origin"]
                                .iter()
                                .any(|tool| {
                                    let k = super::resolution::key(caller, action_id, tool);
                                    eligible.contains(&k) && state.role_resolutions.contains_key(&k)
                                })
                        })
                        .take(RESOLVED_ROLE_KEEP)
                        .cloned()
                        .collect();
                    Ok((remaining, count > 0))
                })
            })
            .map_err(store::unavailable)??;
        scan.retained = remaining;
        *SCAN.lock().unwrap_or_else(|e| e.into_inner()) = Some(scan);
        // Continue a bounded sweep even without new role traffic. There is no
        // age/expiry deletion; uncertain proofs keep the original rows.
        request_maintenance();
        Ok(())
    }

    #[cfg(test)]
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

    #[test]
    fn waste_guard_role_action_never_runs_optional_ask_store_maintenance() {
        let store = Store::open_default().unwrap();
        let prefix = format!("guard-role-{}-", uuid::Uuid::new_v4());
        store
            .update(|state| {
                for n in 0..600 {
                    state.role_resolutions.insert(
                        format!("{prefix}{n}"),
                        RoleResolutionReceipt {
                            raw_digest: format!("synthetic-{n}"),
                            resolution: RoleResolution {
                                project_id: "synthetic".into(),
                                target: "director".into(),
                                epoch: 1,
                            },
                            business: None,
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
        let before = crate::asks::store::whole_state_copies_on_this_thread();
        for _ in 0..4 {
            crate::director_address::with_role_action(|_, _| Ok(())).unwrap();
        }
        let copies = crate::asks::store::whole_state_copies_on_this_thread() - before;
        store
            .update_if(|state| {
                let count = state.role_resolutions.len();
                state
                    .role_resolutions
                    .retain(|key, _| !key.starts_with(&prefix));
                Ok(((), count != state.role_resolutions.len()))
            })
            .unwrap();
        assert_eq!(
            copies, 0,
            "role actions must only request background maintenance, never draft AskState for it"
        );
    }

    #[test]
    fn waste_guard_housekeeping_has_bounded_proofs_zero_ask_drafts_and_preserves_receipts() {
        let (asks, ask_dir) = crate::asks::store::tests::temp_store("guard-role-maintenance");
        let root = std::env::temp_dir().join(format!("guard-pruning-{}", uuid::Uuid::new_v4()));
        let store = Store::at(&root);
        let mut ownership = OwnershipState::default();
        let mut canonical = crate::test_support::waste_scale::asks();
        for n in 0..600 {
            tell(&mut ownership, &mut canonical, &format!("proof-{n:04}"));
        }
        asks.with_state(|state| *state = canonical).unwrap();
        store
            .update(|state| {
                *state = ownership;
                Ok(())
            })
            .unwrap();
        let before = crate::asks::store::whole_state_copies_on_this_thread();
        for _ in 0..8 {
            let calls = std::cell::Cell::new(0usize);
            store
                .prune_redundant_in(&asks, |_| {
                    calls.set(calls.get() + 1);
                    true
                })
                .unwrap();
            assert!(
                calls.get() <= RESOLVED_ROLE_KEEP + ACTIONS_PER_PASS,
                "proof IO exceeds bounded pass"
            );
        }
        assert_eq!(
            crate::asks::store::whole_state_copies_on_this_thread() - before,
            0
        );
        assert_eq!(
            store.snapshot().unwrap().role_resolutions.len(),
            RESOLVED_ROLE_KEEP
        );
        assert_eq!(
            asks.shared().unwrap().actions.len(),
            600,
            "canonical replay history is permanent"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(ask_dir).unwrap();
    }

    #[test]
    #[ignore = "EVA-sized role housekeeping scale probe; runner marked private home only"]
    fn waste_scale_a05_role_housekeeping() {
        let home = std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("scale runner private home");
        assert!(std::path::Path::new(&home)
            .join(".cutex-test-private-home")
            .is_file());
        let asks = crate::asks::store::AskStore::open_default().unwrap();
        asks.with_state(|s| *s = crate::test_support::waste_scale::asks())
            .unwrap();
        let root = std::env::temp_dir().join(format!("scale-pruning-{}", uuid::Uuid::new_v4()));
        let store = Store::at(&root);
        store
            .update(|s| {
                for n in 0..600 {
                    s.role_resolutions.insert(
                        format!("role-{n}"),
                        RoleResolutionReceipt {
                            raw_digest: format!("digest-{n}"),
                            resolution: RoleResolution {
                                project_id: "synthetic".into(),
                                target: "director".into(),
                                epoch: 1,
                            },
                            business: None,
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
        let before = crate::asks::store::whole_state_copies_on_this_thread();
        let mut samples = Vec::new();
        for _ in 0..6 {
            let t = std::time::Instant::now();
            store.prune_redundant_role_bookkeeping().unwrap();
            samples.push(t.elapsed());
        }
        crate::test_support::waste_scale::emit(
            "a05_role_housekeeping",
            &samples,
            crate::asks::store::whole_state_copies_on_this_thread() - before,
            json!({"roles":600,"asks_target_bytes":crate::test_support::waste_scale::ASK_BYTES}),
        );
        std::fs::remove_dir_all(root).unwrap();
    }

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
