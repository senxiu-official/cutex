//! Shared candidates and durable busy-wait timing for automatic admission.
//!
//! This selection grants no permission to send. Publishers reserve the exact
//! candidates again in their source stores while holding the receiving lane.

use super::agent_bus_state::AgentBusMessageRepository;
use super::hot_store;
use crate::asks::model::{DeliveryRecord, EventOutboxRecord};
use crate::asks::store::{AskState, AskStore};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod manifest;
pub const BATCH_ID_PREFIX: &str = "ordered-auto-";

pub use super::hot_store::automatic_input_order::{Candidate, Readiness, Source, Window};

pub struct AutomaticInputQueue {
    database: PathBuf,
}

/// Result payloads fitted and persisted as artifacts before entering the
/// ask-store transaction. The snapshot is a compare-and-reserve guard.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreparedResult {
    pub source: DeliveryRecord,
    pub native_message_id: String,
    pub artifact_id: String,
    pub view: serde_json::Value,
}

/// Revalidate every selected result against the same locked ask generation.
/// Explicit waits and old automatic events use the same source transition.
/// No artifact reads or native IO take place inside the transaction.
pub fn reserve_results(
    store: &AskStore,
    session: &str,
    batch_id: &str,
    prepared: &[PreparedResult],
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Vec<EventOutboxRecord>>> {
    reserve_results_with_budget(store, session, batch_id, prepared, now, None, false)
}

fn reserve_results_with_budget(
    store: &AskStore,
    session: &str,
    batch_id: &str,
    prepared: &[PreparedResult],
    now: DateTime<Utc>,
    budget: Option<&manifest::ResultBudget>,
    rides_start: bool,
) -> anyhow::Result<Option<Vec<EventOutboxRecord>>> {
    use crate::asks::{delivery, pause_admission, returns};
    anyhow::ensure!(
        !batch_id.is_empty(),
        "automatic result batch needs an identity"
    );
    let mut source_ids = BTreeSet::new();
    let mut native_ids = BTreeSet::new();
    anyhow::ensure!(
        prepared
            .iter()
            .all(|entry| source_ids.insert(&entry.source.delivery_id)
                && native_ids.insert(&entry.native_message_id)
                && !entry.native_message_id.is_empty()
                && !entry.artifact_id.is_empty()),
        "invalid or duplicate automatic result member"
    );
    if prepared.is_empty() {
        return Ok(Some(Vec::new()));
    }
    pause_admission::with_state_or(store, Ok(None), |state, _| {
        let prior: BTreeMap<_, _> = state
            .events
            .values()
            .filter(|event| {
                event.cutex_session_id == session
                    && event.automatic_batch_id.as_deref() == Some(batch_id)
            })
            .map(|event| (event.native_message_id.as_str(), event))
            .collect();
        if !prior.is_empty() {
            anyhow::ensure!(
                prior.len() == prepared.len(),
                "automatic result batch membership changed"
            );
            return prepared
                .iter()
                .map(|entry| {
                    let event = prior
                        .get(entry.native_message_id.as_str())
                        .filter(|event| {
                            event.delivery_ids.len() == 1
                                && event.delivery_ids[0] == entry.source.delivery_id
                                && event.event_artifact.as_deref()
                                    == Some(entry.artifact_id.as_str())
                                && event.view.as_ref() == Some(&entry.view)
                        })
                        .ok_or_else(|| anyhow::anyhow!("automatic result batch payload changed"))?;
                    Ok((**event).clone())
                })
                .collect::<anyhow::Result<Vec<_>>>()
                .map(Some);
        }
        anyhow::ensure!(
            !state
                .events
                .values()
                .any(|event| event.cutex_session_id == session
                    && native_ids.contains(&event.native_message_id)),
            "automatic result native identity already reserved"
        );
        if !result_lane_open(state, session, rides_start)
            || !prepared.iter().all(|entry| {
                state.deliveries.get(&entry.source.delivery_id) == Some(&entry.source)
                    && returns::ordinary_automatic_result(state, session, &entry.source)
            })
        {
            return Ok(None);
        }
        if let Some(budget) = budget {
            if !returns::reserve_result_turn(
                state,
                &budget.domain,
                session,
                budget.limit,
                budget.at,
            ) {
                return Ok(None);
            }
        }
        let mut reserved = Vec::with_capacity(prepared.len());
        for entry in prepared {
            let id = delivery::coalesce_selected_into_event(
                state,
                session,
                &entry.native_message_id,
                vec![entry.source.delivery_id.clone()],
                now,
            )
            .expect("exact result revalidated under lock");
            let event = state
                .events
                .get_mut(&id)
                .expect("automatic result event just created");
            event.automatic_batch_id = Some(batch_id.into());
            event.event_artifact = Some(entry.artifact_id.clone());
            event.view = Some(entry.view.clone());
            reserved.push(event.clone());
        }
        Ok(Some(reserved))
    })?
}

impl AutomaticInputQueue {
    pub fn has_startable_results(store: &AskStore, session: &str) -> anyhow::Result<bool> {
        store.inspect(|state| {
            result_lane_open(state, session, true)
                && state.deliveries.values().any(|delivery| {
                    crate::asks::returns::ordinary_automatic_result(state, session, delivery)
                })
        })
    }

    pub fn at_management_root(root: &Path) -> Self {
        Self {
            database: root.join(hot_store::DATABASE_FILE),
        }
    }

    pub fn pending(
        &self,
        session: &str,
        asks: &AskState,
        bus: &AgentBusMessageRepository,
    ) -> anyhow::Result<Vec<Candidate>> {
        self.pending_for_boundary(session, asks, bus, false)
    }

    /// A receive committed between the two source reads invalidates the scan.
    pub fn coherent_pending(
        &self,
        session: &str,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        rides_start: bool,
    ) -> anyhow::Result<Option<(std::sync::Arc<AskState>, Vec<Candidate>)>> {
        for _ in 0..2 {
            let lower = hot_store::automatic_input_order::watermark(&self.database)?;
            let asks = store.shared()?;
            let candidates = self.pending_for_boundary(session, &asks, bus, rides_start)?;
            if hot_store::automatic_input_order::watermark(&self.database)? == lower {
                return Ok(Some((asks, candidates)));
            }
        }
        Ok(None)
    }

    pub fn idle_result_wait(
        store: &AskStore,
        asks: &AskState,
        session: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        use crate::asks::result_coalescing::{
            context_for_session, hold, hold_unless_numbered_input,
        };
        let would_hold = context_for_session(session).is_some_and(|context| {
            hold(
                asks,
                session,
                context,
                crate::config::agent_bus::current().result_coalescing,
                now,
            )
        });
        hold_unless_numbered_input(would_hold, || {
            let admission = store
                .queue_repository()?
                .numbered_admission_with_state(session, asks)?;
            Ok(admission
                .items
                .iter()
                .any(|item| item.state == "waiting" && item.hold.is_none()))
        })
    }

    fn pending_for_boundary(
        &self,
        session: &str,
        asks: &AskState,
        bus: &AgentBusMessageRepository,
        rides_start: bool,
    ) -> anyhow::Result<Vec<Candidate>> {
        let mut candidates = bus.ordinary_automatic_inputs(session, asks)?;
        if result_lane_open(asks, session, rides_start) {
            candidates.extend(asks.deliveries.values().filter_map(|delivery| {
                if !crate::asks::returns::ordinary_automatic_result(asks, session, delivery) {
                    return None;
                }
                Some(Candidate {
                    source: Source::Result,
                    id: delivery.delivery_id.clone(),
                    received_at: crate::asks::parse_time(&delivery.created_at)?,
                    may_wake: true,
                })
            }));
        }
        hot_store::automatic_input_order::order(&self.database, candidates)
    }

    pub fn window(
        &self,
        session: &str,
        candidates: &[Candidate],
        wait: Option<Duration>,
    ) -> anyhow::Result<Window> {
        hot_store::automatic_input_order::window(&self.database, session, candidates, wait)
    }

    pub fn current_window(&self, session: &str) -> anyhow::Result<Option<Window>> {
        hot_store::automatic_input_order::current_window(&self.database, session)
    }

    pub fn retire_window(&self, session: &str, window: &Window) -> anyhow::Result<bool> {
        hot_store::automatic_input_order::retire_window(&self.database, session, window.id)
    }
}

fn result_lane_open(asks: &AskState, session: &str, rides_start: bool) -> bool {
    use crate::asks::{agent_pause, model::LeaseHolder, returns};
    if !rides_start {
        return returns::results_admissible(asks, session).is_some();
    }
    !asks.pauses.contains_key(session)
        && !agent_pause::is_paused(asks, session)
        && !returns::waiting(asks, session)
        && !asks.next_step_leases.contains_key(session)
        && asks.leases.get(session).is_none_or(|lease| {
            matches!(
                &lease.holder,
                LeaseHolder::Owner | LeaseHolder::Ask { .. } | LeaseHolder::Event { .. }
            ) && lease.turn_id.is_none()
        })
}

/// Select a bounded ordered prefix from fitted envelopes. A row that does not
/// fit never allows a later row to overtake it.
pub fn prefix_len(sizes: impl IntoIterator<Item = usize>, limit: usize, bytes: usize) -> usize {
    let mut remaining = bytes;
    sizes
        .into_iter()
        .take(limit)
        .take_while(|size| {
            let Some(left) = remaining.checked_sub(*size) else {
                return false;
            };
            remaining = left;
            true
        })
        .count()
}

/// The first triggering item in the selected prefix determines whether the
/// batch may start when idle. Busy delivery uses the same ordering.
pub fn readiness(
    window: &Window,
    prefix: &[Candidate],
    active: bool,
    now: DateTime<Utc>,
) -> Readiness {
    if prefix.is_empty() {
        return Readiness::Wait;
    }
    window.readiness(
        active,
        now,
        prefix.iter().any(|candidate| candidate.may_wake),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::delivery;
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER};

    #[test]
    fn an_explicit_wait_claim_removes_the_result_from_automatic_selection() {
        let root =
            std::env::temp_dir().join(format!("cutex-automatic-wait-{}", uuid::Uuid::new_v4()));
        let bus = AgentBusMessageRepository::open(&root).unwrap();
        let queue = AutomaticInputQueue::at_management_root(&root);
        let now = Utc::now();
        let mut state = AskState::default();
        let (ask, _) = state.create_ask(new_ask("auto-wait", "work"), now).unwrap();
        let result_id = delivery::record_answer_delivery(&mut state, &ask, "answer", now).unwrap();
        let pending = queue.pending(REQUESTER, &state, &bus).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, result_id);
        assert_eq!(
            delivery::claim_for_wait(&mut state, REQUESTER, &[ask.ask_id], "wait", now).len(),
            1
        );
        assert!(queue.pending(REQUESTER, &state, &bus).unwrap().is_empty());
        assert!(delivery::coalesce_selected_into_event(
            &mut state,
            REQUESTER,
            "stale-event",
            vec![result_id],
            now
        )
        .is_none());
        drop(bus);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_overflowed_prefix_never_pulls_a_later_trigger_forward() {
        assert_eq!(prefix_len([8, 100, 2], 10, 10), 1);
        assert_eq!(prefix_len([8, 1, 1], 2, 10), 2);
        assert_eq!(prefix_len([usize::MAX, 1], 2, 10), 0);
    }

    #[test]
    fn result_reservation_is_all_or_none_against_wait_and_replays_the_same_outbox() {
        let (store, root) = temp_store("auto-reserve-results");
        let now = Utc::now();
        let ids = store
            .with_state(|state| {
                ["earlier", "later"].map(|name| {
                    let (ask, _) = state.create_ask(new_ask(name, "work"), now).unwrap();
                    let result = delivery::record_answer_delivery(state, &ask, name, now).unwrap();
                    (ask.ask_id, result)
                })
            })
            .unwrap();
        let snapshot = store.shared().unwrap();
        let prepared = ids.each_ref().map(|(_, result)| PreparedResult {
            source: snapshot.deliveries[result].clone(),
            native_message_id: format!("native-{result}"),
            artifact_id: store
                .put_artifact(result.as_bytes(), "text/plain", &[], None, now)
                .unwrap(),
            view: serde_json::json!({"id":result}),
        });
        store
            .with_state(|state| {
                assert_eq!(
                    delivery::claim_for_wait(
                        state,
                        REQUESTER,
                        &[ids[1].0.clone()],
                        "wait-first",
                        now
                    )
                    .len(),
                    1
                );
            })
            .unwrap();
        assert!(reserve_results(&store, REQUESTER, "batch", &prepared, now)
            .unwrap()
            .is_none());
        assert!(store.shared().unwrap().events.is_empty());

        let reserved = reserve_results(&store, REQUESTER, "batch", &prepared[..1], now)
            .unwrap()
            .unwrap();
        let replay = reserve_results(&store, REQUESTER, "batch", &prepared[..1], now)
            .unwrap()
            .unwrap();
        assert_eq!(reserved, replay);
        store
            .with_state(|state| {
                assert!(delivery::claim_for_wait(
                    state,
                    REQUESTER,
                    &[ids[0].0.clone()],
                    "wait-later",
                    now
                )
                .is_empty());
            })
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
