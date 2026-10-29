//! Process-local asks lock diagnostics. Fixed labels only: no paths, resource
//! IDs, request IDs, caller names, payloads or error text are accepted.
//! There are 64 atomic live slots; overflow loses diagnostics, never admission.
//! Snapshotting does not acquire a store lock. Records/slow stderr output are
//! constructed only AFTER this guard has closed its OS lock handle.

use serde_json::{json, Value};
use std::cell::Cell;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const SLOTS: usize = 64;
const SLOW_NS: u64 = 250_000_000;
const LOG_INTERVAL_NS: u64 = 1_000_000_000;
const FREE: u8 = 0;
const INITIALIZING: u8 = 1;
const WAITING: u8 = 2;
const HELD: u8 = 3;
const RELEASED: u8 = 4;

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum Operation {
    Snapshot,
    Shared,
    Inspect,
    Write,
    Lookup,
    Preflight,
    LegacyProjection,
    History,
    Archive,
    Export,
    Artifact,
}
const OPERATIONS: [Operation; 11] = [
    Operation::Snapshot,
    Operation::Shared,
    Operation::Inspect,
    Operation::Write,
    Operation::Lookup,
    Operation::Preflight,
    Operation::LegacyProjection,
    Operation::History,
    Operation::Archive,
    Operation::Export,
    Operation::Artifact,
];
impl Operation {
    fn label(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Shared => "shared",
            Self::Inspect => "inspect",
            Self::Write => "write",
            Self::Lookup => "lookup",
            Self::Preflight => "preflight",
            Self::LegacyProjection => "legacy_projection",
            Self::History => "history",
            Self::Archive => "archive",
            Self::Export => "export",
            Self::Artifact => "artifact",
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum Stage {
    LockWait,
    Read,
    RefreshWait,
    SqliteSnapshot,
    Decode,
    Clone,
    Dispatch,
    PeerProjection,
    Equality,
    OwnerSync,
    RowDiff,
    SqliteCommit,
    CachePublish,
    Reclaim,
}
const STAGES: [Stage; 14] = [
    Stage::LockWait,
    Stage::Read,
    Stage::RefreshWait,
    Stage::SqliteSnapshot,
    Stage::Decode,
    Stage::Clone,
    Stage::Dispatch,
    Stage::PeerProjection,
    Stage::Equality,
    Stage::OwnerSync,
    Stage::RowDiff,
    Stage::SqliteCommit,
    Stage::CachePublish,
    Stage::Reclaim,
];
impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::LockWait => "lock_wait",
            Self::Read => "read",
            Self::RefreshWait => "refresh_wait",
            Self::SqliteSnapshot => "sqlite_snapshot",
            Self::Decode => "decode",
            Self::Clone => "clone",
            Self::Dispatch => "dispatch",
            Self::PeerProjection => "peer_projection",
            Self::Equality => "equality",
            Self::OwnerSync => "owner_sync",
            Self::RowDiff => "row_diff",
            Self::SqliteCommit => "sqlite_commit",
            Self::CachePublish => "cache_publish",
            Self::Reclaim => "reclaim",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Mode {
    Shared,
    Exclusive,
}
impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }
}

struct Slot {
    phase: AtomicU8,
    serial: AtomicU64,
    operation: AtomicU8,
    mode: AtomicU8,
    started: AtomicU64,
    acquired: AtomicU64,
    released: AtomicU64,
    last: AtomicU64,
    stage: AtomicU8,
    elapsed: [AtomicU64; STAGES.len()],
}
impl Slot {
    fn new() -> Self {
        Self {
            phase: AtomicU8::new(FREE),
            serial: AtomicU64::new(0),
            operation: AtomicU8::new(0),
            mode: AtomicU8::new(0),
            started: AtomicU64::new(0),
            acquired: AtomicU64::new(0),
            released: AtomicU64::new(0),
            last: AtomicU64::new(0),
            stage: AtomicU8::new(0),
            elapsed: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    fn switch(&self, next: Stage, now: u64) {
        let old = self.stage.load(Ordering::Relaxed) as usize;
        let last = self.last.swap(now, Ordering::Relaxed);
        self.elapsed[old].fetch_add(now.saturating_sub(last), Ordering::Relaxed);
        self.stage.store(next as u8, Ordering::Relaxed);
    }
}
struct Registry {
    origin: Instant,
    serial: AtomicU64,
    lost: AtomicU64,
    completed: AtomicU64,
    slots: [Slot; SLOTS],
    next_log: [AtomicU64; OPERATIONS.len()],
}
impl Registry {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            serial: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            slots: std::array::from_fn(|_| Slot::new()),
            next_log: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    fn now(&self) -> u64 {
        nanos(self.origin.elapsed())
    }
}
static REGISTRY: OnceLock<Registry> = OnceLock::new();
fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}
fn nanos(d: Duration) -> u64 {
    d.as_nanos().min(u64::MAX as u128) as u64
}
thread_local! {static CURRENT:Cell<Option<(usize,u64)>>=const {Cell::new(None)};}

struct Span {
    slot: usize,
    serial: u64,
    previous: Option<(usize, u64)>,
    acquired: bool,
    // TLS stage attribution and span completion must stay on the lock thread.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Span {
    fn begin(operation: Operation, mode: Mode) -> Option<Self> {
        let r = registry();
        let index = r.slots.iter().position(|s| {
            s.phase
                .compare_exchange(FREE, INITIALIZING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        });
        let Some(index) = index else {
            r.lost.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let slot = &r.slots[index];
        let now = r.now();
        let serial = r.serial.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        slot.serial.store(serial, Ordering::Relaxed);
        slot.operation.store(operation as u8, Ordering::Relaxed);
        slot.mode
            .store(matches!(mode, Mode::Exclusive) as u8, Ordering::Relaxed);
        slot.started.store(now, Ordering::Relaxed);
        slot.last.store(now, Ordering::Relaxed);
        slot.acquired.store(0, Ordering::Relaxed);
        slot.released.store(0, Ordering::Relaxed);
        slot.stage.store(Stage::LockWait as u8, Ordering::Relaxed);
        for elapsed in &slot.elapsed {
            elapsed.store(0, Ordering::Relaxed);
        }
        slot.phase.store(WAITING, Ordering::Release);
        let previous = CURRENT.with(|current| current.replace(Some((index, serial))));
        Some(Self {
            slot: index,
            serial,
            previous,
            acquired: false,
            _thread: std::marker::PhantomData,
        })
    }
    fn acquired(&mut self) {
        self.acquired = true;
        let r = registry();
        let slot = &r.slots[self.slot];
        let now = r.now();
        slot.switch(Stage::Read, now);
        slot.acquired.store(now, Ordering::Relaxed);
        slot.phase.store(HELD, Ordering::Release);
    }
    fn released(&mut self) {
        let r = registry();
        let slot = &r.slots[self.slot];
        let now = r.now();
        slot.switch(Stage::Reclaim, now);
        slot.released.store(now, Ordering::Relaxed);
        slot.phase.store(RELEASED, Ordering::Release);
        CURRENT.with(|current| {
            if current.get() == Some((self.slot, self.serial)) {
                current.set(self.previous);
            }
        });
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        let r = registry();
        let slot = &r.slots[self.slot];
        // Only LockGuard/Waiter may own a span: they always release/close first.
        debug_assert_eq!(slot.phase.load(Ordering::Acquire), RELEASED);
        let now = r.now();
        slot.switch(Stage::Reclaim, now);
        let acquired = slot.acquired.load(Ordering::Relaxed);
        let released = slot.released.load(Ordering::Relaxed);
        let wait = if self.acquired { acquired } else { released }
            .saturating_sub(slot.started.load(Ordering::Relaxed));
        let hold = if self.acquired {
            released.saturating_sub(acquired)
        } else {
            0
        };
        let reclaim = now.saturating_sub(released);
        let op = OPERATIONS[slot.operation.load(Ordering::Relaxed) as usize];
        let mode = if slot.mode.load(Ordering::Relaxed) == 0 {
            Mode::Shared
        } else {
            Mode::Exclusive
        };
        let elapsed = std::array::from_fn::<_, { STAGES.len() }, _>(|i| {
            slot.elapsed[i].load(Ordering::Relaxed)
        });
        slot.phase.store(FREE, Ordering::Release);
        r.completed.fetch_add(1, Ordering::Relaxed);
        let slow = wait.max(hold).max(reclaim) >= SLOW_NS;
        let due = &r.next_log[op as usize];
        let until = due.load(Ordering::Relaxed);
        let emit = slow
            && now >= until
            && due
                .compare_exchange(
                    until,
                    now.saturating_add(LOG_INTERVAL_NS),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok();
        #[cfg(test)]
        let emit = emit || TEST_SINK.with(|sink| sink.borrow().is_some());
        if emit {
            let stages = STAGES
                .iter()
                .enumerate()
                .filter(|(i, _)| elapsed[*i] != 0)
                .map(|(i, stage)| (stage.label().to_string(), json!(elapsed[i])))
                .collect::<serde_json::Map<_, _>>();
            let value = json!({"schema":1,"timestamp":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
                "pid":std::process::id(),"operation":op.label(),"mode":mode.label(),
                "acquisition":if self.acquired {"acquired"} else {"failed"},"released":true,
                "waitNs":wait,"holdNs":hold,"reclaimNs":reclaim,"stagesNs":stages});
            #[cfg(test)]
            if TEST_SINK.with(|sink| {
                let hook = sink.borrow_mut().take();
                if let Some(hook) = hook {
                    hook(&value);
                    true
                } else {
                    false
                }
            }) {
                return;
            }
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "cutex-asks-lock-slow {value}");
        }
    }
}

/// Waiting is visible before entering the blocking OS call. Bounded preflight
/// code can use try_shared repeatedly with this SAME waiter and same deadline.
pub struct Waiter {
    span: Option<Span>,
    untracked: Option<SuppressedCurrent>,
}
// An overflowing nested scope must not charge its stages to an outer holder.
struct SuppressedCurrent(Option<(usize, u64)>);
impl Drop for SuppressedCurrent {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.0));
    }
}
impl Waiter {
    pub fn new(operation: Operation, mode: Mode) -> Self {
        let span = Span::begin(operation, mode);
        let untracked = span
            .is_none()
            .then(|| SuppressedCurrent(CURRENT.with(|current| current.replace(None))));
        Self { span, untracked }
    }
    /// Call only after successful OS acquisition on `file`.
    pub fn acquired(mut self, file: File) -> LockGuard {
        if let Some(span) = &mut self.span {
            span.acquired();
        }
        LockGuard {
            file: Some(file),
            span: self.span.take(),
            untracked: self.untracked.take(),
        }
    }
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(mut span) = self.span.take() {
            span.released();
        }
    }
}

pub struct LockGuard {
    file: Option<File>,
    span: Option<Span>,
    untracked: Option<SuppressedCurrent>,
}
pub struct Released {
    span: Option<Span>,
}
impl LockGuard {
    pub fn acquire(file: File, operation: Operation, mode: Mode) -> io::Result<Self> {
        let waiter = Waiter::new(operation, mode);
        let result = match mode {
            Mode::Shared => fs2::FileExt::lock_shared(&file),
            Mode::Exclusive => fs2::FileExt::lock_exclusive(&file),
        };
        match result {
            Ok(()) => Ok(waiter.acquired(file)),
            Err(error) => {
                drop(file);
                drop(waiter);
                Err(error)
            }
        }
    }
    /// File close releases the OS lock BEFORE any reclaim or slow-log output.
    pub fn release(mut self) -> Released {
        drop(self.file.take());
        drop(self.untracked.take());
        let mut span = self.span.take();
        if let Some(span) = &mut span {
            span.released();
        }
        Released { span }
    }
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(mut span) = self.span.take() {
            span.released();
        }
    }
}
impl Drop for Released {
    fn drop(&mut self) {
        drop(self.span.take());
    }
}

pub struct StageGuard {
    slot: Option<(usize, u64)>,
    previous: Stage,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
pub fn stage(next: Stage) -> StageGuard {
    let identity = CURRENT.with(Cell::get);
    let mut previous = Stage::Read;
    if let Some((index, serial)) = identity {
        let r = registry();
        let slot = &r.slots[index];
        if slot.serial.load(Ordering::Relaxed) == serial
            && slot.phase.load(Ordering::Acquire) == HELD
        {
            previous = STAGES[slot.stage.load(Ordering::Relaxed) as usize];
            slot.switch(next, r.now());
        }
    }
    StageGuard {
        slot: identity,
        previous,
        _thread: std::marker::PhantomData,
    }
}
impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some((index, serial)) = self.slot {
            let r = registry();
            let slot = &r.slots[index];
            if CURRENT.with(Cell::get) == Some((index, serial))
                && slot.serial.load(Ordering::Relaxed) == serial
                && slot.phase.load(Ordering::Acquire) == HELD
            {
                slot.switch(self.previous, r.now());
            }
        }
    }
}

/// Approximate per-process active holder/waiter view; not business authority.
/// A released/reclaim slot is explicitly NOT a holder. No cross-process claim.
pub fn snapshot() -> Value {
    let r = registry();
    let now = r.now();
    let mut active = Vec::new();
    for (index, slot) in r.slots.iter().enumerate() {
        let phase = slot.phase.load(Ordering::Acquire);
        if !matches!(phase, WAITING | HELD | RELEASED) {
            continue;
        }
        let serial = slot.serial.load(Ordering::Relaxed);
        let op = slot.operation.load(Ordering::Relaxed) as usize;
        let current = slot.stage.load(Ordering::Relaxed) as usize;
        let record = json!({"slot":index,"operation":OPERATIONS[op].label(),
            "mode":if slot.mode.load(Ordering::Relaxed)==0 {"shared"} else {"exclusive"},
            "phase":match phase {WAITING=>"waiting",HELD=>"held",_=>"released_reclaim"},
            "stage":STAGES[current].label(),"ageNs":now.saturating_sub(slot.started.load(Ordering::Relaxed)),
            "stageAgeNs":now.saturating_sub(slot.last.load(Ordering::Relaxed))});
        if slot.serial.load(Ordering::Relaxed) == serial
            && slot.phase.load(Ordering::Acquire) == phase
        {
            active.push(record);
        }
    }
    json!({"schema":1,"scope":"process","pid":std::process::id(),"approximate":true,
        "capacity":SLOTS,"completed":r.completed.load(Ordering::Relaxed),"untracked":r.lost.load(Ordering::Relaxed),"active":active})
}

#[cfg(test)]
thread_local! {
    static TEST_SINK:std::cell::RefCell<Option<Box<dyn FnOnce(&Value)>>> = const {std::cell::RefCell::new(None)};
    pub(super) static RECLAIM_HOOK:std::cell::RefCell<Option<Box<dyn FnOnce(Option<&super::AskState>)>>> = const {std::cell::RefCell::new(None)};
}

#[cfg(test)]
pub(super) fn test_reclaim(draft: Option<&super::AskState>) {
    RECLAIM_HOOK.with(|hook| {
        let callback = hook.borrow_mut().take();
        if let Some(callback) = callback {
            callback(draft);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::{
        ask_state_changes, canonicalize_changed_peer_asks, note_whole_state_copy,
        persisted_state_equal, row_reuse, Requester, ASK_STATE_CACHE,
    };
    use super::super::{tests as fixtures, ActionRecord, AskState, AskStore};
    use super::*;
    use anyhow::Context;
    use std::sync::{mpsc, Arc, Mutex};

    fn fixture(label: &str) -> (AskStore, std::path::PathBuf) {
        let (store, dir) = fixtures::temp_store(label);
        std::fs::create_dir_all(&dir).unwrap();
        (store, dir)
    }
    fn can_write(store: &AskStore) -> bool {
        let file = store.lock_file().unwrap();
        fs2::FileExt::try_lock_exclusive(&file).is_ok()
    }
    fn large_store(label: &str) -> AskStore {
        let (store, _) = fixture(label);
        store
            .with_state(|state| {
                for n in 0..128 {
                    state.actions.insert(
                        format!("synthetic-{n}"),
                        ActionRecord {
                            tool: "fixture".into(),
                            request_sha256: "synthetic".into(),
                            result: Some(json!("x".repeat(16384))),
                            at: "2026-10-27T00:00:00Z".into(),
                        },
                    );
                }
            })
            .unwrap();
        store
    }

    #[test]
    fn concurrent_holder_and_waiter_are_distinct_without_payloads() {
        let (store, dir) = fixture("diagnostic-secret-path");
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let store = &store;
            let holder = scope.spawn(move || {
                let _lock = LockGuard::acquire(
                    store.lock_file().unwrap(),
                    Operation::Write,
                    Mode::Exclusive,
                )
                .unwrap();
                let _stage = stage(Stage::OwnerSync);
                held_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let waiter = scope.spawn(move || {
                let _lock = LockGuard::acquire(
                    store.lock_file().unwrap(),
                    Operation::Snapshot,
                    Mode::Shared,
                )
                .unwrap();
                done_tx.send(()).unwrap();
            });
            let end = Instant::now() + Duration::from_secs(5);
            let observed = loop {
                let view = snapshot();
                if view["active"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["operation"] == "snapshot" && v["phase"] == "waiting")
                {
                    break view;
                }
                assert!(Instant::now() < end);
                std::thread::yield_now();
            };
            assert!(observed["active"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["operation"] == "write"
                    && v["phase"] == "held"
                    && v["stage"] == "owner_sync"));
            assert!(done_rx.try_recv().is_err());
            assert!(!observed
                .to_string()
                .contains(&dir.to_string_lossy().to_string()));
            assert!(!observed.to_string().contains("diagnostic-secret-path"));
            release_tx.send(()).unwrap();
            holder.join().unwrap();
            waiter.join().unwrap();
        });
        assert!(can_write(&store));
    }

    #[test]
    fn logging_and_reclaim_are_after_unlock_and_hold_excludes_reclaim() {
        let (store, _) = fixture("diagnostic-unlocked-log");
        let log = Arc::new(Mutex::new(None));
        let output = log.clone();
        let probe = store.clone();
        TEST_SINK.with(|sink| {
            *sink.borrow_mut() = Some(Box::new(move |record| {
                assert!(
                    can_write(&probe),
                    "even blocked log output must not own the OS lock"
                );
                std::thread::sleep(Duration::from_millis(25));
                *output.lock().unwrap() = Some(record.clone());
            }))
        });
        let lock = LockGuard::acquire(
            store.lock_file().unwrap(),
            Operation::Write,
            Mode::Exclusive,
        )
        .unwrap();
        let released = lock.release();
        let identity = snapshot();
        assert!(identity["active"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["operation"] == "write" && v["phase"] == "released_reclaim"));
        assert!(can_write(&store));
        std::thread::sleep(Duration::from_millis(40));
        drop(released);
        let log = log.lock().unwrap();
        let record = log.as_ref().unwrap();
        assert_eq!(record["acquisition"], "acquired");
        assert_eq!(record["released"], true);
        assert!(record["reclaimNs"].as_u64().unwrap() >= 40_000_000);
        assert_eq!(record["stagesNs"]["reclaim"], record["reclaimNs"]);
        assert_eq!(record.as_object().unwrap().len(), 11);
        assert!(record["holdNs"].as_u64().unwrap() < record["reclaimNs"].as_u64().unwrap());
    }

    #[test]
    fn nested_stage_restores_and_unwind_closes_lock() {
        let (store, _) = fixture("diagnostic-unwind");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lock = LockGuard::acquire(
                store.lock_file().unwrap(),
                Operation::Write,
                Mode::Exclusive,
            )
            .unwrap();
            let outer = stage(Stage::Dispatch);
            {
                let _inner = stage(Stage::OwnerSync);
            }
            let id = CURRENT.with(Cell::get).unwrap();
            assert_eq!(
                registry().slots[id.0].stage.load(Ordering::Relaxed),
                Stage::Dispatch as u8
            );
            drop(outer);
            panic!("synthetic unwind");
        }));
        assert!(result.is_err());
        assert!(can_write(&store));
        assert!(CURRENT.with(Cell::get).is_none());
    }

    #[test]
    fn capacity_overflow_loses_diagnostics_never_lock_admission() {
        let (store, _) = fixture("diagnostic-capacity");
        let lost = registry().lost.load(Ordering::Relaxed);
        let mut waiters: Vec<_> = (0..SLOTS)
            .map(|_| Waiter::new(Operation::Preflight, Mode::Shared))
            .collect();
        let lock = LockGuard::acquire(
            store.lock_file().unwrap(),
            Operation::Write,
            Mode::Exclusive,
        )
        .unwrap();
        assert!(lock.span.is_none());
        assert!(registry().lost.load(Ordering::Relaxed) > lost);
        drop(lock);
        while let Some(waiter) = waiters.pop() {
            drop(waiter);
        }
        assert!(CURRENT.with(Cell::get).is_none());
        assert!(can_write(&store));
    }

    #[test]
    fn preflight_timeout_stays_bounded_and_reports_failed_not_held() {
        let (store, _) = fixture("diagnostic-timeout");
        let file = store.lock_file().unwrap();
        fs2::FileExt::lock_exclusive(&file).unwrap();
        let record = Arc::new(Mutex::new(None));
        let output = record.clone();
        TEST_SINK.with(|sink| {
            *sink.borrow_mut() = Some(Box::new(move |value| {
                *output.lock().unwrap() = Some(value.clone())
            }))
        });
        let start = Instant::now();
        assert!(store
            .lookup_snapshot_with_timeout(&[], Duration::from_millis(40))
            .unwrap()
            .is_none());
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(40));
        assert!(elapsed < Duration::from_secs(2));
        let record = record.lock().unwrap();
        let record = record.as_ref().unwrap();
        assert_eq!(record["acquisition"], "failed");
        assert_eq!(record["holdNs"], 0);
        drop(file);
        assert!(can_write(&store));
    }

    #[test]
    fn replaced_generation_survives_until_unlocked_and_new_commit_is_visible() {
        let store = large_store("diagnostic-replaced");
        let strong = store.shared().unwrap();
        let old = Arc::downgrade(&strong);
        drop(strong);
        let weak = old.clone();
        let probe = store.clone();
        RECLAIM_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |draft| {
                assert!(draft.is_none(), "changed draft moved into published cache");
                assert!(
                    weak.upgrade().is_some(),
                    "old generation is still retained for deferred destruction"
                );
                assert!(can_write(&probe));
                let current = probe.shared().unwrap();
                assert_eq!(current.next_seq, 17);
                let sql =
                    crate::management::v2::hot_store::verified_rows(&probe.database_path(), "asks")
                        .unwrap()
                        .unwrap();
                assert_eq!(
                    super::super::ask_state_from_rows(&sql.rows)
                        .unwrap()
                        .next_seq,
                    17
                );
            }))
        });
        store.with_state(|s| s.next_seq = 17).unwrap();
        assert!(
            old.upgrade().is_none(),
            "replaced state was reclaimed after unlocking"
        );
    }

    #[test]
    fn noop_large_draft_is_reclaimed_after_unlock_without_revision_or_commit() {
        let store = large_store("diagnostic-noop");
        let generation = store.store_generation().unwrap();
        let revision = store.shared().unwrap().revision;
        let probe = store.clone();
        RECLAIM_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |draft| {
                assert_eq!(draft.unwrap().actions.len(), 128);
                assert!(can_write(&probe));
                assert_eq!(probe.store_generation().unwrap(), generation);
                assert_eq!(probe.shared().unwrap().revision, revision);
            }))
        });
        store
            .with_state(|s| assert_eq!(s.actions.len(), 128))
            .unwrap();
        assert!(RECLAIM_HOOK.with(|hook| hook.borrow().is_none()));
        assert_eq!(store.store_generation().unwrap(), generation);
    }

    #[test]
    fn mutation_unwind_releases_file_and_keeps_published_state() {
        let store = large_store("diagnostic-mutation-unwind");
        let generation = store.store_generation().unwrap();
        let probe = store.clone();
        let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mark = checked.clone();
        RECLAIM_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |draft| {
                assert_eq!(draft.unwrap().next_seq, 99);
                assert!(
                    can_write(&probe),
                    "unwind closes file before draft field destruction too"
                );
                mark.store(true, Ordering::Relaxed);
            }))
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.with_state(|s| {
                s.actions.clear();
                s.next_seq = 99;
                panic!("synthetic aborted mutation");
            })
        }));
        assert!(result.is_err());
        assert!(can_write(&store));
        assert!(checked.load(Ordering::Relaxed));
        assert_eq!(store.shared().unwrap().actions.len(), 128);
        assert_eq!(store.store_generation().unwrap(), generation);
    }

    #[test]
    #[ignore = "isolated local timing probe; not a live or production benchmark"]
    fn uncontended_guard_cost_probe() {
        let (store, _) = fixture("diagnostic-cost");
        let mut base = Vec::new();
        let mut instrumented = Vec::new();
        for batch in 0..12 {
            for tracked in if batch % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = Instant::now();
                for _ in 0..500 {
                    let file = store.lock_file().unwrap();
                    if tracked {
                        let _lock =
                            LockGuard::acquire(file, Operation::Write, Mode::Exclusive).unwrap();
                    } else {
                        fs2::FileExt::lock_exclusive(&file).unwrap();
                        drop(file);
                    }
                }
                let per_lock = start.elapsed().as_secs_f64() * 1e6 / 500.;
                if tracked {
                    instrumented.push(per_lock);
                } else {
                    base.push(per_lock);
                }
            }
        }
        println!(
            "ASKS_DIAGNOSTIC_COST {}",
            json!({"unit":"us_per_open_lock_close","samples":12,"locksPerSample":500,"baseline":base,"instrumented":instrumented})
        );
    }

    fn cost_fixture(count: usize) -> AskStore {
        let (store, _) = fixture("drop-order-cost");
        store.with_state(|state| {
            for n in 0..count {
                state.actions.insert(format!("synthetic-{n}"), ActionRecord {
                    tool: "fixture".into(), request_sha256: "synthetic".into(),
                    result: Some(json!({"fragments":(0..8).map(|part|format!("part-{part}-{}","x".repeat(96))).collect::<Vec<_>>()})),
                    at: "2026-10-27T00:00:00Z".into(),
                });
            }
        }).unwrap();
        store
    }

    fn measure_write(
        store: &AskStore,
        baseline: bool,
        mutate: impl FnOnce(&mut super::super::AskState),
    ) -> Value {
        let captured = Arc::new(Mutex::new(None));
        let output = captured.clone();
        TEST_SINK.with(|sink| {
            *sink.borrow_mut() = Some(Box::new(move |value| {
                *output.lock().unwrap() = Some(value.clone());
            }))
        });
        let started = Instant::now();
        if baseline {
            baseline_with_selection(store, &[], None, mutate).unwrap();
        } else {
            store.with_state(mutate).unwrap();
        }
        let completed = nanos(started.elapsed());
        let mut value = captured
            .lock()
            .unwrap()
            .take()
            .expect("completed lock record");
        assert_eq!(value["acquisition"], "acquired");
        assert_eq!(value["released"], true);
        value["completedWriteNs"] = json!(completed);
        value
    }

    #[test]
    #[ignore = "private paired old-layout versus actual production write/drop timing; run alone"]
    fn paired_write_reclamation_probe() {
        let mut samples = Vec::new();
        for count in [4096, 16384] {
            let store = cost_fixture(count);
            let initial_generation = store.store_generation().unwrap().unwrap();
            let initial_seq = store.shared().unwrap().next_seq;
            let mut writes = 0u64;
            for round in 0..12 {
                for noop in [false, true] {
                    // Interleave arms on one warm, otherwise immutable fixture.
                    for baseline in if round % 2 == 0 {
                        [true, false]
                    } else {
                        [false, true]
                    } {
                        let (go_tx, go_rx) = mpsc::channel();
                        let (ready_tx, ready_rx) = mpsc::channel();
                        let (sample_tx, sample_rx) = mpsc::channel();
                        let foreground = std::thread::scope(|scope| {
                            let store = &store;
                            let waiter = scope.spawn(move || {
                                go_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                                ready_tx.send(()).unwrap();
                                // The waiting writer uses the same arm, so a
                                // current foreground can overlap its reclamation.
                                let record = measure_write(store, baseline, |s| s.next_seq += 1);
                                sample_tx.send(record).unwrap();
                            });
                            let foreground = measure_write(store, baseline, |s| {
                                go_tx.send(()).unwrap();
                                ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                                // Wait until the second operation has actually
                                // registered its OS waiter while this lock is held.
                                let end = Instant::now() + Duration::from_secs(5);
                                while !registry().slots.iter().any(|slot| {
                                    slot.phase.load(Ordering::Acquire) == WAITING
                                        && slot.operation.load(Ordering::Relaxed)
                                            == Operation::Write as u8
                                }) {
                                    assert!(Instant::now() < end, "writer failed to enter OS wait");
                                    std::thread::yield_now();
                                }
                                if !noop {
                                    s.next_seq += 1;
                                }
                            });
                            waiter.join().unwrap();
                            foreground
                        });
                        let waiting = sample_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        assert!(waiting["waitNs"].as_u64().unwrap() > 0);
                        writes += if noop { 1 } else { 2 };
                        let state = store.shared().unwrap();
                        assert_eq!(state.next_seq, initial_seq + writes);
                        assert_eq!(state.actions.len(), count);
                        assert_eq!(
                            store.store_generation().unwrap(),
                            Some(initial_generation + writes)
                        );
                        drop(state);
                        // No paths or fixture payloads enter the measurement.
                        samples.push(json!({"actions":count,"round":round,"foregroundNoop":noop,
                            "arm":if baseline {"baseline_inside"} else {"production_outside"},
                            "foreground":foreground,"waiter":waiting}));
                    }
                }
            }
            let loaded =
                crate::management::v2::hot_store::verified_rows(&store.database_path(), "asks")
                    .unwrap()
                    .unwrap();
            let persisted = super::super::ask_state_from_rows(&loaded.rows).unwrap();
            assert_eq!(persisted.next_seq, initial_seq + writes);
            assert_eq!(persisted.actions.len(), count);
        }
        println!(
            "ASKS_DROP_ORDER_PAIRED {}",
            json!({"schema":1,"roundsPerCase":12,
            "baseline":"pre-slice with_selection body, diagnostic guard added; deallocation inside OS lock",
            "changed":"actual AskStore::with_state production caller",
            "samples":samples})
        );
    }

    // Test-only pre-slice write/destruction ordering from store-before.rs.
    // Only self->store and the diagnostic file guard differ from that source.
    fn baseline_with_selection<T>(
        store: &AskStore,
        records: &[&str],
        action: Option<(&Requester, &str)>,
        mutate: impl FnOnce(&mut AskState) -> T,
    ) -> anyhow::Result<T> {
        use crate::management::v2::control_timing::{Path, Stage, Trace};
        let mut timing = Trace::new(Path::AsksStoreWrite);
        timing.status(500);
        timing.stage(Stage::ContextRead);
        std::fs::create_dir_all(&store.dir)?;
        let lock = store.lock_file()?;
        timing.stage(Stage::LockWait);
        let _lock = LockGuard::acquire(lock, Operation::Write, Mode::Exclusive)
            .context("ask store lock")?;
        timing.stage(Stage::StoreRead);
        if (!records.is_empty() || action.is_some()) && store.archive_configured()? {
            let mut identities: Vec<String> = records.iter().map(|id| (*id).to_owned()).collect();
            if let Some((requester, key)) = action {
                if let Some(ask) =
                    crate::asks::archive::find_by_action(&store.database_path(), requester, key)?
                {
                    identities.push(ask.ask_id);
                }
            }
            if !identities.is_empty() {
                let (_, generation) = store.read_state_shared()?;
                if generation != 0 {
                    crate::asks::archive::promote_many(
                        &store.database_path(),
                        generation,
                        &identities.iter().map(String::as_str).collect::<Vec<_>>(),
                    )?;
                }
            }
        }
        let (before, generation) = store.read_state_shared()?;
        note_whole_state_copy();
        let mut state = (*before).clone();
        timing.stage(Stage::Dispatch);
        let result = mutate(&mut state);
        timing.stage(Stage::StoreWrite);
        crate::asks::returns::promote_next_step_leases(&mut state);
        crate::peers::asks::queue_snapshots(&mut state, &before);
        canonicalize_changed_peer_asks(&before, &mut state)?;
        if !persisted_state_equal(&before, &state)? {
            store.queue_repository()?.sync_asks(&before, &state)?;
            state.revision += 1;
            let changes = ask_state_changes(&before, &state)?;
            let prior_rows = {
                let mut cache = ASK_STATE_CACHE
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache
                    .as_mut()
                    .filter(|(path, cached_generation, _, _)| {
                        path == &store.database_path() && *cached_generation == generation
                    })
                    .and_then(|(_, _, _, rows)| rows.take())
            };
            // Reuse the exact bytes committed by hot_store. On a failed commit
            // discard the companion; the last published parsed state is untouched.
            let rows = prior_rows.and_then(|rows| row_reuse::changed(rows, &changes));
            let next = crate::management::v2::hot_store::commit(
                &store.database_path(),
                "asks",
                generation,
                changes,
            )?;
            let replaced = ASK_STATE_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .replace((
                    store.database_path(),
                    next,
                    std::sync::Arc::new(state),
                    rows,
                ));
            drop(replaced);
        }
        timing.status(200);
        Ok(result)
    }
}
