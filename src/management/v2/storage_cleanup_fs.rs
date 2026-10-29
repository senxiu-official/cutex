//! No-follow cleanup filesystem operations. Handle-relative directory traversal
//! follows the reviewed Claude transfer implementation; cleanup never reads file bytes.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
fn leaf(name: &OsStr) -> io::Result<()> {
    let text = name
        .to_str()
        .ok_or_else(|| io::Error::other("cleanup filesystem name is not Unicode"))?;
    let mut components = Path::new(name).components();
    if text.is_empty()
        || matches!(text, "." | "..")
        || text.ends_with(['.', ' '])
        || text.contains(['/', '\\', ':', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(io::Error::other("unsafe cleanup filesystem path component"));
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    crate::platform::private_fs::identity(file)
        .map(|id| id.publication_key())
        .map_err(io::Error::other)
}

#[derive(Clone, Copy)]
enum Kind {
    Directory,
    File,
    Link,
    Other,
}

/// All child operations are relative to no-follow directory descriptors. Windows permits directory
/// writes (including unrelated atomic saves), denies deletion, and never resolves children by path.
struct Dir {
    file: File,
    path: PathBuf,
    _parent: Option<Arc<Dir>>,
}

impl Dir {
    fn absolute(path: &Path) -> io::Result<Arc<Self>> {
        if !path.is_absolute() {
            return Err(io::Error::other("storage home must be absolute"));
        }
        let mut root = PathBuf::new();
        let mut names = Vec::new();
        for c in path.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => root.push(c.as_os_str()),
                Component::Normal(name) => names.push(name.to_owned()),
                _ => return Err(io::Error::other("storage home contains a parent traversal")),
            }
        }
        let mut dir = Arc::new(Self {
            file: open_root(&root)?,
            path: root,
            _parent: None,
        });
        for name in names {
            dir = dir.directory(&name)?;
        }
        Ok(dir)
    }

    fn directory(self: &Arc<Self>, name: &OsStr) -> io::Result<Arc<Self>> {
        leaf(name)?;
        let file = open_directory(self, name)?;
        Ok(Arc::new(Self {
            file,
            path: self.path.join(name),
            _parent: Some(self.clone()),
        }))
    }
    fn new_directory(self: &Arc<Self>, name: &OsStr) -> io::Result<Arc<Self>> {
        leaf(name)?;
        mkdir(self, name)?;
        self.sync()?;
        self.directory(name)
    }
    fn directory_or_create(self: &Arc<Self>, name: &OsStr) -> io::Result<Arc<Self>> {
        match self.directory(name) {
            Ok(dir) => Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::NotFound => match self.new_directory(name)
            {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self.directory(name),
                result => result,
            },
            Err(error) => Err(error),
        }
    }
    fn exists(&self, name: &OsStr) -> io::Result<bool> {
        match self.kind(name) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    fn read_file(&self, name: &OsStr) -> io::Result<File> {
        leaf(name)?;
        open_file(self, name, false)
    }
    fn create_file(&self, name: &OsStr) -> io::Result<File> {
        leaf(name)?;
        open_file(self, name, true)
    }
    fn sync(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.file.sync_all()
        }
        // Directory FlushFileBuffers is not supported on all Windows filesystems. Every copied
        // file is flushed, and publication is a no-overwrite local filesystem operation.
        #[cfg(windows)]
        {
            Ok(())
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::OpenOptionsExt;

    fn name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(io::Error::other)
    }
    pub(super) fn snapshot(dir: &Dir, child: &OsStr) -> io::Result<Entry> {
        leaf(child)?;
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                dir.file.as_raw_fd(),
                name(child)?.as_ptr(),
                value.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let value = unsafe { value.assume_init() };
        let mode = value.st_mode & libc::S_IFMT;
        let kind = if mode == libc::S_IFREG {
            "file"
        } else if mode == libc::S_IFDIR {
            "directory"
        } else if mode == libc::S_IFLNK {
            "link"
        } else {
            return Err(io::Error::other("special cleanup object"));
        };
        let modified = u128::try_from(value.st_mtime).map_err(io::Error::other)? * 1_000_000_000
            + u128::try_from(value.st_mtime_nsec).map_err(io::Error::other)?;
        Ok(Entry {
            kind: kind.into(),
            bytes: if kind == "file" {
                u64::try_from(value.st_size).map_err(io::Error::other)?
            } else {
                0
            },
            mtime_ns: modified,
            identity: (value.st_dev as u64, value.st_ino as u64),
        })
    }
    pub(super) fn move_child(
        from: &Dir,
        child: &OsStr,
        to: &Dir,
        dest: &OsStr,
        expected: &Entry,
    ) -> io::Result<()> {
        if snapshot(from, child)? != *expected {
            return Err(io::Error::other("cleanup source changed"));
        }
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                from.file.as_raw_fd(),
                name(child)?.as_ptr(),
                to.file.as_raw_fd(),
                name(dest)?.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                from.file.as_raw_fd(),
                name(child)?.as_ptr(),
                to.file.as_raw_fd(),
                name(dest)?.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic cleanup rename unsupported",
        ));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            from.file.sync_all()?;
            to.file.sync_all()?;
            Ok(())
        }
    }
    pub(super) fn unlink(from: &Dir, child: &OsStr, expected: &Entry) -> io::Result<()> {
        let current = snapshot(from, child)?;
        if !same_remaining(&current, expected) {
            return Err(io::Error::other("trash entry changed"));
        }
        if unsafe {
            libc::unlinkat(
                from.file.as_raw_fd(),
                name(child)?.as_ptr(),
                if expected.kind == "directory" {
                    libc::AT_REMOVEDIR
                } else {
                    0
                },
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        from.file.sync_all()
    }
    fn file(fd: libc::c_int) -> io::Result<File> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }
    pub(super) fn open_root(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }
    pub(super) fn open_directory(dir: &Dir, child: &OsStr) -> io::Result<File> {
        file(unsafe {
            libc::openat(
                dir.file.as_raw_fd(),
                name(child)?.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
    }
    pub(super) fn mkdir(dir: &Dir, child: &OsStr) -> io::Result<()> {
        if unsafe { libc::mkdirat(dir.file.as_raw_fd(), name(child)?.as_ptr(), 0o700) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub(super) fn open_file(dir: &Dir, child: &OsStr, create: bool) -> io::Result<File> {
        let flags = if create {
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY | libc::O_NONBLOCK
        };
        let file = file(unsafe {
            libc::openat(
                dir.file.as_raw_fd(),
                name(child)?.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("cleanup filesystem is not a regular file"));
        }
        Ok(file)
    }
    impl Dir {
        pub(super) fn kind(&self, child: &OsStr) -> io::Result<Kind> {
            leaf(child)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name(child)?.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mode = unsafe { stat.assume_init() }.st_mode;
            Ok(if (mode & libc::S_IFMT) == libc::S_IFLNK {
                Kind::Link
            } else if (mode & libc::S_IFMT) == libc::S_IFDIR {
                Kind::Directory
            } else if (mode & libc::S_IFMT) == libc::S_IFREG {
                Kind::File
            } else {
                Kind::Other
            })
        }
        pub(super) fn entries(&self) -> io::Result<Vec<OsString>> {
            self.entries_before(None)
        }
        pub(super) fn entries_before(
            &self,
            deadline: Option<Instant>,
        ) -> io::Result<Vec<OsString>> {
            struct Stream(*mut libc::DIR);
            impl Drop for Stream {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let fd = unsafe { libc::dup(self.file.as_raw_fd()) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                unsafe {
                    libc::close(fd);
                }
                return Err(io::Error::last_os_error());
            }
            let stream = Stream(stream);
            unsafe {
                libc::rewinddir(stream.0);
            }
            let mut names = Vec::new();
            loop {
                if deadline.is_some_and(|end| Instant::now() >= end) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "cleanup preview time budget reached",
                    ));
                }
                // readdir reports both EOF and failure as null; clear/check errno to distinguish them.
                #[cfg(any(target_os = "linux", target_os = "android"))]
                let errno = unsafe { libc::__errno_location() };
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd",
                    target_os = "dragonfly"
                ))]
                let errno = unsafe { libc::__error() };
                unsafe {
                    *errno = 0;
                }
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    if unsafe { *errno } != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    break;
                }
                let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if bytes != b"." && bytes != b".." {
                    names.push(OsString::from_vec(bytes.to_vec()));
                }
            }
            names.sort();
            Ok(names)
        }
    }
}
#[cfg(unix)]
use unix::{mkdir, open_directory, open_file, open_root};

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_RENAME_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
    };

    #[repr(C)]
    union IoStatusValue {
        status: i32,
        pointer: *mut std::ffi::c_void,
    }
    #[repr(C)]
    struct IoStatusBlock {
        value: IoStatusValue,
        information: usize,
    }
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }
    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root: *mut std::ffi::c_void,
        name: *mut UnicodeString,
        attributes: u32,
        security: *mut std::ffi::c_void,
        quality: *mut std::ffi::c_void,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file: *mut *mut std::ffi::c_void,
            access: u32,
            attributes: *mut ObjectAttributes,
            status: *mut IoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea: *const std::ffi::c_void,
            ea_length: u32,
        ) -> i32;
        fn NtQueryDirectoryFile(
            file: *mut std::ffi::c_void,
            event: *mut std::ffi::c_void,
            apc: *mut std::ffi::c_void,
            context: *mut std::ffi::c_void,
            status: *mut IoStatusBlock,
            information: *mut std::ffi::c_void,
            length: u32,
            class: i32,
            single: u8,
            name: *const UnicodeString,
            restart: u8,
        ) -> i32;
        fn NtSetInformationFile(
            file: *mut std::ffi::c_void,
            status: *mut IoStatusBlock,
            information: *const std::ffi::c_void,
            length: u32,
            class: i32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    fn nt_result(status: i32) -> io::Result<()> {
        if status < 0 {
            Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32,
            ))
        } else {
            Ok(())
        }
    }

    // An owner-only protected DACL is supplied at creation, before any history bytes are written.
    // Obtain the account SID from TokenUser, including when running with an elevated TokenOwner.
    struct PrivateDescriptor(*mut std::ffi::c_void);
    impl Drop for PrivateDescriptor {
        fn drop(&mut self) {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0);
            }
        }
    }
    fn private_descriptor() -> io::Result<PrivateDescriptor> {
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        };
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut length);
        }
        let mut data = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                data.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        };
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(token);
        }
        if ok == 0 {
            return Err(error);
        }
        let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
        let mut sid = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut count = 0;
        unsafe {
            while *sid.add(count) != 0 {
                count += 1;
            }
        }
        let sid_text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, count) });
        unsafe {
            LocalFree(sid.cast());
        }
        let sddl: Vec<u16> = format!("O:{sid_text}D:P(A;;FA;;;{sid_text})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(PrivateDescriptor(descriptor))
    }

    /// A single leaf is resolved against the already-open parent object, even if a writable
    /// ancestor has subsequently acquired a reparse point. OPEN_REPARSE_POINT applies to the leaf.
    fn child_handle(
        dir: &Dir,
        child: &OsStr,
        access: u32,
        disposition: u32,
        options: u32,
        directory: bool,
    ) -> io::Result<File> {
        leaf(child)?;
        let parent_identity = file_identity(&dir.file)?;
        if !matches!(kind(&dir.file.metadata()?), Kind::Directory) {
            return Err(io::Error::other(
                "cleanup filesystem parent became a reparse point",
            ));
        }
        let mut text: Vec<u16> = child.encode_wide().collect();
        let length = u16::try_from(
            text.len()
                .checked_mul(2)
                .ok_or_else(|| io::Error::other("cleanup filesystem name too long"))?,
        )
        .map_err(io::Error::other)?;
        let mut name = UnicodeString {
            length,
            maximum_length: length,
            buffer: text.as_mut_ptr(),
        };
        let security = if disposition == 2 {
            Some(private_descriptor()?)
        } else {
            None
        };
        let mut attributes = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root: dir.file.as_raw_handle(),
            name: &mut name,
            attributes: 0x40, // OBJ_CASE_INSENSITIVE
            security: security.as_ref().map_or(std::ptr::null_mut(), |s| s.0),
            quality: std::ptr::null_mut(),
        };
        let mut completion = IoStatusBlock {
            value: IoStatusValue { status: 0 },
            information: 0,
        };
        let mut handle = std::ptr::null_mut();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access | SYNCHRONIZE,
                &mut attributes,
                &mut completion,
                std::ptr::null(),
                0,
                if directory {
                    FILE_SHARE_READ | FILE_SHARE_WRITE
                } else {
                    FILE_SHARE_READ
                },
                disposition,
                options | 0x0020_0000 | 0x20, // FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT
                std::ptr::null(),
                0,
            )
        };
        nt_result(status)?;
        let file = unsafe { File::from_raw_handle(handle) };
        // Verify the opened object before reading any content or returning a writable output.
        // Comparing handle paths also rejects filesystem-specific unexpected root-reparse behavior.
        let expected = handle_path(&dir.file)?.join(child);
        let actual = handle_path(&file)?;
        if !actual
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
            || file_identity(&dir.file)? != parent_identity
            || !matches!(kind(&dir.file.metadata()?), Kind::Directory)
        {
            return Err(io::Error::other(
                "cleanup filesystem parent binding changed",
            ));
        }
        // Materialize the final handle identity now, before the caller can read/copy bytes.
        file_identity(&file)?;
        Ok(file)
    }

    pub(super) fn snapshot(dir: &Dir, child: &OsStr) -> io::Result<Entry> {
        let file = child_handle(dir, child, FILE_READ_ATTRIBUTES, 1, 0, true)?;
        entry(&file)
    }
    pub(super) fn pin_file(dir: &Dir, child: &OsStr) -> io::Result<File> {
        let file = child_handle(dir, child, FILE_READ_ATTRIBUTES, 1, 0x40, true)?;
        if !matches!(kind(&file.metadata()?), Kind::File) {
            return Err(io::Error::other("linked cleanup state file"));
        }
        Ok(file)
    }
    fn entry(file: &File) -> io::Result<Entry> {
        let meta = file.metadata()?;
        let kind = match kind(&meta) {
            Kind::Directory => "directory",
            Kind::File => "file",
            Kind::Link => "link",
            Kind::Other => return Err(io::Error::other("special cleanup object")),
        };
        Ok(Entry {
            kind: kind.into(),
            bytes: if kind == "file" { meta.len() } else { 0 },
            mtime_ns: meta
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos(),
            identity: file_identity(file)?,
        })
    }
    pub(super) fn move_child(
        from: &Dir,
        child: &OsStr,
        to: &Dir,
        dest: &OsStr,
        expected: &Entry,
    ) -> io::Result<()> {
        let source = child_handle(from, child, DELETE | FILE_READ_ATTRIBUTES, 1, 0, true)?;
        if entry(&source)? != *expected {
            return Err(io::Error::other("cleanup source changed"));
        }
        leaf(dest)?;
        let filename: Vec<u16> = dest.encode_wide().chain(Some(0)).collect();
        let bytes = (filename.len() - 1) * 2;
        let size = std::mem::size_of::<FILE_RENAME_INFO>() + bytes + 2;
        let mut buffer = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = to.file.as_raw_handle();
            (*info).FileNameLength = u32::try_from(bytes).map_err(io::Error::other)?;
            std::ptr::copy_nonoverlapping(
                filename.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
                filename.len(),
            );
        }
        let mut completion = IoStatusBlock {
            value: IoStatusValue { status: 0 },
            information: 0,
        };
        nt_result(unsafe {
            NtSetInformationFile(
                source.as_raw_handle(),
                &mut completion,
                info.cast(),
                u32::try_from(size).map_err(io::Error::other)?,
                10,
            )
        })
    }
    pub(super) fn unlink(from: &Dir, child: &OsStr, expected: &Entry) -> io::Result<()> {
        let file = child_handle(from, child, DELETE | FILE_READ_ATTRIBUTES, 1, 0, true)?;
        if !same_remaining(&entry(&file)?, expected) {
            return Err(io::Error::other("trash entry changed"));
        }
        let disposition = 1u8;
        let mut completion = IoStatusBlock {
            value: IoStatusValue { status: 0 },
            information: 0,
        };
        nt_result(unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut completion,
                (&disposition as *const u8).cast(),
                1,
                13,
            )
        })
    }
    fn handle_path(file: &File) -> io::Result<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
        let mut path = vec![0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                0,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length as usize >= path.len() {
            return Err(io::Error::other("cleanup filesystem handle path too long"));
        }
        Ok(PathBuf::from(OsString::from_wide(&path[..length as usize])))
    }
    fn kind(meta: &fs::Metadata) -> Kind {
        if meta.file_attributes() & 0x400 != 0 || meta.file_type().is_symlink() {
            Kind::Link
        } else if meta.is_dir() {
            Kind::Directory
        } else if meta.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }
    pub(super) fn open_root(path: &Path) -> io::Result<File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if !matches!(kind(&file.metadata()?), Kind::Directory) {
            return Err(io::Error::other(
                "linked/reparse cleanup directories are not allowed",
            ));
        }
        Ok(file)
    }
    pub(super) fn open_directory(dir: &Dir, child: &OsStr) -> io::Result<File> {
        let file = child_handle(dir, child, 1 | FILE_READ_ATTRIBUTES, 1, 1, true)?;
        if !matches!(kind(&file.metadata()?), Kind::Directory) {
            return Err(io::Error::other(
                "linked/reparse cleanup directories are not allowed",
            ));
        }
        Ok(file)
    }
    pub(super) fn mkdir(dir: &Dir, child: &OsStr) -> io::Result<()> {
        // FILE_CREATE, FILE_DIRECTORY_FILE: no overwrite and no lookup through an ancestor path.
        child_handle(dir, child, 1 | FILE_READ_ATTRIBUTES, 2, 1, true).map(|_| ())
    }
    pub(super) fn open_file(dir: &Dir, child: &OsStr, create: bool) -> io::Result<File> {
        // FILE_NON_DIRECTORY_FILE; output is owner-private at creation. Reads hold no-write and
        // no-delete file handles, so a leaf cannot become a reparse point while its bytes are read.
        let access = if create { 0xC000_0000 } else { 0x8000_0000 }; // GENERIC_READ [| GENERIC_WRITE]
        let file = child_handle(dir, child, access, if create { 2 } else { 1 }, 0x40, false)?;
        if !matches!(kind(&file.metadata()?), Kind::File) {
            return Err(io::Error::other(
                "cleanup filesystem is a link/reparse point or special file",
            ));
        }
        Ok(file)
    }
    impl Dir {
        pub(super) fn kind(&self, child: &OsStr) -> io::Result<Kind> {
            leaf(child)?;
            let file = child_handle(self, child, FILE_READ_ATTRIBUTES, 1, 0, true)?;
            Ok(kind(&file.metadata()?))
        }
        pub(super) fn entries(&self) -> io::Result<Vec<OsString>> {
            self.entries_before(None)
        }
        pub(super) fn entries_before(
            &self,
            deadline: Option<Instant>,
        ) -> io::Result<Vec<OsString>> {
            // Enumerate the directory object itself, never a path through writable ancestors.
            // FILE_NAMES_INFORMATION has next/index/name-byte-length followed by UTF-16 at +12.
            use std::os::windows::ffi::OsStringExt;
            let mut names = Vec::new();
            let mut buffer = vec![0u32; 16 * 1024];
            let mut first = true;
            loop {
                if deadline.is_some_and(|end| Instant::now() >= end) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "cleanup preview time budget reached",
                    ));
                }
                if !matches!(kind(&self.file.metadata()?), Kind::Directory) {
                    return Err(io::Error::other(
                        "cleanup filesystem directory became a reparse point",
                    ));
                }
                let mut completion = IoStatusBlock {
                    value: IoStatusValue { status: 0 },
                    information: 0,
                };
                let status = unsafe {
                    NtQueryDirectoryFile(
                        self.file.as_raw_handle(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut completion,
                        buffer.as_mut_ptr().cast(),
                        (buffer.len() * 4) as u32,
                        12,
                        1,
                        std::ptr::null(),
                        u8::from(first),
                    )
                };
                first = false;
                if status as u32 == 0x8000_0006 {
                    break;
                } // STATUS_NO_MORE_FILES
                nt_result(status)?;
                let bytes = buffer[2] as usize;
                if completion.information < 12
                    || bytes % 2 != 0
                    || bytes > completion.information - 12
                    || completion.information > buffer.len() * 4
                {
                    return Err(io::Error::other(
                        "invalid cleanup filesystem directory entry",
                    ));
                }
                let name = OsString::from_wide(unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr().cast::<u16>().add(6), bytes / 2)
                });
                if name != OsStr::new(".") && name != OsStr::new("..") {
                    names.push(name);
                }
            }
            names.sort();
            Ok(names)
        }
    }
}
#[cfg(windows)]
use windows::{mkdir, open_directory, open_file, open_root};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    kind: String,
    bytes: u64,
    mtime_ns: u128,
    identity: (u64, u64),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Fingerprint {
    pub(super) bytes: u64,
    pub(super) files: u64,
    pub(super) mtime_ns: u128,
    digest: String,
    entries: BTreeMap<String, Entry>,
}
impl Fingerprint {
    pub(super) fn is_regular_file(&self) -> bool {
        self.entries
            .get("")
            .is_some_and(|entry| entry.kind == "file")
    }
}
pub(super) struct PathGuard {
    _parent: Arc<Dir>,
    _file: File,
}
pub(super) fn pin_regular(root: &Path, path: &Path) -> anyhow::Result<PathGuard> {
    let (parent, name) = parent(root, path, false)?;
    #[cfg(windows)]
    let file = windows::pin_file(&parent, &name)?;
    #[cfg(unix)]
    let file = parent.read_file(&name)?;
    ensure!(
        file.metadata()?.is_file(),
        "SQLite path is not a regular file"
    );
    Ok(PathGuard {
        _parent: parent,
        _file: file,
    })
}
pub(super) enum MoveOutcome {
    Moved { trash: PathBuf },
    Changed { retained: Option<PathBuf> },
}
/// Prepared outside lifecycle locks. The locked operation is one root metadata
/// comparison and one handle-relative rename; no enumeration or tree hashing.
pub(super) struct PreparedRename {
    root: PathBuf,
    relative: PathBuf,
    target: Arc<Dir>,
    target_name: OsString,
    trash: PathBuf,
    expected_root: Entry,
}
pub(super) fn prepare_rename(
    root: &Path,
    path: &Path,
    run_id: &str,
    item_id: &str,
    expected: &Fingerprint,
) -> anyhow::Result<Option<PreparedRename>> {
    if fingerprint(root, path).ok().as_ref() != Some(expected) {
        return Ok(None);
    }
    let trash = trash_relative(run_id, item_id)?;
    let (target, target_name) = parent(root, &trash, true)?;
    if target.exists(&target_name)? {
        return Ok(None);
    }
    Ok(Some(PreparedRename {
        root: root.canonicalize()?,
        relative: path.to_owned(),
        target,
        target_name,
        trash,
        expected_root: expected
            .entries
            .get("")
            .context("missing root fingerprint")?
            .clone(),
    }))
}
pub(super) fn rename_prepared(
    prepared: PreparedRename,
    deadline: std::time::Instant,
) -> anyhow::Result<MoveOutcome> {
    let PreparedRename {
        root,
        relative,
        target,
        target_name,
        trash,
        expected_root,
    } = prepared;
    // Do not retain source-directory pins while waiting for a lifecycle writer.
    let (source, source_name) = parent(&root, &relative, false)?;
    if std::time::Instant::now() >= deadline {
        return Ok(MoveOutcome::Changed { retained: None });
    }
    if target.exists(&target_name)? {
        return Ok(MoveOutcome::Changed {
            retained: Some(trash),
        });
    }
    if snapshot(&source, &source_name).ok().as_ref() != Some(&expected_root) {
        return Ok(MoveOutcome::Changed { retained: None });
    }
    if std::time::Instant::now() >= deadline {
        return Ok(MoveOutcome::Changed { retained: None });
    }
    if let Err(error) = move_child(&source, &source_name, &target, &target_name, &expected_root) {
        if snapshot(&target, &target_name).ok().as_ref() == Some(&expected_root) {
            return Ok(MoveOutcome::Moved { trash });
        }
        if snapshot(&source, &source_name).ok().as_ref() != Some(&expected_root) {
            return Ok(MoveOutcome::Changed { retained: None });
        }
        return Err(error.into());
    }
    Ok(MoveOutcome::Moved { trash })
}
/// Runs after the lifecycle/session locks and source-directory pins are gone.
pub(super) fn verify_renamed(
    root: &Path,
    path: &Path,
    trash: &Path,
    expected: &Fingerprint,
) -> anyhow::Result<MoveOutcome> {
    if scan(root, trash).ok().as_ref() == Some(expected) {
        return Ok(MoveOutcome::Moved {
            trash: trash.into(),
        });
    }
    let restored = (|| -> anyhow::Result<()> {
        let (target, name) = parent(root, trash, false)?;
        let (source, dest) = parent(root, path, false)?;
        let actual = snapshot(&target, &name)?;
        move_child(&target, &name, &source, &dest, &actual)?;
        Ok(())
    })()
    .is_ok();
    Ok(MoveOutcome::Changed {
        retained: (!restored).then(|| trash.to_owned()),
    })
}
#[derive(Debug)]
pub(super) struct ChangedTrash;
impl std::fmt::Display for ChangedTrash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("trash tree contains added or changed entries; retained for inspection")
    }
}
impl std::error::Error for ChangedTrash {}

fn relative(path: &Path) -> anyhow::Result<Vec<OsString>> {
    ensure!(!path.as_os_str().is_empty(), "empty cleanup path");
    path.components()
        .map(|part| match part {
            Component::Normal(name) => {
                leaf(name)?;
                Ok(name.to_owned())
            }
            _ => anyhow::bail!("cleanup path must be relative without traversal"),
        })
        .collect()
}
fn parent(root: &Path, path: &Path, create: bool) -> anyhow::Result<(Arc<Dir>, OsString)> {
    let mut names = relative(path)?;
    let name = names.pop().context("cleanup leaf")?;
    let mut dir = Dir::absolute(&root.canonicalize()?)?;
    for component in names {
        dir = if create {
            dir.directory_or_create(&component)?
        } else {
            dir.directory(&component)?
        };
    }
    Ok((dir, name))
}
#[cfg(unix)]
use unix::{move_child, snapshot, unlink};
#[cfg(windows)]
use windows::{move_child, snapshot, unlink};

fn collect_tree(
    parent: &Arc<Dir>,
    name: &OsStr,
    key: &str,
    entries: &mut BTreeMap<String, Entry>,
    depth: usize,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    check_preview_deadline(deadline)?;
    ensure!(
        depth <= 64 && entries.len() < 500_000,
        "cleanup metadata traversal budget exceeded"
    );
    let before = snapshot(parent, name)?;
    entries.insert(key.into(), before.clone());
    if before.kind == "directory" {
        let dir = parent.directory(name)?;
        ensure!(
            file_identity(&dir.file)? == before.identity,
            "cleanup directory identity changed"
        );
        let children = match dir.entries_before(deadline) {
            Ok(children) => children,
            Err(error) => {
                check_preview_deadline(deadline)?;
                return Err(error.into());
            }
        };
        check_preview_deadline(deadline)?;
        for child in children {
            leaf(&child)?;
            let key = if key.is_empty() {
                child
                    .to_str()
                    .context("non-Unicode cleanup path")?
                    .to_owned()
            } else {
                format!(
                    "{key}/{}",
                    child.to_str().context("non-Unicode cleanup path")?
                )
            };
            collect_tree(&dir, &child, &key, entries, depth + 1, deadline)?;
        }
    }
    ensure!(
        snapshot(parent, name)? == before,
        "cleanup metadata changed during inspection"
    );
    check_preview_deadline(deadline)?;
    Ok(())
}
fn scan(root: &Path, path: &Path) -> anyhow::Result<Fingerprint> {
    let (dir, name) = parent(root, path, false)?;
    scan_parent(&dir, &name, None)
}
fn scan_parent(
    dir: &Arc<Dir>,
    name: &OsStr,
    deadline: Option<Instant>,
) -> anyhow::Result<Fingerprint> {
    let mut entries = BTreeMap::new();
    collect_tree(dir, name, "", &mut entries, 0, deadline)?;
    let bytes = entries
        .values()
        .fold(0u64, |sum, e| sum.saturating_add(e.bytes));
    let files = entries
        .values()
        .filter(|entry| entry.kind == "file")
        .count() as u64;
    let mtime_ns = entries
        .values()
        .map(|entry| entry.mtime_ns)
        .max()
        .unwrap_or(0);
    let digest = crate::asks::sha256_hex(&serde_json::to_vec(&entries)?);
    Ok(Fingerprint {
        bytes,
        files,
        mtime_ns,
        digest,
        entries,
    })
}

#[derive(Debug)]
pub(super) struct PreviewDeadline;
impl std::fmt::Display for PreviewDeadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cleanup preview time budget reached")
    }
}
impl std::error::Error for PreviewDeadline {}
fn check_preview_deadline(deadline: Option<Instant>) -> anyhow::Result<()> {
    if deadline.is_some_and(|end| Instant::now() >= end) {
        return Err(PreviewDeadline.into());
    }
    Ok(())
}

/// Request-local no-follow directory pins. Windows pins deny ancestor renames/deletion, so
/// siblings can share the verified parent handles instead of reopening the whole home per file.
/// Execution still reopens and revalidates every selected item using the normal uncached path.
pub(super) struct PreviewInspector {
    #[cfg(windows)]
    root: Arc<Dir>,
    #[cfg(windows)]
    directories: BTreeMap<PathBuf, Arc<Dir>>,
    #[cfg(not(windows))]
    home: PathBuf,
}
impl PreviewInspector {
    pub(super) fn new(home: &Path) -> anyhow::Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                root: Dir::absolute(&home.canonicalize()?)?,
                directories: BTreeMap::new(),
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self { home: home.into() })
        }
    }
    pub(super) fn fingerprint(
        &mut self,
        path: &Path,
        deadline: Instant,
    ) -> anyhow::Result<Fingerprint> {
        check_preview_deadline(Some(deadline))?;
        ensure!(
            !path.starts_with("runtime/storage"),
            "cleanup state is protected"
        );
        #[cfg(windows)]
        let (dir, name) = {
            let mut names = relative(path)?;
            let name = names.pop().context("cleanup leaf")?;
            let mut dir = self.root.clone();
            let mut key = PathBuf::new();
            for component in names {
                check_preview_deadline(Some(deadline))?;
                key.push(&component);
                dir = match self.directories.get(&key) {
                    Some(pin) => pin.clone(),
                    None => {
                        let pin = dir.directory(&component)?;
                        // Bound pinned directory handles even for a tree with one directory per log.
                        if self.directories.len() >= 128 {
                            self.directories.clear();
                        }
                        self.directories.insert(key.clone(), pin.clone());
                        pin
                    }
                };
            }
            (dir, name)
        };
        // Unix permits renaming open directories: preserve its existing fresh-parent checks.
        #[cfg(not(windows))]
        let (dir, name) = parent(&self.home, path, false)?;
        scan_parent(&dir, &name, Some(deadline))
    }
}
pub(super) fn fingerprint(root: &Path, path: &Path) -> anyhow::Result<Fingerprint> {
    relative(path)?;
    ensure!(
        !path.starts_with("runtime/storage"),
        "cleanup state is protected"
    );
    scan(root, path)
}
pub(super) fn trash_relative(run_id: &str, item_id: &str) -> anyhow::Result<PathBuf> {
    for id in [run_id, item_id] {
        ensure!(
            uuid::Uuid::parse_str(id)?.to_string() == id,
            "canonical cleanup UUID required"
        );
    }
    Ok(Path::new("runtime/storage/trash")
        .join(run_id)
        .join(item_id))
}
pub(super) fn move_to_trash(
    root: &Path,
    path: &Path,
    run_id: &str,
    item_id: &str,
    expected: &Fingerprint,
) -> anyhow::Result<MoveOutcome> {
    let trash = trash_relative(run_id, item_id)?;
    let (target, target_name) = parent(root, &trash, true)?;
    if target.exists(&target_name)? {
        return Ok(if scan(root, &trash).ok().as_ref() == Some(expected) {
            MoveOutcome::Moved { trash }
        } else {
            MoveOutcome::Changed {
                retained: Some(trash),
            }
        });
    }
    if fingerprint(root, path).ok().as_ref() != Some(expected) {
        return Ok(MoveOutcome::Changed { retained: None });
    }
    let (source, source_name) = parent(root, path, false)?;
    if let Err(error) = move_child(
        &source,
        &source_name,
        &target,
        &target_name,
        expected
            .entries
            .get("")
            .context("missing root fingerprint")?,
    ) {
        if scan(root, &trash).ok().as_ref() == Some(expected) {
            return Ok(MoveOutcome::Moved { trash });
        }
        if fingerprint(root, path).ok().as_ref() != Some(expected) {
            return Ok(MoveOutcome::Changed { retained: None });
        }
        return Err(error.into());
    }
    if scan(root, &trash).ok().as_ref() != Some(expected) {
        // A changed tree is never deleted. Restore its original name only if
        // still free; otherwise retain it in this run's named trash location.
        let restored = snapshot(&target, &target_name).ok().is_some_and(|actual| {
            move_child(&target, &target_name, &source, &source_name, &actual).is_ok()
        });
        return Ok(MoveOutcome::Changed {
            retained: (!restored).then_some(trash),
        });
    }
    Ok(MoveOutcome::Moved { trash })
}
fn same_remaining(current: &Entry, expected: &Entry) -> bool {
    current.kind == expected.kind
        && current.identity == expected.identity
        && (current.kind == "directory" || current == expected)
}
fn remove_tree(
    parent: &Arc<Dir>,
    name: &OsStr,
    key: &str,
    expected: &Fingerprint,
) -> anyhow::Result<()> {
    let entry = expected
        .entries
        .get(key)
        .context("unpreviewed trash entry")?;
    ensure!(
        same_remaining(&snapshot(parent, name)?, entry),
        "trash entry changed"
    );
    if entry.kind == "directory" {
        let dir = parent.directory(name)?;
        for child in dir.entries()? {
            leaf(&child)?;
            let child_key = if key.is_empty() {
                child.to_str().context("trash name")?.to_owned()
            } else {
                format!("{key}/{}", child.to_str().context("trash name")?)
            };
            remove_tree(&dir, &child, &child_key, expected)?;
        }
        drop(dir);
    }
    unlink(parent, name, entry)?;
    Ok(())
}
pub(super) fn delete_trash(
    root: &Path,
    path: &Path,
    expected: &Fingerprint,
) -> anyhow::Result<bool> {
    let components = relative(path)?;
    ensure!(
        components.len() == 5
            && components[0] == "runtime"
            && components[1] == "storage"
            && components[2] == "trash",
        "invalid cleanup trash path"
    );
    let run = components[3].to_str().context("trash run")?;
    let item = components[4].to_str().context("trash item")?;
    ensure!(
        trash_relative(run, item)? == path,
        "invalid cleanup trash IDs"
    );
    let (dir, name) = parent(root, path, false)?;
    if !dir.exists(&name)? {
        return Ok(false);
    }
    let current = scan(root, path)?;
    // Missing previewed leaves are allowed after a restart of partial deletion.
    // Added names, replaced identities and modified files always stop deletion.
    if !current.entries.iter().all(|(name, current)| {
        expected
            .entries
            .get(name)
            .is_some_and(|old| same_remaining(current, old))
    }) {
        return Err(ChangedTrash.into());
    }
    remove_tree(&dir, &name, "", expected)?;
    Ok(true)
}

pub(super) struct StorageGuard {
    _dirs: Vec<Arc<Dir>>,
    _files: Vec<File>,
}
fn pin_storage_directory(home: &Path) -> anyhow::Result<StorageGuard> {
    let root = Dir::absolute(&home.canonicalize()?)?;
    let runtime = root.directory_or_create(OsStr::new("runtime"))?;
    let storage = runtime.directory_or_create(OsStr::new("storage"))?;
    #[cfg(windows)]
    let secured = {
        let (file, _) = crate::platform::private_fs::secure_directory(&storage.path)?;
        file
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            storage.file.metadata()?.uid() == unsafe { libc::geteuid() },
            "cleanup state owner mismatch"
        );
        storage
            .file
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    let mut files = Vec::new();
    #[cfg(windows)]
    files.push(secured);
    Ok(StorageGuard {
        _dirs: vec![root, runtime, storage],
        _files: files,
    })
}
pub(super) fn pin_storage(home: &Path) -> anyhow::Result<StorageGuard> {
    pin_storage_files(home, true)
}
pub(super) fn pin_storage_state(home: &Path) -> anyhow::Result<StorageGuard> {
    pin_storage_files(home, false)
}
fn pin_storage_files(home: &Path, include_audit: bool) -> anyhow::Result<StorageGuard> {
    let StorageGuard {
        _dirs,
        _files: mut files,
    } = pin_storage_directory(home)?;
    let storage = _dirs.last().context("cleanup storage directory")?.clone();
    for name in [
        "storage.sqlite3",
        "cleanups.jsonl",
        "consent.lock",
        "worker.lock",
    ] {
        if name == "cleanups.jsonl" && !include_audit {
            continue;
        }
        if !storage.exists(OsStr::new(name))? {
            match storage.create_file(OsStr::new(name)) {
                Ok(file) => drop(file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(windows)]
        files.push(windows::pin_file(&storage, OsStr::new(name))?);
        #[cfg(unix)]
        files.push(storage.read_file(OsStr::new(name))?);
    }
    for name in ["storage.sqlite3-wal", "storage.sqlite3-shm"] {
        match snapshot(&storage, OsStr::new(name)) {
            Ok(entry) => ensure!(entry.kind == "file", "linked cleanup SQLite sidecar"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(StorageGuard {
        _dirs,
        _files: files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn home() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cleanup-fs-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }
    fn file(root: &Path, relative: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"synthetic").unwrap();
    }
    fn ids() -> (String, String) {
        (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        )
    }
    fn moved(root: &Path, path: &Path, expected: &Fingerprint) -> PathBuf {
        let (run, item) = ids();
        match move_to_trash(root, path, &run, &item, expected).unwrap() {
            MoveOutcome::Moved { trash } => trash,
            _ => panic!("move unexpectedly changed"),
        }
    }
    fn link(target: &Path, path: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &path.to_string_lossy(),
                    &target.to_string_lossy(),
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    #[test]
    fn no_traversal_no_state_deletion_and_no_overwrite() {
        let root = home();
        file(&root, "old/a");
        for path in [
            Path::new("../outside"),
            Path::new("old/../../outside"),
            Path::new("runtime/storage/storage.sqlite3"),
            root.as_path(),
        ] {
            assert!(fingerprint(&root, path).is_err(), "{}", path.display());
        }
        assert!(trash_relative("not-a-uuid", "also-not").is_err());
        let snapshot = fingerprint(&root, Path::new("old")).unwrap();
        let (run, item) = ids();
        let trash = trash_relative(&run, &item).unwrap();
        file(&root, &trash.to_string_lossy());
        assert!(matches!(
            move_to_trash(&root, Path::new("old"), &run, &item, &snapshot).unwrap(),
            MoveOutcome::Changed { retained: Some(_) }
        ));
        assert!(root.join("old/a").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn preview_inspector_matches_strong_fingerprints_and_rechecks_changed_leaves() {
        let root = home();
        file(&root, "old/a");
        file(&root, "old/b");
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let mut inspector = PreviewInspector::new(&root).unwrap();
        assert_eq!(
            Dir::absolute(&root)
                .unwrap()
                .entries_before(Some(Instant::now()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        for name in ["old/a", "old/b"] {
            assert_eq!(
                inspector.fingerprint(Path::new(name), deadline).unwrap(),
                fingerprint(&root, Path::new(name)).unwrap()
            );
        }
        let before = inspector.fingerprint(Path::new("old/a"), deadline).unwrap();
        fs::write(
            root.join("old/a"),
            b"changed size after the parent was cached",
        )
        .unwrap();
        assert_ne!(
            before,
            inspector.fingerprint(Path::new("old/a"), deadline).unwrap()
        );
        assert!(inspector
            .fingerprint(Path::new("../outside"), deadline)
            .is_err());
        assert!(inspector
            .fingerprint(Path::new("old/a"), Instant::now())
            .unwrap_err()
            .is::<PreviewDeadline>());
        drop(inspector);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn preview_inspector_never_follows_a_linked_parent() {
        let root = home();
        file(&root, "outside/precious");
        fs::create_dir(root.join("old")).unwrap();
        link(&root.join("outside"), &root.join("old/linked"));
        let mut inspector = PreviewInspector::new(&root).unwrap();
        assert!(inspector
            .fingerprint(
                Path::new("old/linked/precious"),
                Instant::now() + std::time::Duration::from_secs(30)
            )
            .is_err());
        drop(inspector);
        assert_eq!(
            fs::read(root.join("outside/precious")).unwrap(),
            b"synthetic"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn windows_preview_parent_pins_are_bounded_and_prevent_parent_substitution() {
        let root = home();
        let mut inspector = PreviewInspector::new(&root).unwrap();
        for i in 0..140 {
            let path = format!("old/{i}/log");
            file(&root, &path);
            inspector
                .fingerprint(
                    Path::new(&path),
                    Instant::now() + std::time::Duration::from_secs(30),
                )
                .unwrap();
            assert!(inspector.directories.len() <= 128);
        }
        assert!(fs::rename(root.join("old/139"), root.join("substituted")).is_err());
        drop(inspector);
        fs::rename(root.join("old/139"), root.join("substituted")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn preview_detects_added_names_replacements_and_changed_file_metadata() {
        let root = home();
        file(&root, "old/a");
        let snapshot = fingerprint(&root, Path::new("old")).unwrap();
        file(&root, "old/b");
        let (run, item) = ids();
        assert!(matches!(
            move_to_trash(&root, Path::new("old"), &run, &item, &snapshot).unwrap(),
            MoveOutcome::Changed { retained: None }
        ));
        let snapshot = fingerprint(&root, Path::new("old/a")).unwrap();
        fs::rename(root.join("old/a"), root.join("old/a-original")).unwrap();
        file(&root, "old/a");
        assert_ne!(fingerprint(&root, Path::new("old/a")).unwrap(), snapshot);
        fs::write(root.join("old/a"), b"new bytes").unwrap();
        assert_ne!(fingerprint(&root, Path::new("old/a")).unwrap(), snapshot);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resumed_rename_does_not_touch_regenerated_source_and_partial_delete_resumes() {
        let root = home();
        file(&root, "old/a");
        file(&root, "old/b");
        let expected = fingerprint(&root, Path::new("old")).unwrap();
        let (run, item) = ids();
        let trash = match move_to_trash(&root, Path::new("old"), &run, &item, &expected).unwrap() {
            MoveOutcome::Moved { trash } => trash,
            _ => panic!(),
        };
        file(&root, "old/new");
        assert!(matches!(
            move_to_trash(&root, Path::new("old"), &run, &item, &expected).unwrap(),
            MoveOutcome::Moved { .. }
        ));
        fs::remove_file(root.join(&trash).join("a")).unwrap();
        delete_trash(&root, &trash, &expected).unwrap();
        delete_trash(&root, &trash, &expected).unwrap();
        assert!(root.join("old/new").exists());
        assert!(!root.join(trash).exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn trash_added_or_modified_entries_stop_before_other_files_are_deleted() {
        let root = home();
        file(&root, "old/a");
        let expected = fingerprint(&root, Path::new("old")).unwrap();
        let trash = moved(&root, Path::new("old"), &expected);
        file(&root, &trash.join("unseen").to_string_lossy());
        assert!(delete_trash(&root, &trash, &expected).is_err());
        assert!(root.join(&trash).join("a").exists());
        fs::remove_file(root.join(&trash).join("unseen")).unwrap();
        fs::write(root.join(&trash).join("a"), b"changed").unwrap();
        assert!(delete_trash(&root, &trash, &expected).is_err());
        assert!(root.join(&trash).join("a").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn nested_links_are_unlinked_without_reading_or_deleting_target_and_trusted_alias_works() {
        let root = home();
        let outside = home();
        file(&root, "old/a");
        file(&outside, "precious");
        link(&outside, &root.join("old/linked"));
        let expected = fingerprint(&root, Path::new("old")).unwrap();
        assert_eq!(expected.bytes, 9);
        assert_eq!(expected.files, 1);
        assert!(fingerprint(&root, Path::new("old/linked/precious")).is_err());
        let trash = moved(&root, Path::new("old"), &expected);
        delete_trash(&root, &trash, &expected).unwrap();
        assert_eq!(fs::read(outside.join("precious")).unwrap(), b"synthetic");
        let alias_parent = home();
        let alias = alias_parent.join("trusted");
        link(&root, &alias);
        file(&root, "remaining");
        assert_eq!(
            fingerprint(&alias, Path::new("remaining")).unwrap().bytes,
            9
        );
        fs::remove_dir(&alias)
            .or_else(|_| fs::remove_file(&alias))
            .unwrap();
        fs::remove_dir_all(alias_parent).unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
    #[test]
    fn state_root_link_is_refused_without_writing_outside() {
        let root = home();
        let outside = home();
        fs::create_dir_all(root.join("runtime")).unwrap();
        link(&outside, &root.join("runtime/storage"));
        assert!(pin_storage(&root).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_dir(root.join("runtime/storage"))
            .or_else(|_| fs::remove_file(root.join("runtime/storage")))
            .unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
