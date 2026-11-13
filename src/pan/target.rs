//! Check the opened partial before the worker can write bytes. Linux creation
//! is relative to the held directory; Windows verifies and cleans by handle.
use super::*;
use std::{ffi::OsStr, fs, io};

pub(super) struct Directory {
    file: fs::File,
    path: PathBuf,
}
fn changed() -> Error {
    Error::new(
        "pan_target_changed",
        "The download directory changed; restore it or start a new download",
        false,
    )
}
fn normalized(path: &Path) -> Result<PathBuf> {
    path.to_str()
        .map(crate::agent_management::strip_verbatim_prefix)
        .map(PathBuf::from)
        .ok_or_else(changed)
}
impl Directory {
    pub(super) fn open(path: &str) -> Result<Self> {
        let directory = Self {
            file: os::open_directory(Path::new(path)).map_err(|_| changed())?,
            path: path.into(),
        };
        directory.verify()?;
        Ok(directory)
    }
    pub(super) fn verify(&self) -> Result<()> {
        if !self.file.metadata().map_err(|_| changed())?.is_dir()
            || normalized(&os::handle_path(&self.file).map_err(|_| changed())?)? != self.path
        {
            return Err(changed());
        }
        super::download::verify_target_directory(self.path.to_str().ok_or_else(changed)?)
    }
    pub(super) fn open_part(&self, name: &OsStr) -> Result<fs::File> {
        self.open_part_with(name, || {})
    }
    fn open_part_with(&self, name: &OsStr, before_open: impl FnOnce()) -> Result<fs::File> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
            || name.to_string_lossy().contains(['/', '\\'])
        {
            return Err(Error::invalid("Invalid partial file name"));
        }
        self.verify()?;
        before_open();
        let (file, created) = os::open_part(self, name).map_err(Error::storage)?;
        let validation = (|| {
            if normalized(&os::handle_path(&file).map_err(|_| changed())?)? != self.path.join(name)
            {
                return Err(changed());
            }
            self.verify()?;
            let meta = file.metadata().map_err(Error::storage)?;
            if !meta.is_file() || os::is_reparse(&meta) {
                return Err(Error::invalid("Part file is not a regular file"));
            }
            Ok(())
        })();
        if let Err(error) = validation {
            // Do not delete a pre-existing partial or a replacement. Cleanup
            // uses the original directory object / file handle, not targetDir.
            if created {
                os::remove_created(self, name, &file).map_err(Error::storage)?;
            }
            return Err(error);
        }
        Ok(file)
    }
    #[cfg(test)]
    pub(super) fn open_part_after_verify(
        &self,
        name: &OsStr,
        hook: impl FnOnce(),
    ) -> Result<fs::File> {
        self.open_part_with(name, hook)
    }
}

#[cfg(unix)]
mod os {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    };
    pub(super) fn is_reparse(_: &fs::Metadata) -> bool {
        false
    }
    pub(super) fn handle_path(file: &fs::File) -> io::Result<PathBuf> {
        fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }
    pub(super) fn open_directory(path: &Path) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }
    pub(super) fn open_part(dir: &Directory, name: &OsStr) -> io::Result<(fs::File, bool)> {
        let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
        let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        let fd = unsafe {
            libc::openat(
                dir.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CREAT | libc::O_EXCL,
                0o666,
            )
        };
        if fd >= 0 {
            return Ok((unsafe { fs::File::from_raw_fd(fd) }, true));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        let fd = unsafe { libc::openat(dir.file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((unsafe { fs::File::from_raw_fd(fd) }, false))
    }
    pub(super) fn remove_created(dir: &Directory, name: &OsStr, file: &fs::File) -> io::Result<()> {
        let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
        let metadata = file.metadata()?;
        let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
        let status = unsafe {
            libc::fstatat(
                dir.file.as_raw_fd(),
                name.as_ptr(),
                entry.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            };
        }
        let entry = unsafe { entry.assume_init() };
        if entry.st_dev as u64 == metadata.dev()
            && entry.st_ino as u64 == metadata.ino()
            && unsafe { libc::unlinkat(dir.file.as_raw_fd(), name.as_ptr(), 0) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use std::os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            FileDispositionInfo, GetFinalPathNameByHandleW, SetFileInformationByHandle, DELETE,
            FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
    };
    pub(super) fn is_reparse(meta: &fs::Metadata) -> bool {
        meta.file_attributes() & 0x400 != 0
    }
    pub(super) fn handle_path(file: &fs::File) -> io::Result<PathBuf> {
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
            return Err(io::Error::other("Opened path is too long"));
        }
        Ok(PathBuf::from(std::ffi::OsString::from_wide(
            &path[..length as usize],
        )))
    }
    pub(super) fn open_directory(path: &Path) -> io::Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if is_reparse(&file.metadata()?) {
            return Err(io::Error::other("Reparse directory"));
        }
        Ok(file)
    }
    pub(super) fn open_part(dir: &Directory, name: &OsStr) -> io::Result<(fs::File, bool)> {
        let open = |create: bool| {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .create_new(create)
                .open(dir.path.join(name))
        };
        match open(true) {
            Ok(file) => Ok((file, true)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                open(false).map(|file| (file, false))
            }
            Err(error) => Err(error),
        }
    }
    pub(super) fn remove_created(_: &Directory, _: &OsStr, file: &fs::File) -> io::Result<()> {
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
