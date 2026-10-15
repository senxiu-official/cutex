//! Process-wide management v2 repositories.
//!
//! Release builds open each repository once per process: a daemon's home
//! never changes. Debug builds (unit tests, debug harness binaries and the
//! debug daemon) behave the same, except that a repository whose directory
//! has disappeared is reopened for the current home. Tests switch `HOME`
//! between private test homes; a repository pinned to a test home that its
//! test deleted afterwards made unrelated tests fail with "path not found"
//! depending on test order. Staying pinned otherwise keeps tests that run
//! while another test has switched `HOME` on the same (first) repository.

use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) struct ProcessRepository<T: 'static> {
    #[cfg(not(debug_assertions))]
    cell: OnceLock<T>,
    #[cfg(debug_assertions)]
    cell: OnceLock<std::sync::Mutex<Option<(PathBuf, &'static T)>>>,
}

impl<T: Send + Sync + 'static> ProcessRepository<T> {
    pub(crate) const fn new() -> Self {
        Self {
            cell: OnceLock::new(),
        }
    }

    /// The repository for `dir()`, opened with `open` on first use.
    #[cfg(not(debug_assertions))]
    pub(crate) fn get_or_open(
        &'static self,
        dir: impl FnOnce() -> anyhow::Result<PathBuf>,
        open: impl FnOnce(PathBuf) -> anyhow::Result<T>,
    ) -> anyhow::Result<&'static T> {
        use anyhow::Context;
        if let Some(repository) = self.cell.get() {
            return Ok(repository);
        }
        let repository = open(dir()?)?;
        let _ = self.cell.set(repository);
        self.cell
            .get()
            .context("management v2 repository initialization raced without a winner")
    }

    /// The repository opened first, unless its directory is gone: then the
    /// one for the current `dir()`.
    #[cfg(debug_assertions)]
    pub(crate) fn get_or_open(
        &'static self,
        dir: impl FnOnce() -> anyhow::Result<PathBuf>,
        open: impl FnOnce(PathBuf) -> anyhow::Result<T>,
    ) -> anyhow::Result<&'static T> {
        let mut pinned = self
            .cell
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((root, repository)) = pinned.as_ref() {
            if root.exists() {
                return Ok(repository);
            }
        }
        let dir = dir()?;
        // Opened under the lock so callers never see two live repositories.
        // Leaked: a debug process reopens only after a test home vanished.
        let repository: &'static T = Box::leak(Box::new(open(dir.clone())?));
        *pinned = Some((dir, repository));
        Ok(repository)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    static REPOSITORY: ProcessRepository<PathBuf> = ProcessRepository::new();

    #[test]
    fn debug_builds_stay_pinned_until_the_directory_disappears() {
        let base = std::env::temp_dir().join(format!("cutex-process-repository-{}", uuid::Uuid::new_v4()));
        let (first, second) = (base.join("first"), base.join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let opened = REPOSITORY.get_or_open(|| Ok(first.clone()), Ok).unwrap();
        assert_eq!(opened, &first);
        // Another home is current, the first still exists: stay pinned.
        let again = REPOSITORY
            .get_or_open(|| Ok(second.clone()), |_| anyhow::bail!("reopened"))
            .unwrap();
        assert!(std::ptr::eq(opened, again));
        // The first home was deleted: reopen for the current one.
        std::fs::remove_dir_all(&first).unwrap();
        assert!(REPOSITORY
            .get_or_open(|| Ok(second.clone()), |_| anyhow::bail!("open failed"))
            .is_err());
        assert_eq!(REPOSITORY.get_or_open(|| Ok(second.clone()), Ok).unwrap(), &second);
        let _ = std::fs::remove_dir_all(base);
    }
}
