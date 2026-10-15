//! Duplicate resume-runtime projection for durable sessions.

use crate::platform::process::process_is_running;
use crate::runtime::alden::CuteAldenSession;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::model::CutexSessionStore;
use crate::session::service::cutex_session_display_name;
use crate::session::service::cutex_session_key_for_user_id;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateResumeRuntime {
    pub display_name: String,
    pub cutex_session_id: String,
    pub codex_session_id: String,
    pub alden_session_name: String,
    pub alden_pid: u32,
    pub cwd: String,
}

pub fn duplicate_resume_runtime_for_session_id_in_store(
    store: &CutexSessionStore,
    session_id: &str,
    alden_sessions: &[CuteAldenSession],
) -> Option<DuplicateResumeRuntime> {
    let Some(key) = cutex_session_key_for_user_id(store, session_id) else {
        return None;
    };
    let Some(record) = store.sessions.get(&key) else {
        return None;
    };
    let Some(codex_session_id) = record.codex_session_id.as_deref() else {
        return None;
    };
    let Some(alden_session_name) = record.alden_session_name.as_deref() else {
        return None;
    };
    if record.runtime_backend != CutexSessionRuntimeBackend::CuteAlden {
        return None;
    }
    let listed_alden_session = alden_sessions
        .iter()
        .find(|session| session.name.as_deref() == Some(alden_session_name));
    let listed_pid = listed_alden_session.map(|session| session.pid);
    let fallback_pid = record.alden_pid;
    let pid_live =
        fallback_pid.is_some_and(process_is_running) || listed_pid.is_some_and(process_is_running);
    if !pid_live {
        return None;
    }
    let alden_pid = listed_pid.or(fallback_pid).unwrap_or_default();

    Some(DuplicateResumeRuntime {
        display_name: cutex_session_display_name(record),
        cutex_session_id: record.cutex_session_id.clone(),
        codex_session_id: codex_session_id.to_string(),
        alden_session_name: alden_session_name.to_string(),
        alden_pid,
        cwd: record.cwd.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::model::CutexSessionRecord;

    #[test]
    fn duplicate_resume_runtime_detects_live_cute_alden_record() {
        let mut record = CutexSessionRecord::new(
            "cutex.019e-target".to_string(),
            Some("019e-target".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
        )
        .expect("record should be created");
        record.thread_name = Some("observer-smoke".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.alden_session_name = Some("cutex.orchid.host.cutex.019e-target".to_string());
        record.alden_pid = Some(std::process::id());

        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: Some("cutex.orchid.host.cutex.019e-target".to_string()),
        }];

        let duplicate = duplicate_resume_runtime_for_session_id_in_store(
            &store,
            "019e-target",
            &alden_sessions,
        )
        .expect("duplicate runtime should be detected");

        assert_eq!(duplicate.display_name, "observer-smoke");
        assert_eq!(duplicate.codex_session_id, "019e-target");
        assert_eq!(
            duplicate.alden_session_name,
            "cutex.orchid.host.cutex.019e-target"
        );
    }
}
