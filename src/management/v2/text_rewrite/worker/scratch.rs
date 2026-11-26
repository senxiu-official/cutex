use std::{
    fs::{self, File, Metadata},
    io,
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context};
use fs2::FileExt;

const LEASE: &str = "active.lock";

pub(super) struct WorkDir {
    path: PathBuf,
    lease: Option<File>,
}

impl WorkDir {
    pub(super) fn create() -> anyhow::Result<Self> {
        let root = root()?;
        crate::profiles::materialize::create_private_dir_all(&root)?;
        secure_dir(&root)?;
        let _mutation = mutation_lock(&root)?;
        let path = root.join(uuid::Uuid::new_v4().simple().to_string());
        crate::profiles::materialize::create_private_dir_all(&path)?;
        secure_dir(&path)?;
        let lease = lock_file(&path.join(LEASE), true)?;
        FileExt::lock_shared(&lease)?;
        Ok(Self {
            path,
            lease: Some(lease),
        })
    }

    pub(super) fn attach(path: PathBuf) -> anyhow::Result<Self> {
        let root = root()?;
        ensure!(
            path.parent() == Some(root.as_path()) && rewrite_dir_name(&path),
            "invalid rewrite directory"
        );
        let _mutation = mutation_lock(&root)?;
        validate_dir(&path)?;
        let lease = lock_file(&path.join(LEASE), false)?;
        FileExt::lock_shared(&lease)?;
        Ok(Self {
            path,
            lease: Some(lease),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        // Both processes hold shared leases. An orphaned worker can reclaim
        // its own directory on normal exit, including on Unix after daemon loss.
        drop(self.lease.take());
        if self.path.parent().is_some_and(|root| {
            mutation_lock(root)
                .and_then(|_mutation| reclaim(&self.path))
                .is_err()
        }) {
            eprintln!("text rewrite: scratch cleanup incomplete");
        }
    }
}

fn root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("text-rewrite"))
}

pub(super) fn sweep_startup() -> anyhow::Result<()> {
    sweep(&root()?)
}

fn sweep(root: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(root) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let _mutation = mutation_lock(root)?;
    let mut result = Ok(());
    for entry in fs::read_dir(root)? {
        let attempt = entry
            .context("cannot read rewrite entry")
            .and_then(|entry| reclaim(&entry.path()));
        if attempt.is_err() {
            result = attempt;
        }
    }
    result
}

fn reclaim(path: &Path) -> anyhow::Result<()> {
    if !rewrite_dir_name(path) {
        return Ok(());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_plain_dir(&metadata) => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    validate_dir(path)?;
    let lease_path = path.join(LEASE);
    match fs::symlink_metadata(&lease_path) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => {}
        Ok(_) => return Ok(()),
        // A directory without a lease could be from an older build. An
        // empty directory cannot be holding a persisted draft.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return match fs::remove_dir(path) {
                Ok(()) => Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) =>
                {
                    Ok(())
                }
                Err(error) => Err(error.into()),
            };
        }
        Err(error) => return Err(error.into()),
    }
    let lease = lock_file(&lease_path, false)?;
    match FileExt::try_lock_exclusive(&lease) {
        Ok(()) => {}
        Err(error) if crate::platform::file_lock::lock_contended(&error) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    // Keep the root lock across the close and deletion window (needed on
    // Windows). No other Cutex process can attach in between.
    drop(lease);
    fs::remove_dir_all(path)?;
    Ok(())
}

fn rewrite_dir_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            uuid::Uuid::parse_str(name)
                .is_ok_and(|uuid| uuid.get_version_num() == 4 && uuid.simple().to_string() == name)
        })
}

fn mutation_lock(root: &Path) -> anyhow::Result<File> {
    validate_dir(root)?;
    let file = lock_file(&root.join("mutation.lock"), true)?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn is_plain_dir(metadata: &Metadata) -> bool {
    metadata.is_dir() && !is_link(metadata)
}

#[cfg(unix)]
fn is_link(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

fn secure_dir(path: &Path) -> anyhow::Result<()> {
    ensure!(
        is_plain_dir(&fs::symlink_metadata(path)?),
        "invalid rewrite directory"
    );
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(path)?;
    validate_dir(path)
}

fn validate_dir(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(is_plain_dir(&metadata), "invalid rewrite directory");
    #[cfg(unix)]
    validate_private_unix(&metadata)?;
    #[cfg(windows)]
    crate::platform::private_fs::open_validated_directory(path)?;
    Ok(())
}

#[cfg(unix)]
fn validate_private_unix(metadata: &Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
        "rewrite path is not owner-private"
    );
    Ok(())
}

fn lock_file(path: &Path, create: bool) -> anyhow::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "invalid rewrite lock");
        validate_private_unix(&metadata)?;
        Ok(file)
    }
    #[cfg(windows)]
    {
        Ok(crate::platform::private_fs::open_private_file_path(
            path, create, true,
        )?)
    }
}

#[cfg(test)]
pub(super) struct TestDir(pub PathBuf);

#[cfg(test)]
impl TestDir {
    pub(super) fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("rewrite-test-{}", uuid::Uuid::new_v4().simple()));
        crate::profiles::materialize::create_private_dir_all(&path).unwrap();
        secure_dir(&path).unwrap();
        Self(path)
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_reclaims_only_inactive_owned_rewrite_dirs() {
        let fixture = TestDir::new();
        let root = fixture.0.join("text-rewrite");
        crate::profiles::materialize::create_private_dir_all(&root).unwrap();
        secure_dir(&root).unwrap();
        let directory = |name: String| {
            let path = root.join(name);
            crate::profiles::materialize::create_private_dir_all(&path).unwrap();
            secure_dir(&path).unwrap();
            fs::write(path.join("synthetic-draft"), "loopback output").unwrap();
            path
        };
        let inactive = directory(uuid::Uuid::new_v4().simple().to_string());
        drop(lock_file(&inactive.join(LEASE), true).unwrap());
        let active = directory(uuid::Uuid::new_v4().simple().to_string());
        let lease = lock_file(&active.join(LEASE), true).unwrap();
        FileExt::lock_shared(&lease).unwrap();
        let legacy = directory(uuid::Uuid::new_v4().simple().to_string());
        let foreign = directory("foreign".into());
        #[cfg(unix)]
        let link = {
            let path = root.join(uuid::Uuid::new_v4().simple().to_string());
            std::os::unix::fs::symlink(&foreign, &path).unwrap();
            path
        };
        sweep(&root).unwrap();
        assert!(!inactive.exists());
        assert!(active.join("synthetic-draft").is_file());
        assert!(legacy.join("synthetic-draft").is_file());
        assert!(foreign.join("synthetic-draft").is_file());
        #[cfg(unix)]
        assert!(link.is_symlink());
        drop(lease);
        sweep(&root).unwrap();
        assert!(!active.exists());
    }
}
