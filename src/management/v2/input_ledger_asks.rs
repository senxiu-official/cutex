//! Input ledger hooks owned by block C4 (delegations and leases; 054 §6).
//! The mapping, the writer queue, the lease view / release and the dismiss
//! live in [`crate::asks::ledger`] (next to the store hook); this module is
//! what the ledger routes and SessionDelivery (C5) call.

use serde_json::{json, Value};

use super::input_ledger::InputRecord;
use super::user_input::UserInputExecutionError;

fn store() -> Result<crate::asks::store::AskStore, String> {
    crate::asks::store::AskStore::open_default().map_err(|error| format!("{error:#}"))
}

/// Settle the delegation side before the ledger marks the record
/// `discarded{ownerDiscarded}` (owner decision 9): an interrupted /
/// `outcomeUnknown` delegation (delegation-de84ad) is closed without a
/// retry and without native IO; a pending result / question / status is
/// settled and never sent.
pub(crate) fn dismiss(record: &InputRecord) -> Result<bool, String> {
    let store = store()?;
    crate::asks::ledger::dismiss(&store, record, chrono::Utc::now())?;
    // Queued/in-flight deliveries are not withdrawn by the source operation.
    // Only its durable owner settlement authorizes metadata-tolerant completion.
    store.project_snapshot(|state| {
        record.kind != crate::management::v2::input_ledger::InputKind::DelegationRequest
            && state.deliveries.get(&record.input_id).is_some_and(|delivery|
                delivery.state == crate::asks::model::DeliveryState::Acknowledged
                    && delivery.settled_by.as_deref() == Some(crate::asks::ledger::DISMISSED))
    }).map_err(|error| format!("{error:#}"))
}

/// `cutex/input/lease/release {expectedLease}` (§2.14): compare-and-delete
/// of a `stale` lease. `{released: true, lease, records}`, or the error
/// codes `lease_not_stale` / `lease_changed`.
pub(crate) fn lease_release(session: &str, expected_lease: &Value) -> Option<Result<Value, UserInputExecutionError>> {
    let error = |code: &str, message: String, retryable: bool, details: Value| UserInputExecutionError {
        stage: "route".to_string(),
        code: code.to_string(),
        message,
        retryable,
        details,
        outcome_unknown: false,
    };
    let store = match store() {
        Ok(store) => store,
        Err(message) => return Some(Err(error("asks_unavailable", message, true, json!({})))),
    };
    Some(
        crate::asks::ledger::lease_release(&store, session, expected_lease, chrono::Utc::now())
            .map_err(|refused| error(refused.code, refused.message, refused.code == "asks_unavailable", refused.details)),
    )
}

/// 054 §2.6 `SessionDelivery.lease` of a session (for C5): `{holder,
/// turnId, since, runtimeGeneration, inputIds, reconcile{state,
/// lastCheckedAt, nextCheckAt, lastResult}}`, or `None` without a lease.
/// Reads the shared ask snapshot (no lock, no copy). The reconcile state is
/// the scheduler's, so it is only filled in the management process.
#[allow(dead_code)] // C5 builds SessionDelivery from it.
pub(crate) fn lease_view(session: &str) -> Option<Value> {
    let store = store().ok()?;
    crate::asks::ledger::lease_view(&store, session).ok().flatten()
}

/// r274.2 (owner 2026-11-03): passive result / question / status records
/// left from before cutex stopped creating them
/// ([`crate::asks::ledger::passive_record`]: `waiting{passive}`, dismiss
/// only, never delivered by cutex). Run once per engine start by the settle
/// thread (switch on only, like every ledger write), one page of
/// [`PASSIVE_BATCH`] unsettled records per pass: each candidate becomes
/// `discarded{superseded}` (compare-and-set on its `rev`; a record that
/// moved meanwhile is left alone). Ledger only: the asks item stays
/// readable by `delegation_wait` / `delegation_read`. Idempotent: a
/// discarded record is no longer unsettled. No timer.
#[derive(Debug, Default)]
pub struct PassiveNotices {
    /// `None`: done. `Some(None)`: from the start.
    cursor: Option<Option<(String, String)>>,
    pub found: usize,
    pub settled: usize,
}

/// Unsettled records read per pass.
pub const PASSIVE_BATCH: usize = 200;
/// Log name of the summary.
pub const PASSIVE_LOG: &str = "cutex-input-ledger-passive-notices";

impl PassiveNotices {
    pub fn start() -> Self {
        Self { cursor: Some(None), ..Self::default() }
    }

    pub fn done(&self) -> bool {
        self.cursor.is_none()
    }

    /// One page; logs the counts once the last page is done.
    pub fn step(&mut self, ledger: &super::input_ledger::InputLedger) {
        use super::input_ledger::{self as il, Expect, LedgerError, Next};
        let Some(cursor) = self.cursor.clone() else { return };
        let page = match ledger.unsettled_page(cursor.as_ref(), PASSIVE_BATCH) {
            Ok(page) => page,
            Err(error) => {
                eprintln!("{PASSIVE_LOG}: read: {error}");
                self.cursor = None;
                return;
            }
        };
        for record in page.iter().filter(|record| crate::asks::ledger::passive_record(record)) {
            self.found += 1;
            let next = Next::discarded(crate::asks::ledger::PASSIVE_DISCARDED);
            match ledger.transition(&record.cutex_session_id, &record.input_id, Expect::rev(record.rev), next) {
                Ok(transition) => {
                    if let Err(error) = il::publish(&transition) {
                        eprintln!("{PASSIVE_LOG}: publish {}/{}: {error:#}", record.cutex_session_id, record.input_id);
                    }
                    self.settled += usize::from(transition.changed);
                }
                Err(LedgerError::Conflict { .. } | LedgerError::ForbiddenEdge { .. } | LedgerError::NotFound) => {}
                Err(error) => eprintln!("{PASSIVE_LOG}: {}/{}: {error}", record.cutex_session_id, record.input_id),
            }
        }
        self.cursor = match page.last() {
            Some(last) if page.len() == PASSIVE_BATCH => Some(Some((last.cutex_session_id.clone(), last.input_id.clone()))),
            _ => None,
        };
        if self.cursor.is_none() {
            eprintln!("{PASSIVE_LOG}: discarded {} of {} passive delegation record(s) (never delivered)", self.settled, self.found);
        }
    }
}
