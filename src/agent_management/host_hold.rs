//! r273 stab (051 step 3, A02/A03): the host authority fence is not held
//! while a runtime comes online.
//!
//! A lifecycle entry that changes a session registers its execution mutex
//! and host fence here ([`HostHold::enter`]) instead of keeping the guards
//! on its stack. Its intent is already durable when it reaches a slow
//! lifecycle effect (the action phase, the session launch claim or the stock
//! runtime receipt is committed under the fence first). The adapter then
//! wraps the slow part (spawn + readiness wait, native stop) in
//! [`outside_host_fence`]: the fence and the execution mutex are released,
//! the effect runs holding at most the session lane, and the entry takes the
//! fence back before it commits its outcome. Every outcome commit compares
//! the durable claim / generation / action phase it registered (version
//! compare), so a stop, archive or pause that landed in the window makes the
//! entry fail or abandon instead of publishing a stale result.
//!
//! Reacquire order (051 §2): a thread holding no session lane waits for the
//! execution mutex and then the fence, exactly as it did on entry. A thread
//! that holds a lane never waits for the fence: it only tries it for
//! [`REACQUIRE_BUDGET`] and otherwise finishes its (claim-compared) commit
//! without it.
//!
//! `outside_host_fence` is a no-op on threads without a registered hold (the
//! owner admission worker, tests, lane-free internal callers), and nests: an
//! inner call while the hold is already released just runs its closure.
//!
//! In-process single flight: [`InFlight`] keys (action id, agent id) are
//! registered under the fence before the fence is first released; a second
//! entry for the same key waits for it outside every lock, then replays.
//! The keys are memory only: after a crash the durable phase / claim is what
//! the existing replay and launch-claim sweep recover from.
use super::*;
use crate::platform::lock_accounting::FileGuard;
use std::sync::{Condvar, Mutex, MutexGuard};

#[cfg(not(test))]
pub(crate) const REACQUIRE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
pub(crate) const REACQUIRE_BUDGET: std::time::Duration = std::time::Duration::from_millis(400);

struct Held {
    store: AgentManagementStore,
    execution: bool,
    execution_guard: Option<MutexGuard<'static, ()>>,
    fence: Option<FileGuard>,
}

thread_local! {
    static HELD: std::cell::RefCell<Vec<Held>> = const { std::cell::RefCell::new(Vec::new()) };
    static RELEASES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The registered locks of one lifecycle entry. Dropping it releases the
/// fence, then the execution mutex (whatever is still held).
pub(crate) struct HostHold {
    depth: usize,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl HostHold {
    pub(crate) fn enter(
        store: &AgentManagementStore,
        execution_guard: Option<MutexGuard<'static, ()>>,
        fence: FileGuard,
    ) -> Self {
        let depth = HELD.with(|held| {
            let mut held = held.borrow_mut();
            held.push(Held {
                store: store.clone(),
                execution: execution_guard.is_some(),
                execution_guard,
                fence: Some(fence),
            });
            held.len()
        });
        Self { depth, _not_send: std::marker::PhantomData }
    }

}

impl Drop for HostHold {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            while held.len() >= self.depth {
                if let Some(mut top) = held.pop() {
                    drop(top.fence.take());
                    drop(top.execution_guard.take());
                }
            }
        });
    }
}

/// Whether this thread has a registered hold whose fence is held right now.
pub fn host_fence_registered() -> bool {
    HELD.with(|held| held.borrow().last().is_some_and(|h| h.fence.is_some()))
}

/// How many times this thread released a registered fence (tests).
pub fn host_fence_releases() -> u64 {
    RELEASES.with(|n| n.get())
}

/// Run a slow lifecycle effect (spawn, readiness wait, stop) without the
/// host fence and the provider execution mutex, when this thread registered
/// them. See the module docs for the reacquire rules. The closure must not
/// commit an outcome that needs the fence; callers re-validate their claim
/// after this returns.
pub fn outside_host_fence<T>(
    what: &str,
    effect: impl FnOnce() -> T,
) -> Result<T, AgentManagementError> {
    let released = HELD.with(|held| {
        let mut held = held.borrow_mut();
        let Some(top) = held.last_mut() else { return None };
        let fence = top.fence.take()?;
        // Fence first, then the execution mutex: the reverse of entry.
        drop(fence);
        drop(top.execution_guard.take());
        Some((top.store.clone(), top.execution))
    });
    let Some((store, execution)) = released else {
        return Ok(effect());
    };
    RELEASES.with(|n| n.set(n.get() + 1));
    // A panic in the effect leaves the entry's hold released; its guard
    // then has nothing left to drop.
    let value = effect();
    reacquire(&store, execution, what)?;
    Ok(value)
}

/// r273.1 (B1): a slow check of a claim that is already durable (the live
/// runtime claim's PID / endpoint ownership proof before a reconnect) runs
/// outside the registered host fence, like [`outside_host_fence`]. `claim`
/// reads the durable claim cheaply; it is read under the fence before the
/// release and again after the fence is back, and the verification only
/// counts when both reads are equal (a stop, archive, replacement or new
/// generation that landed in the window fails the entry instead of letting
/// it publish a reconnect to a stale claim). Without a registered hold this
/// is a plain call of `verify`.
pub fn verify_claim_outside_host_fence<C: PartialEq + std::fmt::Debug, T>(
    what: &str,
    mut claim: impl FnMut() -> anyhow::Result<C>,
    verify: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if !host_fence_registered() {
        return verify();
    }
    let before = claim()?;
    let releases = host_fence_releases();
    let value = outside_host_fence(what, verify)
        .map_err(|error| anyhow::anyhow!("{what}: host fence not reacquired: {error}"))??;
    if host_fence_releases() != releases {
        let after = claim()?;
        anyhow::ensure!(
            after == before,
            "{what}: durable claim changed while it was verified outside the host fence ({before:?} -> {after:?})"
        );
    }
    Ok(value)
}

fn reacquire(
    store: &AgentManagementStore,
    execution: bool,
    what: &str,
) -> Result<(), AgentManagementError> {
    let lane_held = crate::management::v2::session_coordination::thread_holds_lane();
    let locks = if !lane_held {
        let guard = if execution {
            Some(
                super::provider::provider_execution_lock()
                    .lock()
                    .map_err(|_| AgentManagementError::PersistenceUnavailable)?,
            )
        } else {
            None
        };
        Some((guard, store.lock_mutations()?))
    } else {
        let deadline = std::time::Instant::now() + REACQUIRE_BUDGET;
        loop {
            let guard = if execution {
                match super::provider::provider_execution_lock().try_lock() {
                    Ok(guard) => Some(Some(guard)),
                    Err(std::sync::TryLockError::WouldBlock) => None,
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err(AgentManagementError::PersistenceUnavailable)
                    }
                }
            } else {
                Some(None)
            };
            if let Some(guard) = guard {
                if let Some(fence) = store.try_lock_delivery_mutations()? {
                    break Some((guard, fence));
                }
                drop(guard);
            }
            if std::time::Instant::now() >= deadline {
                // The commit after the effect compares the durable claim; it
                // does not need the fence, and waiting for it under a lane
                // could deadlock with a fence holder waiting for that lane.
                eprintln!("cutex-host-hold: {what}: host fence not reacquired under a session lane; finishing without it");
                break None;
            }
            let jitter = (uuid::Uuid::new_v4().as_u128() % 151) as u64;
            std::thread::sleep(std::time::Duration::from_millis(50 + jitter));
        }
    };
    if let Some((guard, fence)) = locks {
        HELD.with(|held| {
            if let Some(top) = held.borrow_mut().last_mut() {
                top.execution_guard = guard;
                top.fence = Some(fence);
            }
        });
    }
    Ok(())
}

/// In-process single flight for lifecycle entries that release the fence.
pub(crate) struct InFlight {
    keys: Vec<String>,
}

fn in_flight() -> &'static (Mutex<std::collections::BTreeSet<String>>, Condvar) {
    static IN_FLIGHT: std::sync::OnceLock<(Mutex<std::collections::BTreeSet<String>>, Condvar)> =
        std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

impl InFlight {
    /// Register every key, or none when one is already in flight. Call under
    /// the fence so registration and the durable intent are one decision.
    pub(crate) fn try_register(keys: Vec<String>) -> Result<Option<Self>, AgentManagementError> {
        let (set, _) = in_flight();
        let mut set = set.lock().map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        if keys.iter().any(|key| set.contains(key)) {
            return Ok(None);
        }
        set.extend(keys.iter().cloned());
        Ok(Some(Self { keys }))
    }

    /// Wait (holding no lock) until none of `keys` is in flight, or until
    /// `deadline`. Returns whether they cleared.
    pub(crate) fn wait_clear(keys: &[String], deadline: std::time::Instant) -> bool {
        let (set, changed) = in_flight();
        let Ok(mut guard) = set.lock() else { return false };
        loop {
            if !keys.iter().any(|key| guard.contains(key)) {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            guard = match changed.wait_timeout(guard, deadline - now) {
                Ok((guard, _)) => guard,
                Err(_) => return false,
            };
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let (set, changed) = in_flight();
        if let Ok(mut set) = set.lock() {
            for key in &self.keys {
                set.remove(key);
            }
        }
        changed.notify_all();
    }
}
