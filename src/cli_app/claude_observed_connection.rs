//! Proof belongs to one authenticated observer connection, never a candidate row.
use super::manager;
use cutex::{agent_bus::claude_occurrence::Occurrence, session::model::CutexSessionRecord};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Proof {
    occurrence: Occurrence,
    native_session: Option<String>,
    state_dir: PathBuf,
    deployment: cutex::runtime::claude::ClaudeDeployment,
    host: String,
    birth: Option<chrono::DateTime<chrono::Utc>>,
    ticket: u64,
    delivery_blocked: bool,
}
fn proofs() -> &'static Mutex<HashMap<String, Proof>> {
    static PROOFS: OnceLock<Mutex<HashMap<String, Proof>>> = OnceLock::new();
    PROOFS.get_or_init(Default::default)
}
pub(super) fn forget(id: &str) {
    proofs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(id);
}
pub(super) fn snapshot(id: &str) -> Option<Proof> {
    proofs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(id)
        .cloned()
}
pub(super) fn forget_if(id: &str, expected: &Proof) -> bool {
    let mut saved = proofs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if saved.get(id) != Some(expected) {
        return false;
    }
    saved.remove(id);
    true
}
pub(super) fn prepare(record: &CutexSessionRecord) -> anyhow::Result<Proof> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let occurrence = Occurrence::from_record(record)
        .ok_or_else(|| anyhow::anyhow!("Claude occurrence missing after connect"))?;
    let session = record.claude_session.as_ref().unwrap();
    let proof = Proof {
        birth: cutex::platform::process::process_started_at(occurrence.pid).ok(),
        occurrence,
        native_session: session.native_session_id.clone(),
        state_dir: session.state_dir.clone(),
        deployment: session.deployment.clone(),
        host: record.host_id.clone(),
        ticket: NEXT.fetch_add(1, Ordering::Relaxed),
        delivery_blocked: false,
    };
    Ok(proof)
}
pub(super) fn remember(id: &str, proof: Proof) {
    proofs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_owned(), proof);
}
pub(super) fn eligible(record: &CutexSessionRecord) -> anyhow::Result<bool> {
    if record.is_retired()
        || record.pending_launch_id.is_some()
        || record.app_server_launch_claim_id.is_some()
        || !cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &cutex::platform::host::current_host_name(),
        )
    {
        return Ok(false);
    }
    let authority = cutex::agent_management::archive_authority_snapshot_shared()?;
    Ok(
        !cutex::agent_management::agent_execution_blocked_in(&authority, record)
            && record
                .runtime_pid
                .is_some_and(|pid| cutex::platform::process::process_liveness(pid) != Some(false)),
    )
}
pub(super) fn matching(record: &CutexSessionRecord) -> Option<Proof> {
    let proof = proofs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&record.cutex_session_id)
        .cloned();
    let proof = proof?;
    let session = record.claude_session.as_ref()?;
    if Occurrence::from_record(record).as_ref() != Some(&proof.occurrence)
        || session.native_session_id != proof.native_session
        || session.state_dir != proof.state_dir
        || session.deployment != proof.deployment
        || record.host_id != proof.host
    {
        return None;
    }
    Some(proof)
}
pub(super) fn current_for_probe(record: &CutexSessionRecord) -> anyhow::Result<Option<Proof>> {
    let Some(proof) = matching(record) else {
        return Ok(None);
    };
    let session = record.claude_session.as_ref().unwrap();
    if !eligible(record)? {
        return Ok(None);
    }
    if let (Some(before), Ok(now)) = (
        proof.birth,
        cutex::platform::process::process_started_at(proof.occurrence.pid),
    ) {
        if before != now {
            return Ok(None);
        }
    }
    if !manager()
        .status(&record.cutex_session_id)?
        .is_some_and(|status| {
            status.connected
                && status.runtime_generation == record.runtime_generation
                && status.thread_id == session.thread_id
        })
        || manager()
            .handle_for_generation(&record.cutex_session_id, record.runtime_generation)
            .is_err()
    {
        return Ok(None);
    }
    Ok(Some(proof))
}
pub(super) fn still_current(record: &CutexSessionRecord, expected: &Proof) -> anyhow::Result<bool> {
    Ok(current_for_probe(record)?.as_ref() == Some(expected))
}
pub(super) fn current(record: &CutexSessionRecord) -> anyhow::Result<Option<Proof>> {
    #[cfg(test)]
    DELIVERY_CHECKS.with(|count| count.set(count.get() + 1));
    Ok(current_for_probe(record)?.filter(|proof| !proof.delivery_blocked))
}
#[cfg(test)]
thread_local! {static DELIVERY_CHECKS:std::cell::Cell<u64>=const{std::cell::Cell::new(0)};}
#[cfg(test)]
pub(super) fn delivery_checks_on_thread() -> u64 {
    DELIVERY_CHECKS.with(std::cell::Cell::get)
}
pub(super) fn block_delivery(id: &str, expected: &Proof, blocked: bool) -> bool {
    let mut saved = proofs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(proof) = saved.get_mut(id) else {
        return false;
    };
    if &*proof != expected {
        return false;
    }
    proof.delivery_blocked = blocked;
    true
}
pub(super) fn same_verified_process(record: &CutexSessionRecord, prepared: &Proof) -> bool {
    matching(record).is_some_and(|old| old.birth == prepared.birth)
}

#[cfg(test)]
mod occurrence_proof_tests {
    use super::*;
    #[test]
    fn occurrence_proof_binds_full_tuple_and_old_ticket_cannot_remove_reconnect() {
        let mut record = super::super::occurrence_tests::fixture_record(&std::env::temp_dir());
        record.cutex_session_id = format!("cutex.proof-{}", uuid::Uuid::new_v4());
        let old = prepare(&record).unwrap();
        remember(&record.cutex_session_id, old.clone());
        assert!(matching(&record).is_some());
        // A proof alone cannot grant delivery without its real manager stream.
        assert!(current(&record).unwrap().is_none());
        for field in 0..8 {
            let mut changed = record.clone();
            match field {
                0 => changed.runtime_generation += 1,
                1 => changed.runtime_pid = Some(std::process::id().wrapping_add(1)),
                2 => changed.current_runtime_agent_id = Some("claude.replaced".into()),
                3 => changed.claude_session.as_mut().unwrap().thread_id = "other".into(),
                4 => {
                    changed.claude_session.as_mut().unwrap().native_session_id =
                        Some("other".into())
                }
                5 => changed.claude_session.as_mut().unwrap().state_dir = "other".into(),
                6 => changed.claude_session.as_mut().unwrap().deployment.python = "other".into(),
                _ => changed.host_id = "other".into(),
            }
            assert!(
                matching(&changed).is_none(),
                "changed field {field} reused old proof"
            );
        }
        let replacement = prepare(&record).unwrap();
        remember(&record.cutex_session_id, replacement.clone());
        assert!(!forget_if(&record.cutex_session_id, &old));
        assert_eq!(snapshot(&record.cutex_session_id), Some(replacement));
        record.pending_launch_id = Some("private pending".into());
        assert!(!eligible(&record).unwrap());
        record.pending_launch_id = None;
        record.app_server_launch_claim_id = Some("private pending".into());
        assert!(!eligible(&record).unwrap());
        forget(&record.cutex_session_id);
    }
}
