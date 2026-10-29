//! Generation-fenced evidence for an explicitly requested runtime stop.
//!
//! Observers in different processes see the same receipt. A pending receipt is
//! valid only while its operation holds the OS file lock; process death cannot
//! leave an agent permanently "closing". Completion means the stop operation
//! proved the process stopped, never merely that a socket reset occurred.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopState { Closing, Offline, Failed }

#[derive(Serialize, Deserialize)]
struct Receipt {
    session: String,
    generation: u64,
    state: StopState,
}

pub(crate) fn path(session: &str, generation: u64) -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("runtime-stops")
        .join(format!("{:x}-{generation}.json", Sha256::digest(session.as_bytes()))))
}

pub struct RequestedStop {
    path: PathBuf,
    receipt: Receipt,
    // Keep the lock through the final atomic write, including error unwinding.
    _lock: File,
    finished: bool,
}

impl RequestedStop {
    pub fn run<T>(session: &str, generation: u64, operation: impl FnOnce() -> anyhow::Result<T>,
        stopped: impl FnOnce(&T) -> bool) -> anyhow::Result<T> {
        // Never-started legacy sessions have no occurrence to classify.
        let guard = (generation > 0).then(|| Self::begin(session, generation)).transpose()?;
        let result = operation()?;
        if let Some(guard) = guard { guard.finish(stopped(&result))?; }
        Ok(result)
    }

    pub fn begin(session: &str, generation: u64) -> anyhow::Result<Self> {
        Self::begin_at(path(session, generation)?, session, generation)
    }

    fn begin_at(path: PathBuf, session: &str, generation: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(generation > 0, "runtime stop requires a proven generation");
        crate::profiles::materialize::create_private_dir_all(path.parent().context("stop receipt parent")?)?;
        let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(path.with_extension("lock"))?;
        lock.try_lock_exclusive().context("runtime stop already in progress")?;
        let receipt = Receipt { session: session.into(), generation, state: StopState::Closing };
        crate::config::atomic::write_private_pretty_json_atomic(&path, &receipt, "runtime stop")?;
        Ok(Self { path, receipt, _lock: lock, finished: false })
    }

    pub fn finish(mut self, stopped: bool) -> anyhow::Result<()> {
        self.receipt.state = if stopped { StopState::Offline } else { StopState::Failed };
        crate::config::atomic::write_private_pretty_json_atomic(&self.path, &self.receipt, "runtime stop")?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for RequestedStop {
    fn drop(&mut self) {
        if !self.finished {
            self.receipt.state = StopState::Failed;
            let _ = crate::config::atomic::write_private_pretty_json_atomic(
                &self.path, &self.receipt, "failed runtime stop");
        }
    }
}

pub fn state(session: &str, generation: u64) -> Option<StopState> {
    state_at(&path(session, generation).ok()?, session, generation)
}

pub(crate) fn state_at(path: &Path, session: &str, generation: u64) -> Option<StopState> {
    let receipt: Receipt = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if receipt.session != session || receipt.generation != generation || generation == 0 {
        return None;
    }
    if receipt.state != StopState::Closing { return Some(receipt.state); }
    // Do not create files on the observation path. An unlocked pending receipt
    // means its writer died; uncertainty remains an error, never an offline.
    let lock = OpenOptions::new().read(true).write(true).open(path.with_extension("lock")).ok()?;
    match lock.try_lock_exclusive() {
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Some(StopState::Closing),
        Ok(()) => {
            // Completion can race the first read. Reread under the lock so a
            // late disconnect cannot follow a successful offline with error.
            let latest: Receipt = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
            (latest.session == session && latest.generation == generation
                && latest.state != StopState::Closing).then_some(latest.state)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_path() -> PathBuf {
        std::env::temp_dir().join(format!("stop-{}", uuid::Uuid::new_v4())).join("receipt.json")
    }

    #[test]
    fn requested_stop_is_generation_fenced_and_only_success_becomes_offline() {
        let path = receipt_path();
        let stop = RequestedStop::begin_at(path.clone(), "agent", 7).unwrap();
        assert_eq!(state_at(&path, "agent", 7), Some(StopState::Closing));
        assert_eq!(state_at(&path, "agent", 8), None);
        assert_eq!(state_at(&path, "other", 7), None);
        assert!(RequestedStop::begin_at(path.clone(), "agent", 7).is_err());
        stop.finish(true).unwrap();
        assert_eq!(state_at(&path, "agent", 7), Some(StopState::Offline));
        let stop = RequestedStop::begin_at(path.clone(), "agent", 8).unwrap();
        stop.finish(false).unwrap();
        assert_eq!(state_at(&path, "agent", 8), Some(StopState::Failed));
        assert_eq!(state_at(&path, "agent", 7), None);
    }

    #[test]
    fn failure_or_abandoned_pending_receipt_never_hides_a_crash() {
        let path = receipt_path();
        drop(RequestedStop::begin_at(path.clone(), "agent", 2).unwrap());
        assert_eq!(state_at(&path, "agent", 2), Some(StopState::Failed));
        // Model a process exiting without Drop: the OS releases its file lock.
        let receipt = Receipt { session: "agent".into(), generation: 2, state: StopState::Closing };
        crate::config::atomic::write_private_pretty_json_atomic(&path, &receipt, "fixture").unwrap();
        assert_eq!(state_at(&path, "agent", 2), None);
        assert_eq!(state_at(&path, "agent", 0), None);
    }

    #[test]
    fn late_old_generation_cannot_replace_new_pending_or_terminal_receipt() {
        let _home = crate::config::paths::test_home_env_lock();
        let id = format!("cutex.stop-race-{}", uuid::Uuid::new_v4());
        let old = RequestedStop::begin(&id, 7).unwrap();
        let new = RequestedStop::begin(&id, 8).unwrap();
        old.finish(true).unwrap();
        assert_eq!(state(&id, 8), Some(StopState::Closing));
        new.finish(true).unwrap();
        // A stale caller arriving even after the new stop completed is isolated.
        drop(RequestedStop::begin(&id, 7).unwrap());
        assert_eq!(state(&id, 8), Some(StopState::Offline));
        assert_eq!(state(&id, 7), Some(StopState::Failed));
    }

    #[test]
    fn stop_receipt_child_process() {
        let Some(path) = std::env::var_os("CUTEX_STOP_RECEIPT_CHILD") else { return };
        let path = PathBuf::from(path);
        let _stop = RequestedStop::begin_at(path.clone(), "child", 9).unwrap();
        std::fs::write(path.with_extension("ready"), b"ready").unwrap();
        while !path.with_extension("exit").exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Actual process death, deliberately bypassing Rust Drop.
        std::process::exit(23);
    }

    #[test]
    fn another_process_observes_pending_only_until_owner_process_dies() {
        let _home = crate::config::paths::test_home_env_lock();
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let path = receipt_path();
        let mut child = Child(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session::runtime_stop::tests::stop_receipt_child_process", "--nocapture"])
            .env("CUTEX_STOP_RECEIPT_CHILD", &path).spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !path.with_extension("ready").exists() {
            assert!(std::time::Instant::now() < deadline, "child did not acquire receipt");
            assert!(child.0.try_wait().unwrap().is_none());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(state_at(&path, "child", 9), Some(StopState::Closing));
        std::fs::write(path.with_extension("exit"), b"exit").unwrap();
        assert_eq!(child.0.wait().unwrap().code(), Some(23));
        assert_eq!(state_at(&path, "child", 9), None);
    }
}
