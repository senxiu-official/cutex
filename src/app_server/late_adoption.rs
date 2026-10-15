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
    if record.is_retired()
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
}

impl LateAdoptionPlanner {
    /// Whether a later tick may adopt something even if the store does not
    /// change (an occurrence still settling or waiting for a retry).
    pub fn has_pending(&self) -> bool {
        !self.first_seen.is_empty()
    }

    /// The occurrences to adopt now. `candidates` are this tick's
    /// [`adoption_candidate`]s; `is_known` says whether this process already
    /// holds (or is establishing) a connection for a session.
    pub fn plan(
        &mut self,
        candidates: Vec<RuntimeOccurrence>,
        now: Instant,
        is_known: impl Fn(&str) -> bool,
    ) -> Vec<RuntimeOccurrence> {
        let wanted = candidates
            .into_iter()
            .filter(|occurrence| !is_known(&occurrence.cutex_session_id))
            .map(|occurrence| (occurrence.cutex_session_id.clone(), occurrence))
            .collect::<HashMap<_, _>>();
        self.first_seen.retain(|id, (occurrence, _)| wanted.get(id) == Some(occurrence));
        self.failures
            .retain(|id, failure| wanted.get(id) == Some(&failure.occurrence));
        let mut due = Vec::new();
        for (id, occurrence) in wanted {
            let first_seen = self
                .first_seen
                .entry(id.clone())
                .or_insert_with(|| (occurrence.clone(), now))
                .1;
            if now.saturating_duration_since(first_seen) < SETTLE {
                continue;
            }
            if self.failures.get(&id).is_some_and(|failure| failure.retry_at > now) {
                continue;
            }
            due.push(occurrence);
        }
        due.sort_by(|left, right| left.cutex_session_id.cmp(&right.cutex_session_id));
        due
    }

    /// The occurrence is connected now.
    pub fn adopted(&mut self, occurrence: &RuntimeOccurrence) {
        self.first_seen.remove(&occurrence.cutex_session_id);
        self.failures.remove(&occurrence.cutex_session_id);
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
        self.failures.insert(
            id,
            Failure {
                occurrence: occurrence.clone(),
                retry_at: now + backoff,
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
        planner.adopted(&worker);
        assert!(!planner.has_pending());
        // Connected in this process: nothing to do, no state kept.
        assert!(planner.plan(vec![worker.clone()], start + SETTLE * 3, |id| id == "cutex.worker").is_empty());
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
