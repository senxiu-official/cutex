//! Bounded process-local lock accounting. Source locations and process/thread
//! identities only; never accepts paths, session IDs, requests or payloads.
//! Every attempt is counted. Slow samples are rate limited and printed only
//! after the OS lock closes. Cross-process holders correlate by PID + ticket
//! in the daemon logs; this registry never pretends to see another process.
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{sync_channel, SyncSender},
};
static LOG_DROPS: AtomicU64 = AtomicU64::new(0);
fn reporter() -> Option<&'static SyncSender<Value>> {
    static SENDER: OnceLock<Option<SyncSender<Value>>> = OnceLock::new();
    SENDER
        .get_or_init(|| {
            let (sender, receiver) = sync_channel::<Value>(16);
            std::thread::Builder::new()
                .name("cutex-lock-reporter".into())
                .spawn(move || {
                    use std::io::Write;
                    while let Ok(value) = receiver.recv() {
                        let _ = writeln!(std::io::stderr().lock(), "cutex-lock-accounting {value}");
                    }
                })
                .ok()?;
            Some(sender)
        })
        .as_ref()
}
fn enqueue_report(sender: &SyncSender<Value>, value: Value) {
    if sender.try_send(value).is_err() {
        LOG_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}
use std::{
    collections::VecDeque,
    fs::File,
    panic::Location,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    Authority,
    AsksWrite,
    Integration,
    Usage,
}
impl Lock {
    fn label(self) -> &'static str {
        match self {
            Self::Authority => "authority",
            Self::AsksWrite => "asks_write",
            Self::Integration => "integration",
            Self::Usage => "usage",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Try,
    Blocking,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Try => "try",
            Self::Blocking => "blocking",
        }
    }
}
#[derive(Clone)]
struct Active {
    ticket: u64,
    lock: Lock,
    kind: Kind,
    caller: &'static Location<'static>,
    thread: std::thread::ThreadId,
    start: Instant,
    acquired: Option<Instant>,
    released: Option<Instant>,
    waited_for: Vec<u64>,
}
#[derive(Default)]
struct Registry {
    next: u64,
    attempts: u64,
    completed: u64,
    dropped: u64,
    active: Vec<Active>,
    recent: VecDeque<Value>,
    last_log: Option<Instant>,
}
fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(Default::default)
}
pub struct Attempt {
    active: Option<Active>,
}
impl Attempt {
    #[track_caller]
    pub fn begin(lock: Lock, kind: Kind) -> Self {
        let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
        r.attempts += 1;
        if r.active.len() >= 128 {
            r.dropped += 1;
            return Self { active: None };
        }
        r.next += 1;
        let active = Active {
            ticket: r.next,
            lock,
            kind,
            caller: Location::caller(),
            thread: std::thread::current().id(),
            start: Instant::now(),
            acquired: None,
            released: None,
            waited_for: r
                .active
                .iter()
                .filter(|a| a.lock == lock && a.acquired.is_some() && a.released.is_none())
                .map(|a| a.ticket)
                .take(16)
                .collect(),
        };
        r.active.push(active.clone());
        Self {
            active: Some(active),
        }
    }
    pub fn acquired(&mut self) {
        if let Some(a) = &mut self.active {
            a.acquired = Some(Instant::now());
            let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = r.active.iter_mut().find(|slot| slot.ticket == a.ticket) {
                slot.acquired = a.acquired;
            }
        }
    }
    /// Mark release immediately after unlocking, before either diagnostic
    /// subsystem formats/logs its record. Drop alone is sufficient for guards
    /// that are declared after this Attempt and dropped before it.
    pub fn released(&mut self) {
        if let Some(a) = &mut self.active {
            a.released = Some(Instant::now());
            let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = r.active.iter_mut().find(|slot| slot.ticket == a.ticket) {
                slot.released = a.released;
            }
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        let Some(a) = self.active.take() else { return };
        let end = a.released.unwrap_or_else(Instant::now);
        let wait = a.acquired.unwrap_or(end).duration_since(a.start);
        let hold = a
            .acquired
            .map(|t| end.duration_since(t))
            .unwrap_or_default();
        // Format outside the bookkeeping mutex. The owning FileGuard must
        // close the file before dropping this Attempt.
        let value = json!({"schema":1,"observedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"lock":a.lock.label(),"kind":a.kind.label(),"pid":std::process::id(),"ticket":a.ticket,
            "thread":format!("{:?}",a.thread),"caller":{"file":a.caller.file(),"line":a.caller.line()},
            "acquired":a.acquired.is_some(),"waitNs":wait.as_nanos().min(u64::MAX as u128) as u64,
            "holdNs":hold.as_nanos().min(u64::MAX as u128) as u64,"observedLocalHolderTickets":a.waited_for});
        let emit = {
            let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
            r.active.retain(|slot| slot.ticket != a.ticket);
            r.completed += 1;
            if r.recent.len() == 64 {
                r.recent.pop_front();
            }
            r.recent.push_back(value.clone());
            let emit = wait.max(hold) >= Duration::from_millis(250)
                && r.last_log
                    .is_none_or(|t| end.duration_since(t) >= Duration::from_secs(1));
            if emit {
                r.last_log = Some(end);
            }
            emit
        };
        if emit {
            // An AskStore guard can be nested under the host fence. Even
            // after its own unlock it must never wait for a slow stderr sink.
            if let Some(sender) = reporter() {
                enqueue_report(sender, value);
            } else {
                LOG_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
/// Owns the handle so release accounting is correct on error/unwind too.
pub struct FileGuard {
    file: Option<File>,
    attempt: Option<Attempt>,
}
impl FileGuard {
    #[track_caller]
    pub fn acquire(file: File, lock: Lock) -> std::io::Result<Self> {
        let mut attempt = Attempt::begin(lock, Kind::Blocking);
        fs2::FileExt::lock_exclusive(&file)?;
        attempt.acquired();
        Ok(Self {
            file: Some(file),
            attempt: Some(attempt),
        })
    }
    #[track_caller]
    pub fn try_acquire(file: File, lock: Lock) -> std::io::Result<Option<Self>> {
        let mut attempt = Attempt::begin(lock, Kind::Try);
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                attempt.acquired();
                Ok(Some(Self {
                    file: Some(file),
                    attempt: Some(attempt),
                }))
            }
            Err(e) if crate::platform::file_lock::lock_contended(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
impl FileGuard {
    /// A handle under a lock this thread already holds through another,
    /// accounted guard (the r271 owner admission worker's fence): no lock
    /// call and no second record.
    pub fn nested(file: File) -> Self {
        Self {
            file: Some(file),
            attempt: None,
        }
    }
}
impl Drop for FileGuard {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(a) = &mut self.attempt {
            a.released();
        }
        drop(self.attempt.take());
    }
}
pub fn snapshot() -> Value {
    let r = registry().lock().unwrap_or_else(|e| e.into_inner());
    json!({"scope":"process","pid":std::process::id(),"attempts":r.attempts,"completed":r.completed,"overflow":r.dropped,"logDrops":LOG_DROPS.load(Ordering::Relaxed),
        "active":r.active.iter().map(|a|json!({"ticket":a.ticket,"lock":a.lock.label(),"kind":a.kind.label(),"caller":{"file":a.caller.file(),"line":a.caller.line()},
            "thread":format!("{:?}",a.thread),"phase":if a.released.is_some(){"released"}else if a.acquired.is_some(){"held"}else{"waiting"},"elapsedNs":a.start.elapsed().as_nanos().min(u64::MAX as u128) as u64})).collect::<Vec<_>>(),"recent":r.recent})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waste_guard_full_reporter_queue_cannot_hold_up_business_thread() {
        let (sender, receiver) = sync_channel(1);
        sender.send(json!({"first":true})).unwrap();
        let (done, waiting) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            enqueue_report(&sender, json!({"second":true}));
            let _ = done.send(());
        });
        let completed = waiting.recv_timeout(Duration::from_secs(2)).is_ok();
        drop(receiver);
        thread.join().unwrap();
        assert!(
            completed,
            "a full diagnostics queue must drop the sample instead of delaying a lock holder"
        );
    }
    #[test]
    fn waste_guard_accounting_overflow_is_bounded_and_never_blocks_acquisition() {
        let mut attempts = Vec::new();
        for _ in 0..256 {
            let mut attempt = Attempt::begin(Lock::Usage, Kind::Try);
            attempt.acquired();
            attempts.push(attempt);
        }
        {
            let r = registry().lock().unwrap();
            assert!(r.active.len() <= 128);
            assert!(r.dropped > 0);
        }
        drop(attempts);
        let r = registry().lock().unwrap();
        assert!(!r
            .active
            .iter()
            .any(|a| a.lock == Lock::Usage && a.thread == std::thread::current().id()));
    }
    #[test]
    fn waste_guard_lock_accounting_closes_before_observation_and_tracks_try_waiter() {
        let path = std::env::temp_dir().join(format!("accounting-{}.lock", uuid::Uuid::new_v4()));
        let open = || {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let held = FileGuard::acquire(open(), Lock::Authority).unwrap();
        let ticket = held
            .attempt
            .as_ref()
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .ticket;
        assert!(FileGuard::try_acquire(open(), Lock::Authority)
            .unwrap()
            .is_none());
        let failed = snapshot()["recent"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| {
                v["observedLocalHolderTickets"]
                    .as_array()
                    .is_some_and(|xs| xs.iter().any(|v| v == ticket))
            })
            .cloned()
            .unwrap();
        assert_eq!(failed["acquired"], false);
        assert_eq!(failed["kind"], "try");
        drop(held);
        let reacquired = FileGuard::try_acquire(open(), Lock::Authority)
            .unwrap()
            .expect("release before logging");
        drop(reacquired);
        assert!(!snapshot()["active"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["ticket"] == ticket));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn waste_guard_attempt_error_and_unwind_remove_live_slot() {
        let ticket = {
            let attempt = Attempt::begin(Lock::AsksWrite, Kind::Blocking);
            attempt.active.as_ref().unwrap().ticket
        };
        assert!(!snapshot()["active"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["ticket"] == ticket));
        let result = std::panic::catch_unwind(|| {
            let mut attempt = Attempt::begin(Lock::Usage, Kind::Blocking);
            attempt.acquired();
            panic!("synthetic test unwind");
        });
        assert!(result.is_err());
        let r = registry().lock().unwrap();
        assert!(!r
            .active
            .iter()
            .any(|a| a.lock == Lock::Usage && a.thread == std::thread::current().id()));
    }
}
