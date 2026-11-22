//! Advisory disk inventory. Request threads never open or hash historical files.
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

const CAPACITY: usize = 256;
const SNAPSHOT_TTL: Duration = Duration::from_secs(30);
const ERROR_TTL: Duration = Duration::from_secs(30);
const BYTES_PER_SECOND: u64 = 128 * 1024 * 1024;
const REGISTRY_CHECK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq, Eq, Hash)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(windows)]
    identity: (u64, [u8; 16]),
    #[cfg(windows)]
    changed: i64,
}

fn stamp(path: &Path) -> Result<Option<Stamp>, ()> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 * 1024 {
        return Err(());
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(windows)]
    let (identity, changed) = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
        };
        let identity = crate::platform::private_fs::identity(&file).map_err(|_| ())?;
        let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle().cast(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(());
        }
        ((identity.volume, identity.file_id), basic.ChangeTime)
    };
    Ok(Some(Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        identity: (metadata.dev(), metadata.ino()),
        #[cfg(unix)]
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        #[cfg(windows)]
        identity,
        #[cfg(windows)]
        changed,
    }))
}

fn digest(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let start = Instant::now();
    let mut bytes = 0u64;
    loop {
        let size = file.read(&mut buffer).ok()?;
        if size == 0 {
            break;
        }
        bytes = bytes.checked_add(size as u64)?;
        if bytes > 2 * 1024 * 1024 * 1024 || start.elapsed() > Duration::from_secs(30) {
            return None;
        }
        hash.update(&buffer[..size]);
        let expected = Duration::from_secs_f64(bytes as f64 / BYTES_PER_SECOND as f64);
        if let Some(pause) = expected.checked_sub(start.elapsed()) {
            std::thread::sleep(pause);
        }
    }
    Some(format!("{:x}", hash.finalize()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Observation {
    Pending,
    Missing,
    Unavailable,
    Verified(String),
}
struct Record {
    result: Option<(Observation, Instant)>,
    refreshing: bool,
    failures: u32,
}
struct Inventory {
    records: Arc<Mutex<HashMap<PathBuf, Record>>>,
    sender: Arc<mpsc::SyncSender<Work>>,
    maintenance: mpsc::SyncSender<Box<dyn FnOnce() + Send>>,
}
type Hasher = Arc<dyn Fn(&Path) -> Option<String> + Send + Sync>;
type RegistrySource =
    Arc<dyn Fn() -> Option<crate::launch::version_registry::NativeVersionRegistry> + Send + Sync>;
enum Work {
    Digest(PathBuf),
    Wake,
}

fn run_maintenance(work: Box<dyn FnOnce() + Send>) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
        eprintln!("Runtime inventory maintenance task failed");
    }
}

type StartupWork = Option<(Duration, Box<dyn FnOnce() + Send>)>;

impl Inventory {
    #[cfg(test)]
    fn new(compute: Hasher) -> Option<Self> {
        Self::with_registry(compute, None, REGISTRY_CHECK_INTERVAL)
    }

    fn with_registry(
        compute: Hasher,
        source: Option<RegistrySource>,
        interval: Duration,
    ) -> Option<Self> {
        Self::with_startup_work(compute, source, interval, None)
    }

    fn with_startup_work(
        compute: Hasher,
        source: Option<RegistrySource>,
        interval: Duration,
        startup_work: StartupWork,
    ) -> Option<Self> {
        let records = Arc::new(Mutex::new(HashMap::<PathBuf, Record>::new()));
        let (sender, receiver) = mpsc::sync_channel::<Work>(CAPACITY);
        let sender = Arc::new(sender);
        let (maintenance, maintenance_rx) = mpsc::sync_channel::<Box<dyn FnOnce() + Send>>(1);
        let worker_maintenance = maintenance.clone();
        // A weak sender lets test/local inventories shut down when dropped.
        let scheduler = Arc::downgrade(&sender);
        let output = records.clone();
        std::thread::Builder::new()
            .name("runtime-digest-inventory".into())
            .spawn(move || {
                let mut startup_work =
                    startup_work.map(|(delay, work)| (Instant::now() + delay, work));
                let mut memo = HashMap::<Stamp, (String, Instant)>::new();
                let mut snapshot = None;
                let mut sweep_cursor: Option<String> = None;
                let mut sweeping = false;
                let mut next_check = Instant::now();
                loop {
                    if let Ok(work) = maintenance_rx.try_recv() {
                        run_maintenance(work);
                    }
                    if startup_work
                        .as_ref()
                        .is_some_and(|(at, _)| Instant::now() >= *at)
                    {
                        // Removed before invocation: no repeat, queue growth or pycache-driven loop.
                        if let Some((_, work)) = startup_work.take() {
                            run_maintenance(work);
                        }
                    }
                    if Instant::now() >= next_check {
                        if let Some(source) = &source {
                            let current = source();
                            if current != snapshot {
                                // One cursor over this finite snapshot, not an
                                // ever-growing queue or a repeated first batch.
                                sweep_cursor = None;
                                sweeping = current.is_some();
                                snapshot = current;
                            }
                        }
                        next_check = Instant::now() + interval;
                    }
                    if sweeping {
                        if let Some(sender) = scheduler.upgrade() {
                            let inventory = Inventory {
                                records: output.clone(),
                                sender,
                                maintenance: worker_maintenance.clone(),
                            };
                            while sweeping {
                                use std::ops::Bound::{Excluded, Unbounded};
                                let paths = &snapshot
                                    .as_ref()
                                    .expect("active registry sweep")
                                    .runtime_paths;
                                let next = match &sweep_cursor {
                                    Some(cursor) => {
                                        paths.range((Excluded(cursor.clone()), Unbounded)).next()
                                    }
                                    None => paths.iter().next(),
                                }
                                .map(|(path, _)| path.clone());
                                let Some(path) = next else {
                                    sweeping = false;
                                    break;
                                };
                                inventory.observe(Path::new(&path), true);
                                if !output
                                    .lock()
                                    .ok()
                                    .is_some_and(|records| records.contains_key(Path::new(&path)))
                                {
                                    break;
                                }
                                sweep_cursor = Some(path);
                            }
                        }
                    }
                    let wake = startup_work
                        .as_ref()
                        .map_or(next_check, |(at, _)| next_check.min(*at));
                    let path = match receiver
                        .recv_timeout(wake.saturating_duration_since(Instant::now()))
                    {
                        Ok(Work::Digest(path)) => path,
                        Ok(Work::Wake) => continue,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    // A large historical digest must not push the one startup task past its
                    // due time. Digest requests remain bounded and pending during this delay.
                    if let Some((at, _)) = startup_work.as_ref() {
                        while Instant::now() < *at {
                            match maintenance_rx
                                .recv_timeout(at.saturating_duration_since(Instant::now()))
                            {
                                Ok(work) => run_maintenance(work),
                                Err(_) => break,
                            }
                        }
                        if let Some((_, work)) = startup_work.take() {
                            run_maintenance(work);
                        }
                    }
                    // All metadata and data reads happen on this sole worker.
                    let result = match stamp(&path) {
                        Ok(None) => Observation::Missing,
                        Err(()) => Observation::Unavailable,
                        Ok(Some(before)) => (|| {
                            // Identity + size + mtime + ctime + creation already
                            // invalidates changed files. Capacity, not elapsed
                            // time, bounds this advisory digest memo.
                            if let Some((sha, _)) = memo.get(&before) {
                                return Some(sha.clone());
                            }
                            let sha = compute(&path)?;
                            if stamp(&path).ok().flatten().as_ref() != Some(&before) {
                                return None;
                            }
                            if memo.len() >= CAPACITY {
                                if let Some(oldest) = memo
                                    .iter()
                                    .min_by_key(|(_, (_, at))| *at)
                                    .map(|(key, _)| key.clone())
                                {
                                    memo.remove(&oldest);
                                }
                            }
                            memo.insert(before, (sha.clone(), Instant::now()));
                            Some(sha)
                        })()
                        .map(Observation::Verified)
                        .unwrap_or(Observation::Unavailable),
                    };
                    if let Ok(mut records) = output.lock() {
                        if let Some(record) = records.get_mut(&path) {
                            record.failures = if result == Observation::Unavailable {
                                record.failures.saturating_add(1)
                            } else {
                                0
                            };
                            record.result = Some((result, Instant::now()));
                            record.refreshing = false;
                        }
                    }
                }
            })
            .ok()?;
        Some(Self {
            records,
            sender,
            maintenance,
        })
    }

    fn lookup(&self, path: &Path) -> Observation {
        self.observe(path, false)
    }

    fn observe(&self, path: &Path, force: bool) -> Observation {
        let Ok(mut records) = self.records.lock() else {
            return Observation::Unavailable;
        };
        let previous = if let Some(record) = records.get_mut(path) {
            let previous = record
                .result
                .as_ref()
                .map(|(value, _)| value.clone())
                .unwrap_or(Observation::Pending);
            if record.refreshing {
                return previous;
            }
            match &record.result {
                Some((value, at))
                    if !force
                        && at.elapsed()
                            < if *value == Observation::Unavailable {
                                ERROR_TTL
                                    .saturating_mul(
                                        1u32 << record.failures.saturating_sub(1).min(5),
                                    )
                                    .min(Duration::from_secs(600))
                            } else {
                                SNAPSHOT_TTL
                            } =>
                {
                    return value.clone();
                }
                _ => {}
            }
            record.refreshing = true;
            previous
        } else {
            if records.len() >= CAPACITY {
                // Pending work is never evicted, so repeated callers cannot fan out.
                let oldest = records
                    .iter()
                    .filter(|(_, record)| !record.refreshing)
                    .filter_map(|(path, record)| record.result.as_ref().map(|(_, at)| (path, at)))
                    .min_by_key(|(_, at)| **at)
                    .map(|(path, _)| path.clone());
                let Some(oldest) = oldest else {
                    return Observation::Pending;
                };
                records.remove(&oldest);
            }
            records.insert(
                path.into(),
                Record {
                    result: None,
                    refreshing: true,
                    failures: 0,
                },
            );
            Observation::Pending
        };
        if self.sender.try_send(Work::Digest(path.into())).is_err() {
            if previous == Observation::Pending {
                records.remove(path);
            } else if let Some(record) = records.get_mut(path) {
                record.refreshing = false;
            }
        }
        previous
    }

    #[cfg(test)]
    fn ready(&self, path: &Path) -> Option<String> {
        match self.lookup(path) {
            Observation::Verified(sha) => Some(sha),
            _ => None,
        }
    }
}

static INVENTORY: OnceLock<Option<Inventory>> = OnceLock::new();

pub(super) fn start() {
    INVENTORY.get_or_init(|| {
        Inventory::with_startup_work(
            Arc::new(digest),
            Some(Arc::new(crate::launch::version_registry::load_relaxed)),
            REGISTRY_CHECK_INTERVAL,
            // Unit tests schedule explicit fixture callbacks; their process-wide homes change.
            if cfg!(all(windows, not(test))) {
                Some((
                    Duration::from_secs(5),
                    Box::new(super::super::runtime_prune_processes::prewarm_discovery),
                ))
            } else {
                None
            },
        )
    });
}

/// One pending maintenance task, serviced before the next historical digest.
pub(super) fn maintain(work: Box<dyn FnOnce() + Send>) -> bool {
    start();
    let Some(inventory) = INVENTORY.get().and_then(Option::as_ref) else {
        return false;
    };
    if inventory.maintenance.try_send(work).is_err() {
        return false;
    }
    // A full queue implies the worker is already active and will check maintenance.
    let _ = inventory.sender.try_send(Work::Wake);
    true
}

pub(super) fn ready(path: &str) -> Observation {
    start();
    INVENTORY
        .get()
        .and_then(Option::as_ref)
        .map(|inventory| inventory.lookup(Path::new(path)))
        .unwrap_or(Observation::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[test]
    fn startup_panic_does_not_kill_the_resident_inventory_worker() {
        let root = std::env::temp_dir().join(format!("inventory-panic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("runtime.exe");
        std::fs::write(&path, b"fixture").unwrap();
        let (sender, receiver) = mpsc::channel();
        let inventory = Inventory::with_startup_work(
            Arc::new(move |_| {
                sender.send(()).unwrap();
                Some("fixture-sha".into())
            }),
            None,
            Duration::from_millis(10),
            Some((
                Duration::from_millis(20),
                Box::new(|| panic!("fixture startup panic")),
            )),
        )
        .unwrap();
        inventory.lookup(&path);
        receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        await_snapshot(
            &inventory,
            &path,
            Observation::Verified("fixture-sha".into()),
        );
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_historical_digest_cannot_delay_the_startup_prewarm() {
        let root = std::env::temp_dir().join(format!("inventory-order-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("runtime.exe");
        std::fs::write(&path, b"fixture").unwrap();
        let (events_tx, events_rx) = mpsc::channel();
        let digest_events = events_tx.clone();
        let inventory = Inventory::with_startup_work(
            Arc::new(move |_| {
                digest_events.send("digest").unwrap();
                Some("fixture-sha".into())
            }),
            None,
            Duration::from_millis(10),
            Some((
                Duration::from_millis(60),
                Box::new(move || {
                    events_tx.send("prewarm").unwrap();
                }),
            )),
        )
        .unwrap();
        inventory.lookup(&path);
        assert_eq!(
            events_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "prewarm"
        );
        assert_eq!(
            events_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "digest"
        );
        await_snapshot(
            &inventory,
            &path,
            Observation::Verified("fixture-sha".into()),
        );
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delayed_startup_work_runs_once_on_the_existing_digest_worker() {
        let root = std::env::temp_dir().join(format!("inventory-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("runtime.exe");
        std::fs::write(&path, b"fixture").unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (digest_tx, digest_rx) = mpsc::channel();
        let runs = Arc::new(AtomicUsize::new(0));
        let called = runs.clone();
        let began = Instant::now();
        let inventory = Inventory::with_startup_work(
            Arc::new(move |_| {
                digest_tx.send(std::thread::current().id()).unwrap();
                Some("fixture-sha".into())
            }),
            None,
            Duration::from_millis(10),
            Some((
                Duration::from_millis(60),
                Box::new(move || {
                    called.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(std::thread::current().id()).unwrap();
                }),
            )),
        )
        .unwrap();
        let startup_thread = started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(began.elapsed() >= Duration::from_millis(60));
        assert_ne!(startup_thread, std::thread::current().id());
        inventory.lookup(&path);
        assert_eq!(
            digest_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            startup_thread
        );
        await_snapshot(
            &inventory,
            &path,
            Observation::Verified("fixture-sha".into()),
        );
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn registry(paths: &[PathBuf]) -> crate::launch::version_registry::NativeVersionRegistry {
        use crate::launch::version_registry::{NativeVersionRecord, NativeVersionRegistry};
        let record = NativeVersionRecord {
            version: "0.167.0".into(),
            recorded_at: "2026-11-16T00:00:00Z".into(),
        };
        NativeVersionRegistry {
            schema: crate::launch::version_registry::REGISTRY_SCHEMA.into(),
            latest_cli: record.clone(),
            latest_runtime: record.clone(),
            runtime_artifacts: Default::default(),
            runtime_paths: paths
                .iter()
                .map(|path| (path.to_string_lossy().into_owned(), record.clone()))
                .collect(),
        }
    }

    // Read-only assertion: never lookup/ready/observe, which would enqueue
    // work and give a false claim that startup progresses without polling.
    fn await_snapshot(inventory: &Inventory, path: &Path, expected: Observation) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if inventory
                .records
                .lock()
                .unwrap()
                .get(path)
                .and_then(|record| record.result.as_ref())
                .is_some_and(|(result, _)| *result == expected)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "autonomous sweep did not complete: {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn registry_startup_and_revision_complete_without_any_request_or_lookup() {
        let root = std::env::temp_dir().join(format!("digest-autonomous-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let first = root.join("first");
        let alias = root.join("alias");
        let later = root.join("later");
        std::fs::write(&first, b"first").unwrap();
        std::fs::hard_link(&first, &alias).unwrap();
        std::fs::write(&later, b"later").unwrap();
        let source = Arc::new(Mutex::new(registry(&[first.clone(), alias.clone()])));
        let input = source.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let hashes = calls.clone();
        let checks = Arc::new(AtomicUsize::new(0));
        let reads = checks.clone();
        let owner = std::thread::current().id();
        let inventory = Inventory::with_registry(
            Arc::new(move |path| {
                assert_ne!(std::thread::current().id(), owner);
                hashes.fetch_add(1, Ordering::SeqCst);
                String::from_utf8(std::fs::read(path).ok()?).ok()
            }),
            Some(Arc::new(move || {
                assert_ne!(std::thread::current().id(), owner);
                reads.fetch_add(1, Ordering::SeqCst);
                Some(input.lock().unwrap().clone())
            })),
            Duration::from_millis(10),
        )
        .unwrap();
        await_snapshot(&inventory, &first, Observation::Verified("first".into()));
        await_snapshot(&inventory, &alias, Observation::Verified("first".into()));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "hard-link byte identity is memoized"
        );
        let baseline = checks.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while checks.load(Ordering::SeqCst) < baseline + 3 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "unchanged registry does not rescan executables"
        );
        // Same path set, different registry revision, replacement within the
        // 30s TTL: revision sweeps must recheck identity immediately.
        std::fs::rename(&first, root.join("old")).unwrap();
        std::fs::write(&first, b"other").unwrap();
        {
            let mut changed = source.lock().unwrap();
            changed.latest_runtime.version = "0.167.0-r61".into();
        }
        await_snapshot(&inventory, &first, Observation::Verified("other".into()));
        {
            let mut changed = source.lock().unwrap();
            let record = changed.latest_cli.clone();
            changed
                .runtime_paths
                .insert(later.to_string_lossy().into_owned(), record);
        }
        await_snapshot(&inventory, &later, Observation::Verified("later".into()));
        assert!(
            (3..=4).contains(&calls.load(Ordering::SeqCst)),
            "renaming the original can also change the hard link's change time"
        );
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn autonomous_registry_sweep_is_finite_and_capacity_bounded() {
        let root = std::env::temp_dir().join(format!("digest-sweep-cap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let paths: Vec<_> = (0..CAPACITY + 30)
            .map(|i| root.join(format!("file-{i:04}")))
            .collect();
        for path in &paths {
            std::fs::write(path, b"fixture").unwrap();
        }
        let source = registry(&paths);
        let calls = Arc::new(AtomicUsize::new(0));
        let hashes = calls.clone();
        let checks = Arc::new(AtomicUsize::new(0));
        let reads = checks.clone();
        let inventory = Inventory::with_registry(
            Arc::new(move |_| {
                hashes.fetch_add(1, Ordering::SeqCst);
                Some("fixture".into())
            }),
            Some(Arc::new(move || {
                reads.fetch_add(1, Ordering::SeqCst);
                Some(source.clone())
            })),
            Duration::from_millis(10),
        )
        .unwrap();
        await_snapshot(
            &inventory,
            paths.last().unwrap(),
            Observation::Verified("fixture".into()),
        );
        let baseline = checks.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while checks.load(Ordering::SeqCst) < baseline + 3 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let records = inventory.records.lock().unwrap();
        assert_eq!(records.len(), CAPACITY);
        assert!(records.values().all(|record| !record.refreshing));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            paths.len(),
            "all paths beyond the first batch complete once"
        );
        drop(records);
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn streamed_digest_is_exact_and_rate_bounded() {
        let path = std::env::temp_dir().join(format!("digest-rate-{}", uuid::Uuid::new_v4()));
        let bytes = vec![7; 1024 * 1024];
        std::fs::write(&path, &bytes).unwrap();
        let start = Instant::now();
        let actual = digest(&path).unwrap();
        assert!(
            start.elapsed()
                >= Duration::from_secs_f64(bytes.len() as f64 / BYTES_PER_SECOND as f64)
        );
        assert_eq!(actual, format!("{:x}", Sha256::digest(&bytes)));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn blocked_hash_is_off_request_thread_bounded_and_aliases_share_digest() {
        let root = std::env::temp_dir().join(format!("digest-inventory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let first = root.join("first");
        let second = root.join("second");
        std::fs::write(&first, b"fixture").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let count = calls.clone();
        let gate = released.clone();
        let request_thread = std::thread::current().id();
        let inventory = Inventory::new(Arc::new(move |_| {
            assert_ne!(std::thread::current().id(), request_thread);
            count.fetch_add(1, Ordering::SeqCst);
            while !gate.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Some("digest".into())
        }))
        .unwrap();
        let start = Instant::now();
        for _ in 0..1000 {
            assert_eq!(inventory.ready(&first), None);
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(inventory.records.lock().unwrap().len(), 1);
        inventory.ready(&second);
        released.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while inventory.ready(&second).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(inventory.ready(&first).as_deref(), Some("digest"));
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_is_bounded_and_missing_is_distinct_from_unavailable() {
        let root = std::env::temp_dir().join(format!("digest-bound-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let first = root.join("first");
        std::fs::write(&first, b"fixture").unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let gate = released.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let inventory = Inventory::new(Arc::new(move |_| {
            started_tx.send(()).unwrap();
            while !gate.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            None
        }))
        .unwrap();
        assert_eq!(inventory.lookup(&first), Observation::Pending);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for i in 0..1000 {
            assert_eq!(
                inventory.lookup(&root.join(format!("absent-{i}"))),
                Observation::Pending
            );
        }
        assert_eq!(inventory.records.lock().unwrap().len(), CAPACITY);
        released.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while inventory
            .records
            .lock()
            .unwrap()
            .values()
            .any(|record| record.result.is_none())
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(inventory.lookup(&first), Observation::Unavailable);
        assert_eq!(
            inventory.lookup(&root.join("absent-0")),
            Observation::Missing
        );
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_snapshot_is_served_while_refresh_rechecks_replacement_identity() {
        let root = std::env::temp_dir().join(format!("digest-replace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("exe");
        std::fs::write(&path, b"first").unwrap();
        let inventory = Inventory::new(Arc::new(|path| {
            String::from_utf8(std::fs::read(path).ok()?).ok()
        }))
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while inventory.ready(&path).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(inventory.ready(&path).as_deref(), Some("first"));
        std::fs::rename(&path, root.join("old")).unwrap();
        std::fs::write(&path, b"other").unwrap();
        inventory
            .records
            .lock()
            .unwrap()
            .get_mut(&path)
            .unwrap()
            .result
            .as_mut()
            .unwrap()
            .1 = Instant::now() - SNAPSHOT_TTL;
        assert_eq!(
            inventory.lookup(&path),
            Observation::Verified("first".into())
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(sha) = inventory.ready(&path) {
                if sha == "other" {
                    break;
                }
                assert_eq!(sha, "first");
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(inventory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_is_served_while_refreshing_and_unavailable_retries_back_off() {
        let path = std::env::temp_dir().join(format!("digest-negative-{}", uuid::Uuid::new_v4()));
        let inventory =
            Inventory::new(Arc::new(|_| panic!("missing files must not hash"))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while inventory.lookup(&path) != Observation::Missing {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        inventory
            .records
            .lock()
            .unwrap()
            .get_mut(&path)
            .unwrap()
            .result
            .as_mut()
            .unwrap()
            .1 = Instant::now() - SNAPSHOT_TTL;
        assert_eq!(inventory.lookup(&path), Observation::Missing);
        let blocked = path.with_extension("unavailable");
        inventory.records.lock().unwrap().insert(
            blocked.clone(),
            Record {
                result: Some((
                    Observation::Unavailable,
                    Instant::now() - Duration::from_secs(31),
                )),
                refreshing: false,
                failures: 2,
            },
        );
        for _ in 0..1000 {
            assert_eq!(inventory.lookup(&blocked), Observation::Unavailable);
        }
        assert!(
            !inventory.records.lock().unwrap()[&blocked].refreshing,
            "second failure backs off for 60s, not 5s"
        );
    }
}
