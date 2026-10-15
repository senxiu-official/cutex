//! Linux maintenance materialization. All writes are exclusive and anchored to
//! held directory handles. Partial output is evidence, never garbage-collected.
use crate::role_revision::Sha256;
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationFile {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub executable: bool,
    pub modified_seconds: i64,
    pub modified_nanos: i64,
    pub changed_seconds: i64,
    pub changed_nanos: i64,
    pub sha256: Sha256,
}

fn name(value: &std::ffi::OsStr) -> anyhow::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        value != "." && value != ".." && !value.as_bytes().contains(&b'/'),
        "migration child name invalid"
    );
    Ok(std::ffi::CString::new(value.as_bytes())?)
}

fn open_at(parent: &File, child: &std::ffi::OsStr, flags: i32, mode: u32) -> anyhow::Result<File> {
    let child = name(child)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            child.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    ensure!(
        fd >= 0,
        "migration open: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Walk every component without following links. Existing ancestors may be
/// root-owned. Traversal never follows links and writes use held handles, not
/// a subsequently resolved pathname. The final directory
/// must be private to this user, including when created by a previous phase.
pub(super) fn directory(path: &Path, private: bool) -> anyhow::Result<File> {
    ensure!(path.is_absolute(), "migration absolute path required");
    let mut current = File::open("/")?;
    for part in path.components().skip(1) {
        let Component::Normal(part) = part else {
            anyhow::bail!("migration path not normalized")
        };
        current = open_at(&current, part, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let m = current.metadata()?;
        ensure!(
            m.uid() == 0 || m.uid() == unsafe { libc::geteuid() },
            "migration ancestor ownership policy"
        );
    }
    let m = current.metadata()?;
    if private {
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "migration private directory required"
        );
    }
    Ok(current)
}

fn open_source(path: &Path) -> anyhow::Result<File> {
    let parent = directory(path.parent().context("source parent missing")?, false)?;
    let file = open_at(
        &parent,
        path.file_name().context("source filename missing")?,
        libc::O_RDONLY,
        0,
    )?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7022 == 0,
        "migration source type/owner/write policy"
    );
    Ok(file)
}

fn stamp(m: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}

impl MigrationFile {
    pub(super) fn capture(path: &Path, limit: u64) -> anyhow::Result<Self> {
        let mut file = open_source(path)?;
        let before = file.metadata()?;
        ensure!(
            before.len() <= limit,
            "migration source exceeds bounded size"
        );
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        ensure!(
            stamp(&before) == stamp(&file.metadata()?)
                && stamp(&before) == stamp(&open_source(path)?.metadata()?),
            "migration source changed during read"
        );
        Ok(Self {
            path: path.into(),
            device: before.dev(),
            inode: before.ino(),
            length: before.len(),
            executable: before.mode() & 0o100 != 0,
            modified_seconds: before.mtime(),
            modified_nanos: before.mtime_nsec(),
            changed_seconds: before.ctime(),
            changed_nanos: before.ctime_nsec(),
            sha256: Sha256::new(format!("{:x}", hash.finalize()))
                .map_err(|_| anyhow::anyhow!("migration digest invalid"))?,
        })
    }
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            &Self::capture(&self.path, self.length)? == self,
            "migration source stale"
        );
        Ok(())
    }
    pub(super) fn first_line(&self, limit: u64) -> anyhow::Result<Vec<u8>> {
        use std::io::BufRead;
        let file = open_source(&self.path)?;
        self.check(&file)?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        std::io::Read::by_ref(&mut reader)
            .take(limit + 1)
            .read_until(b'\n', &mut line)?;
        ensure!(
            line.len() as u64 <= limit && line.last() == Some(&b'\n'),
            "bounded native metadata line required"
        );
        self.check(reader.get_ref())?;
        self.check(&open_source(&self.path)?)?;
        Ok(line)
    }
    pub(super) fn read_bounded(&self, limit: u64) -> anyhow::Result<Vec<u8>> {
        ensure!(self.length <= limit, "migration metadata exceeds bound");
        let mut file = open_source(&self.path)?;
        self.check(&file)?;
        let mut data = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(limit + 1)
            .read_to_end(&mut data)?;
        ensure!(
            data.len() as u64 == self.length,
            "migration source length changed"
        );
        self.check(&file)?;
        use sha2::Digest;
        ensure!(
            format!("{:x}", sha2::Sha256::digest(&data)) == self.sha256.as_str(),
            "migration source digest changed"
        );
        self.check(&open_source(&self.path)?)?;
        Ok(data)
    }
    fn check(&self, file: &File) -> anyhow::Result<()> {
        ensure!(
            stamp(&file.metadata()?)
                == (
                    self.device,
                    self.inode,
                    self.length,
                    self.modified_seconds,
                    self.modified_nanos,
                    self.changed_seconds,
                    self.changed_nanos
                ),
            "migration source identity/stability changed"
        );
        Ok(())
    }
    pub(super) fn copy_to(&self, parent: &File, child: &str) -> anyhow::Result<()> {
        let mut source = open_source(&self.path)?;
        self.check(&source)?;
        let mut target = open_at(
            parent,
            child.as_ref(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            if self.executable { 0o700 } else { 0o600 },
        )?;
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        let mut length = 0u64;
        let mut buffer = [0; 65536];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            length += count as u64;
            ensure!(length <= self.length, "migration source grew");
            hash.update(&buffer[..count]);
            target.write_all(&buffer[..count])?;
        }
        ensure!(
            length == self.length && format!("{:x}", hash.finalize()) == self.sha256.as_str(),
            "migration source changed while copying"
        );
        self.check(&source)?;
        self.check(&open_source(&self.path)?)?;
        target.sync_all()?;
        parent.sync_all()?;
        Ok(())
    }
}

pub(super) fn mkdir(parent: &File, child: &str) -> anyhow::Result<File> {
    let child_name = name(child.as_ref())?;
    ensure!(
        unsafe { libc::mkdirat(parent.as_raw_fd(), child_name.as_ptr(), 0o700) } == 0,
        "migration destination exists or cannot be created: {}",
        std::io::Error::last_os_error()
    );
    let directory = open_at(
        parent,
        child.as_ref(),
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    parent.sync_all()?;
    Ok(directory)
}

pub(super) fn write_new(parent: &File, child: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = open_at(
        parent,
        child.as_ref(),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    file.write_all(bytes)?;
    file.sync_all()?;
    parent.sync_all()?;
    Ok(())
}

/// Only the fixed SQLite scratch names created in an exclusive owned directory.
/// No recursive deletion, symlink traversal, or source-path cleanup.
pub(super) fn remove_catalog_scratch(
    parent: &Path,
    child: &str,
    held: &File,
) -> anyhow::Result<()> {
    let parent_fd = directory(parent, false)?;
    let current = open_at(
        &parent_fd,
        child.as_ref(),
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    ensure!(
        (current.metadata()?.dev(), current.metadata()?.ino())
            == (held.metadata()?.dev(), held.metadata()?.ino()),
        "scratch directory replaced; retained"
    );
    for filename in [
        "state_5.sqlite",
        "state_5.sqlite-wal",
        "state_5.sqlite-shm",
        "state_5.sqlite-journal",
    ] {
        let path = parent.join(child).join(filename);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
            Ok(m) => ensure!(
                m.is_file() && m.uid() == unsafe { libc::geteuid() },
                "unexpected scratch child; retained"
            ),
        }
        let c = name(filename.as_ref())?;
        ensure!(
            unsafe { libc::unlinkat(held.as_raw_fd(), c.as_ptr(), 0) } == 0,
            "owned scratch unlink failed"
        );
    }
    let c = name(child.as_ref())?;
    ensure!(
        unsafe { libc::unlinkat(parent_fd.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) } == 0,
        "scratch not empty or removal failed; retained"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, DirBuilderExt};
    fn root() -> PathBuf {
        // Caller supplies the existing task-owned TMPDIR, not a shared /tmp.
        let root = std::env::temp_dir().join(format!("migration-fs-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        root
    }
    #[test]
    fn exclusive_copy_stale_and_interruption_are_non_destructive() {
        let root = root();
        let parent = directory(&root, true).unwrap();
        write_new(&parent, "source", b"original history\n").unwrap();
        let source = MigrationFile::capture(&root.join("source"), 1024).unwrap();
        let dest = mkdir(&parent, "new").unwrap();
        source.copy_to(&dest, "history").unwrap();
        assert!(source.copy_to(&dest, "history").is_err());
        assert!(mkdir(&parent, "new").is_err());
        assert_eq!(
            std::fs::read(root.join("source")).unwrap(),
            std::fs::read(root.join("new/history")).unwrap()
        );
        std::fs::write(root.join("source"), b"changed\n").unwrap();
        assert!(source.validate().is_err());
        assert_eq!(
            std::fs::read(root.join("new/history")).unwrap(),
            b"original history\n"
        );
    }
    #[test]
    fn symlink_and_parent_replacement_are_not_followed() {
        let root = root();
        let parent = directory(&root, true).unwrap();
        write_new(&parent, "source", b"source").unwrap();
        symlink(root.join("source"), root.join("link")).unwrap();
        assert!(MigrationFile::capture(&root.join("link"), 1024).is_err());
        symlink(&root, root.join("parent-link")).unwrap();
        assert!(directory(&root.join("parent-link"), true).is_err());
    }
}
