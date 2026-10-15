//! Exact-occurrence cleanup after a recorded systemd scope stop timeout.
//! Callers hold Agent Management's cross-process mutation fence. This module
//! also holds the normal session-store lock across absence proof and commit.

use std::path::Path;

use anyhow::{ensure, Context};

use crate::agent_management::RuntimeOccurrenceFence;
use crate::session::model::CutexSessionRecord;
use crate::session::runtime_reconciliation::clear_runtime_fields;
use crate::session::store::with_locked_session_rows;

pub fn durable_offline_occurrence(record: &CutexSessionRecord) -> RuntimeOccurrenceFence {
    RuntimeOccurrenceFence {
        runtime_generation: record.runtime_generation,
        current_runtime_agent_id: record.current_runtime_agent_id.clone(),
        agent_bus_endpoint_ids: Vec::new(),
        pending_launch_id: record.pending_launch_id.clone(),
        app_server_launch_claim_id: record.app_server_launch_claim_id.clone(),
        alden_session_name: record.alden_session_name.clone(),
        alden_pid: record.alden_pid,
        runtime_pid: record.runtime_pid,
        app_server_pid: record
            .app_server_runtime
            .as_ref()
            .map(|binding| binding.pid),
        app_server_endpoint: record
            .app_server_runtime
            .as_ref()
            .map(|binding| binding.endpoint.clone()),
        app_server_connected: false,
    }
}

pub fn occurrence_pids(fence: &RuntimeOccurrenceFence) -> Vec<u32> {
    let mut pids = [fence.runtime_pid, fence.alden_pid, fence.app_server_pid]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    pids
}

pub fn prove_processes_and_endpoint_absent(
    expected: &RuntimeOccurrenceFence,
) -> anyhow::Result<()> {
    for pid in occurrence_pids(expected) {
        ensure!(
            pid != 0 && pid <= i32::MAX as u32,
            "invalid original runtime PID"
        );
        ensure!(
            !crate::platform::process::process_is_running(pid),
            "original runtime PID is live or reused: {pid}"
        );
    }
    if let Some(endpoint) = expected.app_server_endpoint.as_deref() {
        #[cfg(unix)]
        {
            let path = endpoint
                .strip_prefix("unix://")
                .context("original app-server transport is not a Unix socket")?;
            ensure!(
                Path::new(path).is_absolute(),
                "original app-server socket is not absolute"
            );
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => anyhow::bail!("original app-server endpoint is reachable"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => {
                    return Err(error)
                        .context("original app-server endpoint observation unavailable")
                }
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("systemd timeout endpoint proof is unsupported: {endpoint}");
    }
    Ok(())
}

/// Legacy recovery is limited to an unchanged binding that predates the
/// original offline action and exactly covers its successful PID receipt.
pub fn legacy_timeout_occurrence(
    record: &CutexSessionRecord,
    action_started_at: &str,
    receipt_pids: &[u32],
) -> anyhow::Result<RuntimeOccurrenceFence> {
    let binding = record
        .app_server_runtime
        .as_ref()
        .context("legacy timeout has no original app-server binding")?;
    let binding_started = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
    let action_started = chrono::DateTime::parse_from_rfc3339(action_started_at)?;
    ensure!(
        binding_started < action_started,
        "runtime binding was created after the original offline action"
    );
    let fence = durable_offline_occurrence(record);
    let mut expected_pids = receipt_pids.to_vec();
    expected_pids.sort_unstable();
    ensure!(
        occurrence_pids(&fence) == expected_pids,
        "runtime PIDs differ from the original offline receipt"
    );
    ensure!(
        record.runtime_generation != 0,
        "legacy runtime generation is unavailable"
    );
    Ok(fence)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::session::model::CutexSessionStore;
    use crate::session::store::{
        load_cutex_session_store_from_path, save_cutex_session_store_to_path,
    };

    #[test]
    fn dead_child_cleanup_is_exact_hidden_and_idempotent() {
        let root = std::env::temp_dir().join(format!("cutex-stop-proof-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sessions.json");
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let mut record = CutexSessionRecord::new(
            "cutex.hidden".into(),
            Some("native".into()),
            "private-host".into(),
            root.to_string_lossy().into(),
            None,
        )
        .unwrap();
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        record.runtime_generation = 7;
        record.runtime_pid = Some(child.id());
        record.exposed_to_backend = false;
        let expected = durable_offline_occurrence(&record);
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        save_cutex_session_store_to_path(&path, &store).unwrap();
        let bytes = crate::session::store::session_store_json_bytes(&path).unwrap();
        let run = || {
            reconcile_offline_occurrence(&path, "cutex.hidden", "private-host", &expected, |_| {
                prove_processes_and_endpoint_absent(&expected)
            })
        };
        assert!(run().unwrap_err().to_string().contains("live or reused"));
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), bytes);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(run().unwrap().is_proven_absent());
        let committed = crate::session::store::session_store_json_bytes(&path).unwrap();
        assert!(run().unwrap().is_proven_absent());
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), committed);
        let after = load_cutex_session_store_from_path(&path).unwrap();
        let mut wanted = record;
        wanted.runtime_pid = None;
        wanted.updated_at = after.sessions["cutex.hidden"].updated_at.clone();
        assert_eq!(after.sessions["cutex.hidden"], wanted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scope_unknown_generation_drift_foreign_and_retired_are_no_write() {
        let root = std::env::temp_dir().join(format!("cutex-stop-fences-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sessions.json");
        let mut record = CutexSessionRecord::new(
            "cutex.hidden".into(),
            Some("native".into()),
            "private-host".into(),
            root.to_string_lossy().into(),
            None,
        )
        .unwrap();
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        record.runtime_generation = 7;
        record.runtime_pid = Some(std::process::id());
        let expected = durable_offline_occurrence(&record);
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        save_cutex_session_store_to_path(&path, &store).unwrap();
        let bytes = crate::session::store::session_store_json_bytes(&path).unwrap();
        assert!(reconcile_offline_occurrence(
            &path,
            "cutex.hidden",
            "private-host",
            &expected,
            |_| anyhow::bail!("scope observation unavailable")
        )
        .is_err());
        assert!(reconcile_offline_occurrence(
            &path,
            "cutex.hidden",
            "other-host",
            &expected,
            |_| panic!("foreign effect")
        )
        .is_err());
        let mut changed = expected.clone();
        changed.runtime_generation += 1;
        assert!(reconcile_offline_occurrence(
            &path,
            "cutex.hidden",
            "private-host",
            &changed,
            |_| panic!("changed occurrence effect")
        )
        .is_err());
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), bytes);
        let record = store.sessions.get_mut("cutex.hidden").unwrap();
        record.retired_at = Some("2026-09-01T00:00:00Z".into());
        record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        save_cutex_session_store_to_path(&path, &store).unwrap();
        let retired = crate::session::store::session_store_json_bytes(&path).unwrap();
        assert!(reconcile_offline_occurrence(
            &path,
            "cutex.hidden",
            "private-host",
            &expected,
            |_| panic!("retired effect")
        )
        .is_err());
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), retired);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reachable_socket_and_reused_pid_never_prove_absence() {
        let root = std::env::temp_dir().join(format!("cutex-stop-socket-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let socket = root.join("runtime.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let record = CutexSessionRecord::new(
            "cutex.hidden".into(),
            Some("native".into()),
            "private-host".into(),
            root.to_string_lossy().into(),
            None,
        )
        .unwrap();
        let mut expected = durable_offline_occurrence(&record);
        expected.app_server_endpoint = Some(format!("unix://{}", socket.display()));
        assert!(prove_processes_and_endpoint_absent(&expected)
            .unwrap_err()
            .to_string()
            .contains("reachable"));
        drop(listener);
        assert!(prove_processes_and_endpoint_absent(&expected).is_ok());
        expected.runtime_pid = Some(std::process::id());
        assert!(prove_processes_and_endpoint_absent(&expected)
            .unwrap_err()
            .to_string()
            .contains("live or reused"));
        expected.runtime_pid = None;
        expected.app_server_endpoint = Some("unsupported://endpoint".into());
        assert!(prove_processes_and_endpoint_absent(&expected).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

pub fn reconcile_offline_occurrence(
    path: &Path,
    id: &str,
    local_host: &str,
    expected: &RuntimeOccurrenceFence,
    prove_absent: impl FnOnce(&CutexSessionRecord) -> anyhow::Result<()>,
) -> anyhow::Result<RuntimeOccurrenceFence> {
    with_locked_session_rows(path, |rows| {
        let current = rows.session(id).context("offline target disappeared")?;
        ensure!(
            current.host_id == local_host && !current.is_retired(),
            "offline target host or lifecycle changed"
        );
        ensure!(
            current.registration_class
                == crate::agent_bus::model::AgentRegistrationClass::Persistent,
            "offline target is not a persistent managed Agent"
        );
        let actual = durable_offline_occurrence(current);
        let already_cleared = actual.is_proven_absent()
            && actual.runtime_generation == expected.runtime_generation
            && actual.alden_session_name == expected.alden_session_name;
        ensure!(
            actual == *expected || already_cleared,
            "offline runtime occurrence changed"
        );
        prove_absent(current)?;
        if !already_cleared {
            clear_runtime_fields(
                rows.session_mut(id).context("offline target disappeared")?,
                true,
            );
            rows.commit()?;
        }
        Ok(durable_offline_occurrence(
            rows.session(id).context("offline target disappeared")?,
        ))
    })
}
