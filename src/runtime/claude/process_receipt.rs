//! Persistent launch evidence for read-only pruning. Never removed on stop/archive;
//! a successful new launch atomically replaces the preceding occurrence's receipt.
use anyhow::{ensure, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) const FILE: &str = "process-launch-receipt.json";
const SCHEMA: &str = "cutex/claude-process-launch/v1";
pub(crate) const WARNING: &str = "Launch receipt missing or unverified, including processes started before receipts existed; cleanup waits until the affected process restarts.";

pub(crate) fn retry_once(mut write: impl FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
    write().or_else(|_| write())
}

/// Explicit runtime-status inspection only; never called by session-list projection.
pub(crate) fn warning(state_dir: &Path, pid: u32) -> Option<&'static str> {
    ProcessIdentity::read(pid)
        .and_then(|identity| references(state_dir, &identity))
        .is_err()
        .then_some(WARNING)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub executable: PathBuf,
}
impl ProcessIdentity {
    pub fn read(pid: u32) -> anyhow::Result<Self> {
        let started_at = crate::platform::process::process_started_at(pid)?;
        let executable = crate::platform::process::process_executable(pid)?.canonicalize()?;
        ensure!(
            crate::platform::process::process_started_at(pid)? == started_at,
            "Claude process occurrence changed"
        );
        Ok(Self {
            pid,
            started_at,
            executable,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    schema: String,
    state_dir: PathBuf,
    processes: Vec<ProcessIdentity>,
    // Only immutable launch references: no account, endpoint, token or prompt data.
    python: PathBuf,
    adapter_source: PathBuf,
    claude_cli: PathBuf,
}

pub(crate) fn record(
    state_dir: &Path,
    deployment: &super::ClaudeDeployment,
    launched: ProcessIdentity,
    adapter_pid: u32,
) -> anyhow::Result<()> {
    let mut processes = vec![launched];
    if adapter_pid != processes[0].pid {
        // Windows venv launchers create a base-Python child. Record both exact occurrences;
        // neither a shared interpreter path nor parent PID alone grants ownership.
        processes.push(ProcessIdentity::read(adapter_pid)?);
    } else {
        ensure!(
            ProcessIdentity::read(adapter_pid)? == processes[0],
            "Claude launch occurrence changed"
        );
    }
    let receipt = Receipt {
        schema: SCHEMA.into(),
        state_dir: state_dir.canonicalize()?,
        processes,
        python: deployment.python.canonicalize()?,
        adapter_source: deployment.adapter_source.canonicalize()?,
        claude_cli: deployment.claude_cli.canonicalize()?,
    };
    ensure!(
        serde_json::to_vec_pretty(&receipt)?.len() <= 64 * 1024,
        "Claude launch receipt exceeds budget"
    );
    crate::config::atomic::write_private_pretty_json_atomic(
        &state_dir.join(FILE),
        &receipt,
        "Claude process launch receipt",
    )
}

pub(crate) fn references(
    state_dir: &Path,
    snapshot: &ProcessIdentity,
) -> anyhow::Result<Vec<PathBuf>> {
    references_with(state_dir, snapshot, ProcessIdentity::read)
}

pub(crate) fn references_with(
    state_dir: &Path,
    snapshot: &ProcessIdentity,
    mut live: impl FnMut(u32) -> anyhow::Result<ProcessIdentity>,
) -> anyhow::Result<Vec<PathBuf>> {
    let read = || -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        std::fs::File::open(state_dir.join(FILE))?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 64 * 1024,
            "Claude launch receipt exceeds budget"
        );
        Ok(bytes)
    };
    let bytes = read()?;
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt.schema == SCHEMA && receipt.state_dir == state_dir.canonicalize()?,
        "Claude launch receipt scope mismatch"
    );
    let recorded = receipt
        .processes
        .iter()
        .find(|p| p.pid == snapshot.pid)
        .context("Claude process has no launch receipt")?;
    // CIM timestamps have microsecond precision. Live OS identity must additionally
    // match the full-precision durable birth and executable, rejecting PID reuse.
    ensure!(
        recorded.started_at.timestamp_micros() == snapshot.started_at.timestamp_micros()
            && recorded.executable == snapshot.executable.canonicalize()?,
        "Claude process snapshot changed"
    );
    ensure!(
        &live(snapshot.pid)? == recorded,
        "Claude process occurrence changed"
    );
    let paths = vec![receipt.python, receipt.adapter_source, receipt.claude_cli];
    ensure!(
        paths.iter().all(|p| p.is_absolute()),
        "Claude launch paths unavailable"
    );
    ensure!(
        bytes == read()? && &live(snapshot.pid)? == recorded,
        "Claude launch evidence changed during inspection"
    );
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_write_retries_once_and_stops_after_success() {
        let mut attempts = 0;
        retry_once(|| {
            attempts += 1;
            if attempts == 1 {
                anyhow::bail!("transient")
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(attempts, 2);
        attempts = 0;
        assert!(retry_once(|| {
            attempts += 1;
            anyhow::bail!("unavailable")
        })
        .is_err());
        assert_eq!(attempts, 2);
        attempts = 0;
        retry_once(|| {
            attempts += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(attempts, 1);
    }

    #[test]
    fn receipt_owned_child() {
        if std::env::var_os("CUTEX_RECEIPT_OWNED_CHILD").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    #[test]
    fn venv_launcher_and_adapter_have_two_verified_receipt_entries() {
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root =
            std::env::temp_dir().join(format!("claude-parent-child-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut command = std::process::Command::new(&executable);
        command
            .args([
                "--exact",
                "runtime::claude::process_receipt::tests::receipt_owned_child",
            ])
            .env("CUTEX_RECEIPT_OWNED_CHILD", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let child = Owned(command.spawn().unwrap());
        let parent = ProcessIdentity::read(std::process::id()).unwrap();
        let adapter = ProcessIdentity::read(child.0.id()).unwrap();
        assert_ne!(parent.pid, adapter.pid);
        let deployment = super::super::ClaudeDeployment {
            python: executable.clone(),
            adapter_source: root.clone(),
            claude_cli: executable.clone(),
            claude_home: root.clone(),
            tui_binary: executable,
        };
        record(&root, &deployment, parent.clone(), adapter.pid).unwrap();
        let bytes = std::fs::read(root.join(FILE)).unwrap();
        let receipt: Receipt = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt.processes, vec![parent.clone(), adapter.clone()]);
        assert!(references(&root, &parent).is_ok());
        assert!(references(&root, &adapter).is_ok());
        assert!(warning(&root, adapter.pid).is_none());
        let missing = root.join("missing");
        assert_eq!(warning(&missing, adapter.pid), Some(WARNING));
        drop(child);
        assert_eq!(bytes, std::fs::read(root.join(FILE)).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn persisted_launch_receipt_requires_birth_executable_and_scope() {
        let root =
            std::env::temp_dir().join(format!("claude-launch-proof-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let deployment = super::super::ClaudeDeployment {
            python: std::env::current_exe().unwrap(),
            adapter_source: root.clone(),
            claude_cli: std::env::current_exe().unwrap(),
            claude_home: root.clone(),
            tui_binary: std::env::current_exe().unwrap(),
        };
        let identity = ProcessIdentity::read(std::process::id()).unwrap();
        assert!(references(&root, &identity).is_err());
        record(&root, &deployment, identity.clone(), identity.pid).unwrap();
        let before = std::fs::read(root.join(FILE)).unwrap();
        assert!(references(&root, &identity)
            .unwrap()
            .contains(&root.canonicalize().unwrap()));
        let mut reused = identity.clone();
        reused.started_at += chrono::Duration::seconds(1);
        assert!(references(&root, &reused).is_err());
        assert!(references_with(&root, &identity, |_| Ok(reused.clone())).is_err());
        let mut wrong_exe = identity.clone();
        wrong_exe.executable = root.clone();
        assert!(references(&root, &wrong_exe).is_err());
        let other = root.join("other-session");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(FILE), &before).unwrap();
        assert!(references(&other, &identity).is_err());
        // A failed or successful inspection never removes or rewrites the evidence.
        assert_eq!(before, std::fs::read(root.join(FILE)).unwrap());
        record(&root, &deployment, identity.clone(), identity.pid).unwrap();
        assert!(references(&root, &identity).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
