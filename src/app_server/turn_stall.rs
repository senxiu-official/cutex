//! Bound turn stall check (r274 S18, Director decision 11-02).
//!
//! `turn/completed` can be lost on a live connection. Management then keeps
//! the bound turn active for ever: admission, the delegation / event lease
//! and every input behind it wait on a turn that is over. Ungated: it is a
//! runtime fact, not a 054 behaviour (the input ledger still writes only
//! with its switch on).
//!
//! Rule, per connection (one event worker, one [`Watch`]):
//! - the active turn shows no activity (item / delta / turn events or server
//!   requests of that turn) for [`STALL_AFTER`] (5 min): ONE cheap
//!   `thread/read` (`includeTurns: false`, thread status only, never the
//!   history);
//! - not running (`idle` / `notLoaded` / `systemError`): the turn is settled
//!   as if `turn/completed` had arrived. The worker handles a synthesized
//!   `turn/completed` through the very same path as a native one (runtime
//!   status, admission, journal, durable sink: asks / leases, the ledger
//!   settle of 054 §2.10, delivery state). Its status comes from one
//!   `thread/turns/list` page without items; unknown → `interrupted`
//!   (outcome unknown: §2.10 checks each accepted input once);
//! - still running: read again after 10, 20, 30, 30 … min ([`delay`]);
//! - unreadable: same backoff; each unreadable read in a row is an unproven
//!   lease check (§2.12 c, [`unconfirmed`]), so a lease bound to a turn
//!   nothing can read goes `stale` and the owner may release it.
//!
//! Any activity of the turn resets the clock and the backoff. At most one
//! read is in flight per session (it runs on its own thread, never on the
//! event worker, never under the host fence). Per 051 the hot path is a
//! couple of string compares per event on worker-local state; the shared
//! table below changes only when a read result arrives.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;

use super::client::AppServerEvent;
use super::client::AppServerHandle;
use super::manager::AppServerManagedRuntimeStatus;
use super::manager::AppServerRuntimeEventContext;
use super::protocol::RpcNotification;

/// Quiet time of a bound turn before the first check.
pub const STALL_AFTER: Duration = Duration::from_secs(5 * 60);
/// Waits before the 1st, 2nd, 3rd and later checks, in [`STALL_AFTER`]
/// units: 5 → 10 → 20 → 30 min (cap).
const FACTORS: [u32; 4] = [1, 2, 4, 6];
/// Debug builds in a marked private test home only: [`STALL_AFTER`] in
/// seconds (the E2E S18 uses a few seconds).
pub const OVERRIDE_ENV: &str = "CUTEX_TEST_TURN_STALL_SECS";
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the worker compares its watched turn with the runtime status.
const SYNC_EVERY: Duration = Duration::from_secs(1);
/// Marker on a synthesized `turn/completed` (diagnostics only).
pub const SYNTHESIZED_MARKER: &str = "cutexSynthesized";

/// [`STALL_AFTER`], or the debug override.
pub fn stall_after() -> Duration {
    static BASE: OnceLock<Duration> = OnceLock::new();
    *BASE.get_or_init(|| debug_override().unwrap_or(STALL_AFTER))
}

#[cfg(debug_assertions)]
fn debug_override() -> Option<Duration> {
    let secs: u64 = std::env::var(OVERRIDE_ENV).ok()?.trim().parse().ok().filter(|secs| *secs > 0)?;
    let (Ok(private), Ok(home)) = (std::env::var("CUTEX_TEST_PRIVATE_HOME"), std::env::var("HOME")) else {
        return None;
    };
    (private == home && std::path::Path::new(&private).join(".cutex-test-private-home").is_file())
        .then(|| Duration::from_secs(secs))
}

#[cfg(not(debug_assertions))]
fn debug_override() -> Option<Duration> {
    None
}

/// Wait before the next check after `checks` checks since the last activity.
pub fn delay(base: Duration, checks: u32) -> Duration {
    base * FACTORS[(checks as usize).min(FACTORS.len() - 1)]
}

/// What one check found.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Running,
    /// The thread is not running: the turn to report in `turn/completed`.
    Ended(Value),
    Unreadable(String),
}

/// What the worker does with a check result.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// A stale result (activity or another turn since the read began).
    Ignore,
    /// Still running: the next check is scheduled.
    Running,
    /// Unreadable `n` times in a row.
    Unreadable(u32),
    /// Settle as if `turn/completed` (this turn) had arrived.
    Settle(Value),
}

/// The clock of one connection's active turn. Pure; times are passed in.
#[derive(Debug)]
pub struct Tracker {
    base: Duration,
    turn: Option<String>,
    /// Bumped by every reset: a read result from an older epoch is stale.
    epoch: u64,
    checks: u32,
    unreadable: u32,
    deadline: Option<Instant>,
    in_flight: Option<u64>,
}

impl Tracker {
    pub fn new(base: Duration) -> Self {
        Self { base, turn: None, epoch: 0, checks: 0, unreadable: 0, deadline: None, in_flight: None }
    }

    pub fn turn(&self) -> Option<&str> {
        self.turn.as_deref()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The active turn (runtime status): a different turn starts over.
    pub fn watch(&mut self, turn: Option<&str>, now: Instant) -> bool {
        if self.turn.as_deref() == turn {
            return false;
        }
        self.turn = turn.map(str::to_string);
        self.reset(now);
        true
    }

    /// Activity of the watched turn: the clock and the backoff start over.
    pub fn activity(&mut self, now: Instant) {
        if self.turn.is_some() {
            self.reset(now);
        }
    }

    fn reset(&mut self, now: Instant) {
        self.epoch += 1;
        self.checks = 0;
        self.unreadable = 0;
        self.deadline = self.turn.as_ref().map(|_| now + delay(self.base, 0));
    }

    /// A check is due and none is in flight: `(epoch, turn)` to read.
    pub fn begin(&mut self, now: Instant) -> Option<(u64, String)> {
        if self.in_flight.is_some() || self.deadline.is_none_or(|at| now < at) {
            return None;
        }
        let turn = self.turn.clone()?;
        self.in_flight = Some(self.epoch);
        self.deadline = None;
        Some((self.epoch, turn))
    }

    /// The read begun at `epoch` returned `outcome`.
    pub fn finish(&mut self, epoch: u64, outcome: Outcome, now: Instant) -> Step {
        let flight = self.in_flight.take();
        if flight != Some(epoch) || epoch != self.epoch || self.turn.is_none() {
            return Step::Ignore;
        }
        self.checks = self.checks.saturating_add(1);
        // Also after a settle: if the turn somehow stays active, check again.
        self.deadline = Some(now + delay(self.base, self.checks));
        match outcome {
            Outcome::Running => {
                self.unreadable = 0;
                Step::Running
            }
            Outcome::Unreadable(_) => {
                self.unreadable = self.unreadable.saturating_add(1);
                Step::Unreadable(self.unreadable)
            }
            Outcome::Ended(turn) => Step::Settle(turn),
        }
    }
}

/// Whether `event` is activity of `turn` on `thread`.
pub fn is_turn_activity(event: &AppServerEvent, thread: &str, turn: &str) -> bool {
    let (method, params) = match event {
        AppServerEvent::Notification(notification) => (notification.method.as_str(), notification.params.as_ref()),
        AppServerEvent::ServerRequest(request) => (request.method.as_str(), request.params.as_ref()),
        _ => return false,
    };
    let Some(params) = params else { return false };
    if params.get("threadId").and_then(Value::as_str).is_some_and(|id| id != thread) {
        return false;
    }
    match params.get("turnId").or_else(|| params.pointer("/turn/id")).and_then(Value::as_str) {
        Some(id) => id == turn,
        None => method.starts_with("item/"),
    }
}

/// One check: the thread status, and only when it is not running the ended
/// turn's status (one `thread/turns/list` page without items).
pub fn read(call: &mut dyn FnMut(&str, Value) -> Result<Value, String>, thread: &str, turn: &str) -> Outcome {
    let response = match call("thread/read", json!({"threadId": thread, "includeTurns": false})) {
        Ok(response) => response,
        Err(error) => return Outcome::Unreadable(error),
    };
    match response.pointer("/thread/status/type").and_then(Value::as_str) {
        Some("active") => Outcome::Running,
        Some("idle" | "notLoaded" | "systemError") => Outcome::Ended(ended_turn(call, thread, turn)),
        other => Outcome::Unreadable(format!("thread/read: thread status {other:?}")),
    }
}

fn ended_turn(call: &mut dyn FnMut(&str, Value) -> Result<Value, String>, thread: &str, turn: &str) -> Value {
    let page = call(
        "thread/turns/list",
        json!({"threadId": thread, "cursor": null, "limit": 5, "sortDirection": "desc", "itemsView": "notLoaded"}),
    )
    .ok();
    let found = page.as_ref().and_then(|page| {
        page.get("data")?.as_array()?.iter().find(|listed| listed.get("id").and_then(Value::as_str) == Some(turn))
    });
    let status = found
        .and_then(|listed| listed.get("status"))
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "completed" | "interrupted" | "failed"))
        .unwrap_or("interrupted");
    let mut ended = json!({"id": turn, "items": [], "status": status});
    if let Some(error) = found.and_then(|listed| listed.get("error")).filter(|error| !error.is_null()) {
        ended["error"] = error.clone();
    }
    ended
}

/// The `turn/completed` a lost one would have been.
pub fn completion(thread: &str, turn: Value) -> AppServerEvent {
    let params = json!({"threadId": thread, "turn": turn, SYNTHESIZED_MARKER: "turnStallCheck"});
    let raw = json!({"method": "turn/completed", "params": params});
    AppServerEvent::Notification(RpcNotification { method: "turn/completed".into(), params: Some(params), raw })
}

/// A bound turn whose stall checks could not read it (054 §2.12 c: each is
/// an unproven lease check).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unconfirmed {
    pub generation: u64,
    pub turn: String,
    pub failures: u32,
    pub last_checked_at: DateTime<Utc>,
    pub next_check_at: DateTime<Utc>,
    pub result: String,
}

static UNCONFIRMED: Mutex<BTreeMap<String, Unconfirmed>> = Mutex::new(BTreeMap::new());

/// The unreadable streak of exactly this session, generation and turn.
pub fn unconfirmed(session: &str, generation: u64, turn: &str) -> Option<Unconfirmed> {
    let table = UNCONFIRMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    table.get(session).filter(|entry| entry.generation == generation && entry.turn == turn).cloned()
}

pub(crate) fn publish(session: &str, entry: Unconfirmed) {
    UNCONFIRMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(session.to_string(), entry);
}

pub(crate) fn unpublish(session: &str, generation: u64) {
    let mut table = UNCONFIRMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if table.get(session).is_some_and(|entry| entry.generation == generation) {
        table.remove(session);
    }
}

type Pending = (u64, mpsc::Receiver<Outcome>);

/// The stall check of one connection's event worker.
pub struct Watch {
    tracker: Tracker,
    session: String,
    thread: String,
    generation: u64,
    pending: Option<Pending>,
    published: bool,
    next_sync: Instant,
}

impl Watch {
    pub fn new(context: &AppServerRuntimeEventContext) -> Self {
        Self {
            tracker: Tracker::new(stall_after()),
            session: context.cutex_session_id.clone(),
            thread: context.thread_id.clone(),
            generation: context.runtime_generation,
            pending: None,
            published: false,
            next_sync: Instant::now(),
        }
    }

    /// Every event the worker handles (cheap: no lock, no allocation unless
    /// a turn starts or ends).
    pub fn observe(&mut self, event: &AppServerEvent) {
        let now = Instant::now();
        if let AppServerEvent::Notification(notification) = event {
            let params = notification.params.as_ref();
            let bound = params
                .and_then(|params| params.get("threadId"))
                .and_then(Value::as_str)
                .is_none_or(|thread| thread == self.thread);
            let turn = params.and_then(|params| params.pointer("/turn/id")).and_then(Value::as_str);
            match notification.method.as_str() {
                "turn/started" if bound => {
                    if !self.tracker.watch(turn, now) {
                        self.tracker.activity(now);
                    }
                    self.clear();
                    return;
                }
                "turn/completed" if bound && (turn.is_none() || turn == self.tracker.turn()) => {
                    self.tracker.watch(None, now);
                    self.clear();
                    return;
                }
                _ => {}
            }
        }
        let active = self.tracker.turn().is_some_and(|turn| is_turn_activity(event, &self.thread, turn));
        if active {
            self.tracker.activity(now);
            self.clear();
        }
    }

    /// Every worker pass. Returns a synthesized `turn/completed` to handle
    /// exactly like a native one.
    pub fn poll(&mut self, status: &Arc<Mutex<AppServerManagedRuntimeStatus>>, handle: &AppServerHandle) -> Option<AppServerEvent> {
        let now = Instant::now();
        let mut settle = None;
        if let Some((epoch, receiver)) = &self.pending {
            let outcome = match receiver.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Outcome::Unreadable("stall check ended without a result".into())),
            };
            if let Some(outcome) = outcome {
                let epoch = *epoch;
                self.pending = None;
                match self.tracker.finish(epoch, outcome, now) {
                    Step::Ignore => {}
                    Step::Running => self.clear(),
                    Step::Unreadable(failures) => self.mark_unreadable(failures, now),
                    Step::Settle(turn) => settle = Some(turn),
                }
            }
        }
        let due = self.tracker.deadline().is_some_and(|at| now >= at) && !self.tracker.in_flight();
        if settle.is_none() && !due && now < self.next_sync {
            return None;
        }
        self.next_sync = now + SYNC_EVERY;
        // The runtime status is the authority on the active turn.
        let (connected, active) = {
            let status = status.lock().ok()?;
            let changed = status.active_turn_id.as_deref() != self.tracker.turn();
            (status.connected, changed.then(|| status.active_turn_id.clone()))
        };
        if let Some(active) = active {
            self.tracker.watch(active.as_deref(), now);
            self.clear();
            return None;
        }
        if !connected {
            return None;
        }
        if let Some(turn) = settle {
            self.clear();
            let id = turn.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            eprintln!(
                "cutex: {} turn {id} showed no activity for {:?} and its thread is not running: settled as turn/completed ({})",
                self.session,
                self.tracker.base,
                turn.get("status").and_then(Value::as_str).unwrap_or("?"),
            );
            return Some(completion(&self.thread, turn));
        }
        if let Some((epoch, turn)) = self.tracker.begin(now) {
            let (sender, receiver) = mpsc::channel();
            let handle = handle.clone();
            let thread = self.thread.clone();
            let spawned = std::thread::Builder::new().name("cutex-turn-stall-check".into()).spawn(move || {
                let mut call = |method: &str, params: Value| {
                    handle.request_with_timeout(method, params, READ_TIMEOUT).map_err(|error| error.to_string())
                };
                let _ = sender.send(read(&mut call, &thread, &turn));
            });
            match spawned {
                Ok(_) => self.pending = Some((epoch, receiver)),
                Err(error) => {
                    if let Step::Unreadable(failures) =
                        self.tracker.finish(epoch, Outcome::Unreadable(error.to_string()), now)
                    {
                        self.mark_unreadable(failures, now);
                    }
                }
            }
        }
        None
    }

    fn mark_unreadable(&mut self, failures: u32, now: Instant) {
        let Some(turn) = self.tracker.turn() else { return };
        let wall = Utc::now();
        let next = self.tracker.deadline().map_or(Duration::ZERO, |at| at.saturating_duration_since(now));
        publish(&self.session, Unconfirmed {
            generation: self.generation,
            turn: turn.to_string(),
            failures,
            last_checked_at: wall,
            next_check_at: wall + chrono::Duration::from_std(next).unwrap_or_default(),
            result: "turnUnreadable".into(),
        });
        self.published = true;
    }

    fn clear(&mut self) {
        if self.published {
            self.published = false;
            unpublish(&self.session, self.generation);
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn notification(method: &str, params: Value) -> AppServerEvent {
        AppServerEvent::Notification(RpcNotification { method: method.into(), params: Some(params), raw: Value::Null })
    }

    /// Runs the tracker with every check returning `outcome`; the waits
    /// between checks, in minutes.
    fn waits(tracker: &mut Tracker, start: Instant, outcome: &Outcome, checks: usize) -> Vec<u64> {
        let mut now = start;
        let mut last = start;
        let mut waits = Vec::new();
        while waits.len() < checks {
            if let Some((epoch, _)) = tracker.begin(now) {
                waits.push((now - last).as_secs() / 60);
                last = now;
                tracker.finish(epoch, outcome.clone(), now);
            }
            now += MIN;
        }
        waits
    }

    #[test]
    fn backoff_is_five_ten_twenty_thirty_then_thirty() {
        assert_eq!((0..6).map(|n| delay(STALL_AFTER, n).as_secs() / 60).collect::<Vec<_>>(), [5, 10, 20, 30, 30, 30]);
        let start = Instant::now();
        let mut tracker = Tracker::new(STALL_AFTER);
        tracker.watch(Some("t"), start);
        assert_eq!(waits(&mut tracker, start, &Outcome::Running, 6), [5, 10, 20, 30, 30, 30]);
    }

    #[test]
    fn activity_resets_the_clock_and_the_backoff() {
        let start = Instant::now();
        let mut tracker = Tracker::new(STALL_AFTER);
        tracker.watch(Some("t"), start);
        assert!(tracker.begin(start + 4 * MIN).is_none(), "nothing before 5 min of quiet");
        tracker.activity(start + 4 * MIN);
        assert!(tracker.begin(start + 8 * MIN).is_none(), "activity restarted the 5 min");
        let later = start + 9 * MIN;
        let waits = waits(&mut tracker, later, &Outcome::Running, 3);
        assert_eq!(waits, [0, 10, 20]);
        // Activity again: back to 5 min, not 30.
        let now = later + 31 * MIN;
        tracker.activity(now);
        assert!(tracker.begin(now + 4 * MIN).is_none());
        assert!(tracker.begin(now + 5 * MIN).is_some());
    }

    #[test]
    fn one_read_in_flight_and_stale_results_are_ignored() {
        let start = Instant::now();
        let mut tracker = Tracker::new(STALL_AFTER);
        tracker.watch(Some("t"), start);
        let (epoch, turn) = tracker.begin(start + 5 * MIN).expect("due");
        assert_eq!(turn, "t");
        assert!(tracker.begin(start + 60 * MIN).is_none(), "never a second read while one is in flight");
        // Activity while reading: the result no longer says anything.
        tracker.activity(start + 6 * MIN);
        assert!(tracker.begin(start + 60 * MIN).is_none(), "still in flight");
        assert_eq!(tracker.finish(epoch, Outcome::Ended(json!({"id": "t"})), start + 6 * MIN), Step::Ignore);
        assert!(tracker.begin(start + 10 * MIN).is_none(), "5 min from the activity");
        let (epoch, _) = tracker.begin(start + 11 * MIN).expect("due again");
        // Another turn meanwhile: the result is for the old one.
        tracker.watch(Some("t2"), start + 12 * MIN);
        assert_eq!(tracker.finish(epoch, Outcome::Running, start + 12 * MIN), Step::Ignore);
        assert_eq!(tracker.turn(), Some("t2"));
    }

    #[test]
    fn not_running_settles_running_reschedules_unreadable_counts() {
        let start = Instant::now();
        let mut tracker = Tracker::new(STALL_AFTER);
        tracker.watch(Some("t"), start);
        let (epoch, _) = tracker.begin(start + 5 * MIN).unwrap();
        assert_eq!(tracker.finish(epoch, Outcome::Running, start + 5 * MIN), Step::Running);
        assert_eq!(tracker.deadline(), Some(start + 15 * MIN));
        let (epoch, _) = tracker.begin(start + 15 * MIN).unwrap();
        assert_eq!(tracker.finish(epoch, Outcome::Unreadable("x".into()), start + 15 * MIN), Step::Unreadable(1));
        let (epoch, _) = tracker.begin(start + 35 * MIN).unwrap();
        assert_eq!(tracker.finish(epoch, Outcome::Unreadable("x".into()), start + 35 * MIN), Step::Unreadable(2));
        let (epoch, _) = tracker.begin(start + 65 * MIN).unwrap();
        assert_eq!(tracker.finish(epoch, Outcome::Running, start + 65 * MIN), Step::Running, "a running read ends the unreadable streak");
        let (epoch, _) = tracker.begin(start + 95 * MIN).unwrap();
        assert_eq!(tracker.finish(epoch, Outcome::Unreadable("x".into()), start + 95 * MIN), Step::Unreadable(1));
        let (epoch, _) = tracker.begin(start + 125 * MIN).unwrap();
        let ended = json!({"id": "t", "items": [], "status": "completed"});
        assert_eq!(tracker.finish(epoch, Outcome::Ended(ended.clone()), start + 125 * MIN), Step::Settle(ended));
        // No turn: nothing to check.
        tracker.watch(None, start + 126 * MIN);
        assert!(tracker.begin(start + 1000 * MIN).is_none());
    }

    #[test]
    fn read_is_one_status_read_and_settles_with_the_listed_status() {
        let mut calls = Vec::new();
        let mut running = |method: &str, params: Value| {
            calls.push((method.to_string(), params));
            Ok(json!({"thread": {"id": "th", "status": {"type": "active", "activeFlags": []}}}))
        };
        assert_eq!(read(&mut running, "th", "t"), Outcome::Running);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "thread/read");
        assert_eq!(calls[0].1["includeTurns"], false, "never the history");

        let mut idle = |method: &str, _: Value| match method {
            "thread/read" => Ok(json!({"thread": {"id": "th", "status": {"type": "idle"}}})),
            _ => Ok(json!({"data": [{"id": "t2", "status": "completed"}, {"id": "t", "status": "failed", "error": {"message": "boom"}}]})),
        };
        assert_eq!(read(&mut idle, "th", "t"), Outcome::Ended(json!({"id": "t", "items": [], "status": "failed", "error": {"message": "boom"}})));

        let mut unlisted = |method: &str, _: Value| match method {
            "thread/read" => Ok(json!({"thread": {"id": "th", "status": {"type": "notLoaded"}}})),
            _ => Err("unsupported".to_string()),
        };
        assert_eq!(read(&mut unlisted, "th", "t"), Outcome::Ended(json!({"id": "t", "items": [], "status": "interrupted"})), "unknown ending: interrupted");

        let mut failing = |_: &str, _: Value| Err("history unreadable".to_string());
        assert!(matches!(read(&mut failing, "th", "t"), Outcome::Unreadable(_)));
        let mut statusless = |_: &str, _: Value| Ok(json!({"thread": {"id": "th"}}));
        assert!(matches!(read(&mut statusless, "th", "t"), Outcome::Unreadable(_)), "no status proves nothing");
    }

    #[test]
    fn activity_is_events_of_the_watched_turn_on_the_bound_thread() {
        let delta = notification("item/agentMessage/delta", json!({"threadId": "th", "turnId": "t", "delta": "x"}));
        assert!(is_turn_activity(&delta, "th", "t"));
        assert!(!is_turn_activity(&delta, "th", "other"));
        assert!(!is_turn_activity(&delta, "sub", "t"), "a sub-agent thread");
        assert!(is_turn_activity(&notification("item/started", json!({"threadId": "th", "item": {}})), "th", "t"));
        assert!(!is_turn_activity(&notification("thread/status/changed", json!({"threadId": "th"})), "th", "t"));
        assert!(is_turn_activity(&notification("turn/plan/updated", json!({"threadId": "th", "turnId": "t"})), "th", "t"));
    }

    #[test]
    fn synthesized_completion_is_a_turn_completed_of_the_bound_thread() {
        let AppServerEvent::Notification(event) = completion("th", json!({"id": "t", "items": [], "status": "interrupted"})) else {
            panic!("a notification");
        };
        assert_eq!(event.method, "turn/completed");
        assert_eq!(event.params.as_ref().unwrap()["threadId"], "th");
        assert_eq!(event.params.as_ref().unwrap()["turn"]["id"], "t");
        assert_eq!(event.raw["params"], *event.params.as_ref().unwrap());
    }

    #[test]
    fn unconfirmed_is_per_session_generation_and_turn() {
        let session = format!("stall-{}", uuid::Uuid::new_v4());
        let now = Utc::now();
        publish(&session, Unconfirmed { generation: 3, turn: "t".into(), failures: 2, last_checked_at: now, next_check_at: now, result: "turnUnreadable".into() });
        assert_eq!(unconfirmed(&session, 3, "t").map(|entry| entry.failures), Some(2));
        assert!(unconfirmed(&session, 4, "t").is_none());
        assert!(unconfirmed(&session, 3, "t2").is_none());
        unpublish(&session, 4);
        assert!(unconfirmed(&session, 3, "t").is_some(), "another generation leaves it");
        unpublish(&session, 3);
        assert!(unconfirmed(&session, 3, "t").is_none());
    }
}
