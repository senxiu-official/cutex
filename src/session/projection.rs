//! Read-model helpers for listing and quickly starting durable cutex sessions.

use anyhow::Context;

use crate::session::model::CutexSessionRecord;
use crate::session::service::cutex_session_launch_cwd;

pub use super::duplicate_resume_projection::duplicate_resume_runtime_for_session_id_in_store;
pub use super::duplicate_resume_projection::DuplicateResumeRuntime;
pub use super::list_projection::cutex_session_choice_row;
pub use super::list_projection::cutex_session_choice_row_with_agents;
pub use super::list_projection::cutex_session_choice_rows;
pub use super::list_projection::cutex_session_choice_rows_with_agents;
pub use super::list_projection::cutex_session_filter_note;
pub use super::list_projection::cutex_session_list_row;
pub use super::list_projection::cutex_session_list_row_with_agents;
pub use super::list_projection::filtered_cutex_session_records;
pub use super::list_projection::filtered_cutex_session_records_with_agents;
pub use super::list_projection::CutexSessionChoiceRow;
pub use super::list_projection::CutexSessionFilterNote;
pub use super::list_projection::CutexSessionListFilter;
pub use super::list_projection::CutexSessionListRow;
pub use super::list_projection::CutexSessionListSort;
pub use super::start_quick_actions::primary_start_action_kind_for_record;
pub use super::start_quick_actions::recommended_start_quick_actions;
pub use super::start_quick_actions::recommended_start_quick_actions_with_agents;
pub use super::start_quick_actions::StartQuickAction;
pub use super::start_quick_actions::StartQuickActionKind;
pub use super::status_projection::cutex_session_has_live_managed_core;
pub use super::status_projection::cutex_session_has_live_native_agent;
pub use super::status_projection::cutex_session_is_attachable;
pub use super::status_projection::cutex_session_lifecycle_state_with_agents;
pub use super::status_projection::cutex_session_scope_label;
pub use super::status_projection::cutex_session_status_label;
pub use super::status_projection::cutex_session_status_label_with_agents;
pub use super::status_projection::runtime_backend_short_label;
pub use super::status_projection::CutexSessionLifecycleState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionCwdSummary {
    pub session_cwd: String,
    pub current_cwd: String,
    pub managed_cwd: Option<String>,
    pub effective_launch_cwd: String,
}

pub fn cutex_session_cwd_summary(
    record: &CutexSessionRecord,
) -> anyhow::Result<CutexSessionCwdSummary> {
    let current_cwd = std::env::current_dir()
        .context("Failed to determine current directory")?
        .display()
        .to_string();
    Ok(CutexSessionCwdSummary {
        session_cwd: record.cwd.clone(),
        current_cwd,
        managed_cwd: record.managed_cwd.clone(),
        effective_launch_cwd: cutex_session_launch_cwd(record).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cwd_summary_uses_managed_cwd_when_set() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.cwd".to_string(),
            Some("019e-cwd".to_string()),
            "devbox".to_string(),
            "/tmp/original".to_string(),
            Some("orchid".to_string()),
            "2026-06-28T00:00:00Z".to_string(),
        )
        .expect("record");
        record.managed_cwd = Some("/tmp/managed".to_string());

        let summary = cutex_session_cwd_summary(&record).expect("summary");
        assert_eq!(summary.session_cwd, "/tmp/original");
        assert_eq!(summary.managed_cwd.as_deref(), Some("/tmp/managed"));
        assert_eq!(summary.effective_launch_cwd, "/tmp/managed");
        assert!(!summary.current_cwd.is_empty());
    }
}
