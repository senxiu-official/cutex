//! Supervision and process-local health for Management's delegation workers.
use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const STALL_THRESHOLD: Duration = Duration::from_secs(120);
const HOUSEKEEPING_STALL_THRESHOLD: Duration = Duration::from_secs(15 * 60);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const FAILURE_THRESHOLD: u32 = 3;

pub static SCHEDULER: Worker = Worker::new("cutex-asks");
pub static ADMISSION: Worker = Worker::new("cutex-admission");

#[derive(Clone, Copy)]
enum Phase {
    Starting,
    Running,
    Restarting,
    StartupFailed,
}

struct Status {
    phase: Phase,
    claimed: bool,
    housekeeping: bool,
    last_progress: Instant,
    healthy_since: Option<Instant>,
    restarts: u32,
    tick_failures: u32,
    presence_failures: u32,
    panicking_sessions: BTreeSet<String>,
}

impl Status {
    fn new(now: Instant) -> Self {
        Self {
            phase: Phase::Starting,
            claimed: true,
            housekeeping: false,
            last_progress: now,
            healthy_since: None,
            restarts: 0,
            tick_failures: 0,
            presence_failures: 0,
            panicking_sessions: BTreeSet::new(),
        }
    }

    fn finding(&self, now: Instant) -> Option<&'static str> {
        match self.phase {
            _ if self.housekeeping
                && now.duration_since(self.last_progress) >= HOUSEKEEPING_STALL_THRESHOLD =>
            {
                Some("stalled")
            }
            Phase::StartupFailed => Some("startupFailed"),
            Phase::Restarting => Some("restarting"),
            _ if !self.housekeeping
                && now.duration_since(self.last_progress) >= STALL_THRESHOLD =>
            {
                Some("stalled")
            }
            _ if self.tick_failures >= FAILURE_THRESHOLD => Some("tickFailed"),
            _ if self.presence_failures >= FAILURE_THRESHOLD => Some("presenceRefreshFailed"),
            _ => None,
        }
    }
}

pub struct Worker {
    name: &'static str,
    status: Mutex<Option<Status>>,
}

impl Worker {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            name,
            status: Mutex::new(None),
        }
    }

    fn status(&self) -> MutexGuard<'_, Option<Status>> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn claim(&self) -> bool {
        let mut status = self.status();
        if status.as_ref().is_some_and(|status| status.claimed) {
            return false;
        }
        *status = Some(Status::new(Instant::now()));
        true
    }

    /// A worker owns a single OS thread, including startup and all restarts.
    pub fn spawn<E: std::fmt::Display>(
        &'static self,
        run: impl FnMut() -> Result<(), E> + Send + 'static,
    ) -> std::io::Result<()> {
        if !self.claim() {
            return Ok(());
        }
        let result = std::thread::Builder::new()
            .name(self.name.into())
            .spawn(move || self.supervise(run, std::thread::sleep));
        if result.is_err() {
            if let Some(status) = self.status().as_mut() {
                status.claimed = false;
                status.phase = Phase::StartupFailed;
            }
        }
        result.map(drop)
    }

    pub(crate) fn supervise<E: std::fmt::Display>(
        &self,
        mut run: impl FnMut() -> Result<(), E>,
        mut wait: impl FnMut(Duration),
    ) {
        let mut delay = Duration::from_secs(1);
        loop {
            let (phase, error) =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut run)) {
                    Ok(Ok(())) => return, // Production workers loop until an error or unwind.
                    Ok(Err(error)) => (Phase::StartupFailed, Some(error)),
                    Err(_) => (Phase::Restarting, None),
                };
            let restarts = {
                let mut state = self.status();
                let status = state.get_or_insert_with(|| Status::new(Instant::now()));
                if status
                    .healthy_since
                    .take()
                    .is_some_and(|since| since.elapsed() >= MAX_BACKOFF)
                {
                    delay = Duration::from_secs(1);
                }
                status.phase = phase;
                status.housekeeping = false;
                status.tick_failures = 0;
                status.presence_failures = 0;
                status.restarts = status.restarts.saturating_add(1);
                status.restarts
            };
            if let Some(error) = error {
                eprintln!(
                    "{} startup failed ({error}); retry {restarts} in {delay:?}",
                    self.name
                );
            } else {
                eprintln!("{} panicked; restart {restarts} in {delay:?}", self.name);
            }
            wait(delay);
            delay = (delay * 2).min(MAX_BACKOFF);
        }
    }

    pub fn housekeeping<T>(&self, run: impl FnOnce() -> T) -> T {
        if let Some(status) = self.status().as_mut() {
            status.housekeeping = true;
            status.last_progress = Instant::now();
        }
        let result = run();
        if let Some(status) = self.status().as_mut() {
            status.housekeeping = false;
            status.last_progress = Instant::now();
        }
        result
    }

    pub fn tick_finished(&self, ok: bool) {
        if let Some(status) = self.status().as_mut() {
            let now = Instant::now();
            status.last_progress = now;
            if ok {
                status.tick_failures = 0;
                status.phase = Phase::Running;
                status.healthy_since.get_or_insert(now);
            } else {
                status.tick_failures = status.tick_failures.saturating_add(1);
                status.healthy_since = None;
            }
        }
    }

    pub fn presence_finished(&self, ok: bool) {
        if let Some(status) = self.status().as_mut() {
            status.presence_failures = if ok {
                0
            } else {
                status.presence_failures.saturating_add(1)
            };
        }
    }

    /// Requeue a session that panics without stopping the rest of the batch.
    pub fn run_admission_session(
        &self,
        session: &str,
        run: impl FnOnce(),
        requeue: impl FnOnce(&str),
    ) {
        let completed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_ok();
        if let Some(status) = self.status().as_mut() {
            if completed {
                status.panicking_sessions.remove(session);
            } else {
                status.panicking_sessions.insert(session.into());
            }
        }
        if !completed {
            requeue(session);
        }
    }

    pub(crate) fn panicking_sessions(&self) -> Vec<String> {
        self.status().as_ref().map_or_else(Vec::new, |status| {
            status.panicking_sessions.iter().cloned().collect()
        })
    }

    pub(crate) fn finding(&self) -> Option<(&'static str, u32)> {
        let status = self.status();
        let status = status.as_ref()?;
        Some((status.finding(Instant::now())?, status.restarts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn scheduler_health_restarts_serially_after_startup_error_and_worker_panic() {
        let worker = Worker::new("fault-injection");
        assert!(worker.claim());
        assert!(!worker.claim());
        let active = Cell::new(false);
        struct Active<'a>(&'a Cell<bool>);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let attempts = Cell::new(0);
        let observed = RefCell::new(Vec::new());
        worker.supervise(
            || {
                assert!(!active.replace(true));
                let _active = Active(&active);
                attempts.set(attempts.get() + 1);
                match attempts.get() {
                    1 => Err("injected ENOSPC in startup"),
                    2 => panic!("injected worker panic"),
                    _ => {
                        worker.tick_finished(true);
                        Ok(())
                    }
                }
            },
            |delay| {
                assert!(!active.get());
                assert!(!worker.claim());
                observed
                    .borrow_mut()
                    .push((delay, worker.finding().unwrap()));
            },
        );
        assert_eq!(
            *observed.borrow(),
            [
                (Duration::from_secs(1), ("startupFailed", 1)),
                (Duration::from_secs(2), ("restarting", 2)),
            ]
        );
        assert_eq!(attempts.get(), 3);
        assert_eq!(worker.finding(), None);
    }

    #[test]
    fn scheduler_health_bounds_backoff_and_requires_repeated_failures() {
        let worker = Worker::new("test");
        assert!(worker.claim());
        let tries = Cell::new(0);
        let mut delays = Vec::new();
        worker.supervise(
            || {
                tries.set(tries.get() + 1);
                if tries.get() <= 8 {
                    Err("startup failure")
                } else {
                    Ok(())
                }
            },
            |delay| delays.push(delay.as_secs()),
        );
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
        worker.tick_finished(true);
        for _ in 1..FAILURE_THRESHOLD {
            worker.tick_finished(false);
            worker.presence_finished(false);
            assert_eq!(worker.finding(), None);
        }
        worker.tick_finished(false);
        assert_eq!(worker.finding().unwrap().0, "tickFailed");
        worker.tick_finished(true);
        worker.presence_finished(false);
        assert_eq!(worker.finding().unwrap().0, "presenceRefreshFailed");
        worker.presence_finished(true);
        let mut status = worker.status();
        let status = status.as_mut().unwrap();
        let future = status.last_progress + STALL_THRESHOLD;
        assert_eq!(status.finding(future), Some("stalled"));
        status.housekeeping = true;
        assert_eq!(status.finding(future), None);
        let overdue = status.last_progress + HOUSEKEEPING_STALL_THRESHOLD;
        assert_eq!(status.finding(overdue - Duration::from_secs(1)), None);
        assert_eq!(status.finding(overdue), Some("stalled"));
        status.phase = Phase::StartupFailed;
        assert_eq!(status.finding(overdue), Some("stalled"));
    }
}
