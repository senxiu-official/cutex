//! Write-ahead publication record across the ask and Bus transactions.

use super::{reserve_results_with_budget, AutomaticInputQueue, PreparedResult, BATCH_ID_PREFIX};
use crate::app_server::external_input::{
    automatic_batch::{Batch, STATUS_METHOD, SUBMIT_METHOD},
    busy_refusal_reason, DeliveryState, Envelope, ProcessingState, Status,
};
use crate::asks::{
    delivery,
    model::EventOutboxRecord,
    store::{AskState, AskStore},
};
use crate::management::v2::{agent_bus_state::AgentBusMessageRepository, hot_store, user_input};
use anyhow::{ensure, Context};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PLANNED: i64 = 0;
const SENDING: i64 = 1;
const ABORTING: i64 = 2;
const RECONCILING: i64 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    Result(PreparedResult),
    Bus { canonical_digest: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub envelope: Envelope,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub id: String,
    pub session: String,
    pub window_id: i64,
    pub members: Vec<Member>,
    pub trigger: Option<String>,
    pub notes_at_start: bool,
    #[serde(default)]
    pub rides_start: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_budget: Option<ResultBudget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultBudget {
    pub domain: String,
    pub limit: usize,
    pub at: DateTime<Utc>,
}

impl Publication {
    fn results(&self) -> impl Iterator<Item = &PreparedResult> {
        self.members
            .iter()
            .filter_map(|member| match &member.origin {
                Origin::Result(result) => Some(result),
                Origin::Bus { .. } => None,
            })
    }

    fn result_event<'a>(
        &self,
        state: &'a AskState,
        result: &PreparedResult,
    ) -> Option<&'a EventOutboxRecord> {
        let source_id = &result.source.delivery_id;
        let delivery = state.deliveries.get(source_id)?;
        let event = state.events.get(delivery.event_id.as_deref()?)?;
        (delivery.ask_id == result.source.ask_id
            && event.cutex_session_id == self.session
            && event.automatic_batch_id.as_deref() == Some(self.id.as_str())
            && event.native_message_id == result.native_message_id
            && event.delivery_ids.len() == 1
            && &event.delivery_ids[0] == source_id)
            .then_some(event)
    }

    fn batch(&self) -> anyhow::Result<Batch> {
        ensure!(
            self.id.starts_with(BATCH_ID_PREFIX) && !self.session.is_empty(),
            "automatic publication identity is invalid"
        );
        ensure!(
            !self.rides_start || (self.notes_at_start && self.trigger.is_none()),
            "a startup publication cannot start a turn"
        );
        for member in &self.members {
            ensure!(
                member.envelope.owner_id == self.session,
                "automatic member recipient changed"
            );
            match &member.origin {
                Origin::Result(result) => {
                    ensure!(
                        self.trigger.is_some() || self.rides_start,
                        "automatic results require a trigger"
                    );
                    ensure!(
                        result.native_message_id == member.envelope.message.id,
                        "automatic result envelope identity changed"
                    );
                }
                Origin::Bus { canonical_digest } => ensure!(
                    !canonical_digest.is_empty(),
                    "automatic bus member lacks canonical identity"
                ),
            }
        }
        let batch = Batch::new(
            self.members.iter().map(|m| m.envelope.clone()).collect(),
            self.trigger.clone(),
        )?;
        batch.with_notes_at_start(self.notes_at_start)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    NoPublication,
    Held,
    Unsent,
    Pending,
    WaitingForStart,
    Settled,
}

impl AutomaticInputQueue {
    /// Select one bounded prefix in the receiving lane. The builder may
    /// exclude a source that is independently held or no longer eligible.
    pub fn plan(
        &self,
        session: &str,
        candidates: &[super::Candidate],
        wait: Option<std::time::Duration>,
        active: bool,
        takes_soon: bool,
        now: DateTime<Utc>,
        build: impl FnMut(&super::Candidate) -> anyhow::Result<Option<Member>>,
    ) -> anyhow::Result<Option<Publication>> {
        self.plan_for_boundary(
            session,
            candidates,
            wait,
            active,
            takes_soon,
            now,
            None,
            crate::app_server::pre_turn_notes::Budget::default(),
            build,
        )
    }

    pub fn plan_for_boundary(
        &self,
        session: &str,
        candidates: &[super::Candidate],
        wait: Option<std::time::Duration>,
        active: bool,
        takes_soon: bool,
        now: DateTime<Utc>,
        startup: Option<crate::app_server::pre_turn_notes::Budget>,
        passive: crate::app_server::pre_turn_notes::Budget,
        mut build: impl FnMut(&super::Candidate) -> anyhow::Result<Option<Member>>,
    ) -> anyhow::Result<Option<Publication>> {
        self.plan_prefix_for_boundary(
            session,
            candidates,
            wait,
            active,
            takes_soon,
            now,
            startup,
            passive,
            |candidate| build(candidate).map(std::ops::ControlFlow::Continue),
        )
    }

    /// Keep already verified members when a source's eligibility is uncertain.
    /// A break keeps that source and all successors for a later pass.
    pub fn plan_prefix_for_boundary(
        &self,
        session: &str,
        candidates: &[super::Candidate],
        wait: Option<std::time::Duration>,
        active: bool,
        takes_soon: bool,
        now: DateTime<Utc>,
        startup: Option<crate::app_server::pre_turn_notes::Budget>,
        mut passive: crate::app_server::pre_turn_notes::Budget,
        mut build: impl FnMut(
            &super::Candidate,
        ) -> anyhow::Result<std::ops::ControlFlow<(), Option<Member>>>,
    ) -> anyhow::Result<Option<Publication>> {
        use crate::app_server::external_input::{automatic_batch, Delivery};
        if candidates.is_empty() {
            super::hot_store::automatic_input_order::clear_window(&self.database, session)?;
            return Ok(None);
        }
        let window = self.window(session, candidates, wait)?;
        if active && (!takes_soon || window.readiness(true, now, false) == super::Readiness::Wait) {
            return Ok(None);
        }
        let mut trigger = None;
        let mut members = Vec::new();
        let mut bytes = automatic_batch::MAX_BYTES;
        if let Some(startup) = startup {
            passive.notes = passive.notes.min(startup.notes);
            passive.bytes = passive.bytes.min(startup.bytes);
        }
        let mut blocked = None;
        for (index, candidate) in candidates.iter().enumerate() {
            if passive.notes == 0 && !candidate.may_wake {
                blocked = Some(index);
                break;
            }
            let mut member = match build(candidate)? {
                std::ops::ControlFlow::Continue(Some(member)) => member,
                std::ops::ControlFlow::Continue(None) => continue,
                std::ops::ControlFlow::Break(()) => break,
            };
            let wakes = startup.is_none() && trigger.is_none() && candidate.may_wake;
            member.envelope.message.delivery = match (wakes, active) {
                (true, true) => Delivery::Soon,
                (true, false) => Delivery::AfterTurn,
                _ => Delivery::Passive,
            };
            member.envelope.semantic_sha256 = member.envelope.digest();
            member.envelope.validate()?;
            let size = serde_json::to_vec(&member.envelope)?.len();
            if !wakes {
                if passive.notes == 0 || passive.bytes < size {
                    blocked = Some(index);
                    break;
                }
                passive.notes -= 1;
                passive.bytes -= size;
            }
            let Some(remaining) = bytes.checked_sub(size) else {
                break;
            };
            bytes = remaining;
            if wakes {
                trigger = Some(member.envelope.message.id.clone());
            }
            members.push(member);
            if members.len() == automatic_batch::MAX_MEMBERS {
                break;
            }
        }
        if !active && startup.is_none() && trigger.is_none() {
            if let Some(index) = blocked {
                for candidate in candidates[index + 1..].iter().filter(|item| item.may_wake) {
                    let mut member = match build(candidate)? {
                        std::ops::ControlFlow::Continue(Some(member)) => member,
                        std::ops::ControlFlow::Continue(None) => continue,
                        std::ops::ControlFlow::Break(()) => break,
                    };
                    member.envelope.message.delivery = Delivery::AfterTurn;
                    member.envelope.semantic_sha256 = member.envelope.digest();
                    member.envelope.validate()?;
                    if serde_json::to_vec(&member.envelope)?.len() > automatic_batch::MAX_BYTES {
                        continue;
                    }
                    trigger = Some(member.envelope.message.id.clone());
                    members = vec![member];
                    break;
                }
            }
        }
        if members.is_empty() {
            return Ok(None);
        }
        if startup.is_none()
            && trigger.is_none()
            && members
                .iter()
                .any(|m| matches!(m.origin, Origin::Result(_)))
        {
            return Ok(None);
        }
        Ok(Some(Publication {
            id: format!("{BATCH_ID_PREFIX}{}", uuid::Uuid::new_v4()),
            session: session.into(),
            window_id: window.id,
            notes_at_start: trigger.is_none() && !active,
            rides_start: startup.is_some(),
            members,
            trigger,
            result_budget: None,
        }))
    }

    /// Persist the exact ordered members and envelopes before either source
    /// store claims them. The same identity can be retried with identical data.
    pub fn stage(&self, plan: &Publication) -> anyhow::Result<()> {
        plan.batch()?;
        let mut db = hot_store::connection(&self.database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, Vec<u8>)> = tx
            .query_row(
                "SELECT batch,plan FROM automatic_input_publications WHERE batch=?1 OR (session=?2 AND phase<>?3)
                ORDER BY batch<>?1 LIMIT 1",
                params![plan.id, plan.session, RECONCILING],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let bytes = serde_json::to_vec(plan)?;
        if let Some((batch, prior)) = existing {
            ensure!(
                batch == plan.id && prior == bytes,
                "automatic publication already staged"
            );
        } else {
            let binding = &plan.members[0].envelope;
            tx.execute("INSERT INTO automatic_input_publications(session,batch,thread,generation,plan,phase) VALUES (?1,?2,?3,?4,?5,?6)",
                params![plan.session, plan.id, binding.thread_id, binding.runtime_generation.to_string(), bytes, PLANNED])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn staged(&self, session: &str) -> anyhow::Result<Option<(Publication, i64)>> {
        if !self.database.try_exists()? {
            return Ok(None);
        }
        let db = hot_store::read_connection(&self.database)?;
        if !hot_store::table_exists(&db, "automatic_input_publications")? {
            return Ok(None);
        }
        let row: Option<(Vec<u8>, i64)> = db
            .query_row(
                "SELECT plan,phase FROM automatic_input_publications WHERE session=?1 AND phase<>?2",
                params![session, RECONCILING],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(bytes, phase)| Ok((serde_json::from_slice(&bytes)?, phase)))
            .transpose()
    }

    fn change_phase(&self, plan: &Publication, from: i64, to: i64) -> anyhow::Result<bool> {
        let db = hot_store::connection(&self.database)?;
        Ok(db.execute(
            "UPDATE automatic_input_publications SET phase=?1
            WHERE session=?2 AND batch=?3 AND phase=?4",
            params![to, plan.session, plan.id, from],
        )? == 1)
    }

    fn finish_publication(&self, plan: &Publication, phase: i64) -> anyhow::Result<()> {
        let mut db = hot_store::connection(&self.database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "DELETE FROM automatic_input_publications WHERE session=?1 AND batch=?2 AND phase=?3",
            params![plan.session, plan.id, phase],
        )? == 1
            && phase == SENDING
        {
            tx.execute(
                "DELETE FROM automatic_input_windows WHERE session=?1 AND id=?2",
                params![plan.session, plan.window_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn detach(&self, plan: &Publication) -> anyhow::Result<bool> {
        let mut db = hot_store::connection(&self.database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE automatic_input_publications SET phase=?1 WHERE batch=?2 AND phase=?3",
            params![RECONCILING, plan.id, SENDING],
        )? == 1;
        if changed {
            tx.execute(
                "DELETE FROM automatic_input_windows WHERE session=?1 AND id=?2",
                params![plan.session, plan.window_id],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    fn next_reconciliation(
        &self,
        session: &str,
        thread: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<Publication>> {
        if !self.database.try_exists()? {
            return Ok(None);
        }
        let db = hot_store::read_connection(&self.database)?;
        if !hot_store::table_exists(&db, "automatic_input_publications")? {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = db
            .query_row(
                "SELECT plan FROM automatic_input_publications WHERE session=?1 AND thread=?2
             AND generation=?3 AND phase=?4 AND next_check_ms<=?5
             ORDER BY next_check_ms,rowid LIMIT 1",
                params![
                    session,
                    thread,
                    generation.to_string(),
                    RECONCILING,
                    now.timestamp_millis()
                ],
                |row| row.get(0),
            )
            .optional()?;
        bytes
            .map(|bytes| Ok(serde_json::from_slice(&bytes)?))
            .transpose()
    }

    fn retire_replaced_publication(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        plan: &Publication,
        phase: i64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let generation = plan.members[0].envelope.runtime_generation;
        let asks: Vec<_> = plan
            .results()
            .map(|result| result.source.ask_id.as_str())
            .collect();
        if !asks.is_empty() {
            store.with_records(&asks, |state| {
                let events: Vec<_> = plan
                    .results()
                    .filter_map(|result| plan.result_event(state, result))
                    .map(|event| event.event_id.clone())
                    .collect();
                crate::asks::returns::hold_replaced_automatic_results(
                    state,
                    &plan.session,
                    &events,
                    generation,
                    now,
                )
            })?;
        }
        for member in &plan.members {
            if matches!(member.origin, Origin::Bus { .. }) {
                bus.hold_replaced_automatic(&plan.session, &plan.id, &member.envelope)?;
            }
        }
        self.finish_publication(plan, phase)
    }

    fn retire_old_reconciliation(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        session: &str,
        thread: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        if !self.database.try_exists()? {
            return Ok(());
        }
        let db = hot_store::read_connection(&self.database)?;
        if !hot_store::table_exists(&db, "automatic_input_publications")? {
            return Ok(());
        }
        let bytes: Option<Vec<u8>> = db
            .query_row(
                "SELECT plan FROM automatic_input_publications WHERE session=?1 AND phase=?2
            AND (thread<>?3 OR generation<>?4) LIMIT 1",
                params![session, RECONCILING, thread, generation.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        drop(db);
        if let Some(bytes) = bytes {
            let plan = serde_json::from_slice(&bytes)?;
            self.retire_replaced_publication(store, bus, &plan, RECONCILING, now)?;
        }
        Ok(())
    }

    fn reconcile_one(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        plan: &Publication,
        now: DateTime<Utc>,
        request: &mut impl FnMut(&str, Value) -> anyhow::Result<Value>,
    ) -> anyhow::Result<()> {
        if self.disposed_members(bus, plan)?.len() == plan.members.len() {
            return self.finish_publication(plan, RECONCILING);
        }
        let db = hot_store::connection(&self.database)?;
        db.execute(
            "UPDATE automatic_input_publications SET next_check_ms=?1 WHERE batch=?2 AND phase=?3",
            params![
                now.timestamp_millis().saturating_add(2_000),
                plan.id,
                RECONCILING
            ],
        )?;
        drop(db);
        let batch = plan.batch()?;
        let statuses = batch.validate_status(request(STATUS_METHOD, batch.status_params())?)?;
        if self.settle(store, bus, plan, statuses, now)? {
            self.finish_publication(plan, RECONCILING)?;
        }
        Ok(())
    }

    /// Finish a staged publication through its original bound transport.
    /// The operation lane coordinates with owner submit and other legacy
    /// publishers. SQLite source locks are released before the adapter call.
    pub fn publish(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        session: &str,
        thread: &str,
        generation: u64,
        now: DateTime<Utc>,
        request: impl FnMut(&str, Value) -> anyhow::Result<Value>,
    ) -> anyhow::Result<Outcome> {
        self.publish_with(
            store,
            bus,
            session,
            thread,
            generation,
            now,
            false,
            || Ok(None),
            request,
        )
    }

    pub fn publish_with(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        session: &str,
        thread: &str,
        generation: u64,
        now: DateTime<Utc>,
        start_only: bool,
        select: impl FnOnce() -> anyhow::Result<Option<Publication>>,
        mut request: impl FnMut(&str, Value) -> anyhow::Result<Value>,
    ) -> anyhow::Result<Outcome> {
        let lane = user_input::session_input_lock(session)?;
        let held = if start_only && lane.held_by_current_thread() {
            lane.reserve()
        } else {
            lane.try_lock()
        };
        let _lane = match held {
            Ok(g) => g,
            Err(_) => return Ok(Outcome::Held),
        };
        if !start_only {
            self.retire_old_reconciliation(store, bus, session, thread, generation, now)?;
        }
        let (plan, phase) = if let Some(staged) = self.staged(session)? {
            staged
        } else {
            let recovery = if start_only {
                None
            } else {
                self.next_reconciliation(session, thread, generation, now)?
            };
            if let Some(plan) = recovery {
                if let Err(error) = self.reconcile_one(store, bus, &plan, now, &mut request) {
                    eprintln!(
                        "automatic input receipt recovery for {session}/{}: {error:#}",
                        plan.id
                    );
                }
            }
            let Some(plan) = select()? else {
                return Ok(Outcome::NoPublication);
            };
            self.stage(&plan)?;
            (plan, PLANNED)
        };
        ensure!(
            plan.session == session,
            "automatic publication recipient changed"
        );
        let batch = plan.batch()?;
        let original = &plan.members[0].envelope;
        if phase == ABORTING {
            self.release_unsent(store, bus, &plan, now)?;
            return Ok(Outcome::Unsent);
        }
        if start_only && phase == PLANNED && !plan.notes_at_start {
            if self.change_phase(&plan, PLANNED, ABORTING)? {
                self.release_unsent(store, bus, &plan, now)?;
                return Ok(Outcome::Unsent);
            }
            return Ok(Outcome::Held);
        }
        if original.thread_id != thread || original.runtime_generation != generation {
            if phase == PLANNED && self.change_phase(&plan, PLANNED, ABORTING)? {
                self.release_unsent(store, bus, &plan, now)?;
                return Ok(Outcome::Unsent);
            }
            if phase == SENDING {
                self.retire_replaced_publication(store, bus, &plan, SENDING, now)?;
            }
            return Ok(Outcome::Held);
        }
        let statuses = if phase == PLANNED {
            if crate::asks::pause_admission::is_held(store, session)? {
                return Ok(Outcome::Held);
            }
            let results = plan
                .members
                .iter()
                .filter_map(|m| match &m.origin {
                    Origin::Result(result) => Some(result.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let bus_members = plan
                .members
                .iter()
                .filter_map(|m| match &m.origin {
                    Origin::Bus { canonical_digest } => {
                        Some((canonical_digest.as_str(), &m.envelope))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if self.disposed(&plan)?
                || reserve_results_with_budget(
                    store,
                    session,
                    &plan.id,
                    &results,
                    now,
                    plan.result_budget.as_ref(),
                    plan.rides_start,
                )?
                .is_none()
                || (!bus_members.is_empty()
                    && !bus.reserve_automatic(session, &plan.id, &bus_members)?)
                || self.disposed(&plan)?
            {
                if self.change_phase(&plan, PLANNED, ABORTING)? {
                    self.release_unsent(store, bus, &plan, now)?;
                }
                return Ok(Outcome::Unsent);
            }
            if !self.change_phase(&plan, PLANNED, SENDING)? {
                return Ok(Outcome::Held);
            }
            // From this point any uncertain reply keeps the source claims and
            // their original status keys in reconciliation.
            match request(SUBMIT_METHOD, batch.submit_params()) {
                Ok(reply) => match batch.validate_submit(reply) {
                    Ok(statuses) => statuses,
                    Err(error) => {
                        self.detach(&plan)?;
                        return Err(error);
                    }
                },
                Err(error) if busy_refusal_reason(&error).is_some() => {
                    if self.change_phase(&plan, SENDING, ABORTING)? {
                        self.release_unsent(store, bus, &plan, now)?;
                    }
                    return Ok(Outcome::Unsent);
                }
                Err(error) => {
                    self.detach(&plan)?;
                    return Err(error);
                }
            }
        } else {
            if self.disposed_members(bus, &plan)?.len() == plan.members.len() {
                self.finish_publication(&plan, SENDING)?;
                return Ok(Outcome::Settled);
            }
            match request(STATUS_METHOD, batch.status_params())
                .and_then(|reply| batch.validate_status(reply))
            {
                Ok(statuses) => statuses,
                Err(error) => {
                    self.detach(&plan)?;
                    return Err(error);
                }
            }
        };
        let waiting_for_start = plan.notes_at_start
            && statuses.iter().all(|status| {
                matches!(
                    status.delivery_state,
                    DeliveryState::Pending | DeliveryState::ContextPersisted
                )
            });
        let statuses_unconfirmed = statuses.iter().any(|status| {
            status.processing.state == ProcessingState::Held
                || !matches!(
                    status.delivery_state,
                    DeliveryState::Pending | DeliveryState::ContextPersisted
                )
        }) || self.disposed(&plan)?;
        let settled = match self.settle(store, bus, &plan, statuses, now) {
            Ok(settled) => settled,
            Err(error) => {
                self.detach(&plan)?;
                return Err(error);
            }
        };
        if settled {
            self.finish_publication(&plan, SENDING)?;
            Ok(Outcome::Settled)
        } else if waiting_for_start {
            self.detach(&plan)?;
            Ok(Outcome::WaitingForStart)
        } else {
            if statuses_unconfirmed {
                self.detach(&plan)?;
            }
            Ok(Outcome::Pending)
        }
    }

    /// The replacement observer may expose no batch RPCs. Release claims
    /// left by the preceding binding using local evidence only.
    pub fn retire_replaced_bindings(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        session: &str,
        thread: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        if !self.database.try_exists()? {
            return Ok(());
        }
        let lane = user_input::session_input_lock(session)?;
        let Ok(_lane) = lane.try_lock() else {
            return Ok(());
        };
        self.retire_old_reconciliation(store, bus, session, thread, generation, now)?;
        let Some((plan, phase)) = self.staged(session)? else {
            return Ok(());
        };
        let original = &plan.members[0].envelope;
        if original.thread_id == thread && original.runtime_generation == generation {
            return Ok(());
        }
        if phase == SENDING {
            self.retire_replaced_publication(store, bus, &plan, SENDING, now)?;
        } else if phase == ABORTING || self.change_phase(&plan, PLANNED, ABORTING)? {
            self.release_unsent(store, bus, &plan, now)?;
        }
        Ok(())
    }

    fn disposed(&self, plan: &Publication) -> anyhow::Result<bool> {
        let ids = plan.members.iter().map(|member| match &member.origin {
            Origin::Result(result) => result.source.delivery_id.as_str(),
            Origin::Bus { .. } => member.envelope.message.id.as_str(),
        });
        super::super::input_disposal::any_requested_at(
            &self
                .database
                .with_file_name(super::super::input_ledger::DATABASE_FILE),
            &plan.session,
            ids,
        )
    }

    fn release_unsent(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        plan: &Publication,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        bus.release_owner_carries(&plan.session, &plan.id)?;
        let asks: Vec<_> = plan
            .results()
            .map(|result| result.source.ask_id.as_str())
            .collect();
        if !asks.is_empty() {
            store.with_records(&asks, |state| {
                let ids: Vec<_> = plan
                    .results()
                    .filter_map(|result| plan.result_event(state, result))
                    .filter(|event| !event.acknowledged && event.turn_id.is_none())
                    .map(|event| event.event_id.clone())
                    .collect();
                if !ids.is_empty() {
                    if let Some(budget) = &plan.result_budget {
                        crate::asks::returns::refund_result_turn(state, &budget.domain, budget.at);
                    }
                }
                for id in ids {
                    delivery::release_event(state, &id, now);
                }
            })?;
        }
        self.finish_publication(plan, ABORTING)
    }
    fn settle(
        &self,
        store: &AskStore,
        bus: &AgentBusMessageRepository,
        plan: &Publication,
        statuses: Vec<Status>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let mut complete = true;
        let mut result_proofs = Vec::new();
        let disposed = self.disposed_members(bus, plan)?;
        for (member, status) in plan.members.iter().zip(statuses) {
            if disposed.contains(member.source_id()) {
                continue;
            }
            let Some(receipt) = status.receipt.as_ref() else {
                complete = false;
                continue;
            };
            match &member.origin {
                Origin::Result(result) => result_proofs.push((result, receipt.turn_id.clone())),
                Origin::Bus { .. } => {
                    bus.observe_external_input(
                        &member.envelope.message.id,
                        &member.envelope,
                        &status,
                    )?;
                    if !bus.record_native_context_injected(
                        &plan.session,
                        &member.envelope.message.id,
                        &member.envelope,
                        &receipt.turn_id,
                    )? {
                        let snapshot = bus
                            .snapshot_by_message_id(&member.envelope.message.id)?
                            .context("automatic bus source disappeared")?;
                        ensure!(
                            snapshot.state == "delivered"
                                && snapshot.external_input.as_ref() == Some(&member.envelope),
                            "automatic bus source settlement changed"
                        );
                    }
                }
            }
        }
        if !result_proofs.is_empty() {
            let asks: Vec<_> = result_proofs
                .iter()
                .map(|(result, _)| result.source.ask_id.as_str())
                .collect();
            store.with_records(&asks, |state| -> anyhow::Result<()> {
                let mut proofs = Vec::new();
                for (result, turn) in result_proofs {
                    let event = plan
                        .result_event(state, result)
                        .context("automatic result reservation disappeared")?;
                    ensure!(
                        event.turn_id.as_ref().is_none_or(|prior| prior == &turn),
                        "automatic result receipt conflict"
                    );
                    proofs.push((event.event_id.clone(), turn));
                }
                for (id, turn) in proofs {
                    delivery::acknowledge_event(state, &id, &turn, now);
                }
                Ok(())
            })??;
        }
        Ok(complete)
    }

    fn disposed_members(
        &self,
        bus: &AgentBusMessageRepository,
        plan: &Publication,
    ) -> anyhow::Result<std::collections::BTreeSet<String>> {
        let mut disposed = super::super::input_disposal::requested_ids_at(
            &self
                .database
                .with_file_name(super::super::input_ledger::DATABASE_FILE),
            &plan.session,
            plan.members.iter().map(Member::source_id),
        )?;
        let bus_ids = plan
            .members
            .iter()
            .filter_map(|member| match member.origin {
                Origin::Bus { .. } => Some(member.source_id().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        disposed.extend(bus.ordered_discarded_inputs(&plan.session, &bus_ids)?);
        Ok(disposed)
    }
}

impl Member {
    pub fn input_id(&self) -> &str {
        self.source_id()
    }

    fn source_id(&self) -> &str {
        match &self.origin {
            Origin::Result(result) => &result.source.delivery_id,
            Origin::Bus { .. } => &self.envelope.message.id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::external_input::{Delivery, Message, Receipt, Source, SourceKind};
    use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};
    use crate::management::v2::agent_bus_state::AgentBusQueuedMessage;
    use serde_json::json;

    fn envelope(id: &str, delivery: Delivery) -> Envelope {
        let mut envelope = Envelope {
            version: 1,
            owner_id: REQUESTER.into(),
            thread_id: "thread".into(),
            runtime_generation: 7,
            message: Message {
                id: id.into(),
                source: Source {
                    kind: SourceKind::Agent,
                    id: TARGET.into(),
                },
                event_type: "message".into(),
                delivery,
                text: format!("text of {id}"),
            },
            view: None,
            semantic_sha256: String::new(),
        };
        envelope.semantic_sha256 = envelope.digest();
        envelope
    }

    fn response(plan: &Publication, proved: usize) -> Value {
        json!({"version":1, "ownerId":REQUESTER, "threadId":"thread", "runtimeGeneration":7,
            "statuses":plan.members.iter().enumerate().map(|(index, member)| {
                let envelope = &member.envelope;
                let receipt = (index < proved).then(|| {
                    let mut receipt = Receipt {
                        schema: "codex.external-input-receipt.v1".into(), receipt_id: String::new(),
                        owner_id: REQUESTER.into(), thread_id: "thread".into(), message_id: envelope.message.id.clone(),
                        semantic_sha256: envelope.semantic_sha256.clone(), response_item_id: envelope.message.id.clone(),
                        turn_id: "real-turn".into(), ordinal: index as u64 + 1,
                    };
                    receipt.receipt_id = receipt.digest_id();
                    receipt
                });
                json!({"messageId":envelope.message.id, "semanticSha256":envelope.semantic_sha256,
                    "deliveryState":if receipt.is_some() { "context_persisted" } else { "unknown" },
                    "receipt":receipt, "processing":{"state":"none","attemptId":null,"reason":null}})
            }).collect::<Vec<_>>()
        })
    }

    fn queue_note(bus: &AgentBusMessageRepository, id: &str, now: DateTime<Utc>) -> String {
        let digest = "a".repeat(64);
        bus.record_queued_isolated(AgentBusQueuedMessage {
            owner_cutex_session_id: REQUESTER.into(), message_id: id.into(),
            from_cutex_session_id: Some(TARGET.into()), to_cutex_session_id: REQUESTER.into(),
            from_runtime_agent_id: None, to_runtime_agent_id: None, delivery_mode: "passive".into(),
            content: format!("text of {id}"), queued_at: now, semantic_sha256: digest.clone(),
            canonical_envelope: serde_json::from_value(json!({
                "id":id,"from":TARGET,"to":REQUESTER,"fromCutexSessionId":TARGET,"toCutexSessionId":REQUESTER,
                "triggerTurn":false,"createdAtEpochSecs":1,"senderKind":"agent","deliveryMode":"passive",
                "content":format!("text of {id}")
            })).unwrap(),
        }).unwrap();
        digest
    }

    #[test]
    fn a_later_note_cannot_move_the_deadline_or_overtake_two_results() {
        let root =
            std::env::temp_dir().join(format!("cutex-auto-deadline-{}", uuid::Uuid::new_v4()));
        let queue = AutomaticInputQueue::at_management_root(&root);
        let now = Utc::now();
        let candidates: Vec<_> = ["first", "second", "note"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| super::super::Candidate {
                id: id.into(),
                source: if index < 2 {
                    super::super::Source::Result
                } else {
                    super::super::Source::Bus
                },
                may_wake: index < 2,
                received_at: now + chrono::Duration::seconds(index as i64),
            })
            .collect();
        let build = |candidate: &super::super::Candidate| {
            Ok(Some(Member {
                envelope: envelope(&candidate.id, Delivery::Passive),
                origin: Origin::Bus {
                    canonical_digest: "a".repeat(64),
                },
            }))
        };
        assert!(queue
            .plan(
                REQUESTER,
                &candidates[..1],
                Some(std::time::Duration::from_secs(5)),
                true,
                true,
                now,
                build
            )
            .unwrap()
            .is_none());
        assert!(queue
            .plan(
                REQUESTER,
                &candidates,
                Some(std::time::Duration::ZERO),
                true,
                true,
                now + chrono::Duration::seconds(4),
                build
            )
            .unwrap()
            .is_none());
        let due = queue
            .plan(
                REQUESTER,
                &candidates,
                None,
                true,
                true,
                now + chrono::Duration::seconds(5),
                build,
            )
            .unwrap()
            .unwrap();
        assert_eq!(due.trigger.as_deref(), Some("first"));
        assert_eq!(
            due.members
                .iter()
                .map(|member| member.envelope.message.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "note"]
        );
        assert_eq!(due.members[0].envelope.message.delivery, Delivery::Soon);
        assert_eq!(due.members[1].envelope.message.delivery, Delivery::Passive);
        let startup = queue
            .plan(
                REQUESTER,
                &candidates,
                None,
                false,
                true,
                now + chrono::Duration::seconds(3),
                build,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            startup.members[0].envelope.message.delivery,
            Delivery::AfterTurn
        );
        let ride = queue
            .plan_for_boundary(
                REQUESTER,
                &candidates,
                None,
                false,
                true,
                now + chrono::Duration::seconds(3),
                Some(crate::app_server::pre_turn_notes::Budget {
                    notes: 2,
                    bytes: 1024 * 1024,
                }),
                crate::app_server::pre_turn_notes::Budget::default(),
                build,
            )
            .unwrap()
            .unwrap();
        assert!(ride.notes_at_start && ride.rides_start);
        assert!(ride.trigger.is_none());
        assert_eq!(
            ride.members
                .iter()
                .map(|member| member.envelope.message.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(ride
            .members
            .iter()
            .all(|member| member.envelope.message.delivery == Delivery::Passive));
        ride.batch().unwrap();
        let trigger_only = queue
            .plan_for_boundary(
                REQUESTER,
                &candidates,
                None,
                false,
                true,
                now + chrono::Duration::seconds(3),
                None,
                crate::app_server::pre_turn_notes::Budget { notes: 0, bytes: 0 },
                build,
            )
            .unwrap()
            .unwrap();
        assert_eq!(trigger_only.trigger.as_deref(), Some("first"));
        assert_eq!(trigger_only.members.len(), 1);
        let blocked_first = vec![
            candidates
                .iter()
                .find(|item| !item.may_wake)
                .unwrap()
                .clone(),
            candidates
                .iter()
                .find(|item| item.id == "first")
                .unwrap()
                .clone(),
            candidates
                .iter()
                .find(|item| item.id == "second")
                .unwrap()
                .clone(),
        ];
        for passive in [
            crate::app_server::pre_turn_notes::Budget {
                notes: 0,
                bytes: usize::MAX,
            },
            crate::app_server::pre_turn_notes::Budget {
                notes: 99,
                bytes: 0,
            },
        ] {
            let wake = queue
                .plan_for_boundary(
                    REQUESTER,
                    &blocked_first,
                    None,
                    false,
                    true,
                    now,
                    None,
                    passive,
                    build,
                )
                .unwrap()
                .unwrap();
            assert_eq!(wake.trigger.as_deref(), Some("first"));
            assert_eq!(wake.members.len(), 1);
            assert_eq!(
                wake.members[0].envelope.message.delivery,
                Delivery::AfterTurn
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lost_submit_uses_only_status_and_each_source_waits_for_its_receipt() {
        exercise_mixed_publication(false);
    }

    #[test]
    fn ordered_replaced_binding_releases_claims_for_owner_disposition() {
        exercise_mixed_publication(true);
    }

    fn exercise_mixed_publication(replace_binding: bool) {
        let root =
            std::env::temp_dir().join(format!("cutex-auto-publication-{}", uuid::Uuid::new_v4()));
        let management = root.join("management-v2");
        let store = AskStore::at_runtime(&root);
        let bus = AgentBusMessageRepository::open(&management).unwrap();
        let queue = AutomaticInputQueue::at_management_root(&management);
        let now = Utc::now();
        let result_id = store
            .with_state(|state| {
                let (ask, _) = state
                    .create_ask(new_ask("auto-publish", "work"), now)
                    .unwrap();
                delivery::record_answer_delivery(state, &ask, "answer", now).unwrap()
            })
            .unwrap();
        let digest = queue_note(&bus, "note", now);
        let plan = Publication {
            id: "ordered-auto-fixture".into(),
            session: REQUESTER.into(),
            window_id: 1,
            members: vec![
                Member {
                    envelope: envelope("note", Delivery::Passive),
                    origin: Origin::Bus {
                        canonical_digest: digest,
                    },
                },
                Member {
                    envelope: envelope("result", Delivery::Soon),
                    origin: Origin::Result(PreparedResult {
                        source: store.shared().unwrap().deliveries[&result_id].clone(),
                        native_message_id: "result".into(),
                        artifact_id: store
                            .put_artifact(b"text of result", "text/plain", &[], None, now)
                            .unwrap(),
                        view: json!({"id":"result"}),
                    }),
                },
            ],
            trigger: Some("result".into()),
            notes_at_start: false,
            rides_start: false,
            result_budget: None,
        };
        queue.stage(&plan).unwrap();
        if replace_binding {
            assert_eq!(
                queue
                    .publish(&store, &bus, REQUESTER, "thread", 7, now, |method, _| {
                        assert_eq!(method, SUBMIT_METHOD);
                        let mut reply = response(&plan, 0);
                        for status in reply["statuses"].as_array_mut().unwrap() {
                            status["deliveryState"] = json!("pending");
                        }
                        Ok(reply)
                    })
                    .unwrap(),
                Outcome::Pending
            );
            assert!(bus
                .read_now_notes(REQUESTER)
                .unwrap()
                .1
                .contains(&"note".into()));
            assert_eq!(queue.staged(REQUESTER).unwrap().unwrap().1, SENDING);
            let (unrelated_delivery, unrelated_event) = store
                .with_state(|state| {
                    let (ask, _) = state
                        .create_ask(new_ask("unrelated-publish", "unrelated"), now)
                        .unwrap();
                    let id = delivery::record_answer_delivery(state, &ask, "unrelated-answer", now)
                        .unwrap();
                    let event = delivery::coalesce_selected_into_event(
                        state,
                        REQUESTER,
                        "unrelated-result",
                        vec![id.clone()],
                        now,
                    )
                    .unwrap();
                    // A reused batch label does not make this source a manifest member.
                    state.events.get_mut(&event).unwrap().automatic_batch_id =
                        Some(plan.id.clone());
                    (state.deliveries[&id].clone(), state.events[&event].clone())
                })
                .unwrap();
            queue
                .retire_replaced_bindings(&store, &bus, REQUESTER, "thread", 8, now)
                .unwrap();
            assert!(queue.staged(REQUESTER).unwrap().is_none());
            assert!(!bus.reserved_for_automatic_batch(REQUESTER, "note").unwrap());
            assert!(bus.owner_delivery_blocked(REQUESTER, "note").unwrap());
            let state = store.shared().unwrap();
            assert_eq!(
                plan.result_event(&state, plan.results().next().unwrap())
                    .unwrap()
                    .held_runtime_generation,
                Some(7)
            );
            assert_eq!(state.events[&unrelated_event.event_id], unrelated_event);
            assert_eq!(
                state.deliveries[&unrelated_delivery.delivery_id],
                unrelated_delivery
            );
            assert!(
                crate::asks::ledger::dismiss_input_id(&store, REQUESTER, &result_id, now).unwrap()
            );
            assert!(
                bus.owner_discard("note", None, "discarded by owner", false, now)
                    .unwrap()
                    .unwrap()
                    .changed
            );
            drop(bus);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        assert!(queue
            .publish(
                &store,
                &bus,
                REQUESTER,
                "thread",
                7,
                now,
                |method, payload| {
                    assert_eq!(method, SUBMIT_METHOD);
                    assert_eq!(payload["messages"][0]["message"]["id"], "note");
                    assert!(bus.owner_delivery_blocked(REQUESTER, "note").unwrap());
                    assert!(bus.reserved_for_automatic_batch(REQUESTER, "note").unwrap());
                    assert!(!bus.owner_carried(REQUESTER, "note").unwrap());
                    assert_eq!(store.shared().unwrap().events.len(), 1);
                    anyhow::bail!("response lost")
                }
            )
            .is_err());
        let reopened = AutomaticInputQueue::at_management_root(&management);
        for proved in 0..=2 {
            let poll_at = now + chrono::Duration::seconds(2 * proved as i64);
            let outcome = reopened
                .publish(
                    &store,
                    &bus,
                    REQUESTER,
                    "thread",
                    7,
                    poll_at,
                    |method, _| {
                        assert_eq!(method, STATUS_METHOD);
                        Ok(response(&plan, proved))
                    },
                )
                .unwrap();
            assert_eq!(outcome, Outcome::NoPublication);
            assert_eq!(
                bus.message_state("note").unwrap().unwrap().0,
                if proved > 0 { "delivered" } else { "pending" }
            );
            assert_eq!(
                store
                    .shared()
                    .unwrap()
                    .events
                    .values()
                    .next()
                    .unwrap()
                    .acknowledged,
                proved == 2
            );
        }
        assert!(reopened.staged(REQUESTER).unwrap().is_none());
        assert!(reopened
            .next_reconciliation(REQUESTER, "thread", 7, now)
            .unwrap()
            .is_none());
        drop(bus);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepted_idle_note_does_not_hold_the_next_publication() {
        let root = std::env::temp_dir().join(format!("cutex-auto-idle-{}", uuid::Uuid::new_v4()));
        let management = root.join("management-v2");
        let store = AskStore::at_runtime(&root);
        let bus = AgentBusMessageRepository::open(&management).unwrap();
        let queue = AutomaticInputQueue::at_management_root(&management);
        let now = Utc::now();
        let plan = |id, window_id| Publication {
            id: format!("ordered-auto-{id}"),
            session: REQUESTER.into(),
            window_id,
            members: vec![Member {
                envelope: envelope(id, Delivery::Passive),
                origin: Origin::Bus {
                    canonical_digest: queue_note(&bus, id, now),
                },
            }],
            trigger: None,
            notes_at_start: true,
            rides_start: false,
            result_budget: None,
        };
        let older = plan("old-note", 1);
        queue.stage(&older).unwrap();
        assert_eq!(
            queue
                .publish(
                    &store,
                    &bus,
                    REQUESTER,
                    "thread",
                    7,
                    now,
                    |method, payload| {
                        assert_eq!(method, SUBMIT_METHOD);
                        assert_eq!(payload["notesAtStart"], true);
                        let mut accepted = response(&older, 0);
                        accepted["statuses"][0]["deliveryState"] = json!("pending");
                        Ok(accepted)
                    }
                )
                .unwrap(),
            Outcome::WaitingForStart
        );
        assert_eq!(bus.message_state("old-note").unwrap().unwrap().0, "pending");

        let mut newer = plan("new-note", 2);
        newer.rides_start = true;
        let lane = user_input::session_input_lock(REQUESTER).unwrap();
        let owner_reservation = lane.lock().unwrap();
        assert_eq!(
            queue
                .publish_with(
                    &store,
                    &bus,
                    REQUESTER,
                    "thread",
                    7,
                    now,
                    true,
                    || Ok(Some(newer.clone())),
                    |method, _| {
                        assert_eq!(method, SUBMIT_METHOD);
                        Ok(response(&newer, 1))
                    }
                )
                .unwrap(),
            Outcome::Settled
        );
        drop(owner_reservation);
        assert_eq!(
            queue
                .publish(&store, &bus, REQUESTER, "thread", 7, now, |method, _| {
                    assert_eq!(method, STATUS_METHOD);
                    Ok(response(&older, 1))
                })
                .unwrap(),
            Outcome::NoPublication
        );
        assert_eq!(
            bus.message_state("old-note").unwrap().unwrap().0,
            "delivered"
        );
        assert!(queue
            .next_reconciliation(REQUESTER, "thread", 7, now)
            .unwrap()
            .is_none());

        let dismissed = plan("dismissed-note", 3);
        queue.stage(&dismissed).unwrap();
        assert_eq!(
            queue
                .publish(&store, &bus, REQUESTER, "thread", 7, now, |method, _| {
                    assert_eq!(method, SUBMIT_METHOD);
                    let mut accepted = response(&dismissed, 0);
                    accepted["statuses"][0]["deliveryState"] = json!("pending");
                    Ok(accepted)
                })
                .unwrap(),
            Outcome::WaitingForStart
        );
        assert!(bus
            .reserved_for_automatic_batch(REQUESTER, "dismissed-note")
            .unwrap());
        assert!(
            bus.owner_discard("dismissed-note", None, "discarded by owner", false, now)
                .unwrap()
                .unwrap()
                .changed
        );
        assert!(!bus
            .reserved_for_automatic_batch(REQUESTER, "dismissed-note")
            .unwrap());
        assert_eq!(
            queue
                .publish(&store, &bus, REQUESTER, "thread", 7, now, |_, _| {
                    panic!("fully disposed publication must settle without a runtime call")
                })
                .unwrap(),
            Outcome::NoPublication
        );
        assert!(queue
            .next_reconciliation(REQUESTER, "thread", 7, now)
            .unwrap()
            .is_none());
        drop(bus);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
