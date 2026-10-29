//! Transfer an already-created native Claude fork between accounts of the same provider.
//! Only projects/<one slug>/<UUID>.jsonl and that UUID's optional history directory are copied.
//! The caller holds the session mutation lease and must not resume the target until this succeeds.
//! Source files (including the SDK's new-fork backup) are never written, renamed or deleted.
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context};

#[derive(Debug, Clone)]
pub struct CopiedForkHistory {
    pub source_transcript: PathBuf,
    pub target_transcript: PathBuf,
    pub total_bytes: u64,
    pub files: u64,
}

fn validate_id(id: &str) -> anyhow::Result<()> {
    let uuid = uuid::Uuid::parse_str(id).context("native Claude session id must be a UUID")?;
    ensure!(
        id == uuid.hyphenated().to_string(),
        "native Claude session id must be a lowercase hyphenated UUID"
    );
    Ok(())
}

fn leaf(name: &OsStr) -> io::Result<()> {
    let text = name
        .to_str()
        .ok_or_else(|| io::Error::other("native history name is not Unicode"))?;
    let mut components = Path::new(name).components();
    if text.is_empty()
        || matches!(text, "." | "..")
        || text.ends_with(['.', ' '])
        || text.contains(['/', '\\', ':', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(io::Error::other("unsafe native history path component"));
    }
    Ok(())
}

fn credential_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    matches!(
        name.as_str(),
        "auth.json"
            | "accounts.json"
            | "config.json"
            | ".claude.json"
            | "claude-api-account.json"
            | "credentials.json"
            | ".credentials.json"
            | "tokens.json"
            | "token.json"
            | ".env"
            | "id_rsa"
            | "id_ed25519"
    ) || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.starts_with(".env.")
}

/// Locate exactly one native transcript without entering other account directories or following links.
/// None is not proof of an empty history: only the adapter's empty-fork receipt can authorize that.
pub fn locate_fork_transcript(home: &Path, native_id: &str) -> anyhow::Result<Option<PathBuf>> {
    validate_id(native_id)?;
    let home = Dir::absolute(home)?;
    let projects = match home.directory(OsStr::new("projects")) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let filename = OsString::from(format!("{native_id}.jsonl"));
    let mut found = None;
    for slug in projects.entries()? {
        match projects.kind(&slug)? {
            Kind::Directory => {
                let project = projects.directory(&slug)?;
                match project.kind(&filename) {
                    Ok(Kind::File) => {
                        ensure!(
                            found.is_none(),
                            "native Claude fork UUID is ambiguous between project directories"
                        );
                        found = Some(project.path.join(&filename));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => anyhow::bail!("native Claude transcript is not a regular file"),
                    Err(error) => return Err(error.into()),
                }
            }
            Kind::Link => anyhow::bail!("linked Claude project directories are not allowed"),
            _ => {}
        }
    }
    Ok(found)
}

pub fn copy_fork_history(
    source_home: &Path,
    target_home: &Path,
    native_id: &str,
    source_transcript: &Path,
) -> anyhow::Result<CopiedForkHistory> {
    copy_with(
        source_home,
        target_home,
        native_id,
        source_transcript,
        &crate::config::paths::config_dir()?,
        || Ok(()),
    )
}

/// Measure the same selected history for a same-account fork, without modifying either account.
pub fn measure_fork_history(
    home: &Path,
    native_id: &str,
    transcript: &Path,
) -> anyhow::Result<u64> {
    let (source, _, filename) = source_project(home, native_id, transcript)?;
    let mut bytes = source.read_file(&filename)?.metadata()?.len();
    let mut planned = Vec::new();
    let mut directories = Vec::new();
    match source.directory(OsStr::new(native_id)) {
        Ok(dir) => collect(&dir, Path::new(""), 0, &mut planned, &mut directories)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for (_, parent, name) in planned {
        bytes = bytes
            .checked_add(parent.read_file(&name)?.metadata()?.len())
            .context("native history byte count overflow")?;
    }
    Ok(bytes)
}

fn source_project(
    home: &Path,
    native_id: &str,
    transcript: &Path,
) -> anyhow::Result<(Arc<Dir>, OsString, OsString)> {
    validate_id(native_id)?;
    let relative = transcript
        .strip_prefix(home)
        .context("native transcript is outside its source account")?;
    let parts: Vec<_> = relative.components().collect();
    let filename = OsString::from(format!("{native_id}.jsonl"));
    ensure!(
        parts.len() == 3
            && parts[0] == Component::Normal(OsStr::new("projects"))
            && matches!(parts[1], Component::Normal(_))
            && parts[2] == Component::Normal(&filename),
        "native transcript must be projects/<one project slug>/<native UUID>.jsonl"
    );
    let slug = parts[1].as_os_str().to_owned();
    leaf(&slug)?;
    let source = Dir::absolute(home)?
        .directory(OsStr::new("projects"))?
        .directory(&slug)?;
    Ok((source, slug, filename))
}

fn copy_with(
    source_home: &Path,
    target_home: &Path,
    native_id: &str,
    source_transcript: &Path,
    receipt_home: &Path,
    before_publish: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<CopiedForkHistory> {
    let (source, slug, filename) = source_project(source_home, native_id, source_transcript)?;
    let mut main = source.read_file(&filename)?;
    // Preflight all optional history files before creating anything in the target account.
    let mut planned = Vec::new();
    let mut directories = Vec::new();
    let source_tree = match source.directory(OsStr::new(native_id)) {
        Ok(dir) => {
            collect(&dir, Path::new(""), 0, &mut planned, &mut directories)?;
            Some(dir)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let target = Dir::absolute(target_home)?
        .directory_or_create(OsStr::new("projects"))?
        .directory_or_create(&slug)?;
    ensure!(
        !target.exists(&filename)?,
        "target Claude transcript already exists; refusing to overwrite it"
    );
    // An exclusive UUID directory also reserves this transfer against another cooperating writer.
    // On failure it is retained for inspection; we never recursively delete partial user data.
    let staging_name = OsString::from(format!(".cutex-fork-{}.partial", uuid::Uuid::new_v4()));
    let staging_path = target.path.join(&staging_name);
    let target_path = target.path.join(&filename);
    // Persist visibility before creating any residue, including on process death.
    // This is an inspection receipt, never authority to delete external account paths.
    let receipt_path = receipt_home.join("runtime/claude/fork-copy-leftovers")
        .join(format!("{}.json", uuid::Uuid::new_v4()));
    let receipt = serde_json::json!({
        "version": 1, "kind": "claude-fork-copy", "state": "prepared",
        "nativeId": native_id, "createdAt": chrono::Utc::now().to_rfc3339(),
        "sourceBackup": source_transcript,
        "targetDirectory": target.path.join(native_id),
        "stagedTranscript": staging_path, "publishedTranscript": target_path,
        "cleanup": "manual-required"
    });
    crate::config::atomic::write_private_pretty_json_atomic(&receipt_path, &receipt, "Claude fork copy inspection receipt")?;
    let target_tree = target
        .new_directory(OsStr::new(native_id))
        .with_context(|| format!("target Claude history directory already exists or cannot be reserved; inspection receipt {}", receipt_path.display()))?;
    let transfer = (|| -> anyhow::Result<CopiedForkHistory> {
        let mut staged = target
            .create_file(&staging_name)
            .context("create staged native transcript")?;
        let mut total_bytes = copy_bytes(&mut main, &mut staged)
            .context("copy and flush staged native transcript")?;
        let mut files = 1u64;
        for relative in &directories {
            let mut destination = target_tree.clone();
            for component in relative.components() {
                destination = destination.directory_or_create(component.as_os_str())?;
            }
        }
        for (relative, parent, name) in &planned {
            let mut destination = target_tree.clone();
            if let Some(parent_path) = relative.parent() {
                for component in parent_path.components() {
                    destination = destination.directory_or_create(component.as_os_str())?;
                }
            }
            let mut input = parent.read_file(name)?;
            let mut output = destination.create_file(name)?;
            total_bytes = total_bytes
                .checked_add(copy_bytes(&mut input, &mut output)?)
                .context("native history byte count overflow")?;
            destination.sync()?;
            files += 1;
        }
        before_publish()?;
        // Publish a complete transcript last by an atomic rename that cannot replace an existing
        // name. Drop the staged handle first so Windows can rename it, then check the file identity.
        let staged_identity = file_identity(&staged)?;
        drop(staged);
        target
            .publish_file(&staging_name, &filename, staged_identity)
            .context("publish staged native transcript without replacement")?;
        let published = target
            .read_file(&filename)
            .context("open published native transcript for identity verification")?;
        ensure!(
            file_identity(&published)? == staged_identity,
            "staged native history was replaced during publication"
        );
        target.sync()?;
        drop(published);
        drop(source_tree);
        Ok(CopiedForkHistory {
            source_transcript: source_transcript.into(),
            target_transcript: target_path.clone(),
            total_bytes,
            files,
        })
    })();
    if transfer.is_ok() {
        // Only our own bookkeeping receipt is removed. Native history is never pruned here.
        let _ = fs::remove_file(&receipt_path);
    }
    transfer.with_context(|| format!("Claude fork history transfer failed; source backup preserved at {}; partial target directory {}; staged transcript {}; owner inspection receipt {}",
        source_transcript.display(), target_tree.path.display(), staging_path.display(), receipt_path.display()))
}

type PlannedFile = (PathBuf, Arc<Dir>, OsString);
fn collect(
    dir: &Arc<Dir>,
    relative: &Path,
    depth: usize,
    files: &mut Vec<PlannedFile>,
    directories: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    ensure!(depth <= 64, "native history directory nesting is too deep");
    for name in dir.entries()? {
        leaf(&name)?;
        ensure!(
            !credential_name(&name),
            "credential-like files are not allowed in native session history"
        );
        let child = relative.join(&name);
        match dir.kind(&name)? {
            Kind::Directory => {
                directories.push(child.clone());
                collect(
                    &dir.directory(&name)?,
                    &child,
                    depth + 1,
                    files,
                    directories,
                )?;
            }
            Kind::File => files.push((child, dir.clone(), name)),
            Kind::Link | Kind::Other => {
                anyhow::bail!("native history contains a link, reparse point or special file")
            }
        }
    }
    Ok(())
}

fn copy_bytes(source: &mut File, destination: &mut File) -> anyhow::Result<u64> {
    let before = source.metadata()?;
    ensure!(before.is_file(), "native history is not a regular file");
    let bytes = io::copy(&mut (&mut *source).take(before.len()), destination)?;
    let after = source.metadata()?;
    ensure!(
        bytes == before.len()
            && after.len() == before.len()
            && before.modified()? == after.modified()?,
        "native fork history changed during copy; keep the mutation lease held"
    );
    destination.sync_all()?;
    Ok(bytes)
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
            return Err(io::Error::other("Claude account home must be absolute"));
        }
        let mut root = PathBuf::new();
        let mut names = Vec::new();
        for c in path.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => root.push(c.as_os_str()),
                Component::Normal(name) => names.push(name.to_owned()),
                _ => return Err(io::Error::other("account home contains a parent traversal")),
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
            return Err(io::Error::other("native history is not a regular file"));
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
            Ok(if mode & libc::S_IFMT == libc::S_IFLNK {
                Kind::Link
            } else if mode & libc::S_IFMT == libc::S_IFDIR {
                Kind::Directory
            } else if mode & libc::S_IFMT == libc::S_IFREG {
                Kind::File
            } else {
                Kind::Other
            })
        }
        pub(super) fn entries(&self) -> io::Result<Vec<OsString>> {
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
        pub(super) fn publish_file(
            &self,
            from: &OsStr,
            to: &OsStr,
            _expected: (u64, u64),
        ) -> io::Result<()> {
            leaf(from)?;
            leaf(to)?;
            #[cfg(target_os = "linux")]
            let result = unsafe {
                // The syscall also works with libc implementations lacking a renameat2 wrapper.
                libc::syscall(
                    libc::SYS_renameat2,
                    self.file.as_raw_fd(),
                    name(from)?.as_ptr(),
                    self.file.as_raw_fd(),
                    name(to)?.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            #[cfg(target_os = "macos")]
            let result = unsafe {
                libc::renameatx_np(
                    self.file.as_raw_fd(),
                    name(from)?.as_ptr(),
                    self.file.as_raw_fd(),
                    name(to)?.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic no-replace native history publication is unsupported on this platform",
            ));
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Ok(())
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
                "native history parent became a reparse point",
            ));
        }
        let mut text: Vec<u16> = child.encode_wide().collect();
        let length = u16::try_from(
            text.len()
                .checked_mul(2)
                .ok_or_else(|| io::Error::other("native history name too long"))?,
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
        if !paths_equal(&actual, &expected)?
            || file_identity(&dir.file)? != parent_identity
            || !matches!(kind(&dir.file.metadata()?), Kind::Directory)
        {
            return Err(io::Error::other("native history parent binding changed"));
        }
        // Materialize the final handle identity now, before the caller can read/copy bytes.
        file_identity(&file)?;
        Ok(file)
    }

    /// Use Windows ordinal casing on the original UTF-16. Unicode lowercase or
    /// lossy UTF-8 can reject valid native paths or equate distinct filenames.
    pub(super) fn paths_equal(actual: &Path, expected: &Path) -> io::Result<bool> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CompareStringOrdinal(
                first: *const u16, first_len: i32,
                second: *const u16, second_len: i32, ignore_case: i32,
            ) -> i32;
        }
        let actual: Vec<u16> = actual.as_os_str().encode_wide().collect();
        let expected: Vec<u16> = expected.as_os_str().encode_wide().collect();
        let actual_len = i32::try_from(actual.len()).map_err(io::Error::other)?;
        let expected_len = i32::try_from(expected.len()).map_err(io::Error::other)?;
        // Explicit lengths preserve all code units; TRUE must be exactly 1.
        let comparison = unsafe {
            CompareStringOrdinal(actual.as_ptr(), actual_len, expected.as_ptr(), expected_len, 1)
        };
        match comparison {
            0 => Err(io::Error::last_os_error()),
            2 => Ok(true), // CSTR_EQUAL
            _ => Ok(false),
        }
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
            return Err(io::Error::other("native history handle path too long"));
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
                "linked/reparse Claude history directories are not allowed",
            ));
        }
        Ok(file)
    }
    pub(super) fn open_directory(dir: &Dir, child: &OsStr) -> io::Result<File> {
        let file = child_handle(dir, child, 1 | FILE_READ_ATTRIBUTES, 1, 1, true)?;
        if !matches!(kind(&file.metadata()?), Kind::Directory) {
            return Err(io::Error::other(
                "linked/reparse Claude history directories are not allowed",
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
                "native history is a link/reparse point or special file",
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
            // Enumerate the directory object itself, never a path through writable ancestors.
            // FILE_NAMES_INFORMATION has next/index/name-byte-length followed by UTF-16 at +12.
            use std::os::windows::ffi::OsStringExt;
            let mut names = Vec::new();
            let mut buffer = vec![0u32; 16 * 1024];
            let mut first = true;
            loop {
                if !matches!(kind(&self.file.metadata()?), Kind::Directory) {
                    return Err(io::Error::other(
                        "native history directory became a reparse point",
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
                    return Err(io::Error::other("invalid native history directory entry"));
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
        pub(super) fn publish_file(
            &self,
            from: &OsStr,
            to: &OsStr,
            expected: (u64, u64),
        ) -> io::Result<()> {
            leaf(from)?;
            leaf(to)?;
            // Open relative to the pinned directory, then publish relative to this source handle.
            let source = child_handle(self, from, DELETE | FILE_READ_ATTRIBUTES, 1, 0x40, false)?;
            if !matches!(kind(&source.metadata()?), Kind::File)
                || file_identity(&source)? != expected
            {
                return Err(io::Error::other(
                    "staged native history changed before publication",
                ));
            }
            // FILE_RENAME_INFO has the native FILE_RENAME_INFORMATION layout (the initial union
            // is zero, meaning ReplaceIfExists=false). Keep a trailing WCHAR, excluded from the
            // length, and the full struct's padding when extending its flexible filename array.
            let filename: Vec<u16> = to.encode_wide().chain(Some(0)).collect();
            let name_bytes = (filename.len() - 1)
                .checked_mul(2)
                .ok_or_else(|| io::Error::other("native history name is too long"))?;
            let size = std::mem::size_of::<FILE_RENAME_INFO>()
                .checked_add(name_bytes)
                .and_then(|size| size.checked_add(std::mem::size_of::<u16>()))
                .ok_or_else(|| io::Error::other("native history rename buffer is too large"))?;
            let size_u32 = u32::try_from(size).map_err(io::Error::other)?;
            // usize storage supplies FILE_RENAME_INFO's pointer alignment and owns its flexible
            // trailing WCHAR array. No pathname lookup of the destination's parent is needed.
            let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
            let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            unsafe {
                (*info).Anonymous.ReplaceIfExists = false;
                // Native same-directory rename: a NULL root and a simple leaf
                // name resolve relative to the verified source file's parent.
                // No destination-parent pathname is resolved, even if an ancestor changes.
                (*info).RootDirectory = std::ptr::null_mut();
                (*info).FileNameLength = name_bytes as u32;
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
            // std::fs opens a synchronous, non-overlapped handle above, so this native request
            // completes before its stack-owned buffer/status block may be dropped.
            let status = unsafe {
                NtSetInformationFile(
                    source.as_raw_handle(),
                    &mut completion,
                    info.cast(),
                    size_u32,
                    10, // FileRenameInformation (native enum, not Win32 FileRenameInfo = 3).
                )
            };
            if status < 0 {
                return Err(io::Error::from_raw_os_error(
                    unsafe { RtlNtStatusToDosError(status) } as i32,
                ));
            }
            Ok(())
        }
    }
}
#[cfg(windows)]
use windows::{mkdir, open_directory, open_file, open_root};

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const OTHER: &str = "660e8400-e29b-41d4-a716-446655440001";
    const HISTORY: &[u8] = b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"fixture\",\"signature\":\"opaque-signature\"},{\"type\":\"redacted_thinking\",\"data\":\"opaque-data\"}]}}\n";

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        target: PathBuf,
        project: PathBuf,
        transcript: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("cutex-fork-history-{}", uuid::Uuid::new_v4()));
            let source = root.join("source account");
            let target = root.join("target account");
            let project = source.join("projects").join("D--Work-native-cwd");
            fs::create_dir_all(&project).unwrap();
            fs::create_dir_all(&target).unwrap();
            let transcript = project.join(format!("{ID}.jsonl"));
            fs::write(&transcript, HISTORY).unwrap();
            fs::write(
                project.join(format!("{OTHER}.jsonl")),
                b"original-session-backup",
            )
            .unwrap();
            Self {
                root,
                source,
                target,
                project,
                transcript,
            }
        }
        fn target_project(&self) -> PathBuf {
            self.target.join("projects/D--Work-native-cwd")
        }
        fn children(&self) {
            let sub = self.project.join(ID).join("subagents/workflows/run-1");
            fs::create_dir_all(&sub).unwrap();
            fs::create_dir_all(self.project.join(ID).join("empty")).unwrap();
            fs::write(sub.join("agent-child.jsonl"), b"child-history\n").unwrap();
            fs::write(
                sub.join("agent-child.meta.json"),
                b"{\"parentToolUseId\":\"tool-1\"}",
            )
            .unwrap();
        }
        fn copy(&self) -> anyhow::Result<CopiedForkHistory> {
            copy_with(&self.source, &self.target, ID, &self.transcript, &self.root, || Ok(()))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(windows)]
    fn atomic_unrelated_save(directory: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let destination = directory.join("unrelated.json");
        let staged = directory.join("unrelated.json.tmp");
        fs::write(&destination, b"old unrelated fixture").unwrap();
        fs::write(&staged, b"new unrelated fixture").unwrap();
        let from: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        assert_ne!(
            unsafe {
                MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            },
            0,
            "atomic sibling save in {}: {}",
            directory.display(),
            io::Error::last_os_error()
        );
        assert_eq!(fs::read(destination).unwrap(), b"new unrelated fixture");
    }

    #[cfg(windows)]
    #[test]
    fn directory_handles_allow_unrelated_atomic_saves_during_transfer() {
        let f = Fixture::new();
        f.children();
        let source = Dir::absolute(&f.source).unwrap();
        let projects = source.directory(OsStr::new("projects")).unwrap();
        let project = projects
            .directory(OsStr::new("D--Work-native-cwd"))
            .unwrap();
        let selected = project.directory(OsStr::new(ID)).unwrap();
        let target = Dir::absolute(&f.target).unwrap();
        // Hold the exact helper handles, including every ancestor, while saving sibling files.
        for directory in [
            &f.root,
            &f.source,
            &f.target,
            &f.source.join("projects"),
            &f.project,
        ] {
            atomic_unrelated_save(directory);
        }
        let copied = copy_with(&f.source, &f.target, ID, &f.transcript, &f.root, || {
            // The transfer also holds its source files and destination directories at this point.
            for directory in [
                &f.root,
                &f.source,
                &f.target,
                &f.project,
                &f.target_project(),
            ] {
                atomic_unrelated_save(directory);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&copied.target_transcript).unwrap(), HISTORY);
        assert_eq!(fs::read(&f.transcript).unwrap(), HISTORY);
        drop((selected, project, projects, source, target));
    }

    #[test]
    fn copies_only_selected_native_history_preserving_opaque_thinking_and_source_backups() {
        let f = Fixture::new();
        f.children();
        // These account-level fixtures are deliberately outside the only permitted copy roots.
        fs::write(f.source.join("auth.json"), b"fake-never-read").unwrap();
        fs::write(f.source.join(".credentials.json"), b"fake-never-read").unwrap();
        assert_eq!(
            locate_fork_transcript(&f.source, ID).unwrap(),
            Some(f.transcript.clone())
        );
        let measured = measure_fork_history(&f.source, ID, &f.transcript).unwrap();
        let copied = f.copy().unwrap();
        assert_eq!(copied.source_transcript, f.transcript);
        assert_eq!(
            copied.target_transcript,
            f.target_project().join(format!("{ID}.jsonl"))
        );
        let child = b"child-history\n";
        let metadata = b"{\"parentToolUseId\":\"tool-1\"}";
        assert_eq!(
            copied.total_bytes,
            (HISTORY.len() + child.len() + metadata.len()) as u64
        );
        assert_eq!(measured, copied.total_bytes);
        assert_eq!(copied.files, 3);
        assert_eq!(fs::read(&copied.target_transcript).unwrap(), HISTORY);
        assert_eq!(
            fs::read(
                f.target_project()
                    .join(ID)
                    .join("subagents/workflows/run-1/agent-child.jsonl")
            )
            .unwrap(),
            child
        );
        assert!(f.target_project().join(ID).join("empty").is_dir());
        assert!(!f.target.join("auth.json").exists());
        assert!(!f.target.join(".credentials.json").exists());
        assert!(!f.target_project().join(format!("{OTHER}.jsonl")).exists());
        assert!(fs::read_dir(f.target_project()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cutex-fork-")));
        fs::write(&copied.target_transcript, b"target-edited").unwrap();
        assert_eq!(
            fs::read(&f.transcript).unwrap(),
            HISTORY,
            "target history is independent from source history"
        );
        assert_eq!(
            fs::read(f.project.join(format!("{OTHER}.jsonl"))).unwrap(),
            b"original-session-backup"
        );
    }

    #[test]
    fn copies_a_transcript_without_an_optional_subagent_directory() {
        let f = Fixture::new();
        assert_eq!(
            measure_fork_history(&f.source, ID, &f.transcript).unwrap(),
            HISTORY.len() as u64
        );
        let copied = f.copy().unwrap();
        assert_eq!(
            (copied.files, copied.total_bytes),
            (1, HISTORY.len() as u64)
        );
        assert_eq!(fs::read(&copied.target_transcript).unwrap(), HISTORY);
    }

    #[test]
    fn rejects_existing_target_transcript_or_history_directory_without_overwrite() {
        let f = Fixture::new();
        let project = f.target_project();
        fs::create_dir_all(&project).unwrap();
        let target = project.join(format!("{ID}.jsonl"));
        fs::write(&target, b"existing-transcript").unwrap();
        assert!(f.copy().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"existing-transcript");
        assert!(!project.join(ID).exists());
        fs::remove_file(&target).unwrap();
        fs::create_dir(project.join(ID)).unwrap();
        fs::write(project.join(ID).join("existing.txt"), b"existing-child").unwrap();
        assert!(f.copy().is_err());
        assert_eq!(
            fs::read(project.join(ID).join("existing.txt")).unwrap(),
            b"existing-child"
        );
        assert!(!target.exists());
        assert_eq!(fs::read(&f.transcript).unwrap(), HISTORY);
    }

    #[test]
    fn successful_copy_removes_only_its_inspection_receipt() {
        let f = Fixture::new();
        f.children();
        f.copy().unwrap();
        assert_eq!(fs::read_dir(f.root.join("runtime/claude/fork-copy-leftovers")).unwrap().count(), 0);
        assert_eq!(fs::read(f.target_project().join(format!("{ID}.jsonl"))).unwrap(), HISTORY);
        assert!(f.target_project().join(ID).is_dir());
        assert_eq!(fs::read(&f.transcript).unwrap(), HISTORY);
    }

    #[test]
    fn failed_publication_preserves_new_fork_backup_and_partial_target_without_a_resumable_jsonl() {
        let f = Fixture::new();
        f.children();
        let error = copy_with(&f.source, &f.target, ID, &f.transcript, &f.root, || {
            anyhow::bail!("injected pre-publication failure")
        })
        .unwrap_err();
        assert!(error.to_string().contains("source backup preserved"));
        assert_eq!(fs::read(&f.transcript).unwrap(), HISTORY);
        assert_eq!(
            fs::read(f.project.join(format!("{OTHER}.jsonl"))).unwrap(),
            b"original-session-backup"
        );
        assert!(!f.target_project().join(format!("{ID}.jsonl")).exists());
        assert!(f
            .target_project()
            .join(ID)
            .join("subagents/workflows/run-1/agent-child.jsonl")
            .is_file());
        let staged: Vec<_> = fs::read_dir(f.target_project())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".cutex-fork-"))
            .collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(fs::read(staged[0].path()).unwrap(), HISTORY);
        let receipts: Vec<_> = fs::read_dir(f.root.join("runtime/claude/fork-copy-leftovers")).unwrap().map(Result::unwrap).collect();
        assert_eq!(receipts.len(), 1);
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(receipts[0].path()).unwrap()).unwrap();
        assert_eq!(receipt["cleanup"], "manual-required");
        assert_eq!(Path::new(receipt["targetDirectory"].as_str().unwrap()), f.target_project().join(ID));
        assert_eq!(Path::new(receipt["stagedTranscript"].as_str().unwrap()), staged[0].path());
    }

    #[test]
    fn publication_refuses_a_target_created_after_preflight() {
        let f = Fixture::new();
        let target = f.target_project().join(format!("{ID}.jsonl"));
        let error = copy_with(&f.source, &f.target, ID, &f.transcript, &f.root, || {
            fs::write(&target, b"another-writer")?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("source backup preserved"));
        assert_eq!(
            fs::read(&target).unwrap(),
            b"another-writer",
            "no-replace publication must preserve the racing target"
        );
        assert_eq!(fs::read(&f.transcript).unwrap(), HISTORY);
        let staged: Vec<_> = fs::read_dir(f.target_project())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".cutex-fork-"))
            .collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(fs::read(staged[0].path()).unwrap(), HISTORY);
    }

    #[test]
    fn credential_like_files_inside_session_history_fail_before_target_creation() {
        let f = Fixture::new();
        fs::create_dir_all(f.project.join(ID).join("subagents")).unwrap();
        fs::write(
            f.project.join(ID).join("subagents/config.json"),
            b"fake-never-read",
        )
        .unwrap();
        assert!(f.copy().is_err());
        assert!(measure_fork_history(&f.source, ID, &f.transcript).is_err());
        assert!(!f.target.join("projects").exists());
        assert_eq!(fs::read(&f.transcript).unwrap(), HISTORY);
    }

    #[test]
    fn validates_uuid_exact_source_scope_and_ambiguous_native_ids() {
        let f = Fixture::new();
        for id in [
            "../escape",
            "",
            "not-a-uuid",
            "550e8400e29b41d4a716446655440000",
        ] {
            assert!(locate_fork_transcript(&f.source, id).is_err());
            assert!(copy_fork_history(&f.source, &f.target, id, &f.transcript).is_err());
        }
        assert!(copy_fork_history(
            &f.source,
            &f.target,
            ID,
            &f.root.join(format!("{ID}.jsonl"))
        )
        .is_err());
        assert_eq!(
            locate_fork_transcript(&f.source, "770e8400-e29b-41d4-a716-446655440002").unwrap(),
            None
        );
        let second = f.source.join("projects/second-slug");
        fs::create_dir(&second).unwrap();
        fs::write(second.join(format!("{ID}.jsonl")), HISTORY).unwrap();
        assert!(locate_fork_transcript(&f.source, ID).is_err());
    }

    #[test]
    fn simultaneous_transfers_reserve_the_target_uuid_once() {
        let f = Fixture::new();
        let results = std::thread::scope(|scope| {
            let a = scope.spawn(|| f.copy());
            let b = scope.spawn(|| f.copy());
            [a.join().unwrap(), b.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            fs::read(f.target_project().join(format!("{ID}.jsonl"))).unwrap(),
            HISTORY
        );
        assert_eq!(fs::read(&f.transcript).unwrap(), HISTORY);
    }

    #[cfg(unix)]
    #[test]
    fn unix_rejects_links_in_source_transcript_subtree_and_target_ancestors() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        let external = outside.join("external.jsonl");
        fs::write(&external, b"must-not-copy").unwrap();
        fs::remove_file(&f.transcript).unwrap();
        symlink(&external, &f.transcript).unwrap();
        assert!(locate_fork_transcript(&f.source, ID).is_err());
        assert!(f.copy().is_err());
        fs::remove_file(&f.transcript).unwrap();
        fs::write(&f.transcript, HISTORY).unwrap();
        fs::create_dir(f.project.join(ID)).unwrap();
        symlink(&outside, f.project.join(ID).join("subagents")).unwrap();
        assert!(f.copy().is_err());
        fs::remove_file(f.project.join(ID).join("subagents")).unwrap();
        symlink(&outside, f.target.join("projects")).unwrap();
        assert!(f.copy().is_err());
        assert_eq!(fs::read(&external).unwrap(), b"must-not-copy");
        assert!(!outside.join("D--Work-native-cwd").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_directory_descriptors_do_not_follow_a_replaced_project_path() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let dir = Dir::absolute(&f.source)
            .unwrap()
            .directory(OsStr::new("projects"))
            .unwrap()
            .directory(OsStr::new("D--Work-native-cwd"))
            .unwrap();
        let moved = f.root.join("moved");
        fs::rename(&f.project, &moved).unwrap();
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join(format!("{ID}.jsonl")), b"must-not-read").unwrap();
        symlink(&outside, &f.project).unwrap();
        let mut bytes = Vec::new();
        dir.read_file(OsStr::new(&format!("{ID}.jsonl")))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, HISTORY);
    }

    #[cfg(windows)]
    #[test]
    fn windows_handle_path_comparison_uses_unicode_ordinal_casing_without_loss() {
        use std::os::windows::ffi::OsStringExt;
        for (a, b) in [(r"C:\ÉTÉ\ЖЕНЕВА\Ω", r"c:\été\женева\ω"),
            (r"\\?\C:\ÉTÉ\entry", r"\\?\c:\été\ENTRY")]
        {
            assert!(windows::paths_equal(Path::new(a), Path::new(b)).unwrap());
        }
        assert!(!windows::paths_equal(Path::new(r"C:\straße"), Path::new(r"C:\STRASSE")).unwrap());
        assert!(!windows::paths_equal(Path::new(r"C:\é"), Path::new("C:\\e\u{0301}")).unwrap());
        let unpaired = PathBuf::from(OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800]));
        let replacement = PathBuf::from(OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xfffd]));
        assert!(!windows::paths_equal(&unpaired, &replacement).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_copies_history_when_non_ascii_project_case_differs() {
        let f = Fixture::new();
        let canonical = f.source.join("projects/ЖЕНЕВА");
        fs::rename(&f.project, &canonical).unwrap();
        let requested = f.source.join("projects/женева").join(format!("{ID}.jsonl"));
        let copied = copy_fork_history(&f.source, &f.target, ID, &requested).unwrap();
        assert_eq!(fs::read(&copied.target_transcript).unwrap(), HISTORY);
        assert_eq!(fs::read(canonical.join(format!("{ID}.jsonl"))).unwrap(), HISTORY);
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_junctions_without_following_them() {
        let f = Fixture::new();
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        let junction = f.target.join("projects");
        let result = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(f.copy().is_err());
        assert!(!outside.join("D--Work-native-cwd").exists());
        fs::remove_dir(&junction).unwrap();
        fs::create_dir_all(f.project.join(ID)).unwrap();
        let junction = f.project.join(ID).join("subagents");
        let result = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(f.copy().is_err());
        fs::remove_dir(&junction).unwrap();
    }
}
