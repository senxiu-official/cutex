//! 054 §2.12 "lease 核对": the reconcile state of a bound admission lease
//! whose turn end the observer may have missed.
//!
//! r273 spaced native history reads of one lease to every 15 s. The input
//! ledger contract adds, per lease:
//! - (a) a backoff, 5 s → 10 → 20 → 60 s cap, between checks that could not
//!   prove the turn ended (never a read on every scheduler pass);
//! - (c) `stale` after [`STALE_AFTER_CHECKS`] such checks, or once the
//!   runtime has been offline for [`LEASE_OFFLINE_GRACE_SECS`]; only then may
//!   the owner release the lease (`cutex/input/lease/release`, §2.14).
//!
//! (b) — one turn instead of the whole history — is the runtime's
//! `read_turn`. The state is memory only, in the process that runs the
//! scheduler: after a restart a lease starts over (`waitingTurn`), which
//! errs on the side of checking again rather than offering a release.
//! Times are scheduler time (`now` of the pass), so tests stay
//! deterministic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

use super::model::{AdmissionLease, LeaseHolder};

/// Waits after the 1st, 2nd, 3rd and later unproven checks.
pub const BACKOFF_SECS: [i64; 4] = [5, 10, 20, 60];
/// Unproven checks before the lease is `stale`.
pub const STALE_AFTER_CHECKS: u32 = 10;
/// `leaseOfflineGraceSecs` (054 §2.12, same default as `stopOfflineGraceSecs`).
pub const LEASE_OFFLINE_GRACE_SECS: i64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileState {
    /// The turn is running (or just reserved): nothing to check yet.
    WaitingTurn,
    /// The turn is no longer active; checks run with backoff.
    Checking,
    /// Unprovable: the owner may release the lease.
    Stale,
}

impl ReconcileState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WaitingTurn => "waitingTurn",
            Self::Checking => "checking",
            Self::Stale => "stale",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconcile {
    pub state: ReconcileState,
    /// Checks in a row that could not prove the turn ended.
    pub failures: u32,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub next_check_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub offline_since: Option<DateTime<Utc>>,
    identity: Identity,
}

/// What makes a lease the same lease: a replaced or rebound lease starts over.
type Identity = (String, String, u64, Option<String>, String);
type Key = (PathBuf, String);

static TABLE: Mutex<BTreeMap<Key, Reconcile>> = Mutex::new(BTreeMap::new());

fn identity(lease: &AdmissionLease) -> Identity {
    let holder = match &lease.holder {
        LeaseHolder::Ask { ask_id, attempt } => format!("ask:{ask_id}#{attempt}"),
        LeaseHolder::Event { event_id } => format!("event:{event_id}"),
        LeaseHolder::Owner => "owner".into(),
    };
    (holder, lease.native_message_id.clone(), lease.runtime_generation, lease.turn_id.clone(), lease.since.clone())
}

fn with_entry<T>(dir: &Path, lease: &AdmissionLease, update: impl FnOnce(&mut Reconcile) -> T) -> T {
    let mut table = TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (dir.to_path_buf(), lease.cutex_session_id.clone());
    let identity = identity(lease);
    let entry = table.entry(key).or_insert_with(|| fresh(identity.clone()));
    if entry.identity != identity {
        *entry = fresh(identity);
    }
    let before = visible(entry);
    let result = update(entry);
    let changed = visible(entry) != before;
    drop(table);
    // 054 §2.6: `lease.reconcile` is part of SessionDelivery (C5 publishes
    // `cutex/session/deliveryChanged` on a change).
    if changed && contract() {
        crate::management::v2::input_ledger_settle::mark_dirty(&lease.cutex_session_id);
    }
    result
}

/// What `lease.reconcile` shows of an entry.
fn visible(entry: &Reconcile) -> (ReconcileState, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<String>) {
    (entry.state, entry.last_checked_at, entry.next_check_at, entry.last_result.clone())
}

fn fresh(identity: Identity) -> Reconcile {
    Reconcile {
        state: ReconcileState::WaitingTurn,
        failures: 0,
        last_checked_at: None,
        next_check_at: None,
        last_result: None,
        offline_since: None,
        identity,
    }
}

/// Wait after `failures` unproven checks.
pub fn backoff(failures: u32) -> Duration {
    let index = (failures.max(1) as usize - 1).min(BACKOFF_SECS.len() - 1);
    Duration::seconds(BACKOFF_SECS[index])
}

/// r273.1 spacing, kept while `management.inputLedger.enabled` is off (the
/// switch gates every 054 behaviour change).
pub const LEGACY_SPACING_SECS: i64 = 15;

#[cfg(test)]
thread_local! {
    static CONTRACT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Tests: run `body` with the 054 schedule on (or off) on this thread only.
#[cfg(test)]
pub(crate) fn with_contract<T>(on: bool, body: impl FnOnce() -> T) -> T {
    CONTRACT.with(|cell| cell.set(Some(on)));
    let result = body();
    CONTRACT.with(|cell| cell.set(None));
    result
}

fn contract() -> bool {
    #[cfg(test)]
    if let Some(on) = CONTRACT.with(std::cell::Cell::get) {
        return on;
    }
    crate::management::v2::input_ledger::enabled()
}

/// Wait before the next check: the 054 backoff, or r273.1's fixed spacing
/// with the switch off.
fn next_wait(failures: u32) -> Duration {
    if contract() { backoff(failures) } else { Duration::seconds(LEGACY_SPACING_SECS) }
}

/// The turn is (still) active or was just reserved: no checks, and an
/// earlier unproven streak no longer counts.
pub fn waiting_turn(dir: &Path, lease: &AdmissionLease, result: &str) {
    with_entry(dir, lease, |entry| {
        entry.state = ReconcileState::WaitingTurn;
        entry.failures = 0;
        entry.next_check_at = None;
        entry.offline_since = None;
        entry.last_result = Some(result.to_string());
    });
}

/// The bound turn is the runtime's active turn. Normally nothing to check
/// (`waitingTurn{turnActive}`); but when the turn went quiet and its stall
/// checks could not read it (`app_server::turn_stall`, r274 S18), each such
/// read is an unproven check: [`STALE_AFTER_CHECKS`] in a row make the lease
/// `stale` (§2.12 c) instead of waiting for ever.
pub fn active_turn(dir: &Path, lease: &AdmissionLease) {
    let unconfirmed = lease.turn_id.as_deref().and_then(|turn| {
        crate::app_server::turn_stall::unconfirmed(&lease.cutex_session_id, lease.runtime_generation, turn)
    });
    let Some(stall) = unconfirmed else {
        waiting_turn(dir, lease, "turnActive");
        return;
    };
    unconfirmed_turn(dir, lease, &stall);
}

fn unconfirmed_turn(dir: &Path, lease: &AdmissionLease, stall: &crate::app_server::turn_stall::Unconfirmed) {
    with_entry(dir, lease, |entry| {
        entry.failures = stall.failures;
        entry.last_checked_at = Some(stall.last_checked_at);
        entry.next_check_at = Some(stall.next_check_at);
        entry.offline_since = None;
        entry.last_result = Some(stall.result.clone());
        entry.state = if stall.failures >= STALE_AFTER_CHECKS { ReconcileState::Stale } else { ReconcileState::Checking };
    });
}

/// The runtime is offline: nothing can be checked. Past the grace the lease
/// is `stale`. Returns whether it is stale.
pub fn offline(dir: &Path, lease: &AdmissionLease, now: DateTime<Utc>) -> bool {
    with_entry(dir, lease, |entry| {
        let since = *entry.offline_since.get_or_insert(now);
        entry.last_result = Some("runtimeOffline".into());
        if now - since >= Duration::seconds(LEASE_OFFLINE_GRACE_SECS) {
            entry.state = ReconcileState::Stale;
        }
        entry.state == ReconcileState::Stale
    })
}

/// The runtime is reachable again: the offline grace starts over.
pub fn online(dir: &Path, lease: &AdmissionLease) {
    with_entry(dir, lease, |entry| entry.offline_since = None);
}

/// Whether a check (one native read of one turn) is due now. A due lease
/// that is not stale is `checking`.
pub fn due(dir: &Path, lease: &AdmissionLease, now: DateTime<Utc>) -> bool {
    with_entry(dir, lease, |entry| {
        if entry.state == ReconcileState::WaitingTurn {
            entry.state = ReconcileState::Checking;
        }
        entry.next_check_at.is_none_or(|at| now >= at)
    })
}

/// A check that could not prove the turn ended (`result`: why). Schedules
/// the next one with backoff; [`STALE_AFTER_CHECKS`] in a row make it stale.
pub fn unproven(dir: &Path, lease: &AdmissionLease, now: DateTime<Utc>, result: &str) {
    with_entry(dir, lease, |entry| {
        entry.failures = entry.failures.saturating_add(1);
        entry.last_checked_at = Some(now);
        entry.next_check_at = Some(now + next_wait(entry.failures));
        entry.last_result = Some(result.to_string());
        if entry.failures >= STALE_AFTER_CHECKS {
            entry.state = ReconcileState::Stale;
        } else if entry.state != ReconcileState::Stale {
            entry.state = ReconcileState::Checking;
        }
    });
}

/// The lease is gone (released, finalized or replaced).
pub fn forget(dir: &Path, session: &str) {
    let removed = TABLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(dir.to_path_buf(), session.to_string()));
    if removed.is_some() && contract() {
        crate::management::v2::input_ledger_settle::mark_dirty(session);
    }
}

/// Drop entries of this store whose session holds no lease any more.
pub fn retain(dir: &Path, leased: impl Fn(&str) -> bool) {
    TABLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(path, session), _| path != dir || leased(session));
}

/// The reconcile state of exactly this lease (`None`: never examined).
pub fn view(dir: &Path, lease: &AdmissionLease) -> Option<Reconcile> {
    let table = TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    table
        .get(&(dir.to_path_buf(), lease.cutex_session_id.clone()))
        .filter(|entry| entry.identity == identity(lease))
        .cloned()
}

pub fn is_stale(dir: &Path, lease: &AdmissionLease) -> bool {
    view(dir, lease).is_some_and(|entry| entry.state == ReconcileState::Stale)
}

/// Tests: make this lease stale without waiting for ten checks.
#[cfg(test)]
pub(crate) fn force_stale(dir: &Path, lease: &AdmissionLease) {
    with_entry(dir, lease, |entry| entry.state = ReconcileState::Stale);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(session: &str, turn: Option<&str>) -> AdmissionLease {
        AdmissionLease {
            cutex_session_id: session.into(),
            holder: LeaseHolder::Event { event_id: "aev-1".into() },
            native_message_id: "m".into(),
            runtime_generation: 1,
            turn_id: turn.map(str::to_string),
            since: "2026-11-02T00:00:00Z".into(),
        }
    }

    #[test]
    fn backoff_is_five_ten_twenty_then_sixty() {
        let waits: Vec<i64> = (1..=6).map(|n| backoff(n).num_seconds()).collect();
        assert_eq!(waits, [5, 10, 20, 60, 60, 60]);
    }

    #[test]
    fn unproven_checks_back_off_and_turn_stale_after_ten() {
        let dir = PathBuf::from(format!("lease-reconcile-{}", uuid::Uuid::new_v4()));
        let lease = lease("s", Some("t"));
        let mut now = Utc::now();
        let mut checks = 0;
        let mut waits = Vec::new();
        // 30 minutes of 1 s scheduler passes: only due passes read.
        CONTRACT.with(|cell| cell.set(Some(true)));
        for _ in 0..1800 {
            if due(&dir, &lease, now) {
                checks += 1;
                unproven(&dir, &lease, now, "turnUnreadable");
                let entry = view(&dir, &lease).unwrap();
                waits.push((entry.next_check_at.unwrap() - now).num_seconds());
                if checks == 10 {
                    assert_eq!(entry.state, ReconcileState::Stale);
                }
            }
            now += Duration::seconds(1);
        }
        assert_eq!(&waits[..5], &[5, 10, 20, 60, 60]);
        assert!(checks < 40, "{checks} checks in 30 min: a 60 s cap");
        assert!(is_stale(&dir, &lease));
        // A rebound (other turn) lease starts over.
        let rebound = lease_with_turn(&lease, "t2");
        assert!(!is_stale(&dir, &rebound));
        assert_eq!(view(&dir, &rebound), None);
        forget(&dir, "s");
        CONTRACT.with(|cell| cell.set(None));
    }

    fn lease_with_turn(lease: &AdmissionLease, turn: &str) -> AdmissionLease {
        let mut other = lease.clone();
        other.turn_id = Some(turn.into());
        other
    }

    #[test]
    fn an_active_turn_its_stall_checks_cannot_read_goes_stale_after_ten() {
        use crate::app_server::turn_stall::{publish, unpublish, Unconfirmed};
        let dir = PathBuf::from(format!("lease-reconcile-{}", uuid::Uuid::new_v4()));
        let session = format!("s-{}", uuid::Uuid::new_v4());
        let mut lease = lease(&session, Some("t"));
        lease.runtime_generation = 7;
        let now = Utc::now();
        // Active and readable (or never stalled): waiting, never stale.
        active_turn(&dir, &lease);
        assert_eq!(view(&dir, &lease).unwrap().state, ReconcileState::WaitingTurn);
        let stall = |failures| Unconfirmed {
            generation: 7, turn: "t".into(), failures, last_checked_at: now,
            next_check_at: now + Duration::seconds(60), result: "turnUnreadable".into(),
        };
        publish(&session, stall(3));
        active_turn(&dir, &lease);
        let entry = view(&dir, &lease).unwrap();
        assert_eq!((entry.state, entry.failures, entry.last_result.as_deref()), (ReconcileState::Checking, 3, Some("turnUnreadable")));
        assert_eq!(entry.next_check_at, Some(now + Duration::seconds(60)), "the stall check's schedule is visible");
        publish(&session, stall(STALE_AFTER_CHECKS));
        active_turn(&dir, &lease);
        assert!(is_stale(&dir, &lease));
        // Another turn's streak does not count.
        let other = lease_with_turn(&lease, "t2");
        active_turn(&dir, &other);
        assert_eq!(view(&dir, &other).unwrap().state, ReconcileState::WaitingTurn);
        // Activity (the streak is cleared): waiting again.
        unpublish(&session, 7);
        active_turn(&dir, &lease);
        assert_eq!(view(&dir, &lease).unwrap().state, ReconcileState::WaitingTurn);
        forget(&dir, &session);
    }

    #[test]
    fn offline_past_the_grace_is_stale_and_online_restarts_the_grace() {
        let dir = PathBuf::from(format!("lease-reconcile-{}", uuid::Uuid::new_v4()));
        let lease = lease("s", Some("t"));
        let now = Utc::now();
        assert!(!offline(&dir, &lease, now));
        assert!(!offline(&dir, &lease, now + Duration::seconds(LEASE_OFFLINE_GRACE_SECS - 1)));
        online(&dir, &lease);
        assert!(!offline(&dir, &lease, now + Duration::seconds(LEASE_OFFLINE_GRACE_SECS + 1)), "grace restarted");
        assert!(offline(&dir, &lease, now + Duration::seconds(2 * LEASE_OFFLINE_GRACE_SECS + 2)));
        assert_eq!(view(&dir, &lease).unwrap().state.as_str(), "stale");
        // The turn shows as active again: no longer stale.
        waiting_turn(&dir, &lease, "turnActive");
        assert_eq!(view(&dir, &lease).unwrap().state, ReconcileState::WaitingTurn);
        retain(&dir, |_| false);
        assert_eq!(view(&dir, &lease), None);
    }
}
