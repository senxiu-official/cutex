//! r271 lock order (051 §2) for owner entries that change a session under the
//! host authority fence: take the provider execution mutex and the fence
//! holding no session lane, then only TRY the session lane (and the caller's
//! process-wide mutation lane, if any). A busy lane releases both fence and
//! execution mutex, backs off 50–200 ms with jitter and retries; past
//! [`FENCE_FIRST_BUDGET`] the entry answers a retryable `HostBusy`.
//!
//! Lifecycle takes lanes after the fence (native stop), so a lane holder that
//! waited for the fence could deadlock with it; a lane holder may also be
//! inside a native call outside the fence, and waiting for its lane under the
//! fence would hold the fence for that whole call.
//!
//! The caller opts in with [`with_fence_first`]; provider entries read it
//! through [`fence_first_session_lane`] and [`AgentManagementProvider::entry_locks`].
//! Without it (tests, lane-free internal callers) the entries keep their
//! blocking locks.
//!
//! A caller that already holds a lane it cannot give up (a mutating
//! `cutex/requests` HTTP call keeps its session lane through durable
//! settlement) never waits for the fence either: the entry then only TRIES the
//! execution mutex and the fence, nests into lanes this thread already owns,
//! tries the others, and backs off with jitter up to the same budget.
use super::*;
use crate::management::v2::session_coordination::{OperationLane, OperationReservation};

#[cfg(not(test))]
pub(crate) const FENCE_FIRST_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
pub(crate) const FENCE_FIRST_BUDGET: std::time::Duration = std::time::Duration::from_millis(400);

#[derive(Clone)]
struct FenceFirst {
    session: Option<String>,
    extra: Option<&'static OperationLane>,
}

thread_local! {
    static FENCE_FIRST: std::cell::RefCell<Option<FenceFirst>> = const { std::cell::RefCell::new(None) };
}

/// Run `entry` (one provider entry) fence-first: the provider tries
/// `session`'s lane (or the one the entry names) plus `extra_lane` under the
/// fence. Callers should hold no lane; one that does gets the try-only mode.
pub fn with_fence_first<T>(
    session: Option<&str>,
    extra_lane: Option<&'static OperationLane>,
    entry: impl FnOnce() -> T,
) -> T {
    let next = FenceFirst {
        session: session.map(str::to_string),
        extra: extra_lane,
    };
    let previous = FENCE_FIRST.with(|flag| flag.replace(Some(next)));
    struct Restore(Option<FenceFirst>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            FENCE_FIRST.with(|flag| *flag.borrow_mut() = previous);
        }
    }
    let _restore = Restore(previous);
    entry()
}

/// Whether this thread is inside [`with_fence_first`].
pub fn fence_first_active() -> bool {
    FENCE_FIRST.with(|flag| flag.borrow().is_some())
}

/// The session lane this fence-first entry must try (`None` when the entry is
/// not fence-first). The caller's subject wins over the entry's own.
pub(crate) fn fence_first_session_lane(
    entry_subject: Option<&str>,
) -> Result<Option<std::sync::Arc<OperationLane>>, AgentManagementError> {
    let session = FENCE_FIRST.with(|flag| flag.borrow().as_ref().map(|ff| ff.session.clone()));
    let Some(session) = session else {
        return Ok(None);
    };
    match session.as_deref().or(entry_subject) {
        Some(session) => crate::management::v2::session_coordination::session_lane(session)
            .map(Some)
            .map_err(|_| AgentManagementError::PersistenceUnavailable),
        None => Ok(None),
    }
}

/// Locks of one entry, released lanes first, then the fence, then the
/// execution mutex.
pub(crate) struct EntryLocks<'l> {
    _lanes: (
        Option<OperationReservation<'l>>,
        Option<OperationReservation<'static>>,
    ),
    _mutation: crate::platform::lock_accounting::FileGuard,
    _execution: Option<std::sync::MutexGuard<'static, ()>>,
}

/// [`EntryLocks`] whose fence and execution mutex are registered with
/// `host_hold` (r273, 051 step 3): the entry's runtime readiness wait runs
/// outside the fence. Lanes are released first, as before.
pub(crate) struct RegisteredEntryLocks<'l> {
    _lanes: (
        Option<OperationReservation<'l>>,
        Option<OperationReservation<'static>>,
    ),
    _hold: super::host_hold::HostHold,
}

impl<'l> EntryLocks<'l> {
    pub(crate) fn register(self, store: &AgentManagementStore) -> RegisteredEntryLocks<'l> {
        let EntryLocks { _lanes, _mutation, _execution } = self;
        RegisteredEntryLocks {
            _lanes,
            _hold: super::host_hold::HostHold::enter(store, _execution, _mutation),
        }
    }
}

/// Nest into a lane this thread owns, otherwise only try it.
fn try_lane(
    lane: &OperationLane,
) -> Result<Option<OperationReservation<'_>>, AgentManagementError> {
    would_block(if lane.held_by_current_thread() {
        lane.reserve()
    } else {
        lane.try_lock()
    })
}

fn would_block<T>(
    result: Result<T, crate::management::v2::session_coordination::ReservationError>,
) -> Result<Option<T>, AgentManagementError> {
    match result {
        Ok(reservation) => Ok(Some(reservation)),
        Err(crate::management::v2::session_coordination::ReservationError::WouldBlock) => Ok(None),
        Err(_) => Err(AgentManagementError::PersistenceUnavailable),
    }
}

impl AgentManagementProvider {
    /// An entry's execution mutex (when `execution`) and host fence, plus the
    /// fence-first lanes when the caller opted in (see module docs). A thread
    /// holding a lane only tries the mutex and the fence.
    #[track_caller]
    pub(crate) fn entry_locks<'l>(
        &self,
        session_lane: Option<&'l OperationLane>,
        execution: bool,
        what: &str,
    ) -> Result<EntryLocks<'l>, AgentManagementError> {
        let extra = FENCE_FIRST.with(|flag| flag.borrow().as_ref().map(|ff| ff.extra));
        self.locks(session_lane, extra, execution, what)
    }

    /// A reading entry's fence (no lanes, no execution mutex): blocking when
    /// lane-free, try-only with the budget under a lane.
    #[track_caller]
    pub(crate) fn fence_locks(
        &self,
        what: &str,
    ) -> Result<EntryLocks<'static>, AgentManagementError> {
        self.locks(None, None, false, what)
    }

    #[track_caller]
    fn locks<'l>(
        &self,
        session_lane: Option<&'l OperationLane>,
        extra: Option<Option<&'static OperationLane>>,
        execution: bool,
        what: &str,
    ) -> Result<EntryLocks<'l>, AgentManagementError> {
        let lane_held = crate::management::v2::session_coordination::thread_holds_lane();
        // Lane-free, the budget counts from the first busy lane: waiting for
        // the fence itself (holding nothing) is not lane contention.
        let mut deadline = None;
        loop {
            let locks = if lane_held {
                self.try_fence(execution)?
            } else {
                let held_execution = if execution {
                    Some(
                        super::provider::provider_execution_lock()
                            .lock()
                            .map_err(|_| AgentManagementError::PersistenceUnavailable)?,
                    )
                } else {
                    None
                };
                Some((held_execution, self.store().lock_mutations()?))
            };
            if let Some((held_execution, mutation)) = locks {
                let Some(extra) = extra else {
                    return Ok(EntryLocks {
                        _lanes: (None, None),
                        _mutation: mutation,
                        _execution: held_execution,
                    });
                };
                let session = match session_lane {
                    Some(lane) => try_lane(lane)?.map(Some),
                    None => Some(None),
                };
                if let Some(session) = session {
                    let extra = match extra {
                        Some(extra) => try_lane(extra)?.map(Some),
                        None => Some(None),
                    };
                    if let Some(extra) = extra {
                        return Ok(EntryLocks {
                            _lanes: (session, extra),
                            _mutation: mutation,
                            _execution: held_execution,
                        });
                    }
                }
                // Never back off holding the fence or the execution mutex.
                drop(mutation);
                drop(held_execution);
            }
            let deadline =
                *deadline.get_or_insert_with(|| std::time::Instant::now() + FENCE_FIRST_BUDGET);
            if std::time::Instant::now() >= deadline {
                return Err(AgentManagementError::HostBusy(format!(
                    "hostBusy: the agent is busy with another operation; {what} was not started"
                )));
            }
            let jitter = (uuid::Uuid::new_v4().as_u128() % 151) as u64;
            std::thread::sleep(std::time::Duration::from_millis(50 + jitter));
        }
    }

    /// Try-only execution mutex (when `execution`) and fence; `None` = busy.
    #[track_caller]
    fn try_fence(
        &self,
        execution: bool,
    ) -> Result<
        Option<(
            Option<std::sync::MutexGuard<'static, ()>>,
            crate::platform::lock_accounting::FileGuard,
        )>,
        AgentManagementError,
    > {
        let held_execution = if execution {
            match super::provider::provider_execution_lock().try_lock() {
                Ok(guard) => Some(guard),
                Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(AgentManagementError::PersistenceUnavailable);
                }
            }
        } else {
            None
        };
        Ok(self
            .store()
            .try_lock_delivery_mutations()?
            .map(|mutation| (held_execution, mutation)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::session_coordination::session_lane;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    struct Home {
        root: std::path::PathBuf,
        previous: [Option<std::ffi::OsString>; 2],
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    impl Home {
        fn new() -> Self {
            let guard = crate::config::paths::test_home_env_lock();
            let root = crate::config::paths::real_user_home()
                .unwrap()
                .join(format!(
                    ".cutex-test-fence-first-{}",
                    uuid::Uuid::new_v4().simple()
                ));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join(crate::config::paths::PRIVATE_HOME_MARKER),
                b"test",
            )
            .unwrap();
            let previous = [
                std::env::var_os("HOME"),
                std::env::var_os("CUTEX_TEST_PRIVATE_HOME"),
            ];
            unsafe {
                std::env::set_var("HOME", &root);
                std::env::set_var("CUTEX_TEST_PRIVATE_HOME", &root);
            }
            Self {
                root,
                previous,
                _guard: guard,
            }
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            for (name, previous) in ["HOME", "CUTEX_TEST_PRIVATE_HOME"]
                .into_iter()
                .zip(self.previous.iter())
            {
                unsafe {
                    match previous {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn session() -> String {
        format!("cutex.{}", uuid::Uuid::new_v4())
    }

    fn fence_first_start(
        provider: &AgentManagementProvider,
        id: &str,
        extra: Option<&'static OperationLane>,
    ) -> Result<(), AgentManagementError> {
        with_fence_first(Some(id), extra, || {
            let lane = fence_first_session_lane(None)?;
            let _locks = provider.entry_locks(lane.as_deref(), true, "start")?;
            assert!(crate::management::v2::session_coordination::thread_holds_lane());
            Ok(())
        })
    }

    /// The deadlock r271 logged as a warning: an owner start held the lane
    /// and waited for the fence while lifecycle held the fence and waited for
    /// that lane (native stop). Fence-first, the start holds nothing while it
    /// waits, so the lifecycle stop gets the lane and both finish.
    #[test]
    fn fence_first_entry_and_lifecycle_stop_both_finish() {
        let _home = Home::new();
        let id = session();
        let (fenced_tx, fenced_rx) = channel();
        let (done_tx, done_rx) = channel();
        let lifecycle = {
            let (id, done) = (id.clone(), done_tx.clone());
            std::thread::spawn(move || {
                let provider = AgentManagementProvider::open_default().unwrap();
                let _locks = provider.entry_locks(None, true, "stop").unwrap();
                fenced_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(100)); // the start queues now
                let lane = session_lane(&id).unwrap();
                let _stop = lane.reserve().unwrap(); // fence -> lane, as native stop
                std::thread::sleep(Duration::from_millis(50));
                done.send("lifecycle").unwrap();
            })
        };
        fenced_rx.recv().unwrap();
        let start = {
            let id = id.clone();
            std::thread::spawn(move || {
                let provider = AgentManagementProvider::open_default().unwrap();
                fence_first_start(&provider, &id, None).unwrap();
                done_tx.send("start").unwrap();
            })
        };
        let mut finished = vec![
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("no deadlock"),
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("no deadlock"),
        ];
        finished.sort();
        assert_eq!(finished, ["lifecycle", "start"]);
        lifecycle.join().unwrap();
        start.join().unwrap();
    }

    /// A lane busy for the whole budget is a retryable HostBusy; between
    /// tries neither the fence nor the execution mutex is held, and the
    /// budget counts from the first busy lane, not from the fence wait.
    #[test]
    fn busy_lanes_answer_host_busy_holding_nothing_between_tries() {
        let _home = Home::new();
        let provider = AgentManagementProvider::open_default().unwrap();
        static EXTRA: std::sync::OnceLock<OperationLane> = std::sync::OnceLock::new();
        let extra: &'static OperationLane = EXTRA.get_or_init(OperationLane::new);
        for busy_extra in [false, true] {
            let id = session();
            let lane = session_lane(&id).unwrap();
            let (held_tx, held_rx) = channel();
            let (release_tx, release_rx) = channel::<()>();
            let holder = {
                let lane = lane.clone();
                std::thread::spawn(move || {
                    let _held = if busy_extra {
                        extra.reserve().unwrap()
                    } else {
                        lane.reserve().unwrap()
                    };
                    held_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                })
            };
            held_rx.recv().unwrap();
            // The fence is busy first: that wait does not use the budget.
            let fence = provider.store().lock_mutations().unwrap();
            let waiter = {
                let id = id.clone();
                std::thread::spawn(move || {
                    let provider = AgentManagementProvider::open_default().unwrap();
                    let started = Instant::now();
                    (
                        fence_first_start(&provider, &id, Some(extra)),
                        started.elapsed(),
                    )
                })
            };
            std::thread::sleep(Duration::from_millis(300));
            drop(fence);
            let probe = std::thread::spawn(|| {
                let provider = AgentManagementProvider::open_default().unwrap();
                let end = Instant::now() + Duration::from_millis(300);
                let mut free = 0;
                while Instant::now() < end {
                    if let Ok(_execution) =
                        super::super::provider::provider_execution_lock().try_lock()
                    {
                        if provider
                            .store()
                            .try_lock_delivery_mutations()
                            .unwrap()
                            .is_some()
                        {
                            free += 1;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                free
            });
            let (result, elapsed) = waiter.join().unwrap();
            let error = result.unwrap_err();
            assert!(
                matches!(error, AgentManagementError::HostBusy(_)),
                "{error}"
            );
            assert_eq!(error.code(), "admission_busy");
            assert!(
                elapsed >= Duration::from_millis(300) + FENCE_FIRST_BUDGET,
                "{elapsed:?}"
            );
            assert!(
                probe.join().unwrap() > 0,
                "fence and execution mutex are free between tries"
            );
            release_tx.send(()).unwrap();
            holder.join().unwrap();
            // Released: the next fence-first entry gets all of them.
            fence_first_start(&provider, &id, Some(extra)).unwrap();
        }
    }

    /// An HTTP request keeps its session lane: under it the entry never waits
    /// for the fence (debug builds would panic), nests into its own lane and
    /// answers HostBusy when the fence stays busy; reads behave the same.
    #[test]
    fn lane_holders_only_try_the_fence() {
        let _home = Home::new();
        let provider = AgentManagementProvider::open_default().unwrap();
        static EXTRA: std::sync::OnceLock<OperationLane> = std::sync::OnceLock::new();
        let extra: &'static OperationLane = EXTRA.get_or_init(OperationLane::new);
        let id = session();
        let lane = session_lane(&id).unwrap();
        let _request = lane.reserve().unwrap(); // the HTTP request guard
        let (fenced_tx, fenced_rx) = channel();
        let (release_tx, release_rx) = channel::<()>();
        let holder = std::thread::spawn(move || {
            let provider = AgentManagementProvider::open_default().unwrap();
            let _fence = provider.store().lock_mutations().unwrap();
            fenced_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        fenced_rx.recv().unwrap();
        let started = Instant::now();
        let error = fence_first_start(&provider, &id, Some(extra)).unwrap_err();
        assert!(
            matches!(error, AgentManagementError::HostBusy(_)),
            "{error}"
        );
        assert!(started.elapsed() >= FENCE_FIRST_BUDGET);
        assert!(matches!(
            provider.fence_locks("review").map(drop),
            Err(AgentManagementError::HostBusy(_))
        ));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        fence_first_start(&provider, &id, Some(extra)).unwrap();
        provider.fence_locks("review").unwrap();
        assert!(
            lane.held_by_current_thread(),
            "the request guard is still held"
        );
    }

    /// Without the opt-in the entry keeps its blocking locks and no lane.
    #[test]
    fn entries_outside_fence_first_take_no_lane() {
        let _home = Home::new();
        let provider = AgentManagementProvider::open_default().unwrap();
        assert!(
            fence_first_session_lane(Some("cutex.any"))
                .unwrap()
                .is_none()
        );
        let _locks = provider.entry_locks(None, true, "start").unwrap();
        assert!(!crate::management::v2::session_coordination::thread_holds_lane());
        assert!(!fence_first_active());
    }
}
