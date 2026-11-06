//! r274.1: the native timeline item of records that are already `inContext`
//! with `timelineItemId = null` (written before the writers bound it; 054
//! §3.2: clients join native items and records on `timelineItemId`, and show
//! a tail row only for a record without a native item, so such a record is
//! rendered twice).
//!
//! Run once by the settle thread after it starts (and only while
//! `management.inputLedger.enabled` is on, like every ledger write), one
//! batch of [`BATCH`] ledger rows per pass (051: no whole-state copy, no host
//! fence, no long lock):
//! - the ledger is paged by primary key (`inContext` timeline rows only);
//! - the item is derived from stored native evidence only, read once per
//!   batch from the shared snapshots: a delegation result / question / status
//!   from its `DeliveryRecord.event_id` → that event's `native_message_id`
//!   ([`crate::asks::ledger::backfill_item`]); a delegation request (target
//!   side) from the attempt that started in the record's turn → that
//!   attempt's `native_message_id` ([`crate::asks::ledger::backfill_request_item`]);
//!   a bus message (agent / peer
//!   message, alarm, notice, Claude Task, job notice) from its delivered
//!   receipt's `responseItemId` (or its message id for a Claude context
//!   injection). Owner records carry no stored native item id (their
//!   desktop join is the clientId) and stay as they are;
//! - each record is bound in its own small ledger transaction
//!   ([`il::InputLedger::bind_timeline_item`]: CAS on the `rev` read, normal
//!   `rev` / `stateRevision` bump, `cutex/input/stateChanged` published).
//!   Nothing is sent, retried, recreated or moved back: state, surface,
//!   `inputId`, attempt and evidence stay.
//!
//! Idempotent: a bound record is no longer a candidate. A record whose item
//! cannot be derived is left alone and counted (`notDerivable`).

use super::input_ledger::{self as il, InputKind, InputLedger, InputRecord, InputState, LedgerError, Surface};

/// Ledger rows read per pass.
pub const BATCH: usize = 200;
/// Log name of the backfill summary.
pub const LOG: &str = "cutex-input-ledger-timeline-backfill";

/// Kinds whose records are bound: every kind that can be on the timeline
/// in context (the delegation request too: its native item is its
/// attempt's message, never the `askId` the desktop could join on).
pub fn bindable(_kind: InputKind) -> bool {
    true
}

fn is_delegation(kind: InputKind) -> bool {
    matches!(kind, InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus)
}

fn is_bus(kind: InputKind) -> bool {
    matches!(kind, InputKind::AgentMessage | InputKind::PeerMessage | InputKind::Alarm | InputKind::SystemNotice)
}

/// A record the backfill looks at: `inContext`, on the timeline, unbound.
pub fn candidate(record: &InputRecord) -> bool {
    record.state == InputState::InContext
        && record.surface == Surface::Timeline
        && record.timeline_item_id.is_none()
        && bindable(record.kind)
}

/// Stored native evidence (a trait, so the rules test without the stores).
pub trait NativeEvidence {
    /// `(item, event id)` per delegation record (same order), `None`: not
    /// derivable.
    fn delegations(&self, records: &[&InputRecord]) -> Vec<Option<(String, String)>>;
    /// The item per delegation request record (same order), `None`: not
    /// derivable.
    fn requests(&self, records: &[&InputRecord]) -> Vec<Option<String>>;
    /// The item per bus record (same order), `None`: not derivable.
    fn bus(&self, records: &[&InputRecord]) -> Vec<Option<String>>;
}

/// The live stores: one shared AskStore snapshot projection and one
/// agent-bus read per batch.
pub struct Stores;

impl NativeEvidence for Stores {
    fn delegations(&self, records: &[&InputRecord]) -> Vec<Option<(String, String)>> {
        if records.is_empty() {
            return Vec::new();
        }
        let derived = crate::asks::store::AskStore::open_default().and_then(|store| {
            store.project_snapshot(|state| {
                records.iter().map(|record| crate::asks::ledger::backfill_item(state, record)).collect::<Vec<_>>()
            })
        });
        derived.unwrap_or_else(|error| {
            eprintln!("{LOG}: asks store: {error:#}");
            vec![None; records.len()]
        })
    }

    fn requests(&self, records: &[&InputRecord]) -> Vec<Option<String>> {
        if records.is_empty() {
            return Vec::new();
        }
        let derived = crate::asks::store::AskStore::open_default().and_then(|store| {
            store.project_snapshot(|state| {
                records.iter().map(|record| crate::asks::ledger::backfill_request_item(state, record)).collect::<Vec<_>>()
            })
        });
        derived.unwrap_or_else(|error| {
            eprintln!("{LOG}: asks store: {error:#}");
            vec![None; records.len()]
        })
    }

    fn bus(&self, records: &[&InputRecord]) -> Vec<Option<String>> {
        if records.is_empty() {
            return Vec::new();
        }
        let ids: Vec<String> = records.iter().map(|record| record.input_id.clone()).collect();
        let items = super::agent_bus_state::agent_bus_message_repository().and_then(|repository| repository.input_ledger_items(&ids));
        match items {
            Ok(items) => records
                .iter()
                .map(|record| {
                    items
                        .iter()
                        .find(|(id, owner, _)| *id == record.input_id && *owner == record.cutex_session_id)
                        .and_then(|(_, _, item)| item.clone())
                })
                .collect(),
            Err(error) => {
                eprintln!("{LOG}: agent-bus store: {error:#}");
                vec![None; records.len()]
            }
        }
    }
}

/// What one or more batches did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Candidates seen.
    pub candidates: usize,
    /// Bound now.
    pub fixed: usize,
    /// Left unbound: no reliable stored native evidence.
    pub not_derivable: usize,
    /// Of `not_derivable`: owner records (no stored native item id; the
    /// desktop joins them on the clientId).
    pub owner: usize,
    /// Of `not_derivable`: the record changed meanwhile or the ledger
    /// refused the binding (another batch, already bound).
    pub refused: usize,
}

impl Counts {
    pub fn add(&mut self, other: Counts) {
        self.candidates += other.candidates;
        self.fixed += other.fixed;
        self.not_derivable += other.not_derivable;
        self.owner += other.owner;
        self.refused += other.refused;
    }
}

/// Bind the candidates among `page` (one ledger transaction each). Never
/// changes anything but `timelineItemId`.
pub fn bind_page(ledger: &InputLedger, page: &[InputRecord], evidence: &dyn NativeEvidence) -> Counts {
    let candidates: Vec<&InputRecord> = page.iter().filter(|record| candidate(record)).collect();
    let mut counts = Counts { candidates: candidates.len(), ..Counts::default() };
    let delegations: Vec<&InputRecord> = candidates.iter().copied().filter(|record| is_delegation(record.kind)).collect();
    let requests: Vec<&InputRecord> = candidates.iter().copied().filter(|record| record.kind == InputKind::DelegationRequest).collect();
    let bus: Vec<&InputRecord> = candidates.iter().copied().filter(|record| is_bus(record.kind)).collect();
    let mut derived: Vec<(&InputRecord, String, Option<String>)> = Vec::new();
    for (record, item) in delegations.iter().zip(evidence.delegations(&delegations)) {
        match item {
            Some((item, event)) => derived.push((record, item, Some(event))),
            None => counts.not_derivable += 1,
        }
    }
    for (record, item) in requests.iter().zip(evidence.requests(&requests)) {
        match item {
            Some(item) => derived.push((record, item, None)),
            None => counts.not_derivable += 1,
        }
    }
    for (record, item) in bus.iter().zip(evidence.bus(&bus)) {
        match item {
            Some(item) => derived.push((record, item, None)),
            None => counts.not_derivable += 1,
        }
    }
    let owner = candidates.iter().filter(|record| record.kind == InputKind::Owner).count();
    counts.owner += owner;
    counts.not_derivable += owner;
    for (record, item, batch) in derived {
        let bound = ledger.bind_timeline_item(
            &record.cutex_session_id,
            &record.input_id,
            il::Expect::rev(record.rev),
            &item,
            batch.as_deref(),
        );
        match bound {
            Ok(transition) => {
                if let Err(error) = il::publish(&transition) {
                    eprintln!("{LOG}: publish {}/{}: {error:#}", record.cutex_session_id, record.input_id);
                }
                if transition.changed {
                    counts.fixed += 1;
                }
            }
            Err(LedgerError::Conflict { .. } | LedgerError::Invalid(_) | LedgerError::NotFound) => {
                counts.not_derivable += 1;
                counts.refused += 1;
            }
            Err(error) => {
                eprintln!("{LOG}: {}/{}: {error}", record.cutex_session_id, record.input_id);
                counts.not_derivable += 1;
                counts.refused += 1;
            }
        }
    }
    counts
}

/// The backfill's progress across settle passes.
#[derive(Debug, Default)]
pub struct Backfill {
    /// `None`: done. `Some(None)`: from the start.
    cursor: Option<Option<(String, String)>>,
    counts: Counts,
}

impl Backfill {
    pub fn start() -> Self {
        Self { cursor: Some(None), counts: Counts::default() }
    }

    pub fn done(&self) -> bool {
        self.cursor.is_none()
    }

    #[cfg(test)]
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// One batch; logs the counts once the last batch is done.
    pub fn step(&mut self, ledger: &InputLedger, evidence: &dyn NativeEvidence) {
        let Some(cursor) = self.cursor.clone() else { return };
        let page = match ledger.in_context_page(cursor.as_ref(), BATCH) {
            Ok(page) => page,
            Err(error) => {
                eprintln!("{LOG}: read: {error}");
                self.cursor = None;
                return;
            }
        };
        self.counts.add(bind_page(ledger, &page, evidence));
        self.cursor = match page.last() {
            Some(last) if page.len() == BATCH => Some(Some((last.cutex_session_id.clone(), last.input_id.clone()))),
            _ => None,
        };
        if self.cursor.is_none() {
            let counts = self.counts;
            eprintln!(
                "{LOG}: fixed {} / not derivable {} (owner {}, refused {}) of {} inContext timeline record(s) without timelineItemId",
                counts.fixed, counts.not_derivable, counts.owner, counts.refused, counts.candidates
            );
        }
    }
}

#[cfg(test)]
#[path = "input_ledger_bind_tests.rs"]
mod tests;
