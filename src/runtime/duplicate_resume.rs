//! Duplicate resume detection and takeover planning.

use serde::Serialize;

use crate::platform::process::process_is_running;
use crate::runtime::alden::{cute_alden_sessions, CuteAldenSession};
use crate::session::model::CutexSessionStore;
use crate::session::projection::{
    duplicate_resume_runtime_for_session_id_in_store, DuplicateResumeRuntime,
};
use crate::session::store::load_cutex_session_store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTakeoverTarget {
    pub session_name: String,
    pub pid: u32,
    pub source: SessionTakeoverTargetSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTakeoverTargetSource {
    ManagedRuntime,
    AldenSessionName,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DuplicateResumeCheckResponse {
    pub duplicate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cutex_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alden_session_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alden_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub takeover_command: Option<Vec<String>>,
}

pub fn duplicate_resume_warning_plan(
    codex_args: &[String],
) -> anyhow::Result<Option<DuplicateResumeRuntime>> {
    let Some(session_id) = codex_resume_session_id_from_args(codex_args) else {
        return Ok(None);
    };
    duplicate_resume_runtime_for_session_id(session_id)
}

pub fn codex_resume_session_id_from_args(args: &[String]) -> Option<&str> {
    args.windows(2)
        .filter_map(|pair| {
            let command = pair[0].as_str();
            let session_id = pair[1].as_str();
            (command == "resume" && !session_id.starts_with('-')).then_some(session_id)
        })
        .last()
}

pub fn duplicate_resume_runtime_for_session_id(
    session_id: &str,
) -> anyhow::Result<Option<DuplicateResumeRuntime>> {
    let store = load_cutex_session_store()?;
    let alden_sessions = cute_alden_sessions().unwrap_or_default();
    Ok(duplicate_resume_runtime_for_session_id_from_store(
        &store,
        session_id,
        &alden_sessions,
    ))
}

pub fn duplicate_resume_runtime_for_session_id_from_store(
    store: &CutexSessionStore,
    session_id: &str,
    alden_sessions: &[CuteAldenSession],
) -> Option<DuplicateResumeRuntime> {
    duplicate_resume_runtime_for_session_id_in_store(store, session_id, alden_sessions)
}

pub fn session_takeover_target(id: &str) -> anyhow::Result<Option<SessionTakeoverTarget>> {
    let store = load_cutex_session_store()?;
    let alden_sessions = cute_alden_sessions().unwrap_or_default();
    Ok(session_takeover_target_from_store_and_alden(
        &store,
        id,
        &alden_sessions,
    ))
}

pub fn session_takeover_target_from_store_and_alden(
    store: &CutexSessionStore,
    id: &str,
    alden_sessions: &[CuteAldenSession],
) -> Option<SessionTakeoverTarget> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }

    if let Some(runtime) =
        duplicate_resume_runtime_for_session_id_from_store(store, id, alden_sessions)
    {
        return Some(SessionTakeoverTarget {
            session_name: runtime.alden_session_name,
            pid: runtime.alden_pid,
            source: SessionTakeoverTargetSource::ManagedRuntime,
        });
    }

    alden_sessions
        .iter()
        .find(|session| session.name.as_deref() == Some(id) && process_is_running(session.pid))
        .map(|session| SessionTakeoverTarget {
            session_name: id.to_string(),
            pid: session.pid,
            source: SessionTakeoverTargetSource::AldenSessionName,
        })
}

pub fn duplicate_resume_check_response(id: &str) -> anyhow::Result<DuplicateResumeCheckResponse> {
    duplicate_resume_runtime_for_session_id(id)
        .map(|runtime| duplicate_resume_check_response_from_runtime(id, runtime))
}

pub fn duplicate_resume_check_response_from_runtime(
    _id: &str,
    runtime: Option<DuplicateResumeRuntime>,
) -> DuplicateResumeCheckResponse {
    let Some(runtime) = runtime else {
        return DuplicateResumeCheckResponse {
            duplicate: false,
            reason: None,
            display_name: None,
            cutex_session_id: None,
            codex_session_id: None,
            alden_session_name: None,
            alden_pid: None,
            cwd: None,
            attach_command: None,
            takeover_command: None,
        };
    };
    let attach_name = runtime.alden_session_name.clone();
    let takeover_name = runtime.alden_session_name.clone();
    DuplicateResumeCheckResponse {
        duplicate: true,
        reason: Some("live_cute_alden_runtime".to_string()),
        display_name: Some(runtime.display_name),
        cutex_session_id: Some(runtime.cutex_session_id),
        codex_session_id: Some(runtime.codex_session_id),
        alden_session_name: Some(runtime.alden_session_name),
        alden_pid: Some(runtime.alden_pid),
        cwd: Some(runtime.cwd),
        attach_command: Some(vec![
            "cutex".to_string(),
            "session".to_string(),
            "attach".to_string(),
            "--name".to_string(),
            attach_name,
        ]),
        takeover_command: Some(vec![
            "cutex".to_string(),
            "session".to_string(),
            "attach".to_string(),
            "--name".to_string(),
            takeover_name,
            "--takeover".to_string(),
        ]),
    }
}
