//! A complete filename snapshot is reusable only while a recursive OS watch remains quiet.
//! Failed watches and root replacement always fall back to discovery; no TTL grants authority.
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError},
    time::Duration,
};
use std::{
    fs::{File, OpenOptions},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT},
    Storage::FileSystem::{
        FindCloseChangeNotification, FindFirstChangeNotificationW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME,
    },
    System::Threading::WaitForSingleObject,
};
struct Watch {
    handle: HANDLE,
    root: File,
    identity: crate::platform::private_fs::FileIdentity,
}
// The change handle is only queried and closed under CACHE's mutex; it carries no borrowed memory.
unsafe impl Send for Watch {}
// WaitForSingleObject only observes this notification; Arc keeps it open during every query.
unsafe impl Sync for Watch {}
impl Drop for Watch {
    fn drop(&mut self) {
        unsafe {
            FindCloseChangeNotification(self.handle);
        }
    }
}
fn open(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .access_mode(0x80)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}
impl Watch {
    fn new(path: &Path) -> Option<Self> {
        let root = open(path).ok()?;
        let identity = crate::platform::private_fs::identity(&root).ok()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            FindFirstChangeNotificationW(
                wide.as_ptr(),
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES,
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return None;
        }
        let watch = Self {
            handle,
            root,
            identity,
        };
        watch.fresh(path).then_some(watch)
    }
    fn fresh(&self, path: &Path) -> bool {
        // Keep the old root handle alive so its file identity cannot be recycled.
        let _ = &self.root;
        (unsafe { WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT })
            && open(path)
                .ok()
                .and_then(|f| crate::platform::private_fs::identity(&f).ok())
                == Some(self.identity)
    }
}
#[derive(Clone)]
struct Entry {
    watch: Arc<Watch>,
    names: Arc<HashSet<String>>,
}
static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
// Exactly one recursive walk, whether requested by GET, POST or startup inventory.
static WALK: Mutex<()> = Mutex::new(());
const MAX_ENTRIES: usize = 500_000;
const MAX_NAME_BYTES: usize = 64 * 1024 * 1024;
thread_local! { static IO: RefCell<Option<(usize,usize)>> = const { RefCell::new(None) }; }
struct IoScope(bool);
impl IoScope {
    fn enter() -> Self {
        Self(IO.with(|io| {
            let mut io = io.borrow_mut();
            if io.is_some() {
                false
            } else {
                *io = Some((0, 0));
                true
            }
        }))
    }
}
impl Drop for IoScope {
    fn drop(&mut self) {
        if self.0 {
            IO.with(|io| *io.borrow_mut() = None);
        }
    }
}
/// Count enumerated records and an upper bound on UTF-16 filename bytes; no file data is read.
pub(super) fn visit(name_bytes: usize) -> anyhow::Result<()> {
    crate::management::v2::runtime_prune_inventory::budget::check()?;
    IO.with(|io| {
        if let Some((entries, bytes)) = io.borrow_mut().as_mut() {
            *entries = entries.saturating_add(1);
            *bytes = bytes.saturating_add(name_bytes);
            anyhow::ensure!(
                *entries <= MAX_ENTRIES && *bytes <= MAX_NAME_BYTES,
                "filename discovery IO budget exceeded"
            );
        }
        Ok(())
    })
}
fn lock<T>(mutex: &Mutex<T>) -> anyhow::Result<MutexGuard<'_, T>> {
    loop {
        crate::management::v2::runtime_prune_inventory::budget::check()?;
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => anyhow::bail!("filename inventory unavailable"),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}
fn walk_permit() -> anyhow::Result<MutexGuard<'static, ()>> {
    loop {
        crate::management::v2::runtime_prune_inventory::budget::check()?;
        match WALK.try_lock() {
            Ok(guard) => return Ok(guard),
            // The mutex protects only a permit, not data. A panicking walk publishes
            // no partial root, so releasing poison safely lets the next job retry.
            Err(TryLockError::Poisoned(error)) => {
                WALK.clear_poison();
                return Ok(error.into_inner());
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn cached(path: &Path) -> anyhow::Result<Option<Entry>> {
    let entry = lock(CACHE.get_or_init(Default::default))?
        .get(path)
        .cloned();
    // Identity checks and kernel notification queries run outside the snapshot mutex.
    Ok(entry.filter(|e| e.watch.fresh(path)))
}
pub fn collect(
    targets: &[PathBuf],
    mut walk: impl FnMut(&Path, &mut HashSet<String>) -> anyhow::Result<()>,
) -> anyhow::Result<HashSet<String>> {
    let _io = IoScope::enter();
    lock(CACHE.get_or_init(Default::default))?.retain(|path, _| targets.contains(path));
    let mut names = HashSet::new();
    for target in targets {
        crate::management::v2::runtime_prune_inventory::budget::check()?;
        if let Some(entry) = cached(target)? {
            crate::management::v2::runtime_prune_inventory::timing::count("filenameCacheHits", 1);
            names.extend(entry.names.iter().cloned());
            continue;
        }
        let waiting =
            crate::management::v2::runtime_prune_inventory::timing::stage("filename_wait");
        let _walk = walk_permit()?;
        drop(waiting);
        // A prewarm may have completed this root while the request waited.
        if let Some(entry) = cached(target)? {
            crate::management::v2::runtime_prune_inventory::timing::count("filenameCacheHits", 1);
            names.extend(entry.names.iter().cloned());
            continue;
        }
        lock(CACHE.get_or_init(Default::default))?.remove(target);
        crate::management::v2::runtime_prune_inventory::timing::count("filenameWalks", 1);
        let watch = target.is_dir().then(|| Watch::new(target)).flatten();
        let mut discovered = HashSet::new();
        walk(target, &mut discovered)?;
        names.extend(discovered.iter().cloned());
        if let Some(watch) = watch.filter(|watch| watch.fresh(target)) {
            // A completed, freshly verified root remains useful even if its caller was
            // cancelled immediately after the walk. Publication does no additional walk.
            let mut cache = CACHE
                .get_or_init(Default::default)
                .lock()
                .map_err(|_| anyhow::anyhow!("filename inventory unavailable"))?;
            if cache.len() < 256 {
                cache.insert(
                    target.clone(),
                    Entry {
                        watch: Arc::new(watch),
                        names: Arc::new(discovered),
                    },
                );
            }
        }
    }
    Ok(names)
}

#[path = "runtime_prune_discovery_job.rs"]
mod jobs;
fn ready_names(targets: &[PathBuf]) -> anyhow::Result<Option<Arc<HashSet<String>>>> {
    lock(CACHE.get_or_init(Default::default))?.retain(|path, _| targets.contains(path));
    let mut names = HashSet::new();
    for path in targets {
        crate::management::v2::runtime_prune_inventory::budget::check()?;
        if let Some(entry) = cached(path)? {
            crate::management::v2::runtime_prune_inventory::timing::count("filenameCacheHits", 1);
            names.extend(entry.names.iter().cloned());
            continue;
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        use std::os::windows::fs::MetadataExt;
        if metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0 {
            continue;
        }
        if metadata.is_dir() {
            return Ok(None);
        }
        if metadata.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.insert(name.to_lowercase());
            }
        }
    }
    Ok(Some(Arc::new(names)))
}
/// Notification IO belongs to the persistent inventory thread: exiting an issuing
/// planner thread cancels its outstanding Windows directory notification.
pub(super) fn runtime_names(targets: &[PathBuf]) -> anyhow::Result<Arc<HashSet<String>>> {
    jobs::run(targets)
}

/// Startup only: one attempt, bounded independently of an owner's waiting GET.
/// A signalled watch is never rearmed here; subsequent invalidations refresh lazily on GET.
pub(super) fn prewarm() -> anyhow::Result<()> {
    let home = crate::config::paths::runtime_dir()?;
    let layout = crate::components::kit_layout::candidate();
    prewarm_at(&home, layout.as_ref())
}
pub(super) fn prewarm_at(
    home: &Path,
    layout: Option<&crate::upgrade::layout::Layout>,
) -> anyhow::Result<()> {
    let _priority = BackgroundPriority::enter()?;
    let _budget = crate::management::v2::runtime_prune_inventory::budget::Scope::enter(
        Duration::from_secs(8),
        Default::default(),
    );
    let _io = IoScope::enter();
    let targets = prewarm_targets(home, layout)?;
    collect(&targets, super::executable_names)?;
    crate::management::v2::runtime_prune_inventory::budget::check()
}
fn prewarm_targets(
    home: &Path,
    layout: Option<&crate::upgrade::layout::Layout>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut containers = vec![(home.join("native"), false)];
    if let Some(layout) = layout {
        for kind in ["claude-runtime", "dsh-runtime", "kimi-runtime"] {
            containers.push((crate::components::components_dir(layout).join(kind), false));
        }
        containers.push((layout.runtimes_dir(), true));
    }
    let mut targets = Vec::new();
    for (container, legacy) in containers {
        visit(0)?;
        let entries = match std::fs::read_dir(container) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            visit(name.len().saturating_mul(2))?;
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "bundles" || !entry.file_type()?.is_dir() {
                continue;
            }
            if legacy
                && !name
                    .strip_prefix("claude-r")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            targets.push(entry.path());
            if targets.len() == 256 {
                return Ok(targets);
            }
        }
    }
    Ok(targets)
}
struct BackgroundPriority;
impl BackgroundPriority {
    fn enter() -> anyhow::Result<Self> {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        anyhow::ensure!(
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) } != 0,
            "background scheduling unavailable"
        );
        Ok(Self)
    }
}
impl Drop for BackgroundPriority {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
        };
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panicking_walk_releases_permit_without_publishing_a_partial_root() {
        let root = std::env::temp_dir().join(format!("prune-walk-panic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("known.exe"), b"fixture").unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            collect(&[root.clone()], |_, names| {
                names.insert("partial.exe".into());
                panic!("fixture walk panic");
            })
        }))
        .is_err());
        assert!(cached(&root).unwrap().is_none());
        let names = collect(&[root.clone()], super::super::executable_names).unwrap();
        assert!(names.contains("known.exe") && !names.contains("partial.exe"));
        collect(&[], super::super::executable_names).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resident_inventory_watch_survives_the_request_thread_exiting() {
        let root =
            std::env::temp_dir().join(format!("prune-request-exit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("known.exe"), b"fixture").unwrap();
        let target = root.clone();
        assert!(std::thread::spawn(move || runtime_names(&[target])
            .unwrap()
            .contains("known.exe"))
        .join()
        .unwrap());
        let before = cached(&root)
            .unwrap()
            .expect("request exit must not cancel inventory-owned notification IO");
        let target = root.clone();
        assert!(std::thread::spawn(move || runtime_names(&[target])
            .unwrap()
            .contains("known.exe"))
        .join()
        .unwrap());
        let after = cached(&root).unwrap().unwrap();
        assert!(Arc::ptr_eq(&before.watch, &after.watch));
        collect(&[], super::super::executable_names).unwrap();
        drop((before, after));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn startup_discovery_warms_only_managed_containers_and_stale_roots_refresh_lazily() {
        let root = std::env::temp_dir().join(format!("prune-startup-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let layout = crate::upgrade::layout::Layout::at(root.join("install"));
        for relative in [
            "home/native/r61/helper.exe",
            "home/native/bundles/ignored.exe",
            "install/components/claude-runtime/r55/src/adapter.py",
            "install/runtimes/claude-r49/legacy.py",
            "install/runtimes/unrelated/ignored.exe",
        ] {
            let file = root.join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"fixture").unwrap();
        }
        let targets = prewarm_targets(&home, Some(&layout)).unwrap();
        assert_eq!(targets.len(), 3);
        prewarm_at(&home, Some(&layout)).unwrap();
        let names = collect(&targets, |_, _| {
            panic!("startup prewarm must cover first GET")
        })
        .unwrap();
        assert!(
            names.contains("helper.exe")
                && names.contains("adapter.py")
                && names.contains("legacy.py")
        );
        assert!(!names.contains("ignored.exe"));
        std::fs::write(
            root.join("install/components/claude-runtime/r55/new.py"),
            b"fixture",
        )
        .unwrap();
        let mut walks = 0;
        let names = collect(&targets, |path, names| {
            walks += 1;
            super::super::executable_names(path, names)
        })
        .unwrap();
        assert_eq!(walks, 1);
        assert!(names.contains("new.py"));
        collect(&[], super::super::executable_names).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn finished_root_is_published_even_when_request_was_cancelled_at_completion() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = std::env::temp_dir().join(format!("prune-completed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("known.exe"), b"fixture").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let scope = crate::management::v2::runtime_prune_inventory::budget::Scope::enter(
            Duration::from_secs(1),
            cancel.clone(),
        );
        collect(&[root.clone()], |_, names| {
            names.insert("known.exe".into());
            cancel.store(true, Ordering::Release);
            Ok(())
        })
        .unwrap();
        assert!(crate::management::v2::runtime_prune_inventory::budget::check().is_err());
        drop(scope);
        assert!(collect(&[root.clone()], |_, _| panic!(
            "retry must reuse completed root"
        ))
        .unwrap()
        .contains("known.exe"));
        collect(&[], super::super::executable_names).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn waiting_get_obeys_deadline_and_does_not_start_a_second_walk() {
        let root = std::env::temp_dir().join(format!("prune-inflight-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("known.exe"), b"fixture").unwrap();
        let targets = vec![root.clone()];
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        let worker_targets = targets.clone();
        let worker = std::thread::spawn(move || {
            collect(&worker_targets, |path, names| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                super::super::executable_names(path, names)
            })
            .unwrap();
            finished_tx.send(()).unwrap();
            exit_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let scope = crate::management::v2::runtime_prune_inventory::budget::Scope::enter(
            Duration::from_millis(40),
            Default::default(),
        );
        let began = std::time::Instant::now();
        assert!(collect(&targets, |_, _| panic!("parallel discovery forbidden")).is_err());
        assert!(began.elapsed() < Duration::from_millis(500));
        drop(scope);
        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let entry = CACHE.get().unwrap().lock().unwrap().get(&root).cloned();
        assert!(
            entry.is_some(),
            "completed background discovery must publish its root"
        );
        assert!(
            entry.unwrap().watch.fresh(&root),
            "test root changed after background completion"
        );
        assert!(
            collect(&targets, |_, _| panic!("completed prewarm must be reused"))
                .unwrap()
                .contains("known.exe")
        );
        exit_tx.send(()).unwrap();
        worker.join().unwrap();
        // Windows cancels notification IO when its issuing thread exits.
        let entry = CACHE
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .get(&root)
            .cloned()
            .unwrap();
        for _ in 0..100 {
            if !entry.watch.fresh(&root) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!entry.watch.fresh(&root));
        collect(&[], super::super::executable_names).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn timed_out_get_keeps_background_discovery_alive_and_retry_uses_its_watch() {
        let root =
            std::env::temp_dir().join(format!("prune-worker-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("known.exe"), b"fixture").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        assert!(
            crate::management::v2::runtimes_api::maintain_runtime_inventory(Box::new(move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            }))
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let scope = crate::management::v2::runtime_prune_inventory::budget::Scope::enter(
            Duration::from_millis(40),
            Default::default(),
        );
        assert!(runtime_names(&[root.clone()]).is_err());
        drop(scope);
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while cached(&root).unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let before = cached(&root)
            .unwrap()
            .expect("background discovery must finish after waiting GET cancelled");
        assert!(runtime_names(&[root.clone()])
            .unwrap()
            .contains("known.exe"));
        let after = cached(&root).unwrap().unwrap();
        assert!(Arc::ptr_eq(&before.watch, &after.watch));
        collect(&[], super::super::executable_names).unwrap();
        drop((before, after));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn exhausted_io_budget_never_publishes_partial_discovery() {
        let root = std::env::temp_dir().join(format!("prune-io-limit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let targets = [root.clone()];
        assert!(collect(&targets, |_, names| {
            names.insert("partial.exe".into());
            visit(MAX_NAME_BYTES + 1)
        })
        .is_err());
        let names = collect(&targets, |_, names| {
            names.insert("complete.exe".into());
            Ok(())
        })
        .unwrap();
        assert!(names.contains("complete.exe"));
        assert!(!names.contains("partial.exe"));
        collect(&[], super::super::executable_names).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn reusable_snapshot_walks_again_after_nested_changes_and_uncacheable_roots() {
        let root = std::env::temp_dir().join(format!("prune-names-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/old.exe"), b"fixture").unwrap();
        let targets = vec![root.clone()];
        let mut walks = 0;
        let mut collect_names = || {
            collect(&targets, |path, names| {
                walks += 1;
                super::super::executable_names(path, names)
            })
        };
        assert!(collect_names().unwrap().contains("old.exe"));
        assert!(collect_names().unwrap().contains("old.exe"));
        drop(collect_names);
        assert_eq!(walks, 1);
        std::fs::rename(root.join("nested/old.exe"), root.join("nested/new.exe")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let names = collect(&targets, |path, names| {
            walks += 1;
            super::super::executable_names(path, names)
        })
        .unwrap();
        assert!(names.contains("new.exe"));
        assert!(!names.contains("old.exe"));
        assert_eq!(walks, 2);
        let file_targets = vec![root.join("nested/new.exe")];
        for _ in 0..2 {
            collect(&file_targets, |path, names| {
                walks += 1;
                super::super::executable_names(path, names)
            })
            .unwrap();
        }
        assert_eq!(walks, 4);
        collect(&[], super::super::executable_names).unwrap(); // release handles before fixture removal
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn nested_changes_and_root_replacement_invalidate_filename_snapshot() {
        let root = std::env::temp_dir().join(format!("prune-watch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let watch = Watch::new(&root).unwrap();
        assert!(watch.fresh(&root));
        std::fs::write(root.join("a/b/new.exe"), b"fixture").unwrap();
        for _ in 0..100 {
            if !watch.fresh(&root) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!watch.fresh(&root));
        let replacement_watch = Watch::new(&root).unwrap();
        let old = root.with_extension("old");
        std::fs::rename(&root, &old).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(!replacement_watch.fresh(&root));
        drop((watch, replacement_watch));
        std::fs::remove_dir_all(old).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
