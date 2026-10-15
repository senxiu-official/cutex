//! Foreground resume planning for durable session records.

use crate::agent_bus::model::AgentRegistrationClass;
use crate::runtime::lifecycle::cutex_session_host_is_local;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundResumePlan {
    pub codex_session_id: String,
    pub profile: String,
    pub groups: Vec<String>,
    pub agent_mode: bool,
    pub host_warning: Option<ForegroundResumeHostWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundResumeHostWarning {
    pub session_host: String,
    pub current_host: String,
}

pub fn foreground_resume_plan<F>(
    record: &CutexSessionRecord,
    global_default_profile: F,
    current_host: &str,
) -> anyhow::Result<ForegroundResumePlan>
where
    F: FnOnce() -> Option<String>,
{
    let codex_session_id = record
        .codex_session_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cutex session has no Codex session id"))?
        .to_string();
    let profile = record
        .profile
        .clone()
        .or_else(global_default_profile)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cutex session follows the global default, but no global default profile is set"
            )
        })?;
    let groups = if record.agent_groups.is_empty() {
        Vec::new()
    } else {
        record.agent_groups.clone()
    };
    let agent_mode = foreground_resume_requires_agent_runtime(record, &groups);
    let host_warning = foreground_resume_host_warning(record, current_host);
    Ok(ForegroundResumePlan {
        codex_session_id,
        profile,
        groups,
        agent_mode,
        host_warning,
    })
}

pub fn foreground_resume_requires_agent_runtime(
    record: &CutexSessionRecord,
    groups: &[String],
) -> bool {
    record.runtime_backend == CutexSessionRuntimeBackend::HostForeground
        || record.agent_enabled
        || record.exposed_to_backend
        || record.registration_class == AgentRegistrationClass::Persistent
        || !groups.is_empty()
}

pub fn foreground_resume_host_warning(
    record: &CutexSessionRecord,
    current_host: &str,
) -> Option<ForegroundResumeHostWarning> {
    (!cutex_session_host_is_local(&record.host_id, current_host)).then(|| {
        ForegroundResumeHostWarning {
            session_host: record.host_id.clone(),
            current_host: current_host.to_string(),
        }
    })
}
