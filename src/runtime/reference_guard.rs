//! Cross-process fence between runtime reference publication/use and pruning.
//!
//! Reference users acquire shared access BEFORE reading a deployment/manifest
//! and retain it through binding publication or spawn. Pruning takes the upgrade
//! operation lock, then exclusive access, then any session-store lock. Never
//! wait for this fence while holding a session-store/controller lock.
//!
//! Shared access is reentrant on one thread, including when an exclusive waiter
//! is queued. Guards are deliberately !Send: nesting belongs to that thread.
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use anyhow::{ensure, Context};

const LOCK_FILE: &str = "runtime-reference.lock";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Shared,
    Exclusive,
}

struct Lease {
    file: File,
    mode: Mode,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

thread_local! {
    static HELD: RefCell<HashMap<PathBuf, Weak<Lease>>> = RefCell::new(HashMap::new());
}

/// Shared runtime-reference permit. The last nested guard releases the lock,
/// even when guards are dropped in a different order from acquisition.
#[must_use]
pub struct SharedGuard {
    _lease: Rc<Lease>,
}

/// Exclusive prune permit. Not reentrant; shared-to-exclusive upgrades fail.
#[must_use]
pub struct ExclusiveGuard {
    _lease: Rc<Lease>,
}

fn lock_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join(LOCK_FILE))
}

fn acquire_at(path: &Path, mode: Mode) -> anyhow::Result<Rc<Lease>> {
    let parent = path
        .parent()
        .context("runtime reference lock has no parent")?;
    std::fs::create_dir_all(parent).context("create runtime reference lock directory")?;
    // Resolve aliases before looking up nesting (notably Windows verbatim paths).
    let key = parent
        .canonicalize()?
        .join(path.file_name().context("missing lock filename")?);
    if let Some(held) = HELD.with(|slots| slots.borrow().get(&key).and_then(Weak::upgrade)) {
        ensure!(
            held.mode == Mode::Shared && mode == Mode::Shared,
            "runtime reference lock cannot be upgraded or nested exclusively"
        );
        return Ok(held);
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&key)
        .context("open runtime reference lock")?;
    match mode {
        Mode::Shared => fs2::FileExt::lock_shared(&file),
        Mode::Exclusive => fs2::FileExt::lock_exclusive(&file),
    }
    .context("acquire runtime reference lock")?;
    let lease = Rc::new(Lease { file, mode });
    HELD.with(|slots| {
        let mut slots = slots.borrow_mut();
        slots.retain(|_, lease| lease.strong_count() > 0);
        slots.insert(key, Rc::downgrade(&lease));
    });
    Ok(lease)
}

pub fn acquire_shared() -> anyhow::Result<SharedGuard> {
    Ok(SharedGuard {
        _lease: acquire_at(&lock_path()?, Mode::Shared)?,
    })
}

/// Serialize final reference revalidation and directory isolation against all
/// cooperating reference users. The caller must not already hold shared access.
pub fn acquire_exclusive() -> anyhow::Result<ExclusiveGuard> {
    Ok(ExclusiveGuard {
        _lease: acquire_at(&lock_path()?, Mode::Exclusive)?,
    })
}

/// Short spelling for the prune entry point.
pub fn exclusive() -> anyhow::Result<ExclusiveGuard> {
    acquire_exclusive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Fixture(PathBuf);
    impl Fixture {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("cutex-reference-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Fixture(root)
    }

    fn wait_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn nested_shared_keeps_lease_until_last_guard_and_does_not_wait_for_writer() {
        let dir = fixture();
        let path = dir.path().join(LOCK_FILE);
        let first = acquire_at(&path, Mode::Shared).unwrap();
        let (tx, rx) = mpsc::channel();
        let other = path.clone();
        let writer = std::thread::spawn(move || {
            tx.send(false).unwrap();
            let _exclusive = acquire_at(&other, Mode::Exclusive).unwrap();
            tx.send(true).unwrap();
        });
        assert!(!rx.recv_timeout(Duration::from_secs(5)).unwrap());
        let nested = acquire_at(&dir.path().join(".").join(LOCK_FILE), Mode::Shared).unwrap();
        assert!(Rc::ptr_eq(&first, &nested));
        drop(first);
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(nested);
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        writer.join().unwrap();
    }

    #[test]
    fn rejects_lock_upgrades_and_exclusive_recursion_without_waiting() {
        let dir = fixture();
        let path = dir.path().join(LOCK_FILE);
        let shared = acquire_at(&path, Mode::Shared).unwrap();
        assert!(acquire_at(&path, Mode::Exclusive).is_err());
        drop(shared);
        let exclusive = acquire_at(&path, Mode::Exclusive).unwrap();
        assert!(acquire_at(&path, Mode::Exclusive).is_err());
        assert!(acquire_at(&path, Mode::Shared).is_err());
        drop(exclusive);
        assert!(acquire_at(&path, Mode::Shared).is_ok());
    }

    #[test]
    fn shared_users_overlap_and_independent_paths_do_not_share_leases() {
        let dir = fixture();
        let path = dir.path().join(LOCK_FILE);
        let shared = acquire_at(&path, Mode::Shared).unwrap();
        let other = path.clone();
        std::thread::spawn(move || {
            let _shared = acquire_at(&other, Mode::Shared).unwrap();
        })
        .join()
        .unwrap();
        let _exclusive = acquire_at(&dir.path().join("other.lock"), Mode::Exclusive).unwrap();
        drop(shared);
    }

    // Spawn this exact test in a separate process. No HOME mutation or live ports.
    #[test]
    fn child_lock_process() {
        let Some(root) = std::env::var_os("CUTEX_REFERENCE_GUARD_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let mode = if std::env::var_os("CUTEX_REFERENCE_GUARD_TEST_EXCLUSIVE").is_some() {
            Mode::Exclusive
        } else {
            Mode::Shared
        };
        std::fs::write(root.join("attempting"), b"ready").unwrap();
        let _lease = acquire_at(&root.join(LOCK_FILE), mode).unwrap();
        std::fs::write(root.join("acquired"), b"ready").unwrap();
    }

    fn child(root: &Path, exclusive: bool) -> std::process::Child {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "runtime::reference_guard::tests::child_lock_process",
                "--nocapture",
            ])
            .env("CUTEX_REFERENCE_GUARD_TEST_ROOT", root)
            .env_remove("CUTEX_REFERENCE_GUARD_TEST_EXCLUSIVE");
        if exclusive {
            command.env("CUTEX_REFERENCE_GUARD_TEST_EXCLUSIVE", "1");
        }
        command.spawn().unwrap()
    }

    #[test]
    fn cross_process_shared_blocks_exclusive_and_exclusive_blocks_shared() {
        for parent_mode in [Mode::Shared, Mode::Exclusive] {
            let dir = fixture();
            let permit = acquire_at(&dir.path().join(LOCK_FILE), parent_mode).unwrap();
            let mut child = child(dir.path(), parent_mode == Mode::Shared);
            wait_file(&dir.path().join("attempting"));
            std::thread::sleep(Duration::from_millis(50));
            assert!(!dir.path().join("acquired").exists());
            drop(permit);
            wait_file(&dir.path().join("acquired"));
            assert!(child.wait().unwrap().success());
        }
    }
}
