//! Git scans have their own budget and share work only after root authorization.
use crate::management::v2::workspace::{WorkspaceError, WorkspaceResult};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub(super) const SCANS: usize = 2;
const QUEUED: usize = 8;
const WAIT: Duration = Duration::from_secs(2);
const REUSE: Duration = Duration::from_secs(1);
const CACHED_ROOTS: usize = 8;

pub(super) fn busy() -> WorkspaceError {
    WorkspaceError::new(503, "git_status_busy", "git status is busy; retry shortly")
}
#[derive(Clone, Hash, PartialEq, Eq)]
struct Key { root: Vec<String>, owner_local: bool, narrowed: bool }
impl Key {
    fn new(root: &Path, owner_local: bool) -> Self {
        Self { root: crate::artifacts::paths::key(root), owner_local, narrowed: false }
    }
}
type Shared = Result<Arc<Value>, Arc<WorkspaceError>>;
struct ResultState { result: Option<Shared>, expires: Instant }
struct Flight {
    state: Mutex<ResultState>,
    ready: Condvar,
    dirty: AtomicBool,
}
impl Flight {
    fn new() -> Self { Self { state: Mutex::new(ResultState { result: None, expires: Instant::now() }), ready: Condvar::new(), dirty: AtomicBool::new(false) } }
    fn finish(&self, result: Shared) {
        // Large results still coalesce while in flight, but do not become a
        // persistent multi-root cache. Responses share the Value through Arc.
        let reuse = result.as_ref().is_ok_and(|v| cacheable(v, &mut (4 * 1024 * 1024)));
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expires = Instant::now() + if reuse { REUSE } else { Duration::ZERO };
        state.result = Some(result);
        self.ready.notify_all();
    }
    fn wait(&self, deadline: Instant) -> WorkspaceResult<Shared> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.result.is_none() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { return Err(busy()) };
            state = self.ready.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        Ok(state.result.as_ref().unwrap().clone())
    }
}
fn cacheable(value: &Value, budget: &mut usize) -> bool {
    let overhead = match value { Value::String(v) => 32 + v.len(), Value::Array(_) | Value::Object(_) => 32, _ => 16 };
    let Some(rest) = budget.checked_sub(overhead) else { return false }; *budget = rest;
    match value {
        Value::Array(v) => v.iter().all(|v| cacheable(v, budget)),
        Value::Object(v) => v.iter().all(|(k, v)| {
            let Some(rest) = budget.checked_sub(64 + k.len()) else { return false }; *budget = rest;
            cacheable(v, budget)
        }),
        _ => true,
    }
}
struct PoolState { active: usize, next: u64, queue: VecDeque<u64> }
struct Pool { state: Mutex<PoolState>, ready: Condvar }
struct Scan<'a>(&'a Pool);
impl Drop for Scan<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active -= 1;
        self.0.ready.notify_all();
    }
}
impl Pool {
    fn new() -> Self { Self { state: Mutex::new(PoolState { active: 0, next: 0, queue: VecDeque::new() }), ready: Condvar::new() } }
    fn acquire(&self, deadline: Instant) -> WorkspaceResult<Scan<'_>> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active < SCANS && state.queue.is_empty() { state.active += 1; return Ok(Scan(self)); }
        if state.queue.len() >= QUEUED { return Err(busy()); }
        let ticket = state.next; state.next = state.next.wrapping_add(1); state.queue.push_back(ticket);
        loop {
            if state.active < SCANS && state.queue.front() == Some(&ticket) {
                state.queue.pop_front(); state.active += 1; self.ready.notify_all(); return Ok(Scan(self));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                state.queue.retain(|t| *t != ticket); self.ready.notify_all(); return Err(busy());
            };
            state = self.ready.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }
}
struct Complete(Arc<Flight>, bool);
impl Drop for Complete {
    fn drop(&mut self) { if !self.1 { self.0.finish(Err(Arc::new(busy()))); } }
}
struct Coordinator {
    flights: Mutex<HashMap<Key, Arc<Flight>>>,
    pool: Pool,
    scans: AtomicU64,
    coalesced: AtomicU64,
}
impl Coordinator {
    fn new() -> Self { Self { flights: Mutex::new(HashMap::new()), pool: Pool::new(), scans: AtomicU64::new(0), coalesced: AtomicU64::new(0) } }
    fn get(&self, key: Key, mut load: impl FnMut() -> WorkspaceResult<Value>) -> WorkspaceResult<Arc<Value>> {
        let deadline = Instant::now() + WAIT;
        loop {
            let (flight, leader) = {
                let mut flights = self.flights.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = Instant::now();
                flights.retain(|_, f| {
                    let state = f.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.result.is_none() || (state.expires > now && !f.dirty.load(Ordering::Acquire))
                });
                // Bound completed-result retention independently of live work.
                while flights.len() >= CACHED_ROOTS {
                    let oldest = flights.iter().filter_map(|(k, f)| {
                        let state = f.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.result.as_ref().map(|_| (k.clone(), state.expires))
                    }).min_by_key(|(_, at)| *at).map(|(k, _)| k);
                    if let Some(oldest) = oldest { flights.remove(&oldest); } else { break; }
                }
                if let Some(flight) = flights.get(&key) {
                    self.coalesced.fetch_add(1, Ordering::Relaxed); (flight.clone(), false)
                } else {
                    let flight = Arc::new(Flight::new()); flights.insert(key.clone(), flight.clone()); (flight, true)
                }
            };
            let result = if leader {
                let mut completion = Complete(flight.clone(), false);
                let result = match self.pool.acquire(deadline) {
                    Ok(_scan) => { self.scans.fetch_add(1, Ordering::Relaxed); load().map(Arc::new).map_err(Arc::new) }
                    Err(error) => Err(Arc::new(error)),
                };
                flight.finish(result.clone()); completion.1 = true; result
            } else { flight.wait(deadline)? };
            // A workspace change invalidates both reuse and an older in-flight
            // result. One successor scan serves callers requesting fresh state.
            if flight.dirty.load(Ordering::Acquire) {
                if Instant::now() >= deadline { return Err(busy()); }
                continue;
            }
            return result.map_err(|e| WorkspaceError::new(e.status, e.code, e.message.clone()));
        }
    }
    fn invalidate(&self, root: &Path) {
        let root = crate::artifacts::paths::key(root);
        let flights = self.flights.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, flight) in flights.iter() { if key.root == root { flight.dirty.store(true, Ordering::Release); } }
    }
    fn diagnostics(&self) -> Value {
        let state = self.pool.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        serde_json::json!({"scansActive":state.active,"scansWaiting":state.queue.len(),"scanLimit":SCANS,"scanQueueLimit":QUEUED,"waitMs":WAIT.as_millis(),"resultReuseMs":REUSE.as_millis(),"scansStarted":self.scans.load(Ordering::Relaxed),"coalesced":self.coalesced.load(Ordering::Relaxed)})
    }
}
fn global() -> &'static Coordinator { static GLOBAL: OnceLock<Coordinator> = OnceLock::new(); GLOBAL.get_or_init(Coordinator::new) }
pub(super) fn get(root: &Path, load: impl FnMut() -> WorkspaceResult<Value>) -> WorkspaceResult<Arc<Value>> {
    let full=crate::management::listeners::current_client_token().is_none_or(|token|token.scope.is_full());
    let mut key=Key::new(root, crate::management::users::request_has_owner_local_secret_access());
    key.narrowed=!full;
    global().get(key, load)
}
pub(super) fn invalidate(root: &Path) { global().invalidate(root); }
pub(super) fn diagnostics() -> Value { global().diagnostics() }

#[cfg(test)]
#[path = "git_status_capacity_tests.rs"]
mod tests;
