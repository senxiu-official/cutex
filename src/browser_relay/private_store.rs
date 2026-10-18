//! Narrow private directory operations. The image index never supplies a path.
//! Windows uses the repository's protected owner-SID DACL/reparse checks;
//! Unix traversal, enumeration and children use held no-follow directory handles.
use anyhow::{ensure, Context};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub struct Directory {
    pub path: PathBuf,
    handle: File,
    #[cfg(windows)]
    identity: crate::platform::private_fs::FileIdentity,
}
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
        path::Component,
    };

    /// Each lookup/creation is relative to its already opened parent. A rename
    /// cannot redirect a later mkdir/open through a newly installed symlink.
    pub(super) fn directory(path: &Path, create: bool) -> anyhow::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        ensure!(path.is_absolute(), "private absolute directory required");
        let mut parts = path.components();
        ensure!(parts.next() == Some(Component::RootDir), "private path root");
        ensure!(
            parts.clone().all(|part| matches!(part, Component::Normal(_))),
            "private directory path not normalized"
        );
        let mut current = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?;
        let mut has_child = false;
        for part in parts {
            let Component::Normal(part) = part else {
                anyhow::bail!("private directory path not normalized");
            };
            has_child = true;
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
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    if unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(error.into());
                        }
                    }
                    // An EEXIST race is safe only if this no-follow open succeeds.
                    open()?
                }
                result => result?,
            };
            let meta = next.metadata()?;
            ensure!(
                meta.is_dir() && (meta.uid() == 0 || meta.uid() == unsafe { libc::geteuid() }),
                "private directory ancestor owner/type"
            );
            current = next;
        }
        // Never chmod or accept the filesystem root as a browser store.
        ensure!(has_child, "private child directory required");
        Ok(current)
    }

    pub(super) fn private_metadata(file: &File) -> anyhow::Result<std::fs::Metadata> {
        let meta = file.metadata()?;
        ensure!(
            meta.is_dir()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o7777 == 0o700,
            "private directory owner/mode"
        );
        Ok(meta)
    }

    // readdir distinguishes EOF from an error only through thread-local errno.
    // Unsupported Unix targets fail closed instead of returning a partial list.
    fn errno_ptr() -> anyhow::Result<*mut libc::c_int> {
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "hurd",
            target_os = "emscripten",
            target_os = "redox"
        ))]
        return Ok(unsafe { libc::__errno_location() });
        #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
        return Ok(unsafe { libc::__errno() });
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        return Ok(unsafe { libc::__error() });
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        return Ok(unsafe { libc::___errno() });
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "hurd",
            target_os = "emscripten",
            target_os = "redox",
            target_os = "android",
            target_os = "netbsd",
            target_os = "openbsd",
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "solaris",
            target_os = "illumos"
        )))]
        anyhow::bail!("private directory enumeration unsupported on this Unix target")
    }

    struct Entries(*mut libc::DIR);
    impl Drop for Entries {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }

    pub(super) fn names(handle: &File, max: usize) -> anyhow::Result<Vec<String>> {
        let errno = errno_ptr()?;
        // openat(".") gives an independent directory offset. dup would share
        // the held file's offset and make repeated enumerations incomplete.
        let fd = unsafe {
            libc::openat(
                handle.as_raw_fd(),
                b".\0".as_ptr().cast(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let directory = unsafe { libc::fdopendir(file.as_raw_fd()) };
        if directory.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        // fdopendir owns the fd only on success; Entries closes it on all exits.
        let _ = file.into_raw_fd();
        let entries = Entries(directory);
        let mut names = Vec::new();
        loop {
            unsafe { *errno = 0 };
            let entry = unsafe { libc::readdir(entries.0) };
            if entry.is_null() {
                let error = unsafe { *errno };
                ensure!(
                    error == 0,
                    "private directory enumeration: {}",
                    std::io::Error::from_raw_os_error(error)
                );
                break;
            }
            // Copy before readdir reuses its buffer. No entry is followed.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            ensure!(names.len() < max, "private directory entry limit");
            names.push(
                name.to_str()
                    .context("private filename encoding")?
                    .to_owned(),
            );
        }
        Ok(names)
    }
}

impl Directory {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        // Refuse pre-existing links in every ancestor before creating a private
        // child. The held root and child checks also detect later replacement.
        #[cfg(windows)]
        for ancestor in path.ancestors() {
            if let Ok(meta) = std::fs::symlink_metadata(ancestor) {
                ensure!(!meta.file_type().is_symlink(), "private directory link");
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "private directory reparse point"
                    );
                }
            }
        }
        #[cfg(windows)]
        std::fs::create_dir_all(path)?;
        #[cfg(windows)]
        {
            let (handle, identity) = crate::platform::private_fs::secure_directory(path)?;
            Ok(Self {
                path: path.into(),
                handle,
                identity,
            })
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let handle = unix::directory(path, true)?;
            let meta = handle.metadata()?;
            ensure!(
                meta.is_dir() && meta.uid() == unsafe { libc::geteuid() },
                "private directory owner"
            );
            handle.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            let directory = Self {
                path: path.into(),
                handle,
            };
            directory.validate()?;
            Ok(directory)
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            crate::platform::private_fs::validate_binding(&self.path, self.identity)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let named = unix::private_metadata(&unix::directory(&self.path, false)?)?;
            let held = unix::private_metadata(&self.handle)?;
            ensure!(
                named.dev() == held.dev() && named.ino() == held.ino(),
                "private directory replaced"
            );
        }
        Ok(())
    }
    fn child(&self, name: &str, create: bool) -> anyhow::Result<File> {
        ensure!(name_ok(name), "invalid private filename");
        self.validate()?;
        #[cfg(windows)]
        {
            Ok(crate::platform::private_fs::open_child(
                &self.path,
                self.identity,
                name,
                if create {
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                } else {
                    libc::O_RDONLY
                },
                false,
            )?)
        }
        #[cfg(unix)]
        {
            self.unix_child(name, create)?.context("private child missing")
        }
    }
    #[cfg(unix)]
    fn unix_child(&self, name: &str, create: bool) -> anyhow::Result<Option<File>> {
        ensure!(name_ok(name), "invalid private filename");
        self.validate()?;
        {
            use std::{
                ffi::CString,
                os::{
                    fd::{AsRawFd, FromRawFd},
                    unix::fs::MetadataExt,
                },
            };
            let name = CString::new(name)?;
            let flags = libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if create {
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                } else {
                    libc::O_RDONLY
                };
            let fd = unsafe { libc::openat(self.handle.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if !create && error.kind() == std::io::ErrorKind::NotFound {
                    // Absence is determined by the held fd, never by a pathname
                    // that could now resolve through another directory tree.
                    self.validate()?;
                    return Ok(None);
                }
                return Err(error.into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let meta = file.metadata()?;
            ensure!(
                meta.is_file()
                    && meta.nlink() == 1
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.mode() & 0o077 == 0,
                "private child owner/type"
            );
            self.validate()?;
            Ok(Some(file))
        }
    }
    pub fn read(&self, name: &str, max: usize) -> anyhow::Result<Option<Vec<u8>>> {
        #[cfg(windows)]
        let file = {
            self.validate()?;
            ensure!(name_ok(name), "invalid private filename");
            match std::fs::symlink_metadata(self.path.join(name)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
                Ok(_) => {}
            }
            self.child(name, false)?
        };
        #[cfg(unix)]
        let Some(file) = self.unix_child(name, false)? else {
            return Ok(None);
        };
        ensure!(
            file.metadata()?.len() <= max as u64,
            "private file too large"
        );
        let mut bytes = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= max, "private file grew");
        self.validate()?;
        Ok(Some(bytes))
    }
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let mut file = self.child(name, true)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        self.validate()
    }
    pub fn replace(&self, source: &str, target: &str) -> anyhow::Result<()> {
        ensure!(
            name_ok(source) && name_ok(target),
            "invalid private filename"
        );
        self.validate()?;
        #[cfg(windows)]
        crate::platform::private_fs::replace_child(&self.path, self.identity, source, target)?;
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd};
            let source = CString::new(source)?;
            let target = CString::new(target)?;
            ensure!(
                unsafe {
                    libc::renameat(
                        self.handle.as_raw_fd(),
                        source.as_ptr(),
                        self.handle.as_raw_fd(),
                        target.as_ptr(),
                    )
                } == 0,
                "private rename failed"
            );
            self.handle.sync_all()?;
        }
        self.validate()
    }
    pub fn remove(&self, name: &str) -> anyhow::Result<()> {
        ensure!(name_ok(name), "invalid private filename");
        self.validate()?;
        #[cfg(windows)]
        crate::platform::private_fs::unlink_child(&self.path, self.identity, name)?;
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd};
            let name = CString::new(name)?;
            if unsafe { libc::unlinkat(self.handle.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error.into());
                }
            }
            self.validate()?;
        }
        Ok(())
    }
    pub fn atomic(&self, target: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let temp = format!("tmp-{}.json", uuid::Uuid::new_v4().simple());
        self.write_new(&temp, bytes)?;
        let result = self.replace(&temp, target);
        if result.is_err() {
            let _ = self.remove(&temp);
        }
        result
    }
    pub fn names(&self, max: usize) -> anyhow::Result<Vec<String>> {
        self.validate()?;
        #[cfg(windows)]
        let names = {
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&self.path)? {
                ensure!(names.len() < max, "private directory entry limit");
                names.push(
                    entry?
                        .file_name()
                        .into_string()
                        .ok()
                        .context("private filename encoding")?,
                );
            }
            names
        };
        #[cfg(unix)]
        let names = unix::names(&self.handle, max)?;
        self.validate()?;
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_private_store_bounds_and_names() {
        let path = std::env::temp_dir().join(format!("browser-store-{}", uuid::Uuid::new_v4()));
        let root = Directory::open(&path).unwrap();
        root.write_new("a.json", b"ok").unwrap();
        assert_eq!(root.read("a.json", 2).unwrap().unwrap(), b"ok");
        assert!(root.read("a.json", 1).is_err());
        assert!(root.write_new("a.json", b"overwrite").is_err());
        assert!(root.read("../secret", 20).is_err());
        root.atomic("a.json", b"new").unwrap();
        assert_eq!(root.read("a.json", 3).unwrap().unwrap(), b"new");
        root.remove("a.json").unwrap();
        drop(root);
        std::fs::remove_dir(path).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn browser_private_store_refuses_links() {
        let path = std::env::temp_dir().join(format!("browser-links-{}", uuid::Uuid::new_v4()));
        let root = Directory::open(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", path.join("link")).unwrap();
        assert!(root.read("link", 10000).is_err());
        std::fs::remove_file(path.join("link")).unwrap();
        drop(root);
        std::fs::remove_dir(path).unwrap();
    }

    #[cfg(unix)]
    mod unix_regressions {
        use super::*;
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{symlink, MetadataExt, PermissionsExt},
        };

        fn path() -> PathBuf {
            std::env::temp_dir().join(format!("browser-unix-{}", uuid::Uuid::new_v4()))
        }

        fn mode(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        #[test]
        fn browser_private_store_creates_private_components_without_path_traversal() {
            let base = path();
            let nested = base.join("parent/store");
            let root = Directory::open(&nested).unwrap();
            for path in [&base, &base.join("parent"), &nested] {
                let meta = std::fs::metadata(path).unwrap();
                assert_eq!(meta.uid(), unsafe { libc::geteuid() });
                assert_eq!(meta.mode() & 0o7777, 0o700);
            }
            assert!(Directory::open(Path::new("relative-store")).is_err());
            assert!(Directory::open(Path::new("/")).is_err());
            assert!(Directory::open(&base.join("../not-created")).is_err());
            assert!(Directory::open(&base.join("not-created/../escape")).is_err());
            assert!(!base.join("not-created").exists());
            drop(root);
            std::fs::remove_dir(&nested).unwrap();
            std::fs::remove_dir(base.join("parent")).unwrap();
            std::fs::remove_dir(base).unwrap();
        }

        #[test]
        fn browser_private_store_never_creates_through_an_ancestor_link() {
            let base = path();
            let real = base.join("real");
            let root = Directory::open(&real).unwrap();
            symlink(&real, base.join("alias")).unwrap();
            assert!(Directory::open(&base.join("alias/new/store")).is_err());
            assert!(!real.join("new").exists());
            assert!(Directory::open(&base.join("alias")).is_err());
            std::fs::remove_file(base.join("alias")).unwrap();
            drop(root);
            std::fs::remove_dir(real).unwrap();
            std::fs::remove_dir(base).unwrap();
        }

        #[test]
        fn browser_private_store_rejects_ancestor_rebound_to_the_same_inode() {
            let base = path();
            let parent = base.join("parent");
            let moved = base.join("moved");
            let root = Directory::open(&parent.join("store")).unwrap();
            root.write_new("original.json", b"original").unwrap();
            std::fs::rename(&parent, &moved).unwrap();
            symlink(&moved, &parent).unwrap();
            // A pathname stat sees the original inode, but the component chain
            // is no longer admissible. Every public operation fails closed.
            assert_eq!(
                std::fs::metadata(&root.path).unwrap().ino(),
                root.handle.metadata().unwrap().ino()
            );
            assert!(root.validate().is_err());
            assert!(root.read("original.json", 8).is_err());
            assert!(root.read("absent.json", 8).is_err());
            assert!(root.names(10).is_err());
            assert!(root.write_new("new.json", b"no").is_err());
            assert!(root.replace("original.json", "new.json").is_err());
            assert!(root.remove("original.json").is_err());
            assert_eq!(
                std::fs::read(moved.join("store/original.json")).unwrap(),
                b"original"
            );
            std::fs::remove_file(&parent).unwrap();
            std::fs::rename(&moved, &parent).unwrap();
            root.validate().unwrap();
            root.remove("original.json").unwrap();
            drop(root);
            std::fs::remove_dir(parent.join("store")).unwrap();
            std::fs::remove_dir(parent).unwrap();
            std::fs::remove_dir(base).unwrap();
        }

        #[test]
        fn browser_private_store_enumeration_is_held_fd_and_rebinding_is_rejected() {
            let base = path();
            let original = base.join("store");
            let moved = base.join("moved");
            let root = Directory::open(&original).unwrap();
            root.write_new("held.json", b"held").unwrap();
            std::fs::rename(&original, &moved).unwrap();
            // A missing root cannot be mistaken for a missing optional index.
            assert!(root.read("absent.json", 8).is_err());
            let replacement = Directory::open(&original).unwrap();
            replacement.write_new("replacement.json", b"other").unwrap();
            assert!(root.validate().is_err());
            assert!(root.names(10).is_err());
            assert!(root.read("absent.json", 8).is_err());
            // Exercise the fd enumerator itself across pathname replacement.
            // Even an ABA replacement between validate calls cannot enumerate
            // the replacement directory through the old handle.
            assert_eq!(unix::names(&root.handle, 1).unwrap(), vec!["held.json"]);
            assert_eq!(unix::names(&root.handle, 1).unwrap(), vec!["held.json"]);
            replacement.remove("replacement.json").unwrap();
            drop(replacement);
            std::fs::remove_dir(&original).unwrap();
            std::fs::rename(&moved, &original).unwrap();
            root.remove("held.json").unwrap();
            drop(root);
            std::fs::remove_dir(original).unwrap();
            std::fs::remove_dir(base).unwrap();
        }

        #[test]
        fn browser_private_store_rechecks_private_mode_without_repairing_it() {
            let path = path();
            let root = Directory::open(&path).unwrap();
            root.write_new("a.json", b"a").unwrap();
            for degraded in [0o750, 0o755, 0o1700] {
                mode(&path, degraded);
                assert!(root.validate().is_err());
                assert!(root.read("a.json", 1).is_err());
                assert!(root.read("missing.json", 1).is_err());
                assert!(root.names(10).is_err());
                assert!(root.write_new("new.json", b"no").is_err());
                assert!(root.remove("a.json").is_err());
                assert_eq!(
                    std::fs::metadata(&path).unwrap().mode() & 0o7777,
                    degraded
                );
            }
            mode(&path, 0o700);
            root.validate().unwrap();
            root.remove("a.json").unwrap();
            drop(root);
            std::fs::remove_dir(path).unwrap();
        }

        #[test]
        fn browser_private_store_missing_is_not_a_link_directory_or_hardlink() {
            let path = path();
            let root = Directory::open(&path).unwrap();
            assert_eq!(root.read("missing.json", 1).unwrap(), None);
            symlink("missing.json", path.join("broken.json")).unwrap();
            assert!(root.read("broken.json", 1).is_err());
            root.write_new("a.json", b"a").unwrap();
            std::fs::hard_link(path.join("a.json"), path.join("hard.json")).unwrap();
            assert!(root.read("hard.json", 1).is_err());
            assert!(root.read("a.json", 1).is_err());
            std::fs::create_dir(path.join("directory.json")).unwrap();
            assert!(root.read("directory.json", 1).is_err());
            for file in ["broken.json", "hard.json", "a.json"] {
                root.remove(file).unwrap();
            }
            std::fs::remove_dir(path.join("directory.json")).unwrap();
            drop(root);
            std::fs::remove_dir(path).unwrap();
        }

        #[test]
        fn browser_private_store_enumeration_is_repeatable_bounded_and_no_follow() {
            let path = path();
            let root = Directory::open(&path).unwrap();
            assert_eq!(root.names(0).unwrap(), Vec::<String>::new());
            root.write_new("a.json", b"a").unwrap();
            symlink("missing.json", path.join("broken.json")).unwrap();
            for _ in 0..2 {
                let mut names = root.names(2).unwrap();
                names.sort();
                assert_eq!(names, vec!["a.json", "broken.json"]);
            }
            assert!(root.names(1).is_err());
            assert!(root.names(0).is_err());
            // A failed bounded traversal also closes its own stream, without
            // consuming the offset of a subsequent successful enumeration.
            assert_eq!(root.names(2).unwrap().len(), 2);
            root.remove("a.json").unwrap();
            root.remove("broken.json").unwrap();
            drop(root);
            std::fs::remove_dir(path).unwrap();
        }

        // APFS refuses non-UTF-8 names (EILSEQ), so the fixture cannot exist there.
        #[cfg_attr(target_os = "macos", ignore = "APFS rejects non-UTF-8 file names")]
        #[test]
        fn browser_private_store_enumeration_rejects_non_utf8_names() {
            let path = path();
            let root = Directory::open(&path).unwrap();
            let invalid = path.join(std::ffi::OsStr::from_bytes(b"bad-\xff"));
            File::create(&invalid).unwrap();
            assert!(root.names(1).is_err());
            std::fs::remove_file(invalid).unwrap();
            assert!(root.names(0).unwrap().is_empty());
            drop(root);
            std::fs::remove_dir(path).unwrap();
        }
    }
}
