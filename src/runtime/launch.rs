//! Compatibility facade for runtime launch planning helpers.

#[cfg(test)]
use crate::profiles::model::StoredAccount;
pub use crate::runtime::duplicate_resume::{
    codex_resume_session_id_from_args, duplicate_resume_check_response,
    duplicate_resume_check_response_from_runtime, duplicate_resume_runtime_for_session_id,
    duplicate_resume_runtime_for_session_id_from_store, duplicate_resume_warning_plan,
    session_takeover_target, session_takeover_target_from_store_and_alden,
    DuplicateResumeCheckResponse, SessionTakeoverTarget, SessionTakeoverTargetSource,
};
pub use crate::runtime::foreground_resume::{
    foreground_resume_host_warning, foreground_resume_plan, ForegroundResumeHostWarning,
    ForegroundResumePlan,
};
pub use crate::runtime::managed_launch::{
    default_managed_session_name, default_managed_session_name_for_cwd,
    maybe_wrap_launch_with_session, should_wrap_launch_with_session, ManagedLaunchSessionPlan,
};
#[cfg(test)]
use crate::session::model::CutexSessionStore;
#[cfg(test)]
use crate::session::projection::DuplicateResumeRuntime;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::Path;

    use crate::profiles::model::{CliKind, RuntimeConfig};
    use crate::runtime::alden::CuteAldenSession;
    use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

    fn sample_account(name: &str) -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            claude_code: Default::default(),
            id: format!("account-{name}"),
            name: name.to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: None,
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        }
    }

    #[test]
    fn managed_session_name_uses_profile_runtime_project_and_hash() {
        let account = sample_account("demo session");
        let cwd = Path::new("/home/example/Projects/cutex");

        let name = default_managed_session_name_for_cwd(&account, cwd);

        assert!(name.starts_with("cutex.demo-session.host.cutex."));
        assert_eq!(name.len(), "cutex.demo-session.host.cutex.".len() + 10);
    }

    #[test]
    fn codex_resume_session_id_parser_finds_explicit_target() {
        let args = vec![
            "--model".to_string(),
            "gpt-5.5".to_string(),
            "resume".to_string(),
            "019e-target".to_string(),
        ];

        assert_eq!(
            codex_resume_session_id_from_args(&args),
            Some("019e-target")
        );
        assert_eq!(
            codex_resume_session_id_from_args(&["resume".to_string()]),
            None
        );
        assert_eq!(
            codex_resume_session_id_from_args(&["resume".to_string(), "--last".to_string()]),
            None
        );
    }

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

        let duplicate = duplicate_resume_runtime_for_session_id_from_store(
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

    #[test]
    fn session_takeover_target_prefers_managed_runtime() {
        let mut record = CutexSessionRecord::new(
            "cutex.019e-target".to_string(),
            Some("019e-target".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
        )
        .expect("record should be created");
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

        let target =
            session_takeover_target_from_store_and_alden(&store, "019e-target", &alden_sessions)
                .expect("takeover target should be detected");

        assert_eq!(target.session_name, "cutex.orchid.host.cutex.019e-target");
        assert_eq!(target.pid, std::process::id());
        assert_eq!(target.source, SessionTakeoverTargetSource::ManagedRuntime);
    }

    #[test]
    fn session_takeover_target_accepts_direct_alden_name() {
        let store = CutexSessionStore::default();
        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: Some("direct-alden".to_string()),
        }];

        let target =
            session_takeover_target_from_store_and_alden(&store, "direct-alden", &alden_sessions)
                .expect("direct cute-alden name should resolve");

        assert_eq!(target.session_name, "direct-alden");
        assert_eq!(target.pid, std::process::id());
        assert_eq!(target.source, SessionTakeoverTargetSource::AldenSessionName);
    }

    #[test]
    fn duplicate_resume_check_response_preserves_cli_contract() {
        let runtime = DuplicateResumeRuntime {
            display_name: "observer-smoke".to_string(),
            cutex_session_id: "cutex.019e-target".to_string(),
            codex_session_id: "019e-target".to_string(),
            alden_session_name: "cutex.orchid.host.cutex.019e-target".to_string(),
            alden_pid: 42,
            cwd: "/home/example/Projects/cutex".to_string(),
        };

        let response = duplicate_resume_check_response_from_runtime("019e-target", Some(runtime));

        assert!(response.duplicate);
        assert_eq!(response.reason.as_deref(), Some("live_cute_alden_runtime"));
        assert_eq!(
            response.attach_command.as_ref().expect("attach command"),
            &vec![
                "cutex".to_string(),
                "session".to_string(),
                "attach".to_string(),
                "--name".to_string(),
                "cutex.orchid.host.cutex.019e-target".to_string(),
            ]
        );
        assert_eq!(
            response
                .takeover_command
                .as_ref()
                .expect("takeover command"),
            &vec![
                "cutex".to_string(),
                "session".to_string(),
                "attach".to_string(),
                "--name".to_string(),
                "cutex.orchid.host.cutex.019e-target".to_string(),
                "--takeover".to_string(),
            ]
        );
    }

    #[test]
    fn foreground_resume_plan_uses_record_profile_groups_and_agent_mode() {
        let mut record = CutexSessionRecord::new(
            "cutex.019e-target".to_string(),
            Some("019e-target".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("record-profile".to_string()),
        )
        .expect("record should be created");
        record.agent_groups = vec!["cutex-f7".to_string()];

        let fallback_called = Cell::new(false);
        let plan = foreground_resume_plan(
            &record,
            || {
                fallback_called.set(true);
                Some("active-profile".to_string())
            },
            "devbox",
        )
        .expect("foreground resume plan should be built");

        assert_eq!(plan.codex_session_id, "019e-target");
        assert_eq!(plan.profile, "record-profile");
        assert_eq!(plan.groups, vec!["cutex-f7".to_string()]);
        assert!(plan.agent_mode);
        assert_eq!(plan.host_warning, None);
        assert!(
            !fallback_called.get(),
            "active profile fallback must stay lazy when record profile exists"
        );
    }

    #[test]
    fn foreground_resume_plan_treats_host_foreground_as_agent_runtime() {
        let mut record = CutexSessionRecord::new(
            "cutex.019e-target".to_string(),
            Some("019e-target".to_string()),
            "winbox".to_string(),
            "E:\\Projects (Orchid)\\waveline-backend".to_string(),
            Some("orchid".to_string()),
        )
        .expect("record should be created");
        record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;

        let plan = foreground_resume_plan(&record, || None, "winbox")
            .expect("foreground resume plan should be built");

        assert!(plan.groups.is_empty());
        assert!(
            plan.agent_mode,
            "managed host_foreground sessions must register on the agent bus"
        );
    }

    #[test]
    fn foreground_resume_plan_falls_back_to_global_default_and_warns_on_remote_host() {
        let record = CutexSessionRecord::new(
            "cutex.019e-target".to_string(),
            Some("019e-target".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            None,
        )
        .expect("record should be created");

        let plan = foreground_resume_plan(&record, || Some("global-profile".to_string()), "winbox")
            .expect("foreground resume plan should be built");

        assert_eq!(plan.profile, "global-profile");
        assert!(!plan.agent_mode);
        assert_eq!(
            plan.host_warning,
            Some(ForegroundResumeHostWarning {
                session_host: "devbox".to_string(),
                current_host: "winbox".to_string(),
            })
        );
    }

    #[test]
    fn foreground_resume_plan_requires_codex_session_id_and_profile() {
        let no_session_id = CutexSessionRecord::new(
            "cutex.no-session".to_string(),
            None,
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("record-profile".to_string()),
        )
        .expect("record should be created");
        assert_eq!(
            foreground_resume_plan(&no_session_id, || None, "devbox")
                .expect_err("missing Codex session id should fail")
                .to_string(),
            "cutex session has no Codex session id"
        );

        let no_profile = CutexSessionRecord::new(
            "cutex.019e-target".to_string(),
            Some("019e-target".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            None,
        )
        .expect("record should be created");
        assert_eq!(
            foreground_resume_plan(&no_profile, || None, "devbox")
                .expect_err("missing profile should fail")
                .to_string(),
            "cutex session follows the global default, but no global default profile is set"
        );
    }
}
