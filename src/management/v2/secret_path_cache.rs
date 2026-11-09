//! Cached private namespaces; target identity is obtained from one open handle.
use super::{canonicalize_existing_prefix, FileIdentity};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};

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
