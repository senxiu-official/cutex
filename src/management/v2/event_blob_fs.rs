//! Immutable event-blob children of an owner-private, handle-bound directory.
//! No caller-controlled pathname is resolved below the root. Unix uses openat;
//! Windows pins every ancestor against rename and rejects all reparse points.
use anyhow::{ensure, Context, Result};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

fn filename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && !name.ends_with(['.', ' '])
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid event blob filename"
    );
    // Windows device aliases are special even when a normal extension follows.
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    ensure!(
        !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9')),
        "reserved event blob filename"
    );
    Ok(())
}

struct Root {
    path: PathBuf,
    handle: File,
    #[cfg(windows)]
    identity: crate::platform::private_fs::FileIdentity,
    #[cfg(windows)]
    _ancestors: Vec<File>,
}

#[cfg(unix)]
mod os {
    use super::*;
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
    };

    fn traversal(path: &Path, create: bool) -> Result<File> {
        ensure!(path.is_absolute(), "event blob root must be absolute");
        let mut parts = path.components();
        ensure!(
            parts.next() == Some(Component::RootDir),
            "invalid event blob root"
        );
        ensure!(
            parts.clone().all(|p| matches!(p, Component::Normal(_))),
            "event blob root must be normalized"
        );
        let mut current = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?;
        let mut child = false;
        let mut private_anchor = false;
        for part in parts {
            let Component::Normal(part) = part else {
                unreachable!()
            };
            child = true;
            let name = CString::new(part.as_bytes())?;
            let open = || {
                let fd = unsafe {
                    libc::openat(
                        current.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(unsafe { File::from_raw_fd(fd) })
                }
            };
            let next = match open() {
                Err(e) if create && e.kind() == std::io::ErrorKind::NotFound => {
                    if unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                        let e = std::io::Error::last_os_error();
                        if e.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(e.into());
                        }
                    }
                    open()?
                }
                result => result?,
            };
            let meta = next.metadata()?;
            ensure!(
                meta.is_dir() && (meta.uid() == 0 || meta.uid() == unsafe { libc::geteuid() }),
                "event blob ancestor owner/type"
            );
            // An owner-only ancestor prevents other identities reaching later
            // components, even when an owned descendant inherits umask 0002.
            // Re-validation walks this boundary again before/after operations.
            // Outside that boundary only non-writable or sticky ancestors are safe.
            ensure!(
                private_anchor || meta.mode() & 0o022 == 0 || meta.mode() & 0o1000 != 0,
                "event blob ancestor is writable by another identity"
            );
            if meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0 {
                private_anchor = true;
            }
            current = next;
        }
        ensure!(child, "filesystem root is not an event blob store");
        Ok(current)
    }

    fn private_root(file: &File) -> Result<()> {
        let m = file.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
            "event blob root is not owner private"
        );
        Ok(())
    }

    pub(super) fn root(path: &Path, create: bool) -> Result<Root> {
        use std::os::unix::fs::PermissionsExt;
        let handle = traversal(path, create)?;
        ensure!(
            handle.metadata()?.uid() == unsafe { libc::geteuid() },
            "event blob root owner mismatch"
        );
        if create {
            handle.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        private_root(&handle)?;
        let root = Root {
            path: path.into(),
            handle,
        };
        validate(&root)?;
        Ok(root)
    }

    pub(super) fn validate(root: &Root) -> Result<()> {
        private_root(&root.handle)?;
        let named = traversal(&root.path, false)?;
        private_root(&named)?;
        let a = root.handle.metadata()?;
        let b = named.metadata()?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino(),
            "event blob root binding changed"
        );
        Ok(())
    }

    pub(super) fn child(root: &Root, name: &str, create: bool) -> Result<File> {
        filename(name)?;
        validate(root)?;
        let name = CString::new(name)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if create {
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        let fd = unsafe { libc::openat(root.handle.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let m = file.metadata()?;
        ensure!(
            m.is_file()
                && m.nlink() == 1
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0,
            "event blob child owner/type/links"
        );
        validate(root)?;
        Ok(file)
    }

    pub(super) fn same_file(a: &File, b: &File) -> Result<()> {
        let a = a.metadata()?;
        let b = b.metadata()?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino(),
            "event blob child binding changed"
        );
        Ok(())
    }

    pub(super) fn publish(root: &Root, temp: &str, name: &str) -> Result<()> {
        validate(root)?;
        let temp = CString::new(temp)?;
        let name = CString::new(name)?;
        // Atomic no-replace publication: a crash cannot leave a second hard
        // link that would make a legitimate blob fail our nlink == 1 check.
        #[cfg(target_os = "linux")]
        let published = unsafe {
            libc::renameat2(
                root.handle.as_raw_fd(),
                temp.as_ptr(),
                root.handle.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let published = unsafe {
            libc::renameatx_np(
                root.handle.as_raw_fd(),
                temp.as_ptr(),
                root.handle.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!(
            "atomic no-replace event blob publication is unsupported on this Unix target"
        );
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if published != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Ok(())
    }

    pub(super) fn remove_temp(root: &Root, name: &str) -> Result<()> {
        filename(name)?;
        let name = CString::new(name)?;
        if unsafe { libc::unlinkat(root.handle.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
        root.handle.sync_all()?;
        validate(root)
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, MoveFileExW, BY_HANDLE_FILE_INFORMATION,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_WRITE_THROUGH, OPEN_EXISTING, READ_CONTROL,
        },
    };

    fn long_path(path: &Path) -> PathBuf {
        // Win32 long-path spelling also keeps the short temporary names usable
        // under deeply nested private test homes.
        // Verbatim Win32 paths do not translate '/' separators. Configured
        // private homes and API callers may use the ordinary slash spelling.
        let s = path.as_os_str().to_string_lossy().replace('/', "\\");
        if s.starts_with(r"\\?\") {
            PathBuf::from(s)
        } else if let Some(rest) = s.strip_prefix(r"\\") {
            PathBuf::from(format!(r"\\?\UNC\{rest}"))
        } else {
            PathBuf::from(format!(r"\\?\{s}"))
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        long_path(path)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn open(path: &Path, directory: bool) -> Result<File> {
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
        // A held directory cannot be renamed/replaced. File readers deny both
        // writes and deletion until their bytes and binding have been checked.
        let share = FILE_SHARE_READ | if directory { FILE_SHARE_WRITE } else { 0 };
        // Metadata-only opens do not establish a rename exclusion on Windows.
        // GENERIC_READ makes the missing FILE_SHARE_DELETE effective for dirs.
        let access = FILE_READ_ATTRIBUTES | READ_CONTROL | GENERIC_READ;
        let handle = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                access,
                share,
                null(),
                OPEN_EXISTING,
                flags,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_handle(handle) };
        use std::os::windows::fs::MetadataExt;
        let meta = file.metadata()?;
        ensure!(
            meta.file_attributes() & 0x400 == 0
                && if directory {
                    meta.is_dir()
                } else {
                    meta.is_file()
                },
            "event blob reparse point or unexpected object type"
        );
        Ok(file)
    }

    pub(super) fn root(path: &Path, create: bool) -> Result<Root> {
        ensure!(path.is_absolute(), "event blob root must be absolute");
        let extended = long_path(path);
        let path = extended.as_path();
        let mut current = PathBuf::new();
        let mut held = Vec::new();
        let mut normal = false;
        for part in path.components() {
            match part {
                Component::Prefix(_) => {
                    current.push(part.as_os_str());
                    continue;
                }
                Component::RootDir => current.push(part.as_os_str()),
                Component::Normal(_) => {
                    current.push(part.as_os_str());
                    normal = true;
                }
                _ => anyhow::bail!("event blob root must be normalized"),
            }
            let file = match open(&current, true) {
                Err(e) if create && missing(&e) => {
                    match std::fs::create_dir(&current) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                    open(&current, true)?
                }
                result => result?,
            };
            held.push(file);
        }
        ensure!(normal, "filesystem root is not an event blob store");
        // All ancestors are now held without FILE_SHARE_DELETE, so no path
        // operation below can be redirected through a replacement directory.
        let (handle, identity) = if create {
            crate::platform::private_fs::secure_directory(path)?
        } else {
            crate::platform::private_fs::open_validated_directory(path)?
        };
        let root = Root {
            path: path.into(),
            handle,
            identity,
            _ancestors: held,
        };
        validate(&root)?;
        Ok(root)
    }

    pub(super) fn validate(root: &Root) -> Result<()> {
        crate::platform::private_fs::validate_binding(&root.path, root.identity)?;
        ensure!(
            crate::platform::private_fs::identity(&root.handle)? == root.identity,
            "event blob held root binding changed"
        );
        Ok(())
    }

    pub(super) fn child(root: &Root, name: &str, create: bool) -> Result<File> {
        filename(name)?;
        validate(root)?;
        let path = root.path.join(name);
        let file = if create {
            crate::platform::private_fs::create_new_private_file(&path)?
        } else {
            open(&path, false)?
        };
        crate::platform::private_fs::validate_private_file(&file)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "event blob file information unavailable"
        );
        ensure!(
            info.nNumberOfLinks == 1,
            "event blob hard links are not allowed"
        );
        validate(root)?;
        Ok(file)
    }

    pub(super) fn same_file(a: &File, b: &File) -> Result<()> {
        ensure!(
            crate::platform::private_fs::identity(a)? == crate::platform::private_fs::identity(b)?,
            "event blob child binding changed"
        );
        Ok(())
    }

    pub(super) fn publish(root: &Root, temp: &str, name: &str) -> Result<()> {
        validate(root)?;
        // No REPLACE_EXISTING: a competing writer may only win publication,
        // never overwrite an existing immutable child.
        if unsafe {
            MoveFileExW(
                wide(&root.path.join(temp)).as_ptr(),
                wide(&root.path.join(name)).as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        validate(root)
    }

    pub(super) fn remove_temp(root: &Root, name: &str) -> Result<()> {
        filename(name)?;
        validate(root)?;
        match std::fs::remove_file(root.path.join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn read_child(root: &Root, name: &str, max: usize) -> Result<Vec<u8>> {
    let mut file = os::child(root, name, false)?;
    let before = file.metadata()?;
    ensure!(before.len() <= max as u64, "event blob exceeds read limit");
    let mut bytes = Vec::new();
    let reservation = usize::try_from(before.len())?
        .checked_add(1)
        .context("event blob read length overflow")?;
    bytes
        .try_reserve_exact(reservation)
        .context("reserve event blob read buffer")?;
    (&mut file)
        .take(before.len().saturating_add(1))
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() <= max
            && before.len() == after.len()
            && bytes.len() as u64 == after.len()
            && before.modified()? == after.modified()?,
        "event blob changed while reading"
    );
    let named = os::child(root, name, false)?;
    os::same_file(&file, &named)?;
    os::validate(root)?;
    Ok(bytes)
}

/// Read a bounded regular child without creating missing storage.
pub(super) fn read(root: &Path, name: &str, max: usize) -> Result<Vec<u8>> {
    sharing_retry(|| read_once(root, name, max))
}

fn read_once(root: &Path, name: &str, max: usize) -> Result<Vec<u8>> {
    filename(name)?;
    let root = os::root(root, false)
        .with_context(|| format!("open event blob root {}", root.display()))?;
    #[cfg(unix)]
    fs2::FileExt::lock_shared(&root.handle)?;
    read_child(&root, name, max)
}

/// Publish immutable bytes atomically, or verify an identical existing child.
/// A failed or competing write never replaces existing content.
pub(super) fn write_once(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    sharing_retry(|| publish_once(root, name, bytes))
}

// Windows can briefly hold a winning rename against a concurrent immutable
// reader/writer. Retrying this content-checked operation is idempotent. Never
// retry permission, ownership, link, binding, or differing-content refusals.
#[cfg(windows)]
fn sharing_retry<T>(mut run: impl FnMut() -> Result<T>) -> Result<T> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let result = run();
        let sharing = result.as_ref().err().is_some_and(retryable_race);
        if sharing && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }
        return result;
    }
}

#[cfg(windows)]
fn retryable_race(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        .or_else(
            || match e.downcast_ref::<crate::platform::private_fs::PrivateFsError>() {
                Some(crate::platform::private_fs::PrivateFsError::Io(io)) => io.raw_os_error(),
                _ => None,
            },
        )
        .is_some_and(|code| matches!(code, 32 | 33 | 183))
}

#[cfg(not(windows))]
fn retryable_race(_: &anyhow::Error) -> bool {
    false
}

#[cfg(not(windows))]
fn sharing_retry<T>(mut run: impl FnMut() -> Result<T>) -> Result<T> {
    run()
}

fn publish_once(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    filename(name)?;
    let root = os::root(root, true)
        .with_context(|| format!("create event blob root {}", root.display()))?;
    #[cfg(unix)]
    fs2::FileExt::lock_exclusive(&root.handle)?;
    match read_child(&root, name, bytes.len()) {
        Ok(existing) => {
            ensure!(existing == bytes, "existing event blob differs");
            return Ok(());
        }
        Err(e) if missing(&e) => {}
        Err(e) => return Err(e),
    }
    // Unlike atomic_write's target-derived name, this stays short even when
    // the destination is a 64-character content hash.
    let temp = format!(".e-{}", uuid::Uuid::new_v4().simple());
    let result = (|| -> Result<()> {
        let mut file = os::child(&root, &temp, true)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        let publication = os::publish(&root, &temp, name);
        #[cfg(unix)]
        os::remove_temp(&root, &temp)?;
        // This also verifies the winner of a competing atomic publication.
        let actual = read_child(&root, name, bytes.len());
        match actual {
            Ok(actual) => ensure!(actual == bytes, "published event blob differs"),
            Err(e) => {
                // A losing rename can report AlreadyExists/AccessDenied while
                // its winning child is briefly share-locked. Preserve that
                // retryable read error rather than hiding it behind the rename.
                if retryable_race(&e) {
                    return Err(e);
                }
                publication?;
                return Err(e);
            }
        }
        os::validate(&root)
    })();
    let cleanup = os::remove_temp(&root, &temp);
    if result.is_ok() {
        cleanup?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "ebf-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ));
            let root = os::root(&p, true).unwrap();
            drop(root);
            Self(p)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trip_bounds_and_immutable_content() {
        let f = Fixture::new();
        let root = f.0.join("event-blobs");
        assert!(read(&root, "a.bin", 8).is_err());
        assert!(!root.exists());
        write_once(&root, "a.bin", b"bytes").unwrap();
        write_once(&root, "a.bin", b"bytes").unwrap();
        assert_eq!(read(&root, "a.bin", 5).unwrap(), b"bytes");
        assert!(read(&root, "a.bin", 4).is_err());
        assert!(write_once(&root, "a.bin", b"other").is_err());
        assert_eq!(read(&root, "a.bin", 5).unwrap(), b"bytes");
        for bad in [
            "../x", "a/b", "a\\b", "a:stream", ".", "..", "NUL.bin", "COM1", "x.",
        ] {
            assert!(write_once(&root, bad, b"x").is_err());
            assert!(read(&root, bad, 1).is_err());
        }
        let grants = root.join("grants");
        write_once(&grants, &format!("{}.json", "a".repeat(64)), b"grant").unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        assert_eq!(std::fs::read_dir(&grants).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_identical_writers_publish_complete_bytes() {
        let f = Fixture::new();
        for round in 0..8 {
            let root = f.0.join(format!("store-{round}"));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let root = root.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        write_once(&root, "same.bin", &[42; 65536])
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }
            assert_eq!(read(&root, "same.bin", 65536).unwrap(), vec![42; 65536]);
            assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
        }
    }

    #[test]
    fn directories_and_hard_links_are_not_blob_files() {
        let f = Fixture::new();
        let root = f.0.join("store");
        write_once(&root, "original", b"x").unwrap();
        std::fs::hard_link(root.join("original"), root.join("linked")).unwrap();
        assert!(read(&root, "linked", 1).is_err());
        assert!(write_once(&root, "linked", b"x").is_err());
        std::fs::create_dir(root.join("dir")).unwrap();
        assert!(read(&root, "dir", 1).is_err());
        assert!(write_once(&root, "dir", b"x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn owned_group_writable_descendants_require_an_owner_private_anchor() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let base = std::fs::canonicalize("/tmp")
            .unwrap()
            .join(format!("eb-anchor-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .unwrap();
        let fixture = Fixture(base.clone());
        let middle = base.join("group-writable");
        std::fs::create_dir(&middle).unwrap();
        std::fs::set_permissions(&middle, std::fs::Permissions::from_mode(0o775)).unwrap();
        let root = middle.join("store");
        write_once(&root, "good", b"ok").unwrap();
        assert_eq!(read(&root, "good", 2).unwrap(), b"ok");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(read(&root, "good", 2).is_err());
        assert!(write_once(&root, "other", b"x").is_err());
        assert!(!root.join("other").exists());
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(fixture);
    }

    #[cfg(unix)]
    #[test]
    fn links_fifo_and_replaced_root_are_refused() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
        };
        let f = Fixture::new();
        let path = f.0.join("store");
        write_once(&path, "good", b"ok").unwrap();
        symlink(path.join("good"), path.join("link")).unwrap();
        assert!(read(&path, "link", 2).is_err());
        assert!(write_once(&path, "link", b"ok").is_err());
        let fifo = CString::new(path.join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read(&path, "fifo", 1).is_err());
        let held = os::root(&path, false).unwrap();
        let moved = f.0.join("moved");
        std::fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        assert!(os::validate(&held).is_err());
        assert!(read(&path, "good", 2).is_err());
        assert!(write_once(&path, "new", b"x").is_err());
        assert!(!moved.join("new").exists());
        std::fs::remove_file(&path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn slash_spelled_windows_roots_are_normalized_before_verbatim_open() {
        let f = Fixture::new();
        let root = PathBuf::from(f.0.join("slash-root").to_string_lossy().replace('\\', "/"));
        write_once(&root, "image.bin", b"image").unwrap();
        assert_eq!(read(&root, "image.bin", 5).unwrap(), b"image");
    }

    #[cfg(windows)]
    #[test]
    fn long_roots_and_hash_names_use_short_temporary_names() {
        let f = Fixture::new();
        // Stay beyond MAX_PATH even when the test runner uses a short private
        // TEMP root (for example D:\t). The component itself stays below 255.
        let path = f.0.join("d".repeat(180)).join("grants");
        let name = format!("{}.json", "a".repeat(64));
        assert!(path.join(&name).as_os_str().len() > 260);
        write_once(&path, &name, b"grant").unwrap();
        assert_eq!(read(&path, &name, 5).unwrap(), b"grant");
        assert_eq!(std::fs::read_dir(path).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn junction_ancestors_are_refused_and_held_root_cannot_move() {
        let f = Fixture::new();
        let path = f.0.join("store");
        write_once(&path, "good", b"ok").unwrap();
        let held = os::root(&path, false).unwrap();
        assert!(std::fs::rename(&path, f.0.join("moved")).is_err());
        drop(held);
        let junction = f.0.join("junction");
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&path)
            .output()
            .unwrap();
        assert!(status.status.success(), "junction fixture creation failed");
        assert!(read(&junction, "good", 2).is_err());
        assert!(write_once(&junction.join("child"), "new", b"x").is_err());
        assert!(!path.join("child").exists());
        std::fs::remove_dir(junction).unwrap();
    }
}
