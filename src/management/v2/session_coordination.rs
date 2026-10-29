//! Process-local operation reservations. The mutex protects admission metadata
//! only; an admitted operation owns a token, never a mutex guard, during IO.
//!
//! Reservations are deliberately non-preemptible: a successor cannot send,
//! revert or settle the same session while its predecessor is still executing.
//! This closes the check/forward race without treating an epoch comparison as
//! permission to send. Durable delivery/receipt CAS remains the crash fence;
//! these process-local tokens do not authorize replay after a restart.

use std::sync::{Condvar, Mutex, MutexGuard};

/// The single process-local lane registry shared by HTTP, input, lifecycle and
/// registration fences. Resolve aliases to durable session IDs before calling.
pub fn session_lane(session: &str) -> anyhow::Result<std::sync::Arc<OperationLane>> {
    static LANES: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, std::sync::Arc<OperationLane>>>,
    > = std::sync::OnceLock::new();
    let mut lanes = LANES
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("session operation registry poisoned"))?;
    Ok(lanes
        .entry(session.to_string())
        .or_insert_with(|| std::sync::Arc::new(OperationLane::new()))
        .clone())
}

#[derive(Debug, Default)]
pub struct OperationLane {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    next: u64,
    serving: u64,
    active: Option<u64>,
    owner: Option<std::thread::ThreadId>,
    depth: u32,
    poisoned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneSnapshot {
    pub active: Option<u64>,
    pub waiting: u64,
    pub poisoned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationError {
    Poisoned,
    Exhausted,
    WouldBlock,
}

impl std::fmt::Display for ReservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Poisoned => "session operation lane is poisoned",
            Self::Exhausted => "session operation sequence is exhausted",
            Self::WouldBlock => "session operation lane is occupied",
        })
    }
}

impl std::error::Error for ReservationError {}

/// An exclusive execution capability, including settlement. Dropping it makes
/// the next ticket eligible; no reference to business/store data is exposed.
#[derive(Debug)]
pub struct OperationReservation<'a> {
    lane: &'a OperationLane,
    token: u64,
    // Nested admission is valid only on the owning execution thread.
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl OperationLane {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, ReservationError> {
        self.state.lock().map_err(|_| ReservationError::Poisoned)
    }

    /// FIFO admission. Condvar::wait releases the metadata mutex while queued.
    /// The returned reservation contains no MutexGuard; callers may do IO.
    pub fn reserve(&self) -> Result<OperationReservation<'_>, ReservationError> {
        let mut state = self.state()?;
        if state.poisoned {
            return Err(ReservationError::Poisoned);
        }
        if state.owner == Some(std::thread::current().id()) {
            state.depth = state
                .depth
                .checked_add(1)
                .ok_or(ReservationError::Exhausted)?;
            return Ok(OperationReservation {
                lane: self,
                token: state.active.expect("owner has token"),
                _thread_bound: std::marker::PhantomData,
            });
        }
        let token = state.next;
        state.next = state
            .next
            .checked_add(1)
            .ok_or(ReservationError::Exhausted)?;
        while state.active.is_some() || state.serving != token {
            state = self
                .changed
                .wait(state)
                .map_err(|_| ReservationError::Poisoned)?;
            if state.poisoned {
                return Err(ReservationError::Poisoned);
            }
        }
        state.active = Some(token);
        state.owner = Some(std::thread::current().id());
        state.depth = 1;
        Ok(OperationReservation {
            lane: self,
            token,
            _thread_bound: std::marker::PhantomData,
        })
    }

    /// Compatibility entrypoint for callers migrating from session_input_lock.
    pub fn lock(&self) -> Result<OperationReservation<'_>, ReservationError> {
        self.reserve()
    }

    pub fn try_lock(&self) -> Result<OperationReservation<'_>, ReservationError> {
        let mut state = self.state()?;
        if state.poisoned {
            return Err(ReservationError::Poisoned);
        }
        if state.active.is_some() || state.next != state.serving {
            return Err(ReservationError::WouldBlock);
        }
        let token = state.next;
        state.next = state
            .next
            .checked_add(1)
            .ok_or(ReservationError::Exhausted)?;
        state.active = Some(token);
        state.owner = Some(std::thread::current().id());
        state.depth = 1;
        Ok(OperationReservation {
            lane: self,
            token,
            _thread_bound: std::marker::PhantomData,
        })
    }

    /// Observers can inspect admission even while a native operation is waiting.
    pub fn snapshot(&self) -> Result<LaneSnapshot, ReservationError> {
        let state = self.state()?;
        Ok(LaneSnapshot {
            active: state.active,
            waiting: state.next - state.serving - u64::from(state.active.is_some()),
            poisoned: state.poisoned,
        })
    }
}

impl OperationReservation<'_> {
    pub fn token(&self) -> u64 {
        self.token
    }

    /// Validate an existing execution capability without admitting another owner.
    /// Tokens alone are not identities: independent lanes may have equal tokens.
    /// The private fields and !Send/!Sync marker prevent manufacturing or moving
    /// a reservation to bypass the owning thread's native-operation fence.
    pub(crate) fn is_current_for(&self, lane: &OperationLane) -> Result<bool, ReservationError> {
        if !std::ptr::eq(self.lane, lane) {
            return Ok(false);
        }
        let state = lane.state()?;
        Ok(!state.poisoned
            && state.active == Some(self.token)
            && state.owner == Some(std::thread::current().id())
            && state.depth > 0)
    }
}

impl Drop for OperationReservation<'_> {
    fn drop(&mut self) {
        // Poison is fail-closed as with the previous mutex: unwinding through
        // native/store IO cannot authorize the next mutation as a safe retry.
        let mut state = self
            .lane
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.active != Some(self.token) {
            state.poisoned = true;
        } else {
            state.poisoned |= std::thread::panicking();
            state.depth -= 1;
            if state.depth == 0 {
                state.active = None;
                state.owner = None;
                state.serving = self.token + 1; // reserve checked the successor.
            }
        }
        drop(state);
        self.lane.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    #[test]
    fn existing_capability_validates_lane_thread_and_poison_without_readmission() {
        let first = OperationLane::new();
        let second = OperationLane::new();
        let held = first.reserve().unwrap();
        let other = second.reserve().unwrap();
        assert_eq!(held.token(), other.token(), "tokens alone are not proof");
        assert!(held.is_current_for(&first).unwrap());
        assert!(!held.is_current_for(&second).unwrap());
        assert_eq!(first.state.lock().unwrap().depth, 1, "validation adds no admission");
        let owner = std::thread::current().id();
        let foreign = std::thread::spawn(|| std::thread::current().id()).join().unwrap();
        first.state.lock().unwrap().owner = Some(foreign);
        assert!(!held.is_current_for(&first).unwrap());
        first.state.lock().unwrap().owner = Some(owner);
        first.state.lock().unwrap().poisoned = true;
        assert!(!held.is_current_for(&first).unwrap());
        first.state.lock().unwrap().poisoned = false;
        assert!(held.is_current_for(&first).unwrap());
    }

    fn await_waiters(lane: &OperationLane, expected: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while lane.snapshot().unwrap().waiting != expected {
            assert!(
                Instant::now() < deadline,
                "waiter failed to reach admission barrier"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn registry_shares_one_lane_per_identity_and_other_sessions_progress() {
        let id = uuid::Uuid::new_v4().to_string();
        let first = session_lane(&id).unwrap();
        let same = session_lane(&id).unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        let _held = first.reserve().unwrap();
        assert_eq!(same.try_lock().unwrap_err(), ReservationError::WouldBlock);
        let other = session_lane(&uuid::Uuid::new_v4().to_string()).unwrap();
        assert!(other.try_lock().is_ok());
    }

    #[test]
    fn native_wait_holds_capability_but_not_metadata_mutex() {
        let lane = Arc::new(OperationLane::new());
        let lease = lane.reserve().unwrap();
        // Directly prove that the execution capability contains no locked mutex.
        assert!(lane.state.try_lock().is_ok());
        let other = Arc::clone(&lane);
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let next = other.reserve().unwrap();
            tx.send(next.token()).unwrap();
        });
        await_waiters(&lane, 1);
        assert_eq!(lane.snapshot().unwrap().active, Some(lease.token()));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(lane.try_lock().unwrap_err(), ReservationError::WouldBlock);
        drop(lease);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        worker.join().unwrap();
        assert_eq!(lane.snapshot().unwrap().waiting, 0);
    }

    #[test]
    fn stop_then_revert_cannot_overtake_pending_native_settlement() {
        let lane = Arc::new(OperationLane::new());
        let sending = lane.reserve().unwrap();
        let (tx, rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let stop_lane = Arc::clone(&lane);
        let stop_tx = tx.clone();
        let stop = std::thread::spawn(move || {
            let lease = stop_lane.reserve().unwrap();
            stop_tx.send(("stop", lease.token())).unwrap();
            release_rx.recv().unwrap();
        });
        await_waiters(&lane, 1);
        let revert_lane = Arc::clone(&lane);
        let revert = std::thread::spawn(move || {
            let lease = revert_lane.reserve().unwrap();
            tx.send(("revert", lease.token())).unwrap();
        });
        await_waiters(&lane, 2);
        drop(sending); // durable/native settlement has finished.
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ("stop", 1)
        );
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release_tx.send(()).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ("revert", 2)
        );
        stop.join().unwrap();
        revert.join().unwrap();
    }

    #[test]
    fn unwind_fails_closed_and_wakes_existing_waiter() {
        let lane = Arc::new(OperationLane::new());
        let first = lane.reserve().unwrap();
        let other = Arc::clone(&lane);
        let waiter = std::thread::spawn(move || other.reserve().unwrap_err());
        await_waiters(&lane, 1);
        let failure = std::panic::catch_unwind(move || {
            let _lease = first;
            panic!("injected uncertain native outcome");
        });
        assert!(failure.is_err());
        assert_eq!(waiter.join().unwrap(), ReservationError::Poisoned);
        assert_eq!(lane.try_lock().unwrap_err(), ReservationError::Poisoned);
    }

    #[test]
    fn nested_input_keeps_outer_lifecycle_capability_until_final_settlement() {
        let lane = Arc::new(OperationLane::new());
        let outer = lane.reserve().unwrap();
        let nested = lane.reserve().unwrap();
        assert_eq!(outer.token(), nested.token());
        let other = Arc::clone(&lane);
        let (tx, rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let lease = other.reserve().unwrap();
            tx.send(lease.token()).unwrap();
        });
        await_waiters(&lane, 1);
        drop(nested);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(lane.snapshot().unwrap().active, Some(outer.token()));
        drop(outer);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        waiter.join().unwrap();
    }
}
