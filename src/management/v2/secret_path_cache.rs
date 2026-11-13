//! Cached private namespaces; target identity is obtained from one open handle.
use super::{canonicalize_existing_prefix, FileIdentity};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

struct OpenedPath {
    path: PathBuf,
    identity: FileIdentity,
    _file: std::fs::File,
}

impl OpenedPath {
    fn open(path: &Path) -> std::io::Result<Self> {
        #[cfg(test)]
        super::record_identity_read(path);
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, GetFileInformationByHandleEx, GetFileInformationByHandle, FileIdInfo, FILE_ID_INFO, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE};
            let file = std::fs::OpenOptions::new().read(true).access_mode(0)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
            let raw = file.as_raw_handle();
            let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
            let identity = if unsafe { GetFileInformationByHandleEx(raw, FileIdInfo, &mut info as *mut _ as *mut _, std::mem::size_of_val(&info) as u32) } != 0 {
                FileIdentity { volume: info.VolumeSerialNumber, id: u128::from_le_bytes(info.FileId.Identifier) }
            } else {
                // Older/removable filesystems may only expose the legacy ID.
                let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
                if unsafe { GetFileInformationByHandle(raw, &mut info) } == 0 { return Err(std::io::Error::last_os_error()); }
                FileIdentity { volume: u64::from(info.dwVolumeSerialNumber), id: ((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)) as u128 }
            };
            let mut buffer = vec![0_u16; 512];
            loop {
                let size = unsafe { GetFinalPathNameByHandleW(raw, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
                if size == 0 { return Err(std::io::Error::last_os_error()); }
                if (size as usize) < buffer.len() {
                    use std::os::windows::ffi::OsStringExt;
                    let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..size as usize]));
                    return Ok(Self { path, identity, _file: file });
                }
                buffer.resize(size as usize + 1, 0);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            use std::os::fd::AsRawFd;
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(target_os = "linux")]
            options.custom_flags(libc::O_PATH | libc::O_CLOEXEC);
            #[cfg(not(target_os = "linux"))]
            options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
            let file = options.open(path)?;
            let metadata = file.metadata()?;
            #[cfg(target_os = "linux")]
            let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            #[cfg(not(target_os = "linux"))]
            let path = std::fs::canonicalize(path)?;
            Ok(Self { path, identity: FileIdentity { volume: metadata.dev(), id: metadata.ino() as u128 }, _file: file })
        }
    }
}

pub(super) fn inspect_credential_file(path: &Path) -> Option<(PathBuf, FileIdentity)> {
    let opened = OpenedPath::open(path).ok()?;
    if !opened._file.metadata().ok()?.is_file() { return None; }
    Some((opened.path, opened.identity))
}

struct SavedPath { path: PathBuf, identity: FileIdentity }
impl From<OpenedPath> for SavedPath {
    fn from(opened: OpenedPath) -> Self { Self { path: opened.path, identity: opened.identity } }
}

struct Namespace {
    key: Vec<String>,
    root: Option<SavedPath>,
}
impl Namespace {
    fn new(path: &Path) -> Self {
        let root = OpenedPath::open(path).ok();
        let canonical = root.as_ref().map(|v| v.path.clone())
            .or_else(|| canonicalize_existing_prefix(path)).unwrap_or_else(|| path.to_path_buf());
        Self { key: crate::artifacts::paths::key(&canonical), root: root.map(Into::into) }
    }
    fn contains(&self, key: &[String], identity: Option<&FileIdentity>) -> bool {
        key.starts_with(&self.key)
            || self.root.as_ref().is_some_and(|root| identity == Some(&root.identity))
    }
}

struct Snapshot {
    relay: Namespace,
    config_key: Vec<String>,
    profile_link: Namespace,
    // Preserve known final-file hard-link identities without reopening every
    // protected file for every request. Bounded even if the directory is abused.
    files: Vec<SavedPath>,
}
impl Snapshot {
    fn load(home: &Path) -> Self {
        let relay_path = home.join("management-relay");
        let config_path = home.join("config.json");
        let config = OpenedPath::open(&config_path).ok();
        let config_final = config.as_ref().map(|v| v.path.clone())
            .or_else(|| canonicalize_existing_prefix(&config_path)).unwrap_or(config_path);
        let mut files: Vec<SavedPath> = config.into_iter().map(Into::into).collect();
        if let Ok(file) = OpenedPath::open(&home.join("runtime/profile-link/secret-v1")) { files.push(file.into()); }
        if let Ok(entries) = std::fs::read_dir(&relay_path) {
            files.extend(entries.flatten().take(64).filter_map(|entry| OpenedPath::open(&entry.path()).ok().map(Into::into)));
        }
        Self {
            relay: Namespace::new(&relay_path),
            config_key: crate::artifacts::paths::key(&config_final),
            profile_link: Namespace::new(&home.join("runtime/profile-link")),
            files,
        }
    }
    fn matches(&self, path: &Path, identity: Option<&FileIdentity>, relay_only: bool) -> bool {
        let key = crate::artifacts::paths::key(path);
        if self.relay.contains(&key, identity) { return true; }
        if !relay_only && (key == self.config_key || self.profile_link.contains(&key, identity)) { return true; }
        self.files.iter().any(|file| {
            identity == Some(&file.identity)
                && (!relay_only || crate::artifacts::paths::key(&file.path).starts_with(&self.relay.key))
        })
    }
}

struct Cached {
    home: PathBuf,
    generation: u64,
    external_stamps: [Option<crate::platform::file_stamp::FileStamp>; 2],
    snapshot: Arc<Snapshot>,
}
static GENERATION: AtomicU64 = AtomicU64::new(0);
static CACHE: OnceLock<RwLock<Option<Cached>>> = OnceLock::new();

fn snapshot() -> Option<Arc<Snapshot>> {
    let home = crate::config::paths::config_dir().ok()?;
    // These files also have official writers in other processes. Two cheap
    // metadata stamps preserve their fresh hard-link checks without reopening
    // the relay root or walking ancestor identities on every request.
    let external_stamps = [
        crate::platform::file_stamp::FileStamp::of(&home.join("config.json")),
        crate::platform::file_stamp::FileStamp::of(&home.join("runtime/profile-link/secret-v1")),
    ];
    let cache = CACHE.get_or_init(|| RwLock::new(None));
    loop {
        let generation = GENERATION.load(Ordering::Acquire);
        {
            let guard = cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = guard.as_ref() {
                if cached.home == home && cached.generation == generation && cached.external_stamps == external_stamps { return Some(cached.snapshot.clone()); }
            }
        }
        let mut guard = cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if GENERATION.load(Ordering::Acquire) != generation { continue; }
        if let Some(cached) = guard.as_ref() {
            if cached.home == home && cached.generation == generation && cached.external_stamps == external_stamps { return Some(cached.snapshot.clone()); }
        }
        // Single-flight cold initialization; no per-request root reopens when
        // several workers observe the same new generation concurrently.
        let built = Arc::new(Snapshot::load(&home));
        if GENERATION.load(Ordering::Acquire) != generation { continue; }
        *guard = Some(Cached { home: home.clone(), generation, external_stamps, snapshot: built.clone() });
        return Some(built);
    }
}

pub(super) fn prime() { let _ = snapshot(); }
pub(super) fn refresh() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    prime();
}

/// Caller has already performed the A1 network/reparse/device preflight. That
/// separate guard remains necessary before opening a target that might be SMB.
pub(super) fn classify_validated(path: &Path, relay_only: bool) -> bool {
    let Some(snapshot) = snapshot() else { return false };
    match OpenedPath::open(path) {
        Ok(target) => snapshot.matches(&target.path, Some(&target.identity), relay_only),
        // Protect fixed namespaces even if a concurrent unlink removed a leaf
        // after the caller resolved it; never turn a missing secret into allowed.
        Err(_) => snapshot.matches(path, None, relay_only),
    }
}

/// Aggregate reads such as Git diff cannot include any protected namespace,
/// including config/profile-link when no relay directory has been created.
pub(super) fn tree_contains_private(path: &Path) -> bool {
    let Some(snapshot) = snapshot() else { return true; };
    let path = canonicalize_existing_prefix(path).unwrap_or_else(|| path.to_path_buf());
    let key = crate::artifacts::paths::key(&path);
    snapshot.config_key.starts_with(&key)
        || snapshot.profile_link.key.starts_with(&key)
        || snapshot.relay.key.starts_with(&key)
}

// Each request pins an immutable index. Only the background worker enumerates
// credentials; it rebuilds after directory stamps change, never after a TTL.
#[cfg(test)]
thread_local! {
    pub(super) static CREDENTIAL_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(1_000_000) };
    pub(super) static CREDENTIAL_IDLE_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(600_000) };
}
#[derive(Clone, PartialEq, Eq)]
struct CredentialStamp {
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    len: u64,
    identity_only: bool,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
fn credential_stamp(path: &Path) -> Option<CredentialStamp> {
    if crate::artifacts::paths::validate_resolved_local_path(path).is_err() { return None; }
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(CredentialStamp { modified: metadata.modified().ok(), created: metadata.created().ok(), len: metadata.len(), identity_only: false,
        #[cfg(unix)]
        identity: (metadata.dev(), metadata.ino(), metadata.ctime(), metadata.ctime_nsec()),
    })
}
fn credential_watch_stamp(path: &Path, identity_only: bool) -> Option<CredentialStamp> {
    let mut stamp = credential_stamp(path)?;
    if identity_only {
        stamp.identity_only = true;
        stamp.modified = None;
        stamp.len = 0;
        #[cfg(unix)]
        { stamp.identity.2 = 0; stamp.identity.3 = 0; }
    }
    Some(stamp)
}
fn watch_changed(path: &Path, stamp: &Option<CredentialStamp>) -> bool {
    credential_watch_stamp(path, stamp.as_ref().is_some_and(|stamp| stamp.identity_only)) != *stamp
}
struct CredentialNamespace { lexical: Vec<String>, canonical: Vec<String>, tree: bool, runtime: bool }
impl CredentialNamespace {
    fn contains(&self, path: &[String], canonical: bool) -> bool {
        let root = if canonical { &self.canonical } else { &self.lexical };
        if self.runtime { super::credential_locations::runtime_secret_key(root, path) }
        else if self.tree { path.starts_with(root) }
        else { super::credential_locations::same_file_family(root, path) }
    }
}
struct CredentialIndex {
    locations: Vec<CredentialNamespace>,
    known: std::collections::HashSet<(u64, u128)>,
    ready: bool,
    limited: bool,
}
impl CredentialIndex {
    fn empty(roots: &[super::credential_locations::Location]) -> Self {
        use super::credential_locations::key;
        let locations = roots.iter().map(|root| {
            let real = if crate::artifacts::paths::validate_resolved_local_path(&root.path).is_ok() {
                canonicalize_existing_prefix(&root.path).unwrap_or_else(|| root.path.clone())
            } else { root.path.clone() };
            CredentialNamespace { lexical: key(&root.path), canonical: key(&real), tree: root.tree, runtime: root.scan == super::credential_locations::IdentityScan::Runtime }
        }).collect();
        Self { locations, known: std::collections::HashSet::new(), ready: false, limited: false }
    }
    fn build(roots: &[super::credential_locations::Location], limit: usize) -> (Self, Vec<(PathBuf, Option<CredentialStamp>)>) {
        use super::credential_locations::{self, IdentityScan};
        let mut index = Self::empty(roots);
        let mut watched = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut pending = roots.iter().map(|root| (root.path.clone(), root.tree, root.scan, 0usize)).collect::<Vec<_>>();
        let mut remaining = limit;
        while let Some((candidate, recurse, scan, depth)) = pending.pop() {
            if remaining == 0 { index.limited = true; break; }
            remaining -= 1;
            if crate::artifacts::paths::validate_resolved_local_path(&candidate).is_err() { continue; }
            if credential_locations::skip_identity_subtree(&candidate) { continue; }
            let stamp = credential_watch_stamp(&candidate, matches!(scan, IdentityScan::ToolHome | IdentityScan::Runtime));
            let opened = match OpenedPath::open(&candidate) {
                Ok(opened) => opened,
                Err(_) => { watched.push((candidate, stamp)); continue; }
            };
            let Ok(metadata) = opened._file.metadata() else { continue; };
            if metadata.is_file() { index.known.insert((opened.identity.volume, opened.identity.id)); }
            if !recurse || metadata.is_dir() { watched.push((candidate.clone(), stamp)); }
            if (!recurse && scan != IdentityScan::Runtime) || !metadata.is_dir() || !visited.insert((opened.identity.volume, opened.identity.id)) { continue; }
            let mut push = |path, recurse, policy| {
                if pending.len() < remaining { pending.push((path, recurse, policy, depth + 1)); true }
                else { index.limited = true; false }
            };
            if scan == IdentityScan::Runtime {
                for (name, policy) in credential_locations::RUNTIME_IDENTITY_TREES { push(opened.path.join(name), true, *policy); }
                for name in credential_locations::RUNTIME_IDENTITY_FILES { push(opened.path.join(name), false, IdentityScan::Full); }
                continue;
            }
            if matches!(scan, IdentityScan::ToolHome | IdentityScan::AccountHomes) {
                // Never descend into transcript, project, cache or tool-output
                // directories of an agent home. Probe only credential writers.
                for name in credential_locations::TOOL_CREDENTIAL_FILES { push(opened.path.join(name), false, IdentityScan::Full); }
                for name in credential_locations::TOOL_CREDENTIAL_DIRS { push(opened.path.join(name), true, IdentityScan::Full); }
                if depth < 8 {
                    for name in credential_locations::NESTED_TOOL_HOMES { push(opened.path.join(name), true, IdentityScan::ToolHome); }
                }
                if scan == IdentityScan::ToolHome { continue; }
            }
            if let Ok(entries) = std::fs::read_dir(&opened.path) {
                for entry in entries.flatten() {
                    if scan == IdentityScan::AccountHomes {
                        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) { continue; }
                        let grouping = matches!(entry.file_name().to_str(), Some(".login" | "login" | "claude" | "kimi" | "codex" | "homes"));
                        if depth < 8 && !push(entry.path(), true, if grouping { IdentityScan::AccountHomes } else { IdentityScan::ToolHome }) { break; }
                    } else if !push(entry.path(), true, IdentityScan::Full) { break; }
                }
            }
        }
        for root in roots {
            if !watched.iter().any(|(path, _)| path == &root.path) {
                watched.push((root.path.clone(), credential_watch_stamp(&root.path, matches!(root.scan, IdentityScan::ToolHome | IdentityScan::Runtime))));
            }
        }
        index.ready = true;
        (index, watched)
    }
}
struct CredentialSnapshot {
    index: RwLock<Arc<CredentialIndex>>,
    builds: AtomicU64,
    starts: AtomicU64,
    published_generation: AtomicU64,
    running: AtomicBool,
    last_used: std::sync::Mutex<std::time::Instant>,
    roots: Vec<super::credential_locations::Location>,
    home: PathBuf,
    external: bool,
    limit: AtomicUsize,
    idle: std::time::Duration,
}
struct CachedCredentials { home: PathBuf, snapshot: Arc<CredentialSnapshot> }
static CREDENTIAL_CACHE: OnceLock<RwLock<Option<CachedCredentials>>> = OnceLock::new();
fn credential_snapshot() -> Arc<CredentialSnapshot> {
    let cache = CREDENTIAL_CACHE.get_or_init(|| RwLock::new(None));
    let home = crate::config::paths::config_dir().unwrap_or_default();
    let existing = cache.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref()
        .filter(|cached| cached.home == home).map(|cached| cached.snapshot.clone());
    if let Some(snapshot) = existing { activate_credentials(&snapshot); return snapshot; }
    let roots = super::credential_locations::locations();
    #[cfg(test)]
    let idle = std::time::Duration::from_millis(CREDENTIAL_IDLE_MS.with(|value| value.get()));
    #[cfg(not(test))]
    let idle = std::time::Duration::from_secs(600);
    let built = Arc::new(CredentialSnapshot {
        index: RwLock::new(Arc::new(CredentialIndex::empty(&roots))), builds: AtomicU64::new(0), starts: AtomicU64::new(0),
        published_generation: AtomicU64::new(0), running: AtomicBool::new(false), last_used: std::sync::Mutex::new(std::time::Instant::now()),
        roots, home: home.clone(), external: !cfg!(test) && !crate::config::paths::real_home_forbidden(), limit: AtomicUsize::new(1_000_000), idle,
    });
    let mut guard = cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = if let Some(cached) = guard.as_ref().filter(|cached| cached.home == home) {
        cached.snapshot.clone()
    } else {
        *guard = Some(CachedCredentials { home, snapshot: built.clone() }); built
    };
    drop(guard);
    activate_credentials(&result);
    result
}
fn activate_credentials(snapshot: &Arc<CredentialSnapshot>) {
    // Only narrowed-token paths call this; daemon startup, full-token requests
    // and config saves leave the lazy worker and its idle clock untouched.
    let mut used = snapshot.last_used.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *used = std::time::Instant::now();
    #[cfg(test)]
    snapshot.limit.store(CREDENTIAL_LIMIT.with(|value| value.get()), Ordering::Release);
    if snapshot.running.swap(true, Ordering::AcqRel) { return; }
    drop(used);
    snapshot.starts.fetch_add(1, Ordering::Relaxed);
    let weak = Arc::downgrade(snapshot);
    if std::thread::Builder::new().name("credential-index".into()).spawn(move || {
        let mut watched: Vec<(PathBuf, Option<CredentialStamp>)> = Vec::new();
        let mut dirty_since = Some(std::time::Instant::now());
        let mut seen_generation = 0;
        let mut seen_limit = 0;
        loop {
            let Some(snapshot) = weak.upgrade() else { break; };
            let used = snapshot.last_used.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if used.elapsed() >= snapshot.idle || Arc::strong_count(&snapshot) == 1 {
                snapshot.running.store(false, Ordering::Release);
                break;
            }
            drop(used);
            let generation = GENERATION.load(Ordering::Acquire);
            let limit = snapshot.limit.load(Ordering::Acquire);
            let ready = snapshot.index.read().unwrap_or_else(std::sync::PoisonError::into_inner).ready;
            if watched.is_empty() || generation != seen_generation || limit != seen_limit
                || watched.iter().any(|(path, stamp)| watch_changed(path, stamp)) {
                dirty_since.get_or_insert_with(std::time::Instant::now);
            }
            // Batch changes for three seconds, retaining the ready index. The
            // initial lazy build is immediate; active file churn never resets it.
            if !ready || dirty_since.is_some_and(|since| since.elapsed() >= std::time::Duration::from_secs(3)) {
                let mut current_roots = snapshot.roots.clone();
                let manifest_path = snapshot.home.join(super::launch_credentials::MANIFEST);
                let mut descriptors = vec![(manifest_path.clone(), credential_stamp(&manifest_path))];
                current_roots.extend(super::launch_credentials::paths(&snapshot.home).into_iter().map(|path| super::credential_locations::Location { path, tree: false, scan: super::credential_locations::IdentityScan::Full }));
                if snapshot.external {
                    for relative in ["runtime/claude/deployment.json", "runtime/light/deployment.json", "hosts.json"] {
                        let path = snapshot.home.join(relative);
                        descriptors.push((path.clone(), credential_stamp(&path)));
                    }
                    current_roots.extend(super::credential_locations::deployment_locations(&snapshot.home));
                }
                let (index, mut next) = CredentialIndex::build(&current_roots, limit);
                next.extend(descriptors);
                {
                    let mut published = snapshot.index.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                    *published = Arc::new(index);
                    snapshot.builds.fetch_add(1, Ordering::Release);
                    snapshot.published_generation.store(generation, Ordering::Release);
                }
                watched = next;
                seen_generation = generation;
                seen_limit = limit;
                dirty_since = None;
            }
            let pause = std::time::Duration::from_secs(1).min(snapshot.idle / 2);
            drop(snapshot);
            std::thread::sleep(pause);
        }
    }).is_err() { snapshot.running.store(false, Ordering::Release); }
}
thread_local! { static PINNED_CREDENTIALS: std::cell::RefCell<Option<Arc<CredentialIndex>>> = const { std::cell::RefCell::new(None) }; }
pub(super) struct CredentialRequestGuard { previous: Option<Arc<CredentialIndex>>, _launch: super::launch_credentials::RequestGuard }
impl Drop for CredentialRequestGuard {
    fn drop(&mut self) { PINNED_CREDENTIALS.with(|slot| *slot.borrow_mut() = self.previous.take()); }
}
pub(super) fn pin_credential_request() -> CredentialRequestGuard {
    let index = if super::narrowed_credential_read() { Some(credential_index()) } else { None };
    CredentialRequestGuard { previous: PINNED_CREDENTIALS.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), index)), _launch: super::launch_credentials::pin() }
}
fn credential_index() -> Arc<CredentialIndex> {
    if let Some(index) = PINNED_CREDENTIALS.with(|slot| slot.borrow().clone()) { return index; }
    credential_snapshot().index.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialDecision { Clear, Credential, Unverified(&'static str) }
pub(super) fn credential_decision(path: &Path, tree: bool) -> CredentialDecision {
    use super::credential_locations::key;
    let index = credential_index();
    let opened = OpenedPath::open(path).ok();
    let target = opened.as_ref().map(|file| file.path.as_path()).unwrap_or(path);
    if super::launch_credentials::matches(target, opened.as_ref().map(|file| file.identity)) || super::launch_credentials::matches(path, None) { return CredentialDecision::Credential; }
    let lexical = key(path);
    let canonical = key(target);
    for root in &index.locations {
        if root.contains(&lexical, false) || root.contains(&canonical, true)
            || (tree && root.canonical.starts_with(&canonical)) { return CredentialDecision::Credential; }
    }
    if tree { return CredentialDecision::Clear; }
    let Some(target) = opened else { return CredentialDecision::Clear; };
    let Ok(metadata) = target._file.metadata() else { return CredentialDecision::Unverified("file identity could not be inspected"); };
    if !metadata.is_file() { return CredentialDecision::Clear; }
    #[cfg(unix)]
    let links = { use std::os::unix::fs::MetadataExt; metadata.nlink() };
    #[cfg(windows)]
    let links = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(target._file.as_raw_handle() as _, &mut info) } == 0 { return CredentialDecision::Unverified("file identity could not be inspected"); }
        u64::from(info.nNumberOfLinks)
    };
    if links <= 1 { return CredentialDecision::Clear; }
    if index.known.contains(&(target.identity.volume, target.identity.id)) { return CredentialDecision::Credential; }
    if !index.ready { return CredentialDecision::Unverified("credential identity index is building in the background; retry shortly"); }
    if index.limited { return CredentialDecision::Unverified("credential identity index reached its entry limit; this hardlinked file cannot be cleared"); }
    CredentialDecision::Clear
}
#[cfg(unix)]
pub(super) fn credential_entry(path: &Path, metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return credential_decision(path, false) == CredentialDecision::Credential;
    }
    use std::os::unix::fs::MetadataExt;
    if super::launch_credentials::matches(path, Some(FileIdentity { volume: metadata.dev(), id: metadata.ino() as u128 })) { return true; }
    let index = credential_index();
    let key = super::credential_locations::key(path);
    if index.locations.iter().any(|root| {
        root.contains(&key, false) || root.contains(&key, true)
    }) { return true; }
    metadata.is_file() && metadata.nlink() > 1 && index.known.contains(&(metadata.dev(), metadata.ino() as u128))
}
pub(super) fn narrowed_credential(path: &Path, tree: bool) -> bool {
    credential_decision(path, tree) != CredentialDecision::Clear
}
#[cfg(test)]
pub(super) fn wait_for_credentials() -> u64 {
    let snapshot = credential_snapshot();
    let start = std::time::Instant::now();
    while !snapshot.index.read().unwrap().ready || snapshot.published_generation.load(Ordering::Acquire) != GENERATION.load(Ordering::Acquire) {
        assert!(start.elapsed() < std::time::Duration::from_secs(30), "background index timeout");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    snapshot.builds.load(Ordering::Acquire)
}

#[cfg(test)]
pub(super) fn credential_test_state() -> Option<(u64, u64, bool, bool)> {
    let home = crate::config::paths::config_dir().ok()?;
    let guard = CREDENTIAL_CACHE.get()?.read().unwrap();
    let snapshot = &guard.as_ref().filter(|cached| cached.home == home)?.snapshot;
    let ready = snapshot.index.read().unwrap().ready;
    Some((snapshot.starts.load(Ordering::Acquire), snapshot.builds.load(Ordering::Acquire), snapshot.running.load(Ordering::Acquire), ready))
}
