//! Late adoption of app-server runtimes that another cutex process started.
//!
//! Each process keeps its own in-memory app-server connections. `cutex
//! management serve` adopts every live runtime once at startup; a runtime that
//! a different process launches afterwards (Agent Management `agent_create` /
//! `member_runtime` run inside `cutex agent serve`) was never connected to
//! Management, so `/v2/sessions` reported it offline and submits failed with
//! `app_server_unavailable` / `session_offline` until the next Management
//! restart. Management therefore keeps observing the durable store (like the
//! Claude observer does) and adopts such a runtime once its claim has settled.
//!
//! The launching process stays connected after Management adopted the
//! runtime (Agent Management observes it there), so both receive every native
//! event; `single_writer` hands the journal over at an emission-time cut so
//! each event is journaled once.
//!
//! This module holds the pure planning part: which records are candidates,
//! when one has settled, and the retry backoff after a failed adoption. The
//! adoption itself reuses the startup recovery path (`cli_app`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::session::model::CutexSessionRecord;

/// How long a durable runtime claim must stay unchanged and unconnected
/// before Management adopts it. A launch in this process connects right after
/// it publishes the binding; the grace keeps the observer from racing it.
pub const SETTLE: Duration = Duration::from_secs(2);
/// Interval of the observer loop. An unchanged store costs one small query.
pub const TICK: Duration = Duration::from_secs(2);
const FIRST_RETRY: Duration = Duration::from_secs(10);
const MAX_RETRY: Duration = Duration::from_secs(600);

/// One durable runtime occurrence. Any changed field is a different
/// occurrence (restart, relaunch, new registration) and resets settle and
/// backoff state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeOccurrence {
    pub cutex_session_id: String,
    pub runtime_generation: u64,
    pub pid: u32,
    pub started_at: String,
    pub runtime_agent_id: String,
}

/// The settled occurrence a local, active, non-Claude record claims, or
/// `None` when there is nothing to adopt: no binding, a pending launch claim
/// (its launcher still owns it), no runtime agent identity, or a binding
/// process that is not running. Never probes a retired or remote record.
pub fn adoption_candidate(
    record: &CutexSessionRecord,
    current_host: &str,
    process_is_live: impl Fn(&CutexSessionRecord, u32) -> bool,
) -> Option<RuntimeOccurrence> {
    if crate::agent_management::agent_execution_blocked(&record)
        || record.claude_session.is_some()
        || !crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, current_host)
        || record
            .app_server_launch_claim_id
            .as_deref()
            .is_some_and(|claim| !claim.trim().is_empty())
    {
        return None;
    }
    let binding = record.app_server_runtime.as_ref()?;
    let runtime_agent_id = record
        .current_runtime_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    if binding.pid == 0 || !process_is_live(record, binding.pid) {
        return None;
    }
    Some(RuntimeOccurrence {
        cutex_session_id: record.cutex_session_id.clone(),
        runtime_generation: record.runtime_generation,
        pid: binding.pid,
        started_at: binding.started_at.clone(),
        runtime_agent_id: runtime_agent_id.to_string(),
    })
}

/// Whether this process's own connection entry for a session still accounts
/// for `occurrence`, so the observer must leave it alone. A live entry always
/// does (this process owns or observes it). A dead entry (the runtime it was
/// connected to exited or the connection dropped) does not cover the durable
/// occurrence of its own or a newer generation:
/// - newer (r273.1 O1): another process (the Agent Bus handling a Director
///   `online` after an `offline`) launched a new occurrence; before r273.1 any
///   entry counted, so such an agent stayed offline in Management until its
///   next restart;
/// - same (r274): only the observation link dropped while the runtime lives
///   on; before r274 the dead entry hid it, so Management never reconnected a
///   Codex agent by itself (only an owner start, an owner submit or an ask
///   did). The candidate still has to be a live, exact durable claim, and the
///   planner limits such reconnects ([`RECONNECT_INTERVAL`]).
///
/// Adoption replaces the dead entry (`connect_endpoint` drops a disconnected
/// runtime before connecting). A dead entry of a generation newer than the
/// durable claim still covers it (the store is behind this process).
pub fn local_connection_covers(
    local: Option<(bool, u64)>,
    occurrence: &RuntimeOccurrence,
) -> bool {
    local.is_some_and(|(connected, generation)| {
        connected || generation > occurrence.runtime_generation
    })
}

/// At most one reconnect of the same occurrence per session in this window
/// (like `ManagementAskRuntime::reconnect`), so a link that keeps dropping
/// right after connecting cannot make the observer churn.
pub const RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

static CONNECTION_LOSSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A runtime connection of this process went down (the runtime manager calls
/// this whenever it marks one disconnected). The observer re-plans on the
/// next tick even though the session store did not change.
pub fn note_connection_lost() {
    CONNECTION_LOSSES.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Count of [`note_connection_lost`] calls in this process.
pub fn connection_losses() -> u64 {
    CONNECTION_LOSSES.load(std::sync::atomic::Ordering::Acquire)
}

/// After this process disconnected a runtime on purpose (stop, cutover,
/// failed launch cleanup), the runtime may live on for a moment while it is
/// being stopped; it must not be adopted again in that window.
pub const LOCAL_DISCONNECT_QUIET: Duration = Duration::from_secs(60);

static LOCAL_DISCONNECTS: std::sync::Mutex<Option<HashMap<String, Instant>>> =
    std::sync::Mutex::new(None);

/// Records that this process disconnected `cutex_session_id` on purpose.
pub fn note_local_disconnect(cutex_session_id: &str) {
    note_local_disconnect_at(cutex_session_id, Instant::now());
}

fn note_local_disconnect_at(cutex_session_id: &str, now: Instant) {
    let mut guard = LOCAL_DISCONNECTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = guard.get_or_insert_with(HashMap::new);
    map.retain(|_, at| now.saturating_duration_since(*at) < LOCAL_DISCONNECT_QUIET);
    map.insert(cutex_session_id.to_string(), now);
}

/// Whether this process disconnected `cutex_session_id` on purpose within
/// [`LOCAL_DISCONNECT_QUIET`].
pub fn disconnected_locally_since(cutex_session_id: &str, now: Instant) -> bool {
    LOCAL_DISCONNECTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|map| map.get(cutex_session_id))
        .is_some_and(|at| now.saturating_duration_since(*at) < LOCAL_DISCONNECT_QUIET)
}

#[derive(Debug)]
struct Failure {
    occurrence: RuntimeOccurrence,
    retry_at: Instant,
    backoff: Duration,
    detail: String,
}

/// Settle and retry state of the observer loop.
#[derive(Debug, Default)]
pub struct LateAdoptionPlanner {
    first_seen: HashMap<String, (RuntimeOccurrence, Instant)>,
    failures: HashMap<String, Failure>,
    /// Last adoption per session ([`RECONNECT_INTERVAL`]); pruned by age only.
    adopted_at: HashMap<String, (RuntimeOccurrence, Instant)>,
    /// When a waiting occurrence (settling, backoff, rate limit) is next due.
    wake_at: Option<Instant>,
    /// Store revision and [`connection_losses`] the last plan saw.
    last_seen: Option<(u64, u64)>,
}

impl LateAdoptionPlanner {
    /// Whether a later tick may adopt something even if the store does not
    /// change (an occurrence still settling or waiting for a retry).
    pub fn has_pending(&self) -> bool {
        !self.first_seen.is_empty()
    }

    /// Whether this tick has to plan: the store changed, a connection of this
    /// process went down, or a waiting occurrence is due. An unchanged tick
    /// costs nothing beyond reading the two counters (051: no per-tick scan
    /// while an occurrence waits out its grace, backoff or rate limit).
    pub fn should_plan(&mut self, store_revision: u64, connection_losses: u64, now: Instant) -> bool {
        let seen = Some((store_revision, connection_losses));
        let changed = self.last_seen != seen;
        self.last_seen = seen;
        changed || self.wake_at.is_some_and(|at| at <= now)
    }

    /// The occurrences to adopt now. `candidates` are this tick's
    /// [`adoption_candidate`]s; `is_known` says whether this process already
    /// holds (or is establishing) a connection that covers the occurrence
    /// (see [`local_connection_covers`]).
    pub fn plan(
        &mut self,
        candidates: Vec<RuntimeOccurrence>,
        now: Instant,
        is_known: impl Fn(&RuntimeOccurrence) -> bool,
    ) -> Vec<RuntimeOccurrence> {
        let wanted = candidates
            .into_iter()
            .filter(|occurrence| !is_known(occurrence))
            .map(|occurrence| (occurrence.cutex_session_id.clone(), occurrence))
            .collect::<HashMap<_, _>>();
        self.first_seen.retain(|id, (occurrence, _)| wanted.get(id) == Some(occurrence));
        self.failures
            .retain(|id, failure| wanted.get(id) == Some(&failure.occurrence));
        self.adopted_at
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < RECONNECT_INTERVAL);
        let mut due = Vec::new();
        let mut wake_at: Option<Instant> = None;
        for (id, occurrence) in wanted {
            let mut ready_at = self
                .first_seen
                .entry(id.clone())
                .or_insert_with(|| (occurrence.clone(), now))
                .1
                + SETTLE;
            if let Some(failure) = self.failures.get(&id) {
                ready_at = ready_at.max(failure.retry_at);
            }
            // The same occurrence again: its connection dropped after this
            // observer (re)connected it.
            if let Some((_, at)) = self.adopted_at.get(&id).filter(|(adopted, _)| adopted == &occurrence) {
                ready_at = ready_at.max(*at + RECONNECT_INTERVAL);
            }
            if ready_at > now {
                wake_at = Some(wake_at.map_or(ready_at, |wake| wake.min(ready_at)));
                continue;
            }
            due.push(occurrence);
        }
        self.wake_at = wake_at;
        due.sort_by(|left, right| left.cutex_session_id.cmp(&right.cutex_session_id));
        due
    }

    /// The occurrence is connected now (or needs nothing any more).
    pub fn adopted(&mut self, occurrence: &RuntimeOccurrence, now: Instant) {
        self.first_seen.remove(&occurrence.cutex_session_id);
        self.failures.remove(&occurrence.cutex_session_id);
        self.adopted_at
            .insert(occurrence.cutex_session_id.clone(), (occurrence.clone(), now));
    }

    /// Adoption failed: retry the same occurrence later (10 s, doubling, at
    /// most 10 min). Returns whether `detail` differs from the last failure of
    /// this occurrence, so the caller logs each distinct failure once.
    pub fn failed(&mut self, occurrence: &RuntimeOccurrence, detail: String, now: Instant) -> bool {
        let id = occurrence.cutex_session_id.clone();
        let previous = self
            .failures
            .remove(&id)
            .filter(|failure| &failure.occurrence == occurrence);
        let backoff = previous
            .as_ref()
            .map(|failure| (failure.backoff * 2).min(MAX_RETRY))
            .unwrap_or(FIRST_RETRY);
        let changed = previous.as_ref().is_none_or(|failure| failure.detail != detail);
        let retry_at = now + backoff;
        self.wake_at = Some(self.wake_at.map_or(retry_at, |wake| wake.min(retry_at)));
        self.failures.insert(
            id,
            Failure {
                occurrence: occurrence.clone(),
                retry_at,
                backoff,
                detail,
            },
        );
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::{CutexAppServerRuntimeBinding, CutexSessionArchiveState};

    fn binding(pid: u32) -> CutexAppServerRuntimeBinding {
        serde_json::from_value(serde_json::json!({
            "transport": "loopback_web_socket", "endpoint": "ws://127.0.0.1:1", "pid": pid,
            "runtime_dir": "/missing", "diagnostic_journal_path": "/missing/j", "schema_version": "v",
            "schema_sha256": "b".repeat(64), "started_at": "2026-10-14T16:30:47Z"
        }))
        .unwrap()
    }

    /// A Codex record as the Agent Bus process leaves it after `agent_create`:
    /// settled binding, runtime agent id, no launch claim.
    fn bus_created(id: &str, pid: u32) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            id.into(),
            Some("018e0000-0000-7000-8000-0000d0003353".into()),
            "devbox".into(),
            "/tmp/ws".into(),
            None,
            "2026-10-14T16:30:40Z".into(),
        )
        .unwrap();
        record.app_server_runtime = Some(binding(pid));
        record.runtime_pid = Some(pid);
        record.current_runtime_agent_id = Some("stock.46f8".into());
        record.runtime_generation = 1;
        record
    }

    fn occurrence(id: &str, pid: u32) -> RuntimeOccurrence {
        adoption_candidate(&bus_created(id, pid), "devbox", |_, _| true).unwrap()
    }

    #[test]
    fn only_settled_live_local_codex_claims_are_candidates() {
        let live = |_: &CutexSessionRecord, pid: u32| pid == 42;
        let record = bus_created("cutex.a", 42);
        let candidate = adoption_candidate(&record, "devbox", live).expect("candidate");
        assert_eq!(candidate.runtime_agent_id, "stock.46f8");
        assert_eq!((candidate.pid, candidate.runtime_generation), (42, 1));

        assert!(adoption_candidate(&bus_created("cutex.a", 7), "devbox", live).is_none(), "dead pid");
        assert!(adoption_candidate(&record, "other-host", live).is_none(), "remote");
        let mut claimed = record.clone();
        claimed.app_server_launch_claim_id = Some("claim".into());
        assert!(adoption_candidate(&claimed, "devbox", live).is_none(), "launch in flight");
        let mut anonymous = record.clone();
        anonymous.current_runtime_agent_id = Some(" ".into());
        assert!(adoption_candidate(&anonymous, "devbox", live).is_none(), "no runtime agent id");
        let mut unbound = record.clone();
        unbound.app_server_runtime = None;
        assert!(adoption_candidate(&unbound, "devbox", live).is_none(), "no binding");
        let mut claude = record.clone();
        claude.claude_session = Some(
            serde_json::from_value(serde_json::json!({
                "deployment": {"python": "/py", "adapter_source": "/a", "claude_cli": "/c",
                    "claude_home": "/h", "tui_binary": "/t"},
                "thread_id": "t", "state_dir": "/s", "model": "sonnet"
            }))
            .expect("claude session"),
        );
        assert!(adoption_candidate(&claude, "devbox", live).is_none(), "Claude has its own observer");
        let mut retired = record;
        retired.archive_state = CutexSessionArchiveState::Retired;
        retired.retired_at = Some("2026-10-14T17:00:00Z".into());
        assert!(
            adoption_candidate(&retired, "devbox", |_, _| panic!("retired records are not probed")).is_none()
        );
    }

    #[test]
    fn a_runtime_registered_after_startup_is_adopted_once_it_settled() {
        let mut planner = LateAdoptionPlanner::default();
        let start = Instant::now();
        let worker = occurrence("cutex.worker", 42);
        // First sighting: never adopted at once (an in-process launch may be
        // about to connect it).
        assert!(planner.plan(vec![worker.clone()], start, |_| false).is_empty());
        assert!(planner.has_pending());
        assert!(planner.plan(vec![worker.clone()], start + Duration::from_secs(1), |_| false).is_empty());
        assert_eq!(planner.plan(vec![worker.clone()], start + SETTLE, |_| false), vec![worker.clone()]);
        planner.adopted(&worker, start + SETTLE);
        assert!(!planner.has_pending());
        // Connected in this process: nothing to do, no state kept.
        assert!(planner.plan(vec![worker.clone()], start + SETTLE * 3, |o| o.cutex_session_id == "cutex.worker").is_empty());
        assert!(!planner.has_pending());
    }

    #[test]
    fn a_connection_owned_by_this_process_is_never_adopted() {
        let mut planner = LateAdoptionPlanner::default();
        let start = Instant::now();
        let director = occurrence("cutex.director", 42);
        for step in 0..5 {
            assert!(planner
                .plan(vec![director.clone()], start + SETTLE * step, |_| true)
                .is_empty());
        }
        assert!(!planner.has_pending());
    }

    #[test]
    fn a_dead_local_connection_does_not_hide_a_newer_occurrence() {
        // r273.1 O1: created by the Agent Bus (generation 1, adopted here),
        // taken offline and brought online again by the Bus (generation 2).
        // This process still holds the dead generation-1 entry.
        let mut relaunched = bus_created("cutex.worker", 43);
        relaunched.runtime_generation = 2;
        let relaunched = adoption_candidate(&relaunched, "devbox", |_, _| true).unwrap();
        assert!(!local_connection_covers(None, &relaunched));
        assert!(!local_connection_covers(Some((false, 1)), &relaunched), "dead older entry");
        assert!(!local_connection_covers(Some((false, 2)), &relaunched), "own generation, dead (r274)");
        assert!(local_connection_covers(Some((false, 3)), &relaunched), "a newer dead entry: the store is behind");
        assert!(local_connection_covers(Some((true, 1)), &relaunched), "a live entry is never replaced here");
        assert!(local_connection_covers(Some((true, 2)), &relaunched));

        let mut planner = LateAdoptionPlanner::default();
        let start = Instant::now();
        let dead_gen1 = |o: &RuntimeOccurrence| local_connection_covers(Some((false, 1)), o);
        assert!(planner.plan(vec![relaunched.clone()], start, dead_gen1).is_empty());
        assert_eq!(planner.plan(vec![relaunched.clone()], start + SETTLE, dead_gen1), vec![relaunched]);
    }

    #[test]
    fn a_changed_occurrence_settles_again() {
        let mut planner = LateAdoptionPlanner::default();
        let start = Instant::now();
        assert!(planner.plan(vec![occurrence("cutex.w", 42)], start, |_| false).is_empty());
        // Relaunched (new pid) before it settled: the grace starts over.
        let relaunched = occurrence("cutex.w", 43);
        assert!(planner.plan(vec![relaunched.clone()], start + SETTLE, |_| false).is_empty());
        assert_eq!(
            planner.plan(vec![relaunched.clone()], start + SETTLE * 2, |_| false),
            vec![relaunched]
        );
        // A vanished candidate leaves no state behind.
        assert!(planner.plan(Vec::new(), start + SETTLE * 3, |_| false).is_empty());
        assert!(!planner.has_pending());
    }

    #[test]
    fn failures_back_off_per_occurrence_and_log_each_distinct_detail_once() {
        let mut planner = LateAdoptionPlanner::default();
        let start = Instant::now();
        let worker = occurrence("cutex.worker", 42);
        planner.plan(vec![worker.clone()], start, |_| false);
        let at = start + SETTLE;
        assert_eq!(planner.plan(vec![worker.clone()], at, |_| false), vec![worker.clone()]);
        assert!(planner.failed(&worker, "no Ready receipt".into(), at));
        assert!(planner.plan(vec![worker.clone()], at + Duration::from_secs(9), |_| false).is_empty());
        let retry = at + FIRST_RETRY;
        assert_eq!(planner.plan(vec![worker.clone()], retry, |_| false), vec![worker.clone()]);
        assert!(!planner.failed(&worker, "no Ready receipt".into(), retry), "same detail: not logged again");
        assert!(planner.plan(vec![worker.clone()], retry + FIRST_RETRY, |_| false).is_empty(), "doubled");
        assert_eq!(
            planner.plan(vec![worker.clone()], retry + FIRST_RETRY * 2, |_| false),
            vec![worker.clone()]
        );
        // A new occurrence of the same session is tried after its own grace,
        // not after the old backoff.
        let relaunched = occurrence("cutex.worker", 43);
        let later = retry + FIRST_RETRY * 2;
        assert!(planner.plan(vec![relaunched.clone()], later, |_| false).is_empty());
        assert_eq!(planner.plan(vec![relaunched.clone()], later + SETTLE, |_| false), vec![relaunched]);
    }

    #[test]
    fn a_runtime_this_process_disconnected_stays_quiet_for_a_while() {
        let now = Instant::now();
        let id = "cutex.late-adoption-quiet-test";
        assert!(!disconnected_locally_since(id, now));
        note_local_disconnect_at(id, now);
        assert!(disconnected_locally_since(id, now + Duration::from_secs(59)));
        assert!(!disconnected_locally_since(id, now + LOCAL_DISCONNECT_QUIET));
        assert!(!disconnected_locally_since("cutex.other", now));
    }

    /// r274: the observation link to a live Codex runtime drops (same
    /// binding, same generation). The loop notices without a store change,
    /// reconnects after the grace, and a link that keeps dropping is
    /// reconnected at most once per [`RECONNECT_INTERVAL`].
    #[test]
    fn a_dropped_same_generation_connection_is_reconnected_rate_limited() {
        let worker = occurrence("cutex.worker", 42);
        let mut connected = true;
        let start = Instant::now();
        let mut planner = LateAdoptionPlanner::default();
        let covers = |connected: bool| move |o: &RuntimeOccurrence| local_connection_covers(Some((connected, 1)), o);
        // Live connection: an unchanged store and no loss never plans, and a
        // plan leaves it alone (no churn).
        assert!(planner.should_plan(7, 0, start));
        assert!(planner.plan(vec![worker.clone()], start, covers(connected)).is_empty());
        for tick in 1..10 {
            assert!(!planner.should_plan(7, 0, start + TICK * tick), "no per-tick scan");
        }
        assert!(planner.plan(vec![worker.clone()], start + SETTLE * 9, covers(connected)).is_empty());
        assert!(!planner.has_pending());

        // The link drops; the store does not change.
        connected = false;
        let dropped = start + Duration::from_secs(60);
        assert!(planner.should_plan(7, 1, dropped), "a connection loss re-plans");
        assert!(planner.plan(vec![worker.clone()], dropped, covers(connected)).is_empty(), "grace");
        assert!(!planner.should_plan(7, 1, dropped + TICK / 2));
        assert!(planner.should_plan(7, 1, dropped + SETTLE), "due after the grace");
        assert_eq!(planner.plan(vec![worker.clone()], dropped + SETTLE, covers(connected)), vec![worker.clone()]);
        let reconnected = dropped + SETTLE;
        planner.adopted(&worker, reconnected);
        connected = true;
        assert!(planner.plan(vec![worker.clone()], reconnected + TICK, covers(connected)).is_empty());

        // It drops again at once: at most one reconnect per interval.
        connected = false;
        let again = reconnected + TICK * 2;
        assert!(planner.should_plan(7, 2, again));
        assert!(planner.plan(vec![worker.clone()], again, covers(connected)).is_empty());
        assert!(planner.plan(vec![worker.clone()], again + SETTLE, covers(connected)).is_empty(), "rate limited");
        assert!(!planner.should_plan(7, 2, reconnected + RECONNECT_INTERVAL - TICK));
        assert!(planner.should_plan(7, 2, reconnected + RECONNECT_INTERVAL));
        assert_eq!(
            planner.plan(vec![worker.clone()], reconnected + RECONNECT_INTERVAL, covers(connected)),
            vec![worker.clone()]
        );
        // A new occurrence (relaunch) is not held back by the old one's limit.
        let relaunched = occurrence("cutex.worker", 43);
        planner.adopted(&worker, reconnected + RECONNECT_INTERVAL);
        let later = reconnected + RECONNECT_INTERVAL + TICK;
        assert!(planner.plan(vec![relaunched.clone()], later, |_| false).is_empty());
        assert_eq!(planner.plan(vec![relaunched.clone()], later + SETTLE, |_| false), vec![relaunched]);
    }

    #[test]
    fn a_failed_reconnect_wakes_the_loop_at_its_retry_only() {
        let worker = occurrence("cutex.worker", 42);
        let start = Instant::now();
        let mut planner = LateAdoptionPlanner::default();
        assert!(planner.should_plan(1, 1, start));
        assert!(planner.plan(vec![worker.clone()], start, |_| false).is_empty());
        let at = start + SETTLE;
        assert!(planner.should_plan(1, 1, at));
        assert_eq!(planner.plan(vec![worker.clone()], at, |_| false), vec![worker.clone()]);
        planner.failed(&worker, "refused".into(), at);
        assert!(!planner.should_plan(1, 1, at + FIRST_RETRY - TICK), "waits out the backoff without scanning");
        assert!(planner.should_plan(1, 1, at + FIRST_RETRY));
        assert!(planner.should_plan(2, 1, at + TICK), "a store change still plans at once");
    }

    #[test]
    fn connection_losses_count_up() {
        let before = connection_losses();
        note_connection_lost();
        assert!(connection_losses() > before);
    }

    #[test]
    fn backoff_is_capped() {
        let mut planner = LateAdoptionPlanner::default();
        let worker = occurrence("cutex.worker", 42);
        let now = Instant::now();
        for _ in 0..20 {
            planner.failed(&worker, "x".into(), now);
        }
        assert_eq!(planner.failures["cutex.worker"].backoff, MAX_RETRY);
    }
}
