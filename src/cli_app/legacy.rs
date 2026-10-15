#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::process::Command;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
use super::account_store::load_store;
#[cfg(test)]
use super::launch_command::{codex_launch_command, codex_launch_command_with_agent_mode};
#[cfg(test)]
use super::launch_output::LaunchOutput;
#[cfg(test)]
use super::launch_session::maybe_wrap_launch_with_session;
#[cfg(test)]
use super::profile::{cmd_profile_clone_status_line, cmd_profile_copy, cmd_profile_pin};

#[cfg(test)]
use cutex::agent_bus::delivery::*;
#[cfg(test)]
use cutex::agent_bus::groups::*;
#[cfg(test)]
use cutex::agent_bus::model::*;
#[cfg(test)]
use cutex::agent_bus::queue::*;
#[cfg(test)]
use cutex::agent_bus::routing::*;
#[cfg(test)]
use cutex::agent_bus::store::*;
#[cfg(test)]
use cutex::agent_management::AgentOperationKind;
#[cfg(test)]
use cutex::cli::args::*;
#[cfg(test)]
use cutex::config::atomic::*;
#[cfg(test)]
use cutex::config::env::*;
#[cfg(test)]
use cutex::config::paths::*;
#[cfg(test)]
use cutex::config::proxy::*;
#[cfg(test)]
use cutex::config::store::*;
#[cfg(test)]
use cutex::im::registry::*;
#[cfg(test)]
use cutex::launch::args::*;
#[cfg(test)]
use cutex::launch::command::LaunchCommand;
#[cfg(test)]
use cutex::launch::docker::*;
#[cfg(test)]
use cutex::platform::host::current_host_name;
#[cfg(test)]
use cutex::platform::now_epoch_secs;
#[cfg(test)]
use cutex::profiles::inspect::*;
#[cfg(test)]
use cutex::profiles::materialize::*;
#[cfg(test)]
use cutex::profiles::model::*;
#[cfg(test)]
use cutex::profiles::profile_config::*;
#[cfg(test)]
use cutex::profiles::references::*;
#[cfg(test)]
use cutex::profiles::store::save_store;
#[cfg(test)]
use cutex::runtime::alden::*;
#[cfg(test)]
use cutex::runtime::lifecycle::*;
#[cfg(test)]
use cutex::session::model::*;
#[cfg(test)]
use cutex::session::projection::*;
#[cfg(test)]
use cutex::session::service::*;
#[cfg(test)]
use cutex::session::store::*;

#[cfg(test)]
fn apply_annotation(
    account: &mut StoredAccount,
    source: Option<String>,
    clear_source: bool,
    plan: Option<String>,
    clear_plan: bool,
    email: Option<String>,
    clear_email: bool,
) {
    if clear_source {
        account.source = None;
    } else if let Some(source) = source {
        account.source = Some(source);
    }

    if clear_plan {
        account.plan_type = None;
    } else if let Some(plan) = plan {
        account.plan_type = Some(plan);
    }

    if clear_email {
        account.email = None;
    } else if let Some(email) = email {
        account.email = Some(email);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_app::management_lifecycle;
    use crate::cli_app::profile_settings::{ProfileApiKeyUpdate, ProfileSettingsPatch};
    use crate::cli_app::prompt::{normalize_prompt_input, parse_cli_args_value};
    use anyhow::Context;
    use clap::Parser;
    use cutex::config::global_settings::ConfigValueUpdate;
    use cutex::profiles::codex_profile::CodexProfileConfigPatch;
    use cutex::runtime::launch::codex_resume_session_id_from_args;
    use cutex::session::identity::{normalize_codex_session_id, normalize_cutex_session_id};
    use std::collections::HashSet;
    use std::sync::Mutex;

    mod agent_bus_tests;
    mod cli_command_parse_tests;
    mod management_cli_tests;
    mod session_cli_tests;
    mod session_reconcile_tests;
    mod session_start_menu_tests;

    fn env_lock() -> &'static crate::cli_app::test_home::TestEnvironmentLock {
        crate::cli_app::test_home::environment_lock()
    }

    fn path_ends_with(value: &str, suffix: &str) -> bool {
        value.replace('\\', "/").ends_with(suffix)
    }

    fn last_launch_env<'a>(launch: &'a LaunchCommand, key: &str) -> Option<&'a str> {
        launch
            .envs
            .iter()
            .rev()
            .find_map(|(candidate, value)| (candidate == key).then_some(value.as_str()))
    }

    fn restore_env_var(key: &str, old_value: Option<std::ffi::OsString>) {
        unsafe {
            match old_value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    fn sample_account(name: &str) -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: format!("{name}-id"),
            name: name.to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: Some("official".to_string()),
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
    fn host_foreground_runtime_backend_parses_and_serializes() {
        assert_eq!(
            parse_cutex_session_runtime_backend("host_foreground").expect("parse host_foreground"),
            CutexSessionRuntimeBackend::HostForeground
        );
        assert_eq!(
            parse_cutex_session_runtime_backend("native").expect("parse native"),
            CutexSessionRuntimeBackend::HostForeground
        );
        assert_eq!(
            serde_json::to_string(&CutexSessionRuntimeBackend::HostForeground)
                .expect("serialize backend"),
            "\"host_foreground\""
        );
    }

    #[test]
    fn managed_session_default_backend_matches_platform() {
        let expected = if cfg!(windows) {
            CutexSessionRuntimeBackend::HostForeground
        } else {
            CutexSessionRuntimeBackend::CuteAlden
        };
        assert_eq!(default_managed_session_runtime_backend(), expected);
    }

    fn sample_bus_agent(
        id: &str,
        name: &str,
        base_name: Option<&str>,
        path_key: Option<&str>,
    ) -> AgentBusAgent {
        AgentBusAgent {
            id: id.to_string(),
            name: name.to_string(),
            base_name: base_name.map(str::to_string),
            thread_name: base_name.map(str::to_string),
            path_key: path_key.map(str::to_string),
            session_id: None,
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: "/tmp/project".to_string(),
            pid: std::process::id(),
            host_id: None,
            groups: vec!["project:test".to_string()],
            registration_class: AgentRegistrationClass::LocalOnly,
            last_seen_epoch_secs: now_epoch_secs(),
        }
    }

    fn sample_im_registration(session_id: &str) -> CodingSessionRegistration {
        CodingSessionRegistration {
            session_id: session_id.to_string(),
            display_name: "aria-data".to_string(),
            host_id: "devbox".to_string(),
            cwd: "/home/example/Projects/scgpt".to_string(),
            profile: Some("orchid".to_string()),
            groups: vec!["aria".to_string(), "project:scgpt".to_string()],
            registration_class: AgentRegistrationClass::Persistent,
            visible: true,
            created_at: "2026-06-24T00:00:00Z".to_string(),
            updated_at: "2026-06-24T00:00:00Z".to_string(),
            last_runtime_agent_id: None,
        }
    }

    #[test]
    fn session_stop_target_collects_all_local_session_endpoint_pids() {
        let local_host = current_host_name();
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            local_host.clone(),
            "/tmp/project".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        record.runtime_pid = Some(1111);
        record.current_runtime_agent_id = Some("agent-current".to_string());

        let mut old_agent =
            sample_bus_agent("agent-old", "worker.old", Some("worker"), Some("old"));
        old_agent.session_id = Some("019e-alpha".to_string());
        old_agent.host_id = Some(local_host.clone());
        old_agent.pid = 2222;

        let mut new_agent =
            sample_bus_agent("agent-new", "worker.new", Some("worker"), Some("new"));
        new_agent.session_id = Some("019e-alpha".to_string());
        new_agent.host_id = Some(local_host.clone());
        new_agent.pid = 3333;

        let mut other_agent =
            sample_bus_agent("agent-other", "other.new", Some("other"), Some("new"));
        other_agent.session_id = Some("019e-other".to_string());
        other_agent.host_id = Some(local_host.clone());
        other_agent.pid = 4444;

        let target = session_runtime_stop_target(
            &record,
            &[old_agent, new_agent, other_agent],
            None,
            &local_host,
        );

        assert!(target.had_runtime);
        assert_eq!(target.pids, vec![1111, 2222, 3333]);
    }

    #[test]
    fn cutex_session_identifier_normalization_rejects_empty_or_path_like_ids() {
        assert_eq!(
            normalize_cutex_session_id("  cutex.abc  ").expect("id should normalize"),
            "cutex.abc"
        );
        assert!(normalize_cutex_session_id(" ").is_err());
        assert!(normalize_cutex_session_id("cutex/bad").is_err());
        assert!(normalize_codex_session_id("..\\bad").is_err());
    }

    #[test]
    fn cutex_session_record_defaults_from_codex_session_id() {
        let record = CutexSessionRecord::from_codex_session_id("019e-session")
            .expect("session record should be created");

        assert_eq!(record.cutex_session_id, "cutex.019e-session");
        assert_eq!(record.codex_session_id.as_deref(), Some("019e-session"));
        assert!(record.managed_cwd.is_none());
        assert_eq!(cutex_session_launch_cwd(&record), record.cwd);
        assert_eq!(record.runtime_backend, CutexSessionRuntimeBackend::Host);
        assert_eq!(record.registration_class, AgentRegistrationClass::LocalOnly);
        assert!(!record.agent_enabled);
        assert!(!record.exposed_to_backend);
        assert_eq!(record.quick_action, CutexSessionQuickActionMode::Auto);
        assert_eq!(record.runtime_generation, 0);
        assert!(record.current_runtime_agent_id.is_none());
        assert!(record.last_user_selected_at.is_none());
        assert!(record.last_user_action.is_none());
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

    #[test]
    fn cutex_session_store_missing_file_defaults() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let store = load_cutex_session_store().expect("missing store should load as default");
        assert!(store.sessions.is_empty());

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn cutex_session_store_round_trips_full_record() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut record = CutexSessionRecord::new(
            "cutex.019e-session".to_string(),
            Some("019e-session".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
        )
        .expect("record should be created");
        record.thread_name = Some("cutex-dev".to_string());
        record.display_name_hint = Some("Cutex Dev".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.agent_enabled = true;
        record.agent_groups = vec!["project:cutex".to_string(), "waveline".to_string()];
        record.registration_class = AgentRegistrationClass::Persistent;
        record.exposed_to_backend = true;
        record.default_cli_args = vec!["--sandbox".to_string(), "danger-full-access".to_string()];
        record.permission_defaults = Some("full-access".to_string());
        record.approval_policy = Some("never".to_string());
        record.sandbox_mode = Some("danger-full-access".to_string());
        record.model_defaults = Some("gpt-5.5".to_string());
        record.reasoning_defaults = Some("xhigh".to_string());
        record.alden_session_name = Some("cutex.dev.019e".to_string());
        record.alden_pid = Some(4242);
        record.current_runtime_agent_id = Some("cutex.orchid.cutex.abcdef".to_string());
        record.runtime_generation = 7;
        record.last_runtime_agent_id = Some("cutex.orchid.cutex.old".to_string());
        record.last_seen_at = Some("2026-06-25T00:00:00Z".to_string());
        record.quick_action = CutexSessionQuickActionMode::Pinned;
        record.last_user_selected_at = Some("2026-06-25T00:01:00Z".to_string());
        record.last_user_action = Some(CutexSessionUserAction::Takeover);

        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        save_cutex_session_store(&store).expect("store should save");

        let loaded = load_cutex_session_store().expect("store should load");
        assert_eq!(loaded.sessions.get("cutex.019e-session"), Some(&record));

        let raw = String::from_utf8(
            cutex::session::store::session_store_json_bytes(
                &cutex_sessions_path().expect("path should resolve"),
            )
            .expect("store should be readable"),
        )
        .expect("store JSON is UTF-8");
        // Session store Phase 0 writes compact JSON.
        assert!(raw.contains("\"runtime_backend\":\"cute_alden\""));
        assert!(raw.contains("\"current_runtime_agent_id\""));
        assert!(raw.contains("\"approval_policy\""));
        assert!(raw.contains("\"sandbox_mode\""));
        assert!(raw.contains("\"quick_action\":\"pinned\""));
        assert!(raw.contains("\"last_user_action\":\"takeover\""));

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn cutex_session_key_resolves_unique_thread_or_display_name() {
        let mut store = CutexSessionStore::default();
        let mut first = CutexSessionRecord::new(
            "cutex.019e-first".to_string(),
            Some("019e-first".to_string()),
            "devbox".to_string(),
            "/tmp/first".to_string(),
            Some("orchid".to_string()),
        )
        .expect("first record should be valid");
        first.thread_name = Some("aria-data".to_string());
        first.display_name_hint = Some("orchid-1".to_string());

        let mut second = CutexSessionRecord::new(
            "cutex.019e-second".to_string(),
            Some("019e-second".to_string()),
            "devbox".to_string(),
            "/tmp/second".to_string(),
            Some("orchid".to_string()),
        )
        .expect("second record should be valid");
        second.thread_name = Some("aria-eval".to_string());

        store.sessions.insert(first.cutex_session_id.clone(), first);
        store
            .sessions
            .insert(second.cutex_session_id.clone(), second);

        assert_eq!(
            cutex_session_key_for_user_id(&store, "orchid-1").as_deref(),
            Some("cutex.019e-first")
        );
        assert_eq!(
            cutex_session_key_for_user_id(&store, "aria-eval").as_deref(),
            Some("cutex.019e-second")
        );
        assert_eq!(
            cutex_session_key_for_user_id(&store, "019e-first").as_deref(),
            Some("cutex.019e-first")
        );
    }

    #[test]
    fn cutex_session_key_does_not_guess_duplicate_display_name() {
        let mut store = CutexSessionStore::default();
        for suffix in ["one", "two"] {
            let mut record = CutexSessionRecord::new(
                format!("cutex.019e-{suffix}"),
                Some(format!("019e-{suffix}")),
                "devbox".to_string(),
                format!("/tmp/{suffix}"),
                Some("orchid".to_string()),
            )
            .expect("record should be valid");
            record.display_name_hint = Some("duplicate".to_string());
            store
                .sessions
                .insert(record.cutex_session_id.clone(), record);
        }

        assert!(cutex_session_key_for_user_id(&store, "duplicate").is_none());
    }

    #[test]
    fn atomic_pretty_json_write_replaces_longer_existing_file() {
        let temp_dir = std::env::temp_dir().join(format!("cutex-atomic-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let path = temp_dir.join("state.json");
        let long_value = serde_json::json!({
            "kind": "long",
            "marker": "old-tail-marker",
            "items": vec!["old-tail-marker"; 256],
        });
        write_pretty_json_atomic(&path, &long_value, "test json").expect("long JSON should write");
        let long_len = fs::metadata(&path).expect("metadata should load").len();

        let short_value = serde_json::json!({ "kind": "short" });
        write_pretty_json_atomic(&path, &short_value, "test json")
            .expect("short JSON should replace long JSON");

        let raw = fs::read_to_string(&path).expect("state file should be readable");
        assert!(
            raw.len() < usize::try_from(long_len).expect("long len should fit"),
            "short rewrite should shrink the file"
        );
        assert!(!raw.contains("old-tail-marker"));
        let parsed: Value =
            serde_json::from_str(&raw).expect("rewritten file should be valid JSON");
        assert_eq!(parsed, short_value);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn atomic_pretty_json_write_concurrent_writers_leave_complete_json() {
        let temp_dir = std::env::temp_dir().join(format!("cutex-atomic-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let path = temp_dir.join("state.json");

        let mut handles = Vec::new();
        for writer in 0..8 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for iteration in 0..50 {
                    let value = serde_json::json!({
                        "writer": writer,
                        "iteration": iteration,
                        "padding": vec![format!("writer-{writer}-iteration-{iteration}"); 32 + writer],
                    });
                    write_pretty_json_atomic(&path, &value, "test json")
                        .expect("concurrent JSON write should succeed");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("writer thread should finish");
        }

        let raw = fs::read_to_string(&path).expect("state file should be readable");
        let parsed: Value = serde_json::from_str(&raw).expect("final file should be valid JSON");
        assert!(parsed.get("writer").and_then(Value::as_u64).is_some());
        assert!(parsed.get("iteration").and_then(Value::as_u64).is_some());
        let temp_leftovers = fs::read_dir(&temp_dir)
            .expect("temp dir should list")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".state.json.tmp-")
            })
            .count();
        assert_eq!(temp_leftovers, 0);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn cutex_session_store_save_replaces_longer_prior_contents() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut long_store = CutexSessionStore::default();
        for index in 0..32 {
            let session_id = format!("019e-long-{index}");
            let mut record = CutexSessionRecord::new(
                format!("cutex.{session_id}"),
                Some(session_id),
                "devbox".to_string(),
                "/home/example/Projects/cutex".to_string(),
                Some("orchid".to_string()),
            )
            .expect("record should be created");
            record.thread_name = Some(format!("old-tail-marker-{index}"));
            record.default_cli_args = vec!["old-tail-marker".to_string(); 16];
            long_store
                .sessions
                .insert(record.cutex_session_id.clone(), record);
        }
        save_cutex_session_store(&long_store).expect("long store should save");
        let long_len = cutex::session::store::session_store_json_bytes(
            &cutex_sessions_path().expect("path should resolve"),
        )
        .expect("store should be readable")
        .len() as u64;

        let mut short_store =
            load_cutex_session_store().expect("current store should load before replacement");
        short_store.sessions.clear();
        save_cutex_session_store(&short_store).expect("short store should replace long store");

        let raw = String::from_utf8(
            cutex::session::store::session_store_json_bytes(
                &cutex_sessions_path().expect("path should resolve"),
            )
            .expect("store should be readable"),
        )
        .expect("store JSON is UTF-8");
        assert!(
            raw.len() < usize::try_from(long_len).expect("long len should fit"),
            "short store rewrite should shrink the file"
        );
        assert!(!raw.contains("old-tail-marker"));
        let loaded = load_cutex_session_store().expect("short store should load");
        assert!(loaded.sessions.is_empty());

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_cwd_command_updates_and_clears_managed_cwd() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = CutexSessionStore::default();
        let record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "devbox".to_string(),
            "/tmp/session-original".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        store
            .sessions
            .insert("cutex.019e-alpha".to_string(), record);
        save_cutex_session_store(&store).expect("store should save");

        super::super::session::cmd_session_cwd(SessionCwdCommand::Set {
            id: "019e-alpha".to_string(),
            path: "~/managed".to_string(),
        })
        .expect("cwd set should work");
        let store = load_cutex_session_store().expect("store should reload");
        let record = store
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should exist");
        assert_eq!(record.cwd, "/tmp/session-original");
        let expected_managed = temp_home.join("managed").to_string_lossy().to_string();
        assert_eq!(
            record.managed_cwd.as_deref(),
            Some(expected_managed.as_str())
        );

        super::super::session::cmd_session_cwd(SessionCwdCommand::Clear {
            id: "019e-alpha".to_string(),
        })
        .expect("cwd clear should work");
        let store = load_cutex_session_store().expect("store should reload");
        let record = store
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should exist");
        assert!(record.managed_cwd.is_none());
        assert_eq!(cutex_session_launch_cwd(record), "/tmp/session-original");

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_adopt_command_marks_recent_session_managed_with_alden_defaults() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cwd = std::env::current_dir().expect("cwd should resolve");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        let workdir = temp_home.join("managed-cwd");
        fs::create_dir_all(&workdir).expect("managed cwd should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }
        std::env::set_current_dir(&workdir).expect("cwd should be changed");

        let mut store = CutexSessionStore::default();
        let record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "devbox".to_string(),
            "/tmp/session-original".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        store
            .sessions
            .insert("cutex.019e-alpha".to_string(), record);
        save_cutex_session_store(&store).expect("store should save");

        super::super::session::cmd_session_adopt(
            "019e-alpha",
            Some("test-agent".to_string()),
            None,
            true,
            vec!["waveline".to_string()],
            true,
            true,
        )
        .expect("session adopt should work");

        let store = load_cutex_session_store().expect("store should reload");
        let record = store
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should exist");
        assert_eq!(record.display_name_hint.as_deref(), Some("test-agent"));
        assert_eq!(
            record.runtime_backend,
            default_managed_session_runtime_backend()
        );
        assert_eq!(
            record.registration_class,
            AgentRegistrationClass::Persistent
        );
        assert!(record.agent_enabled);
        assert!(record.exposed_to_backend);
        assert_eq!(record.quick_action, CutexSessionQuickActionMode::Pinned);
        assert!(record.agent_groups.iter().any(|group| group == "waveline"));
        assert!(record
            .agent_groups
            .iter()
            .any(|group| group.starts_with("project:")));
        assert_eq!(
            record.managed_cwd.as_deref(),
            Some(workdir.to_string_lossy().as_ref())
        );

        super::super::session::cmd_session_unmanage("019e-alpha")
            .expect("session unmanage should work");
        let store = load_cutex_session_store().expect("store should reload after unmanage");
        let record = store
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should remain");
        assert_eq!(record.registration_class, AgentRegistrationClass::LocalOnly);
        assert!(!record.exposed_to_backend);
        assert!(!record.agent_enabled);
        assert!(record.managed_cwd.is_none());
        assert_eq!(record.quick_action, CutexSessionQuickActionMode::Auto);

        std::env::set_current_dir(old_cwd).expect("cwd should be restored");
        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_defaults_command_updates_runtime_defaults() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = CutexSessionStore::default();
        let record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "devbox".to_string(),
            "/tmp/session-original".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        store
            .sessions
            .insert("cutex.019e-alpha".to_string(), record);
        save_cutex_session_store(&store).expect("store should save");

        super::super::session::cmd_session_defaults(SessionDefaultsCommand::Set {
            id: "019e-alpha".to_string(),
            runtime_backend: Some("cute-alden".to_string()),
            permission_defaults: Some("full access".to_string()),
            approval_policy: None,
            sandbox_mode: None,
            model: Some("gpt-5.5".to_string()),
            reasoning: Some("xhigh".to_string()),
            cli_args: vec!["--no-alt-screen".to_string()],
            clear_cli_args: false,
        })
        .expect("defaults set should work");

        let store = load_cutex_session_store().expect("store should reload");
        let record = store
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should exist");
        assert_eq!(
            record.runtime_backend,
            CutexSessionRuntimeBackend::CuteAlden
        );
        assert_eq!(record.permission_defaults.as_deref(), Some("full-access"));
        assert_eq!(record.model_defaults.as_deref(), Some("gpt-5.5"));
        assert_eq!(record.reasoning_defaults.as_deref(), Some("xhigh"));
        assert_eq!(record.default_cli_args, vec!["--no-alt-screen"]);

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn runtime_default_cli_args_map_permission_model_and_reasoning() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/scgpt".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.permission_defaults = Some("full-access".to_string());
        record.model_defaults = Some("gpt-5.5".to_string());
        record.reasoning_defaults = Some("xhigh".to_string());

        let args = cutex_session_runtime_default_cli_args(&record);

        assert_eq!(
            args,
            vec![
                "--model",
                "gpt-5.5",
                "--sandbox",
                "danger-full-access",
                "--ask-for-approval",
                "never",
                "-c",
                "model_reasoning_effort=xhigh",
            ]
        );
    }

    #[test]
    fn host_foreground_uses_the_managed_app_server_lifecycle() {
        assert!(
            management_lifecycle::runtime_backend_uses_managed_app_server(
                CutexSessionRuntimeBackend::HostForeground
            )
        );
        assert!(
            management_lifecycle::runtime_backend_uses_managed_app_server(
                CutexSessionRuntimeBackend::Host
            )
        );
        assert!(
            management_lifecycle::runtime_backend_uses_managed_app_server(
                CutexSessionRuntimeBackend::CuteAlden
            )
        );
        assert!(
            !management_lifecycle::runtime_backend_uses_managed_app_server(
                CutexSessionRuntimeBackend::Docker
            )
        );
    }

    #[test]
    fn parse_cli_args_value_supports_shell_quoting() {
        let args =
            parse_cli_args_value("--sandbox danger-full-access --system-prompt 'hello world'")
                .expect("cli args should parse");
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "danger-full-access".to_string(),
                "--system-prompt".to_string(),
                "hello world".to_string()
            ]
        );
    }

    fn write_profile_files(
        account: &StoredAccount,
        auth_json: &str,
        config_toml: Option<&str>,
    ) -> anyhow::Result<()> {
        let files = materialized_account_files(account)?;
        if let Some(parent) = files.auth_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create account dir: {}", parent.display()))?;
        }
        fs::write(&files.auth_path, auth_json)
            .with_context(|| format!("Failed to write auth.json: {}", files.auth_path.display()))?;
        match config_toml {
            Some(config) => fs::write(&files.config_path, config).with_context(|| {
                format!(
                    "Failed to write config.toml: {}",
                    files.config_path.display()
                )
            })?,
            None => {
                if files.config_path.exists() {
                    fs::remove_file(&files.config_path).with_context(|| {
                        format!(
                            "Failed to remove config.toml: {}",
                            files.config_path.display()
                        )
                    })?;
                }
            }
        }
        Ok(())
    }

    #[test]
    fn extract_profile_config_keeps_only_profile_specific_keys() {
        let config = r#"
cli_auth_credentials_store = "file"
model_provider = "anthropic"
model_context_window = 1000000
model_auto_compact_token_limit = 400000
other_key = "keep-out"

[tui]
status_line = ["launch-profile", "model-with-reasoning", "current-dir"]
session_picker_provider_filter = "all"

[model_providers.anthropic]
base_url = "https://example.test"
env_key = "ANTHROPIC_API_KEY"

[model_providers.openai]
base_url = "https://api.openai.com"
"#;

        let extracted = extract_profile_config_toml(config)
            .expect("extract should succeed")
            .expect("profile config should exist");

        assert!(extracted.contains("cli_auth_credentials_store = \"file\""));
        assert!(extracted.contains("model_provider = \"anthropic\""));
        assert!(extracted.contains("model_context_window = 1000000"));
        assert!(extracted.contains("model_auto_compact_token_limit = 400000"));
        assert!(extracted.contains("[model_providers.anthropic]"));
        assert!(extracted.contains("base_url = \"https://example.test\""));
        assert!(!extracted.contains("other_key"));
        assert!(!extracted.contains("[model_providers.openai]"));

        let extracted_table = parse_toml_table(&extracted).expect("extracted config should parse");
        assert_eq!(
            extracted_table
                .get("tui")
                .and_then(|value| value.as_table())
                .and_then(|table| table.get("status_line"))
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                }),
            Some(vec![
                "launch-profile".to_string(),
                "model-with-reasoning".to_string(),
                "current-dir".to_string(),
            ])
        );
        assert_eq!(
            extracted_table
                .get("tui")
                .and_then(|value| value.as_table())
                .and_then(|table| table.get("session_picker_provider_filter"))
                .and_then(|value| value.as_str()),
            Some("all")
        );
    }

    #[test]
    fn merge_and_write_config_replaces_selected_provider_only() {
        let tempdir = std::env::temp_dir().join(format!("codez-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&tempdir).expect("tempdir should be created");
        let path = tempdir.join("config.toml");

        let existing = r#"
foo = "bar"
model_context_window = 222222
model_auto_compact_token_limit = 111111
model_provider = "openai"

[tui]
status_line = ["model-name"]
session_picker_provider_filter = "current"

[model_providers.openai]
base_url = "https://old.example"

[model_providers.other]
base_url = "https://other.example"
"#;
        fs::write(&path, existing).expect("existing config should be written");

        let profile = r#"
cli_auth_credentials_store = "file"
model_provider = "anthropic"
model_context_window = 1000000
model_auto_compact_token_limit = 400000

[tui]
status_line = ["launch-profile", "model-with-reasoning", "current-dir"]
session_picker_provider_filter = "all"

[model_providers.anthropic]
base_url = "https://new.example"
"#;

        merge_and_write_config_toml(&path, Some(profile), false).expect("merge should succeed");
        let merged = fs::read_to_string(&path).expect("merged config should be readable");

        assert!(merged.contains("foo = \"bar\""));
        assert!(merged.contains("cli_auth_credentials_store = \"file\""));
        assert!(merged.contains("model_provider = \"anthropic\""));
        assert!(merged.contains("model_context_window = 1000000"));
        assert!(merged.contains("model_auto_compact_token_limit = 400000"));
        assert!(merged.contains("[model_providers.anthropic]"));
        assert!(merged.contains("base_url = \"https://new.example\""));
        assert!(merged.contains("[model_providers.other]"));
        assert!(!merged.contains("https://old.example"));
        assert!(!merged.contains("model_context_window = 222222"));
        assert!(!merged.contains("model_auto_compact_token_limit = 111111"));

        let merged_table = parse_toml_table(&merged).expect("merged config should parse");
        assert_eq!(
            merged_table
                .get("tui")
                .and_then(|value| value.as_table())
                .and_then(|table| table.get("status_line"))
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                }),
            Some(vec![
                "launch-profile".to_string(),
                "model-with-reasoning".to_string(),
                "current-dir".to_string(),
            ])
        );
        assert_eq!(
            merged_table
                .get("tui")
                .and_then(|value| value.as_table())
                .and_then(|table| table.get("session_picker_provider_filter"))
                .and_then(|value| value.as_str()),
            Some("all")
        );

        let _ = fs::remove_dir_all(&tempdir);
    }

    #[test]
    fn normalize_profile_config_adds_default_cutex_status_line() {
        let account = StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: "demo-id".to_string(),
            name: "demo".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: Some("official".to_string()),
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
        };

        let normalized = normalize_profile_config_for_account(&account, None)
            .expect("normalize should succeed")
            .expect("default config should be materialized");
        let table = parse_toml_table(&normalized).expect("normalized config should parse");
        let tui = table
            .get("tui")
            .and_then(|value| value.as_table())
            .expect("tui table should exist");

        let status_line = tui
            .get("status_line")
            .and_then(|value| value.as_array())
            .expect("status_line should exist")
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(status_line, DEFAULT_CUTEX_STATUS_LINE);
        assert_eq!(
            tui.get("status_line_use_colors")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn normalize_profile_config_preserves_explicit_status_line() {
        let account = StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: "demo-id".to_string(),
            name: "demo".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: Some("official".to_string()),
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
        };
        let existing = r#"
[tui]
status_line = ["current-dir"]
status_line_use_colors = false
"#;

        let normalized = normalize_profile_config_for_account(&account, Some(existing.to_string()))
            .expect("normalize should succeed")
            .expect("config should remain materialized");
        let table = parse_toml_table(&normalized).expect("normalized config should parse");
        let tui = table
            .get("tui")
            .and_then(|value| value.as_table())
            .expect("tui table should exist");

        let status_line = tui
            .get("status_line")
            .and_then(|value| value.as_array())
            .expect("status_line should exist")
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(status_line, vec!["current-dir".to_string()]);
        assert_eq!(
            tui.get("status_line_use_colors")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn custom_status_items_catalog_includes_cutex_defaults() {
        let json = custom_status_items_catalog_json(&CodezConfig::default())
            .expect("catalog json should serialize")
            .expect("default catalog should be materialized");
        let catalog: CustomStatusItemsCatalogFile =
            serde_json::from_str(&json).expect("catalog should parse");
        let ids = catalog
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"cutex_welcome"));
        assert!(ids.contains(&"cutex_profile"));
    }

    #[test]
    fn merge_and_write_config_adds_managed_proxy_excludes_when_enabled() {
        let tempdir = std::env::temp_dir().join(format!("codez-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&tempdir).expect("tempdir should be created");
        let path = tempdir.join("config.toml");

        let existing = r#"
        [shell_environment_policy]
exclude = ["PATH", "http_proxy"]
"#;
        fs::write(&path, existing).expect("existing config should be written");
        let profile = extract_profile_config_toml(existing)
            .expect("profile config should parse")
            .expect("shell policy should be profile-scoped");

        merge_and_write_config_toml(&path, Some(&profile), true).expect("merge should succeed");
        let merged = fs::read_to_string(&path).expect("merged config should be readable");
        let merged_table = parse_toml_table(&merged).expect("merged config should parse");
        let excludes = merged_table
            .get("shell_environment_policy")
            .and_then(|value| value.as_table())
            .and_then(|policy| policy.get("exclude"))
            .and_then(|value| value.as_array())
            .expect("exclude list should exist");
        let excludes_upper = excludes
            .iter()
            .filter_map(|value| value.as_str().map(|entry| entry.to_ascii_uppercase()))
            .collect::<Vec<_>>();

        assert!(excludes_upper.iter().any(|entry| entry == "PATH"));
        for managed in TOOL_PROXY_ENV_EXCLUDE_PATTERNS {
            assert!(
                excludes_upper.iter().any(|entry| entry == managed),
                "missing managed exclude `{managed}`"
            );
            assert_eq!(
                excludes_upper
                    .iter()
                    .filter(|entry| entry.as_str() == managed)
                    .count(),
                1,
                "managed exclude `{managed}` should not be duplicated"
            );
        }

        let _ = fs::remove_dir_all(&tempdir);
    }

    #[test]
    fn merge_and_write_config_removes_managed_proxy_excludes_when_disabled() {
        let tempdir = std::env::temp_dir().join(format!("codez-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&tempdir).expect("tempdir should be created");
        let path = tempdir.join("config.toml");

        let existing = r#"
foo = "bar"

        [shell_environment_policy]
exclude = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "PATH"]
"#;
        fs::write(&path, existing).expect("existing config should be written");
        let profile = extract_profile_config_toml(existing)
            .expect("profile config should parse")
            .expect("shell policy should be profile-scoped");

        merge_and_write_config_toml(&path, Some(&profile), false).expect("merge should succeed");
        let merged = fs::read_to_string(&path).expect("merged config should be readable");
        let merged_table = parse_toml_table(&merged).expect("merged config should parse");

        assert_eq!(
            merged_table.get("foo").and_then(|value| value.as_str()),
            Some("bar")
        );
        let excludes = merged_table
            .get("shell_environment_policy")
            .and_then(|value| value.as_table())
            .and_then(|policy| policy.get("exclude"))
            .and_then(|value| value.as_array())
            .expect("exclude list should exist");
        let excludes_upper = excludes
            .iter()
            .filter_map(|value| value.as_str().map(|entry| entry.to_ascii_uppercase()))
            .collect::<Vec<_>>();

        assert!(excludes_upper.iter().any(|entry| entry == "PATH"));
        for managed in TOOL_PROXY_ENV_EXCLUDE_PATTERNS {
            assert!(
                !excludes_upper.iter().any(|entry| entry == managed),
                "managed exclude `{managed}` should be removed when disabled"
            );
        }

        let _ = fs::remove_dir_all(&tempdir);
    }

    #[test]
    fn ensure_materialized_account_files_adds_managed_proxy_excludes_for_enabled_proxy() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut global = CodezConfig::default();
        global.proxy = Some(
            proxy_config_from_parts(
                true,
                Some("socks5h://127.0.0.1:7890".to_string()),
                Some("localhost,127.0.0.1".to_string()),
                true,
            )
            .expect("proxy config should be valid"),
        );
        save_codez_config(&global).expect("config should save");

        let account = sample_account("proxy-materialize");
        write_profile_files(
            &account,
            "{\"demo\":true}\n",
            Some(
                r#"
model_provider = "openai"

[tui]
status_line = ["launch-profile", "current-dir"]
"#,
            ),
        )
        .expect("profile files should be written");

        let files =
            ensure_materialized_account_files(&account).expect("account files should materialize");
        let merged = fs::read_to_string(&files.config_path).expect("config should be readable");
        let merged_table = parse_toml_table(&merged).expect("merged config should parse");
        let excludes = merged_table
            .get("shell_environment_policy")
            .and_then(|value| value.as_table())
            .and_then(|policy| policy.get("exclude"))
            .and_then(|value| value.as_array())
            .expect("exclude list should exist");
        let excludes_upper = excludes
            .iter()
            .filter_map(|value| value.as_str().map(|entry| entry.to_ascii_uppercase()))
            .collect::<Vec<_>>();

        for managed in TOOL_PROXY_ENV_EXCLUDE_PATTERNS {
            assert!(
                excludes_upper.iter().any(|entry| entry == managed),
                "missing managed exclude `{managed}`"
            );
        }
        for managed in PROFILE_ROUTING_ENV_EXCLUDE_PATTERNS {
            assert!(
                excludes_upper.iter().any(|entry| entry == managed),
                "missing profile routing exclude `{managed}`"
            );
        }

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn determine_default_profile_prefers_directory_mapping_then_config_then_state_then_first() {
        let store = AccountsStore {
            version: STORE_VERSION,
            accounts: vec![sample_account("alpha"), sample_account("beta")],
            active_account_id: None,
        };

        let mut state = QuickRunState::default();
        state.last_global_profile = Some("beta".to_string());
        state
            .per_directory
            .insert("/workspace/project".to_string(), "alpha".to_string());
        let global_config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };

        assert_eq!(
            super::super::launch::determine_default_profile(
                &store,
                &state,
                &global_config,
                Some("/workspace/project")
            ),
            "alpha"
        );
        assert_eq!(
            super::super::launch::determine_default_profile(
                &store,
                &state,
                &global_config,
                Some("/workspace/other")
            ),
            "alpha"
        );
        assert_eq!(
            super::super::launch::determine_default_profile(
                &store,
                &state,
                &CodezConfig::default(),
                Some("/workspace/other")
            ),
            "beta"
        );
        assert_eq!(
            super::super::launch::determine_default_profile(
                &store,
                &QuickRunState::default(),
                &CodezConfig::default(),
                None
            ),
            "alpha"
        );
    }

    #[test]
    fn normalize_docker_user_name_rejects_invalid_values() {
        assert!(normalize_docker_user_name(Some("valid.user-1".to_string())).is_ok());
        assert!(normalize_docker_user_name(Some("".to_string())).is_err());
        assert!(normalize_docker_user_name(Some("../bad".to_string())).is_err());
        assert!(normalize_docker_user_name(Some("-bad".to_string())).is_err());
        assert!(normalize_docker_user_name(Some("bad name".to_string())).is_err());
    }

    #[test]
    fn codez_config_defaults_match_expected_runtime_behavior() {
        let config = CodezConfig::default();

        assert!(!config.docker_use_sudo);
        assert!(config.custom_status_items.is_empty());
        assert!(config.proxy.is_none());
        assert!(!config.session.enabled);
        assert!(config.default_profile.is_none());
        assert!(!config.default_profile_direct_launch);
    }

    #[test]
    fn rename_and_remove_global_default_profile_references_follow_profile_changes() {
        let mut config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };

        assert!(rename_global_profile_references(
            &mut config,
            "alpha",
            "beta"
        ));
        assert_eq!(config.default_profile.as_deref(), Some("beta"));

        assert!(remove_global_profile_references(&mut config, "beta"));
        assert!(config.default_profile.is_none());
    }

    #[test]
    fn profile_mutation_services_persist_every_reference_and_retain_materialized_files() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-profile-tui-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let alpha = sample_account("alpha");
        let beta = sample_account("beta");
        let store = AccountsStore {
            version: STORE_VERSION,
            accounts: vec![alpha.clone(), beta.clone()],
            active_account_id: Some(alpha.id.clone()),
        };
        save_store(&store).expect("account store should save");
        write_profile_files(&alpha, "{\"profile\":\"alpha\"}\n", None)
            .expect("alpha files should save");
        let alpha_files = materialized_account_files(&alpha).expect("alpha files should resolve");

        save_codez_config(&CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        })
        .expect("global config should save");
        save_quick_state(&QuickRunState {
            last_global_profile: Some("alpha".to_string()),
            per_directory: [("/tmp/project".to_string(), "alpha".to_string())]
                .into_iter()
                .collect(),
        })
        .expect("quick state should save");

        let codex_session_id = Uuid::new_v4().to_string();
        let record = CutexSessionRecord::new_at(
            "cutex.profile-ref".to_string(),
            Some(codex_session_id.clone()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            Some("alpha".to_string()),
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("session record");
        let mut sessions = CutexSessionStore::default();
        sessions
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        persist_cutex_session_store_and_im_record(&sessions, "cutex.profile-ref")
            .expect("initial session and IM record should save");

        let renamed = super::super::profile::rename_profile(&alpha.id, "gamma")
            .expect("profile should rename");
        assert_eq!(renamed.old_name, "alpha");
        assert_eq!(renamed.account.name, "gamma");
        assert_eq!(renamed.account.id, alpha.id);
        assert_eq!(
            load_codez_config().default_profile.as_deref(),
            Some("gamma")
        );
        let quick = load_quick_state();
        assert_eq!(quick.last_global_profile.as_deref(), Some("gamma"));
        assert_eq!(
            quick.per_directory.get("/tmp/project").map(String::as_str),
            Some("gamma")
        );
        assert_eq!(
            load_cutex_session_store()
                .expect("session store should reload")
                .sessions
                .get("cutex.profile-ref")
                .and_then(|record| record.profile.as_deref()),
            Some("gamma")
        );
        assert_eq!(
            load_im_registry()
                .expect("IM registry should reload")
                .sessions
                .get(&codex_session_id)
                .and_then(|entry| entry.profile.as_deref()),
            Some("gamma")
        );
        assert_eq!(
            fs::read_to_string(&alpha_files.auth_path).expect("materialized auth should remain"),
            "{\"profile\":\"alpha\"}\n"
        );

        let removed = super::super::profile::remove_profile(&alpha.id)
            .expect("renamed profile should remove by durable id");
        assert_eq!(removed.removed.name, "gamma");
        assert_eq!(
            removed.active.as_ref().map(|account| account.name.as_str()),
            Some("beta")
        );
        assert!(load_codez_config().default_profile.is_none());
        let quick = load_quick_state();
        assert!(quick.last_global_profile.is_none());
        assert!(!quick.per_directory.contains_key("/tmp/project"));
        assert!(load_cutex_session_store()
            .expect("session store should reload")
            .sessions
            .get("cutex.profile-ref")
            .is_some_and(|record| record.profile.is_none()));
        assert!(load_im_registry()
            .expect("IM registry should reload")
            .sessions
            .get(&codex_session_id)
            .is_some_and(|entry| entry.profile.is_none()));
        assert!(alpha_files.auth_path.exists());

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn profile_settings_update_materializes_config_only_deepseek_changes() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-provider-tui-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut account = sample_account("deepseek");
        account.source = Some("api-key".to_string());
        let store = AccountsStore {
            version: STORE_VERSION,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        };
        save_store(&store).expect("account store should save");
        write_profile_files(
            &account,
            "{\"OPENAI_API_KEY\":\"test-only\"}\n",
            Some(
                r#"
model_provider = "deepseek"

[model_providers.deepseek]
name = "Old name"
future_provider_key = "preserved"
request_max_retries = 7
"#,
            ),
        )
        .expect("profile files should save");
        let accounts_before =
            fs::read(accounts_path().expect("accounts path")).expect("accounts should read");

        let result = super::super::profile::update_profile_settings(
            &account.id,
            &ProfileSettingsPatch {
                codex_config: CodexProfileConfigPatch {
                    apply_deepseek_preset: true,
                    request_max_retries: ConfigValueUpdate::Set(8),
                    ..CodexProfileConfigPatch::default()
                },
                ..ProfileSettingsPatch::default()
            },
        )
        .expect("config-only update should save");
        assert!(result.changed);

        let files = materialized_account_files(&account).expect("profile paths");
        let config = fs::read_to_string(&files.config_path).expect("config should read");
        let table = parse_toml_table(&config).expect("config should parse");
        let provider = table
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("deepseek"))
            .and_then(toml::Value::as_table)
            .expect("DeepSeek provider should materialize");
        assert_eq!(
            provider
                .get("request_max_retries")
                .and_then(toml::Value::as_integer),
            Some(8)
        );
        assert_eq!(
            provider
                .get("future_provider_key")
                .and_then(toml::Value::as_str),
            Some("preserved")
        );
        assert!(files.model_catalog_path.exists());
        assert_eq!(
            fs::read(accounts_path().expect("accounts path")).expect("accounts should reread"),
            accounts_before,
            "a config-only edit must not rewrite accounts.json"
        );

        let catalog = super::super::account_store::load_profile_catalog_read_only()
            .expect("read-only profile catalog");
        assert_eq!(
            catalog[0]
                .codex_config
                .as_ref()
                .and_then(|config| config.request_max_retries),
            Some(8)
        );

        restore_env_var("HOME", old_home);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn profile_api_key_update_replaces_stale_oauth_without_rewriting_other_files() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-profile-key-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut account = sample_account("provider-key");
        account.source = Some("api-key".to_string());
        let store = AccountsStore {
            version: STORE_VERSION,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        };
        save_store(&store).expect("account store should save");
        let original_config = "model = \"deepseek-chat\"\n";
        write_profile_files(
            &account,
            r#"{"auth_mode":"chatgpt","tokens":{"refresh_token":"stale-test-token"}}"#,
            Some(original_config),
        )
        .expect("stale profile fixture should save");
        let files = materialized_account_files(&account).expect("profile paths");
        let accounts_before =
            fs::read(accounts_path().expect("accounts path")).expect("accounts should read");
        let config_before = fs::read(&files.config_path).expect("config should read");
        let test_key = "sk-test-provider-replacement";
        let patch = ProfileSettingsPatch {
            api_key: ProfileApiKeyUpdate::Replace(test_key.to_string()),
            ..ProfileSettingsPatch::default()
        };
        assert!(!format!("{patch:?}").contains(test_key));

        let result = super::super::profile::update_profile_settings(&account.id, &patch)
            .expect("API key replacement should save");
        assert!(result.changed);
        let auth: serde_json::Value =
            serde_json::from_slice(&fs::read(&files.auth_path).expect("updated auth should read"))
                .expect("updated auth should parse");
        assert_eq!(
            auth.get("OPENAI_API_KEY")
                .and_then(serde_json::Value::as_str),
            Some(test_key)
        );
        assert_eq!(auth.as_object().map(serde_json::Map::len), Some(1));
        assert!(auth.get("tokens").is_none());
        assert!(auth.get("auth_mode").is_none());
        assert_eq!(
            fs::read(accounts_path().expect("accounts path")).expect("accounts should reread"),
            accounts_before,
            "an auth-only edit must not rewrite accounts.json"
        );
        assert_eq!(
            fs::read(&files.config_path).expect("config should reread"),
            config_before,
            "an auth-only edit must not rewrite config.toml"
        );
        assert!(!files.model_catalog_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&files.auth_path)
                    .expect("auth metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let catalog = super::super::account_store::load_profile_catalog_read_only()
            .expect("read-only profile catalog");
        assert!(catalog[0].api_key_configured);

        restore_env_var("HOME", old_home);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn profile_api_key_update_rejects_non_api_key_profiles_without_writes() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home =
            std::env::temp_dir().join(format!("cutex-profile-key-reject-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let account = sample_account("official");
        let store = AccountsStore {
            version: STORE_VERSION,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        };
        save_store(&store).expect("account store should save");
        write_profile_files(
            &account,
            r#"{"OPENAI_API_KEY":"existing-test-key"}"#,
            Some("model = \"gpt-test\"\n"),
        )
        .expect("official profile fixture should save");
        let files = materialized_account_files(&account).expect("profile paths");
        let auth_before = fs::read(&files.auth_path).expect("auth should read");
        let config_before = fs::read(&files.config_path).expect("config should read");
        let accounts_before =
            fs::read(accounts_path().expect("accounts path")).expect("accounts should read");

        let error = super::super::profile::update_profile_settings(
            &account.id,
            &ProfileSettingsPatch {
                api_key: ProfileApiKeyUpdate::Replace("sk-test-rejected".to_string()),
                ..ProfileSettingsPatch::default()
            },
        )
        .expect_err("official profile must reject API key editing");
        assert!(error
            .to_string()
            .contains("only available for Codex API-key profiles"));
        assert_eq!(
            fs::read(&files.auth_path).expect("auth should remain"),
            auth_before
        );
        assert_eq!(
            fs::read(&files.config_path).expect("config should remain"),
            config_before
        );
        assert_eq!(
            fs::read(accounts_path().expect("accounts path")).expect("accounts should remain"),
            accounts_before
        );

        restore_env_var("HOME", old_home);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn deepseek_preset_rejects_oauth_profile_before_writing_files() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home =
            std::env::temp_dir().join(format!("cutex-provider-auth-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let account = sample_account("oauth");
        let store = AccountsStore {
            version: STORE_VERSION,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        };
        save_store(&store).expect("account store should save");
        let original_config = "model = \"gpt-original\"\n";
        write_profile_files(
            &account,
            r#"{"tokens":{"access_token":"test-only"}}"#,
            Some(original_config),
        )
        .expect("OAuth profile files should save");
        let files = materialized_account_files(&account).expect("profile paths");

        let error = super::super::profile::update_profile_settings(
            &account.id,
            &ProfileSettingsPatch {
                codex_config: CodexProfileConfigPatch {
                    apply_deepseek_preset: true,
                    ..CodexProfileConfigPatch::default()
                },
                ..ProfileSettingsPatch::default()
            },
        )
        .expect_err("OAuth profile must not accept a DeepSeek preset");
        assert!(error.to_string().contains("requires an API-key profile"));
        assert_eq!(
            fs::read_to_string(&files.config_path).expect("config should remain"),
            original_config
        );
        assert!(!files.model_catalog_path.exists());

        restore_env_var("HOME", old_home);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn failed_profile_materialization_restores_config_and_model_catalog() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home =
            std::env::temp_dir().join(format!("cutex-provider-rollback-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut account = sample_account("rollback");
        account.source = Some("api-key".to_string());
        let store = AccountsStore {
            version: STORE_VERSION,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        };
        save_store(&store).expect("account store should save");
        let original_config = "model = \"original\"\n";
        let original_auth = "{\"OPENAI_API_KEY\":\"test-only\"}\n";
        write_profile_files(&account, original_auth, Some(original_config))
            .expect("profile files should save");
        let files = materialized_account_files(&account).expect("profile paths");
        assert!(!files.model_catalog_path.exists());
        fs::create_dir(&files.custom_status_items_path)
            .expect("directory fixture should force a late materialization failure");
        let accounts_before =
            fs::read(accounts_path().expect("accounts path")).expect("accounts should read");

        let error = super::super::profile::update_profile_settings(
            &account.id,
            &ProfileSettingsPatch {
                api_key: ProfileApiKeyUpdate::Replace("sk-test-rollback".to_string()),
                codex_config: CodexProfileConfigPatch {
                    apply_deepseek_preset: true,
                    ..CodexProfileConfigPatch::default()
                },
                ..ProfileSettingsPatch::default()
            },
        )
        .expect_err("late materialization failure should be returned");
        assert!(
            format!("{error:#}").contains("cutex-status-items.json"),
            "unexpected materialization error: {error:#}"
        );
        assert_eq!(
            fs::read_to_string(&files.config_path).expect("config should be restored"),
            original_config
        );
        assert_eq!(
            fs::read_to_string(&files.auth_path).expect("auth should be restored"),
            original_auth
        );
        assert!(
            !files.model_catalog_path.exists(),
            "newly materialized catalog should be removed during rollback"
        );
        assert_eq!(
            fs::read(accounts_path().expect("accounts path")).expect("accounts should reread"),
            accounts_before
        );

        restore_env_var("HOME", old_home);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn set_codez_codex_home_uses_codez_cli_subdir_and_migrates_legacy_home() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_codex_home = std::env::var_os("CODEX_HOME");
        let legacy_home = temp_home.join(".codex-codez");
        let new_home = temp_home.join(".cutex").join("codex-home");

        fs::create_dir_all(&legacy_home).expect("legacy codex home should be created");
        fs::write(legacy_home.join("marker.txt"), "demo").expect("legacy marker should be written");

        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        crate::cli_app::root_wizard::set_codez_codex_home(true).expect("codex home should be set");

        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(new_home.clone().into_os_string())
        );
        assert!(!legacy_home.exists());
        assert_eq!(
            fs::read_to_string(new_home.join("marker.txt")).expect("migrated marker should exist"),
            "demo"
        );

        match old_codex_home {
            Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }
        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn set_codez_codex_home_migrates_legacy_codez_cli_root_to_cutex() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_codex_home = std::env::var_os("CODEX_HOME");
        let legacy_root = temp_home.join(".codez-cli");
        let new_root = temp_home.join(".cutex");

        fs::create_dir_all(&legacy_root).expect("legacy root should be created");
        fs::write(legacy_root.join("config.json"), "{\"demo\":true}\n")
            .expect("legacy config should be written");

        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        crate::cli_app::root_wizard::set_codez_codex_home(true).expect("codex home should be set");

        assert!(!legacy_root.exists());
        assert_eq!(
            fs::read_to_string(new_root.join("config.json")).expect("config should migrate"),
            "{\"demo\":true}\n"
        );

        match old_codex_home {
            Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }
        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn official_codex_login_scrubs_profile_env_overrides() {
        let mut command = Command::new("cute-codex");
        command.env("CODEX_HOME", "/tmp/cutex-login");
        for key in super::super::auth::codex_login_env_override_keys() {
            command.env(key, "/tmp/profile-value");
        }

        super::super::auth::scrub_codex_login_env(&mut command);

        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.iter().any(|(key, value)| {
            *key == std::ffi::OsStr::new("CODEX_HOME")
                && value == &Some(std::ffi::OsStr::new("/tmp/cutex-login"))
        }));
        for expected_key in super::super::auth::codex_login_env_override_keys() {
            assert!(
                envs.iter().any(|(key, value)| {
                    *key == std::ffi::OsStr::new(expected_key) && value.is_none()
                }),
                "{expected_key} should be explicitly removed for login"
            );
        }
    }

    #[test]
    fn materialized_account_files_live_under_codez_profiles_dir() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let account = sample_account("demo");
        write_profile_files(
            &account,
            "{\"demo\":true}\n",
            Some("model_provider = \"openai\"\n"),
        )
        .expect("profile files should be written");

        let files =
            ensure_materialized_account_files(&account).expect("account files should materialize");

        assert_eq!(
            files.auth_path,
            temp_home
                .join(".cutex")
                .join("profiles")
                .join("demo-id")
                .join("auth.json")
        );
        assert_eq!(
            files.config_path,
            temp_home
                .join(".cutex")
                .join("profiles")
                .join("demo-id")
                .join("config.toml")
        );
        assert_eq!(
            fs::read_to_string(&files.auth_path).expect("auth should be readable"),
            "{\"demo\":true}\n"
        );
        let config = fs::read_to_string(&files.config_path).expect("config should be readable");
        let config_table = parse_toml_table(&config).expect("config should parse");
        assert_eq!(
            config_table
                .get("model_provider")
                .and_then(|value| value.as_str()),
            Some("openai")
        );
        assert_eq!(
            config_table
                .get("tui")
                .and_then(|value| value.as_table())
                .and_then(|table| table.get("status_line"))
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                }),
            Some(DEFAULT_CUTEX_STATUS_LINE.map(str::to_string).to_vec())
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn activate_account_preserves_existing_materialized_config() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = AccountsStore::default();
        let account = sample_account("demo");
        store.accounts.push(account.clone());
        save_store(&store).expect("store should save");

        write_profile_files(
            &account,
            "{\"demo\":true}\n",
            Some("model_provider = \"openai\"\n"),
        )
        .expect("profile files should be written");
        let files =
            ensure_materialized_account_files(&account).expect("account files should materialize");
        let edited = r#"
model_provider = "anthropic"
model_context_window = 1000000
model_auto_compact_token_limit = 400000

[tui]
status_line = ["launch-profile", "current-dir"]
status_line_use_colors = true
"#;
        fs::write(&files.config_path, edited).expect("edited config should be written");

        let activated =
            super::super::profile::activate_account("demo").expect("account should activate");
        let persisted = fs::read_to_string(&files.config_path).expect("config should remain");
        let reloaded = load_store().expect("store should reload");

        let persisted_table =
            parse_toml_table(&persisted).expect("persisted config should parse as TOML");
        let edited_table = parse_toml_table(edited).expect("edited config should parse as TOML");
        for key in [
            "model_provider",
            "model_context_window",
            "model_auto_compact_token_limit",
            "tui",
        ] {
            assert_eq!(persisted_table.get(key), edited_table.get(key));
        }
        let excludes = persisted_table
            .get("shell_environment_policy")
            .and_then(toml::Value::as_table)
            .and_then(|policy| policy.get("exclude"))
            .and_then(toml::Value::as_array)
            .expect("profile routing excludes should be applied");
        for managed in PROFILE_ROUTING_ENV_EXCLUDE_PATTERNS {
            assert!(excludes.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|entry| entry.eq_ignore_ascii_case(managed))
            }));
        }
        assert_eq!(
            account_model_provider(&activated).as_deref(),
            Some("anthropic")
        );

        let active_auth = fs::read_to_string(
            host_codex_home_dir()
                .expect("host codex home should resolve")
                .join("auth.json"),
        )
        .expect("active auth should be synced");
        assert_eq!(active_auth, "{\"demo\":true}\n");
        assert!(reloaded
            .accounts
            .iter()
            .any(|candidate| candidate.id == account.id));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn activate_account_syncs_active_codex_home_for_app_server() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = AccountsStore::default();
        let account = sample_account("demo");
        store.accounts.push(account.clone());
        save_store(&store).expect("store should save");

        let codex_home = host_codex_home_dir().expect("host codex home should resolve");
        fs::create_dir_all(&codex_home).expect("codex home should be created");
        fs::write(
            codex_home.join("config.toml"),
            r#"
approval_policy = "never"
model_provider = "old"

[model_providers.old]
base_url = "https://old.example.test"
"#,
        )
        .expect("existing shared config should be written");

        write_profile_files(
            &account,
            "{\"profile\":true}\n",
            Some(
                r#"
model_provider = "custom"

[model_providers.custom]
base_url = "https://custom.example.test/v1"

[tui]
status_line = ["launch-profile", "current-dir"]
"#,
            ),
        )
        .expect("profile files should be written");

        super::super::profile::activate_account("demo").expect("account should activate");

        let active_auth =
            fs::read_to_string(codex_home.join("auth.json")).expect("active auth should sync");
        assert_eq!(active_auth, "{\"profile\":true}\n");

        let active_config =
            fs::read_to_string(codex_home.join("config.toml")).expect("active config should sync");
        let table = parse_toml_table(&active_config).expect("active config should parse");
        assert_eq!(
            table
                .get("approval_policy")
                .and_then(|value| value.as_str()),
            Some("never")
        );
        assert_eq!(
            table.get("model_provider").and_then(|value| value.as_str()),
            Some("custom")
        );
        assert!(
            table
                .get("model_providers")
                .and_then(|value| value.as_table())
                .and_then(|providers| providers.get("old"))
                .is_none(),
            "previous active provider should be removed"
        );
        assert!(
            table
                .get("model_providers")
                .and_then(|value| value.as_table())
                .and_then(|providers| providers.get("custom"))
                .is_some(),
            "selected profile provider should be present"
        );
        assert_eq!(
            table
                .get("tui")
                .and_then(|value| value.as_table())
                .and_then(|tui| tui.get("status_line"))
                .and_then(|value| value.as_array())
                .map(|items| items.len()),
            Some(2)
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn active_home_syncs_and_clears_profile_local_model_catalogs() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let deepseek = sample_account("deepseek");
        let plain = sample_account("plain");
        let mut store = AccountsStore::default();
        store.accounts = vec![deepseek.clone(), plain.clone()];
        save_store(&store).expect("store should save");

        write_profile_files(
            &deepseek,
            "{}",
            Some(
                r#"
model = "deepseek-v4-flash"
model_provider = "deepseek"
model_catalog_json = "models.json"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/"
"#,
            ),
        )
        .expect("DeepSeek profile files");
        let deepseek_files = materialized_account_files(&deepseek).expect("DeepSeek profile paths");
        fs::write(&deepseek_files.model_catalog_path, r#"{"models":[]}"#)
            .expect("DeepSeek catalog");
        write_profile_files(
            &plain,
            "{}",
            Some("model = \"gpt-test\"\nmodel_provider = \"openai\"\n"),
        )
        .expect("plain profile files");

        super::super::profile::activate_account("deepseek").expect("activate DeepSeek");
        let codex_home = host_codex_home_dir().expect("active Codex home");
        assert_eq!(
            fs::read_to_string(codex_home.join("models.json")).expect("active catalog"),
            r#"{"models":[]}"#
        );

        super::super::profile::activate_account("plain").expect("activate plain profile");
        assert!(!codex_home.join("models.json").exists());
        let active_config =
            fs::read_to_string(codex_home.join("config.toml")).expect("active config");
        let active_table = parse_toml_table(&active_config).expect("active config should parse");
        assert_eq!(
            active_table.get("model").and_then(toml::Value::as_str),
            Some("gpt-test")
        );
        assert!(!active_table.contains_key("model_catalog_json"));
        assert!(active_table
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("deepseek"))
            .is_none());

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn prepare_account_for_launch_does_not_switch_or_sync_active_codex_home() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = AccountsStore::default();
        let active = sample_account("active");
        let run_only = sample_account("run-only");
        store.accounts.push(active.clone());
        store.accounts.push(run_only.clone());
        store.active_account_id = Some(active.id.clone());
        save_store(&store).expect("store should save");

        let codex_home = host_codex_home_dir().expect("host codex home should resolve");
        fs::create_dir_all(&codex_home).expect("codex home should be created");
        fs::write(codex_home.join("auth.json"), "{\"active\":true}\n")
            .expect("active auth should be written");
        fs::write(
            codex_home.join("config.toml"),
            "model_provider = \"active\"\n",
        )
        .expect("active config should be written");

        write_profile_files(
            &run_only,
            "{\"run_only\":true}\n",
            Some("model_provider = \"run_only\"\n"),
        )
        .expect("run-only profile files should be written");

        let prepared = super::super::launch::prepare_account_for_launch("run-only")
            .expect("account should prepare for launch");
        assert_eq!(prepared.id, run_only.id);

        let reloaded = load_store().expect("store should reload");
        assert_eq!(
            reloaded.active_account_id.as_deref(),
            Some(active.id.as_str())
        );
        assert!(reloaded
            .accounts
            .iter()
            .find(|account| account.id == run_only.id)
            .and_then(|account| account.last_used_at.as_ref())
            .is_some());

        let active_auth =
            fs::read_to_string(codex_home.join("auth.json")).expect("active auth should remain");
        let active_config = fs::read_to_string(codex_home.join("config.toml"))
            .expect("active config should remain");
        assert_eq!(active_auth, "{\"active\":true}\n");
        assert_eq!(active_config, "model_provider = \"active\"\n");

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn one_launch_profile_resolution_and_command_are_byte_stable() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = AccountsStore::default();
        let active = sample_account("active");
        let selected = sample_account("run-only");
        store.accounts = vec![active.clone(), selected.clone()];
        store.active_account_id = Some(active.id.clone());
        save_store(&store).expect("store should save");
        write_profile_files(
            &selected,
            "{\"OPENAI_API_KEY\":\"test-only\"}\n",
            Some("model = \"gpt-test\"\nmodel_catalog_json = \"models.json\"\n"),
        )
        .expect("selected profile files");
        let selected_files = materialized_account_files(&selected).expect("selected files");
        fs::write(&selected_files.model_catalog_path, r#"{"models":[]}"#)
            .expect("profile model catalog");

        let codex_home = host_codex_home_dir().expect("active Codex home");
        fs::create_dir_all(&codex_home).expect("active Codex home should exist");
        fs::write(codex_home.join("auth.json"), "{\"active\":true}\n").expect("active auth");
        fs::write(codex_home.join("config.toml"), "model = \"active\"\n").expect("active config");
        fs::write(codex_home.join("models.json"), r#"{"active":true}"#)
            .expect("active model catalog");

        let accounts_file = accounts_path().expect("accounts path");
        let before = [
            fs::read(&accounts_file).expect("accounts before"),
            fs::read(&selected_files.auth_path).expect("profile auth before"),
            fs::read(&selected_files.config_path).expect("profile config before"),
            fs::read(&selected_files.model_catalog_path).expect("profile catalog before"),
            fs::read(codex_home.join("auth.json")).expect("active auth before"),
            fs::read(codex_home.join("config.toml")).expect("active config before"),
            fs::read(codex_home.join("models.json")).expect("active catalog before"),
        ];

        let resolved = super::super::launch::resolve_launch_profile_override("run-only")
            .expect("one-launch profile should resolve");
        let command = super::super::launch_command::codex_launch_command_with_prevalidated_profile(
            &resolved.account,
            &["--version".to_string()],
            false,
            &[],
            &resolved.files,
        )
        .expect("prevalidated launch command");

        assert_eq!(resolved.requested, "run-only");
        assert_eq!(resolved.effective_name(), "run-only");
        assert_eq!(
            last_launch_env(&command, CODEX_LAUNCH_PROFILE_ENV_VAR),
            Some("run-only")
        );
        assert_eq!(fs::read(&accounts_file).expect("accounts after"), before[0]);
        assert_eq!(
            fs::read(&selected_files.auth_path).expect("profile auth after"),
            before[1]
        );
        assert_eq!(
            fs::read(&selected_files.config_path).expect("profile config after"),
            before[2]
        );
        assert_eq!(
            fs::read(&selected_files.model_catalog_path).expect("profile catalog after"),
            before[3]
        );
        assert_eq!(
            fs::read(codex_home.join("auth.json")).expect("active auth after"),
            before[4]
        );
        assert_eq!(
            fs::read(codex_home.join("config.toml")).expect("active config after"),
            before[5]
        );
        assert_eq!(
            fs::read(codex_home.join("models.json")).expect("active catalog after"),
            before[6]
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn save_store_round_trips_profile_default_cli_args() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = AccountsStore::default();
        let mut account = sample_account("work");
        account.default_cli_args = vec!["--sandbox".to_string(), "danger-full-access".to_string()];
        store.accounts.push(account);
        save_store(&store).expect("store should save");

        let reloaded = load_store().expect("store should reload");
        assert_eq!(
            reloaded.accounts[0].default_cli_args,
            vec!["--sandbox".to_string(), "danger-full-access".to_string(),]
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn host_launch_sets_codex_install_dir_for_cute_codex_app_server() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_path = std::env::var_os("PATH");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(temp_home.join("bin")).expect("temp bin should be created");
        let cute_codex = temp_home.join("bin").join("cute-codex");
        fs::write(&cute_codex, "#!/usr/bin/env sh\nexit 0\n")
            .expect("fake cute-codex should write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&cute_codex, fs::Permissions::from_mode(0o755))
                .expect("fake cute-codex should be executable");
        }
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var("PATH", temp_home.join("bin"));
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "cute-codex");
        }

        let account = sample_account("demo");
        write_profile_files(
            &account,
            "{\"demo\":true}\n",
            Some("model_provider = \"openai\"\n"),
        )
        .expect("profile files should be written");

        let launch = codex_launch_command(&account, &[]).expect("launch should build");
        let install_dir = launch
            .envs
            .iter()
            .find_map(|(key, value)| (key == CODEX_INSTALL_DIR_ENV_VAR).then_some(value.clone()))
            .expect("CODEX_INSTALL_DIR should be set");
        let wrapper = PathBuf::from(&install_dir).join("codex");
        let wrapper_contents = fs::read_to_string(&wrapper).expect("codex wrapper should exist");
        assert!(wrapper_contents.contains(cute_codex.to_string_lossy().as_ref()));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_path {
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn host_launch_command_exports_account_file_envs() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/custom-codex");
        }

        let account = sample_account("demo");
        write_profile_files(
            &account,
            "{\"demo\":true}\n",
            Some("model_provider = \"openai\"\n"),
        )
        .expect("profile files should be written");

        let launch =
            codex_launch_command(&account, &["resume".to_string()]).expect("launch should build");

        assert_eq!(launch.program, "/tmp/custom-codex");
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == CODEX_AUTH_FILE_ENV_VAR
                && path_ends_with(value, "/.cutex/profiles/demo-id/auth.json")));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == CODEX_CONFIG_FILE_ENV_VAR
                && path_ends_with(value, "/.cutex/profiles/demo-id/config.toml")));

        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn codex_session_exists_detects_session_index_and_rollout_files() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let codex_home = temp_home.join(".cutex").join("codex-home");
        let rollout_dir = codex_home
            .join("sessions")
            .join("2026")
            .join("06")
            .join("25");
        fs::create_dir_all(&rollout_dir).expect("temp sessions dir should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        fs::write(
            codex_home.join("session_index.jsonl"),
            "{\"id\":\"019e-index\",\"timestamp\":\"2026-06-25T00:00:00Z\"}\n",
        )
        .expect("session index should be written");
        fs::write(
            rollout_dir.join("rollout-2026-06-25T00-00-00-019e-rollout.jsonl"),
            "{}\n",
        )
        .expect("rollout file should be written");

        assert!(codex_session_exists_in_home("019e-index").expect("session lookup should succeed"));
        assert!(
            codex_session_exists_in_home("019e-rollout").expect("session lookup should succeed")
        );
        assert!(
            !codex_session_exists_in_home("019e-missing").expect("session lookup should succeed")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn host_launch_command_keeps_explicit_notify_timeout_env_over_global_config() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        let old_notify_idle = std::env::var_os(CODEX_NOTIFY_IDLE_TIMEOUT_ENV_VAR);
        let old_notify_composer = std::env::var_os(CODEX_NOTIFY_COMPOSER_IDLE_TIMEOUT_ENV_VAR);
        let old_notify_approval = std::env::var_os(CODEX_NOTIFY_APPROVAL_TIMEOUT_ENV_VAR);
        let old_notify_startup_idle = std::env::var_os(CODEX_NOTIFY_STARTUP_IDLE_TIMEOUT_ENV_VAR);
        let old_notify_events = std::env::var_os(CODEX_NOTIFY_EVENTS_ENV_VAR);
        let old_notify_content = std::env::var_os(CODEX_NOTIFY_USER_MESSAGE_CONTENT_ENV_VAR);
        let old_notify_preview = std::env::var_os(CODEX_NOTIFY_USER_MESSAGE_PREVIEW_CHARS_ENV_VAR);
        let old_threshold_warning_mode =
            std::env::var_os(CODEX_RATE_LIMIT_THRESHOLD_WARNING_MODE_ENV_VAR);
        let old_model_nudge_mode = std::env::var_os(CODEX_RATE_LIMIT_MODEL_NUDGE_MODE_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
            std::env::set_var(CODEX_NOTIFY_IDLE_TIMEOUT_ENV_VAR, "20");
            std::env::set_var(CODEX_NOTIFY_COMPOSER_IDLE_TIMEOUT_ENV_VAR, "5");
            std::env::set_var(CODEX_NOTIFY_APPROVAL_TIMEOUT_ENV_VAR, "30");
            std::env::set_var(CODEX_NOTIFY_STARTUP_IDLE_TIMEOUT_ENV_VAR, "180");
            std::env::set_var(CODEX_NOTIFY_EVENTS_ENV_VAR, "task_completed");
            std::env::set_var(CODEX_NOTIFY_USER_MESSAGE_CONTENT_ENV_VAR, "none");
            std::env::set_var(CODEX_NOTIFY_USER_MESSAGE_PREVIEW_CHARS_ENV_VAR, "40");
            std::env::set_var(CODEX_RATE_LIMIT_THRESHOLD_WARNING_MODE_ENV_VAR, "always");
            std::env::set_var(CODEX_RATE_LIMIT_MODEL_NUDGE_MODE_ENV_VAR, "daily");
        }

        let mut config = CodezConfig::default();
        config.notify_service_idle_timeout_secs = Some(60);
        config.notify_service_composer_idle_timeout_secs = Some(600);
        config.notify_service_approval_timeout_secs = Some(90);
        config.notify_service_startup_idle_timeout_secs = Some(240);
        config.notify_service_events = Some(vec!["user_message_sent".to_string()]);
        config.notify_service_user_message_content = Some("full".to_string());
        config.notify_service_user_message_preview_chars = Some(200);
        config.rate_limit_threshold_warning_mode = Some("off".to_string());
        config.rate_limit_model_nudge_mode = Some("off".to_string());
        save_codez_config(&config).expect("config should be saved");

        let account = sample_account("notify-timeout-env");
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");

        let launch =
            codex_launch_command(&account, &["resume".to_string()]).expect("launch should build");

        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_NOTIFY_IDLE_TIMEOUT_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_NOTIFY_COMPOSER_IDLE_TIMEOUT_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_NOTIFY_APPROVAL_TIMEOUT_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_NOTIFY_STARTUP_IDLE_TIMEOUT_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_NOTIFY_EVENTS_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_NOTIFY_USER_MESSAGE_CONTENT_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_NOTIFY_USER_MESSAGE_PREVIEW_CHARS_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_RATE_LIMIT_THRESHOLD_WARNING_MODE_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CODEX_RATE_LIMIT_MODEL_NUDGE_MODE_ENV_VAR));

        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        match old_notify_idle {
            Some(value) => unsafe { std::env::set_var(CODEX_NOTIFY_IDLE_TIMEOUT_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CODEX_NOTIFY_IDLE_TIMEOUT_ENV_VAR) },
        }
        match old_notify_composer {
            Some(value) => unsafe {
                std::env::set_var(CODEX_NOTIFY_COMPOSER_IDLE_TIMEOUT_ENV_VAR, value)
            },
            None => unsafe { std::env::remove_var(CODEX_NOTIFY_COMPOSER_IDLE_TIMEOUT_ENV_VAR) },
        }
        match old_notify_approval {
            Some(value) => unsafe {
                std::env::set_var(CODEX_NOTIFY_APPROVAL_TIMEOUT_ENV_VAR, value)
            },
            None => unsafe { std::env::remove_var(CODEX_NOTIFY_APPROVAL_TIMEOUT_ENV_VAR) },
        }
        match old_notify_startup_idle {
            Some(value) => unsafe {
                std::env::set_var(CODEX_NOTIFY_STARTUP_IDLE_TIMEOUT_ENV_VAR, value)
            },
            None => unsafe { std::env::remove_var(CODEX_NOTIFY_STARTUP_IDLE_TIMEOUT_ENV_VAR) },
        }
        match old_notify_events {
            Some(value) => unsafe { std::env::set_var(CODEX_NOTIFY_EVENTS_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CODEX_NOTIFY_EVENTS_ENV_VAR) },
        }
        match old_notify_content {
            Some(value) => unsafe {
                std::env::set_var(CODEX_NOTIFY_USER_MESSAGE_CONTENT_ENV_VAR, value)
            },
            None => unsafe { std::env::remove_var(CODEX_NOTIFY_USER_MESSAGE_CONTENT_ENV_VAR) },
        }
        match old_notify_preview {
            Some(value) => unsafe {
                std::env::set_var(CODEX_NOTIFY_USER_MESSAGE_PREVIEW_CHARS_ENV_VAR, value)
            },
            None => unsafe {
                std::env::remove_var(CODEX_NOTIFY_USER_MESSAGE_PREVIEW_CHARS_ENV_VAR)
            },
        }
        match old_threshold_warning_mode {
            Some(value) => unsafe {
                std::env::set_var(CODEX_RATE_LIMIT_THRESHOLD_WARNING_MODE_ENV_VAR, value)
            },
            None => unsafe {
                std::env::remove_var(CODEX_RATE_LIMIT_THRESHOLD_WARNING_MODE_ENV_VAR)
            },
        }
        match old_model_nudge_mode {
            Some(value) => unsafe {
                std::env::set_var(CODEX_RATE_LIMIT_MODEL_NUDGE_MODE_ENV_VAR, value)
            },
            None => unsafe { std::env::remove_var(CODEX_RATE_LIMIT_MODEL_NUDGE_MODE_ENV_VAR) },
        }
        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn docker_runtime_adds_sandbox_bypass_by_default() {
        let mut account = sample_account("docker");
        account.runtime = RuntimeConfig::Docker {
            image: "img".to_string(),
            user_name: Some("user".to_string()),
        };

        let args = codex_args_for_runtime(&account, vec!["resume".to_string()]);

        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "danger-full-access".to_string(),
                "resume".to_string()
            ]
        );
    }

    #[test]
    fn profile_default_cli_args_are_prepended_before_user_args() {
        let mut account = sample_account("work");
        account.default_cli_args = vec!["--sandbox".to_string(), "danger-full-access".to_string()];

        let args = combined_profile_cli_args(&account, vec!["resume".to_string()]);

        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "danger-full-access".to_string(),
                "resume".to_string()
            ]
        );
    }

    #[test]
    fn docker_runtime_keeps_profile_default_sandbox_choice() {
        let mut account = sample_account("docker");
        account.runtime = RuntimeConfig::Docker {
            image: "img".to_string(),
            user_name: Some("user".to_string()),
        };
        account.default_cli_args = vec!["--sandbox".to_string(), "danger-full-access".to_string()];

        let args = codex_args_for_runtime(&account, account.default_cli_args.clone());

        assert_eq!(
            args,
            vec!["--sandbox".to_string(), "danger-full-access".to_string()]
        );
    }

    #[test]
    fn docker_runtime_skips_sandbox_bypass_for_non_codex_binary() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        unsafe {
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/sh");
        }

        let mut account = sample_account("docker");
        account.runtime = RuntimeConfig::Docker {
            image: "img".to_string(),
            user_name: Some("user".to_string()),
        };

        let args = codex_args_for_runtime(&account, vec!["-lc".to_string(), "env".to_string()]);
        assert_eq!(args, vec!["-lc".to_string(), "env".to_string()]);

        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
    }

    #[test]
    fn docker_runtime_preserves_explicit_sandbox_choice() {
        let mut account = sample_account("docker");
        account.runtime = RuntimeConfig::Docker {
            image: "img".to_string(),
            user_name: Some("user".to_string()),
        };

        let args = codex_args_for_runtime(
            &account,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "resume".to_string(),
            ],
        );

        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "resume".to_string()
            ]
        );
    }

    #[test]
    fn host_launch_command_includes_profile_and_runtime_envs() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
        }

        let mut account = sample_account("test-profile");
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");
        account.plan_type = Some("plus".to_string());
        account.email = Some("test-profile@example.test".to_string());

        let launch =
            codex_launch_command(&account, &["resume".to_string()]).expect("launch should build");

        assert_eq!(launch.program, "/tmp/cute-codex");
        assert!(launch.envs.iter().any(|(key, value)| {
            key == CODEX_LAUNCH_PROFILE_ENV_VAR && value == "test-profile"
        }));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| { key == CODEX_LAUNCH_RUNTIME_ENV_VAR && value == "host" }));
        assert!(launch.envs.iter().any(|(key, value)| {
            key == CODEX_LAUNCH_PROFILE_SOURCE_ENV_VAR && value == "official"
        }));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| { key == CODEX_LAUNCH_PROFILE_TYPE_ENV_VAR && value == "plus" }));
        assert!(launch.envs.iter().any(|(key, value)| {
            key == CODEX_LAUNCH_PROFILE_EMAIL_ENV_VAR && value == "test-profile@example.test"
        }));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn ordinary_exec_launch_omits_agent_bus_envs_and_top_level_marker() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
        }

        let mut config = CodezConfig::default();
        config.agent_bus_enabled = true;
        config.agent_bus_port = Some(24261);
        config.agent_bus_token = Some("agent-test-token".to_string());
        save_codez_config(&config).expect("config should be saved");

        let mut account = sample_account("profile-name");
        account.agent_name = Some("worker-one".to_string());
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");

        let launch = codex_launch_command(&account, &["exec".to_string(), "Hi.".to_string()])
            .expect("launch should build");

        assert_eq!(launch.args, ["exec", "Hi."]);
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CUTEX_AGENT_BUS_URL_ENV_VAR));
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CUTEX_AGENT_ID_ENV_VAR));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn host_launch_command_includes_agent_bus_envs_in_agent_mode() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
        }

        let mut config = CodezConfig::default();
        config.agent_bus_enabled = true;
        config.agent_bus_port = Some(24261);
        config.agent_bus_token = Some("agent-test-token".to_string());
        save_codez_config(&config).expect("config should be saved");

        let mut account = sample_account("profile-name");
        account.agent_name = Some("worker-one".to_string());
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");

        let launch = codex_launch_command_with_agent_mode(
            &account,
            &["resume".to_string()],
            true,
            &["aria".to_string(), "scGPT".to_string()],
        )
        .expect("launch should build");

        assert!(launch.envs.iter().any(|(key, value)| {
            key == CUTEX_AGENT_BUS_URL_ENV_VAR && value == "http://127.0.0.1:24261"
        }));
        // The shared root token is never exported to an agent launch.
        assert!(!launch
            .envs
            .iter()
            .any(|(key, _)| key == CUTEX_AGENT_BUS_TOKEN_ENV_VAR));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| { key == CUTEX_AGENT_NAME_ENV_VAR && value == "worker-one" }));
        assert!(launch.envs.iter().any(|(key, value)| {
            key == CUTEX_AGENT_ID_ENV_VAR && value.starts_with("cutex.worker-one.")
        }));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| { key == CUTEX_AGENT_HOST_ID_ENV_VAR && !value.is_empty() }));
        let groups = launch
            .envs
            .iter()
            .find_map(|(key, value)| (key == CUTEX_AGENT_GROUPS_ENV_VAR).then_some(value))
            .expect("agent groups should be exported");
        assert!(groups.contains("project:"));
        assert!(groups.contains("aria"));
        assert!(groups.contains("scgpt"));
        assert!(launch.envs.iter().any(|(key, value)| {
            key == CUTEX_AGENT_HINT_ENV_VAR && value.contains("cutex agent send")
        }));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn host_api_key_launch_exports_openai_api_key_from_profile_auth() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
        }

        let mut account = sample_account("api-key-host");
        account.source = Some("api-key".to_string());
        write_profile_files(
            &account,
            r#"{ "openai_api_key": "sk-host-test", "tokens": null }"#,
            Some(
                r#"
model_provider = "codexapis"

[model_providers.codexapis]
base_url = "https://www.codexapis.com/v1"
env_key = "OPENAI_API_KEY"
requires_openai_auth = false
"#,
            ),
        )
        .expect("profile files should be written");

        let launch = codex_launch_command(&account, &[]).expect("launch should build");

        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "OPENAI_API_KEY" && value == "sk-host-test"));
        // Native tools and shell snapshots never see the key.
        assert_eq!(
            launch.args[..2],
            cutex::launch::selected_profile::api_key_shell_env_blank_args()
        );
        let files = materialized_account_files(&account).expect("account files should resolve");
        let config =
            fs::read_to_string(&files.config_path).expect("profile config should be readable");
        let table = parse_toml_table(&config).expect("profile config should parse");
        let provider = table
            .get("model_providers")
            .and_then(|value| value.as_table())
            .and_then(|providers| providers.get("codexapis"))
            .and_then(|value| value.as_table())
            .expect("codexapis provider should exist");
        assert_eq!(
            provider.get("env_key").and_then(|value| value.as_str()),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            provider
                .get("requires_openai_auth")
                .and_then(|value| value.as_bool()),
            Some(false)
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn managed_session_wraps_default_host_launch() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        let old_cutex_alden_bin = std::env::var_os(CUTEX_ALDEN_BIN_ENV_VAR);
        let old_cute_alden_active = std::env::var_os("CUTE_ALDEN_SESSION_ACTIVE");
        let old_alden_active = std::env::var_os("ALDEN_SESSION_ACTIVE");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
            std::env::set_var(CUTEX_ALDEN_BIN_ENV_VAR, "/tmp/cute-alden");
            std::env::remove_var("CUTE_ALDEN_SESSION_ACTIVE");
            std::env::remove_var("ALDEN_SESSION_ACTIVE");
        }

        let account = sample_account("demo session");
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");
        let mut global = CodezConfig::default();
        global.session.enabled = true;
        save_codez_config(&global).expect("global config should be saved");

        let direct = codex_launch_command(&account, &[]).expect("launch should build");
        let wrapped = maybe_wrap_launch_with_session(&account, &[], LaunchOutput::Human, direct)
            .expect("session wrapping should work");
        let shell_command = wrapped.to_shell_command();

        assert_eq!(wrapped.program, "/tmp/cute-alden");
        assert!(shell_command.contains("'--name'"));
        assert!(shell_command.contains("'--'"));
        assert!(shell_command.contains("'/tmp/cute-codex'"));
        assert!(shell_command.contains("cutex.demo-session.host"));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        match old_cutex_alden_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_ALDEN_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_ALDEN_BIN_ENV_VAR) },
        }
        restore_env_var("CUTE_ALDEN_SESSION_ACTIVE", old_cute_alden_active);
        restore_env_var("ALDEN_SESSION_ACTIVE", old_alden_active);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_online_launch_wraps_resume_command_with_cute_alden() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_key = Uuid::new_v4().simple().to_string();
        let temp_home = std::env::temp_dir().join(format!("ch-{}", &temp_key[..8]));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        let old_cutex_alden_bin = std::env::var_os(CUTEX_ALDEN_BIN_ENV_VAR);
        let old_thread_id = std::env::var_os("CODEX_THREAD_ID");
        let old_agent_id = std::env::var_os(CUTEX_AGENT_ID_ENV_VAR);
        let old_agent_name = std::env::var_os(CUTEX_AGENT_NAME_ENV_VAR);
        let old_agent_groups = std::env::var_os(CUTEX_AGENT_GROUPS_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex").join("codex-home"))
            .expect("temp codex home should be created");
        fs::write(
            temp_home
                .join(".cutex")
                .join("codex-home")
                .join("session_index.jsonl"),
            "{\"id\":\"019e-alpha\",\"timestamp\":\"2026-06-25T00:00:00Z\"}\n",
        )
        .expect("session index should be written");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
            std::env::set_var(CUTEX_ALDEN_BIN_ENV_VAR, "/tmp/cute-alden");
            std::env::set_var("CODEX_THREAD_ID", "wrong-current-thread");
            std::env::set_var(CUTEX_AGENT_ID_ENV_VAR, "cutex.wrong.current");
            std::env::set_var(CUTEX_AGENT_NAME_ENV_VAR, "wrong-current-agent");
            std::env::set_var(CUTEX_AGENT_GROUPS_ENV_VAR, "wrong-current-group");
        }
        let mut config = CodezConfig::default();
        config.agent_bus_port = Some(24261);
        config.agent_bus_token = Some("agent-test-token".to_string());
        save_codez_config(&config).expect("config should be saved");

        let mut account = sample_account("orchid");
        account.source = Some("api-key".to_string());
        account.default_cli_args = vec!["--model".to_string(), "profile-cli-model".to_string()];
        write_profile_files(
            &account,
            "{\"OPENAI_API_KEY\":\"test-only\"}\n",
            Some(
                r#"
model = "deepseek-v4-flash"
model_provider = "deepseek"
model_reasoning_effort = "high"
model_catalog_json = "models.json"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
requires_openai_auth = false
"#,
            ),
        )
        .expect("profile files should be written");
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/scgpt".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.thread_name = Some("observer-smoke".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.permission_defaults = Some("full-access".to_string());
        record.model_defaults = Some("gpt-5.5".to_string());
        record.reasoning_defaults = Some("xhigh".to_string());
        record.default_cli_args = vec!["--no-alt-screen".to_string()];
        record.agent_groups = vec!["waveline".to_string()];

        let planned = management_lifecycle::session_online_launch_command(&record, &account)
            .expect("session online launch should build");
        let launch = planned.launch;
        let alden_name = planned
            .alden_session_name
            .expect("cute-alden backend should have a session name");
        let shell_command = launch.to_shell_command();

        assert_eq!(launch.program, "/tmp/cute-alden");
        assert_eq!(planned.backend, CutexSessionRuntimeBackend::CuteAlden);
        assert!(alden_name.contains("cutex.orchid.host.scgpt"));
        assert!(shell_command.contains("'--server-only'"));
        assert!(shell_command.contains("'--history-bytes'"));
        assert!(shell_command.contains("'262144'"));
        assert!(shell_command.contains("-u"));
        assert!(shell_command.contains("NO_COLOR"));
        assert!(shell_command.contains("'/tmp/cute-codex'"));
        assert!(shell_command.contains("'--model'"));
        assert!(shell_command.contains("'--sandbox'"));
        assert!(shell_command.contains("'danger-full-access'"));
        assert!(shell_command.contains("'--ask-for-approval'"));
        assert!(shell_command.contains("'never'"));
        assert!(shell_command.contains("'model_reasoning_effort=xhigh'"));
        assert!(shell_command.contains("'--no-alt-screen'"));
        assert!(shell_command.contains("'--cd'"));
        assert!(shell_command.contains("'/home/example/Projects/scgpt'"));
        assert!(shell_command.contains("'resume'"));
        assert!(shell_command.contains("'-c'"));
        assert!(shell_command.contains("'tui.resume_cwd=\"current\"'"));
        assert!(shell_command.contains("'019e-alpha'"));
        assert_eq!(
            launch.args.iter().filter(|arg| *arg == "--model").count(),
            1
        );
        assert!(launch
            .args
            .windows(2)
            .any(|args| args == ["--model", "gpt-5.5"]));
        assert!(!launch.args.iter().any(|arg| arg == "profile-cli-model"));
        assert_eq!(
            last_launch_env(&launch, CODEX_CONFIG_FILE_ENV_VAR),
            Some(
                materialized_account_files(&account)
                    .expect("profile paths")
                    .config_path
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(
            launch.args.iter().filter(|arg| *arg == "--sandbox").count(),
            1
        );
        assert_eq!(
            launch
                .args
                .iter()
                .filter(|arg| *arg == "--ask-for-approval")
                .count(),
            1
        );
        assert_eq!(launch.args.iter().filter(|arg| *arg == "--cd").count(), 1);
        assert_eq!(
            launch
                .args
                .windows(3)
                .filter(|args| {
                    args[0] == "resume"
                        && args[1] == "-c"
                        && args[2] == "tui.resume_cwd=\"current\""
                })
                .count(),
            1
        );
        let groups = launch
            .envs
            .iter()
            .find_map(|(key, value)| (key == CUTEX_AGENT_GROUPS_ENV_VAR).then_some(value))
            .expect("agent groups should be exported");
        assert!(groups.contains("project:"));
        assert!(groups.contains("waveline"));
        assert_eq!(
            last_launch_env(&launch, "CODEX_THREAD_ID"),
            Some("019e-alpha")
        );
        assert_eq!(
            last_launch_env(&launch, CUTEX_AGENT_NAME_ENV_VAR),
            Some("observer-smoke")
        );
        let agent_id = last_launch_env(&launch, CUTEX_AGENT_ID_ENV_VAR)
            .expect("session-specific agent id should be exported");
        assert!(
            agent_id.starts_with("cutex.observer-smoke.scgpt."),
            "unexpected session-specific agent id: {agent_id}"
        );
        assert_ne!(agent_id, "cutex.wrong.current");
        let effective_groups = last_launch_env(&launch, CUTEX_AGENT_GROUPS_ENV_VAR)
            .expect("session-specific groups should be exported");
        assert!(effective_groups.contains("waveline"));
        assert!(!effective_groups.contains("wrong-current-group"));
        assert_eq!(
            last_launch_env(&launch, CUTEX_AGENT_BUS_URL_ENV_VAR),
            Some("http://127.0.0.1:24261")
        );
        let credential = last_launch_env(&launch, CUTEX_AGENT_BUS_TOKEN_ENV_VAR)
            .expect("per-agent Agent Bus credential should be exported");
        assert!(credential.starts_with(cutex::agent_bus::agent_auth::AGENT_CREDENTIAL_PREFIX));
        assert!(!credential.contains("agent-test-token"));
        assert!(last_launch_env(&launch, CUTEX_AGENT_HOST_ID_ENV_VAR).is_some());
        assert!(last_launch_env(&launch, CUTEX_AGENT_HINT_ENV_VAR)
            .is_some_and(|hint| hint.contains("agent_list")));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == "CODEX_THREAD_ID"));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_BUS_URL_ENV_VAR));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_BUS_TOKEN_ENV_VAR));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_ID_ENV_VAR));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_NAME_ENV_VAR));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_GROUPS_ENV_VAR));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_HOST_ID_ENV_VAR));
        assert!(launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_HINT_ENV_VAR));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| { key == CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR && value == "1" }));
        assert!(launch.env_removes.iter().any(|key| key == "NO_COLOR"));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "COLORTERM" && value == "truecolor"));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "CLICOLOR" && value == "1"));

        let app_server_layout =
            cutex::app_server::runtime::AppServerRuntimeLayout::prepare(&record.cutex_session_id)
                .expect("app-server layout should build");
        let app_server_launch = management_lifecycle::app_server_launch_command(
            &record,
            &account,
            &record.agent_groups,
            &app_server_layout,
            "cutex.runtime.fixed",
        )
        .expect("app-server launch should build");
        assert_eq!(app_server_launch.program, "/tmp/cute-codex");
        assert!(app_server_launch.args.iter().any(|arg| arg == "app-server"));
        assert!(app_server_launch.args.iter().any(|arg| arg == "--listen"));
        assert!(!app_server_launch.args.iter().any(|arg| arg == "resume"));
        assert_eq!(
            last_launch_env(&app_server_launch, CUTEX_AGENT_ID_ENV_VAR),
            Some("cutex.runtime.fixed")
        );
        for key in [
            "CUTEX_OBSERVER_URL",
            "CUTEX_OBSERVER_TOKEN",
            CUTEX_RUNTIME_HEARTBEAT_URL_ENV_VAR,
            CUTEX_RUNTIME_HEARTBEAT_TOKEN_ENV_VAR,
        ] {
            assert!(app_server_launch.envs.iter().all(|(name, _)| name != key));
            assert!(app_server_launch.env_removes.iter().any(|name| name == key));
        }

        let remote_tui_launch =
            management_lifecycle::remote_tui_launch_command(&record, &account, &app_server_layout)
                .expect("remote TUI launch should build");
        assert!(remote_tui_launch.args.iter().any(|arg| arg == "--remote"));
        assert!(remote_tui_launch
            .args
            .windows(4)
            .any(|args| args == ["resume", "-c", "tui.resume_cwd=\"current\"", "019e-alpha"]));
        for key in [
            CUTEX_AGENT_BUS_URL_ENV_VAR,
            CUTEX_AGENT_BUS_TOKEN_ENV_VAR,
            CUTEX_AGENT_ID_ENV_VAR,
            CUTEX_AGENT_NAME_ENV_VAR,
            CUTEX_AGENT_GROUPS_ENV_VAR,
        ] {
            assert!(remote_tui_launch.envs.iter().all(|(name, _)| name != key));
            assert!(remote_tui_launch.env_removes.iter().any(|name| name == key));
        }

        let windows_runtime_dir = temp_home
            .join(".cutex")
            .join("runtime")
            .join("app-server")
            .join("windows-visible-test");
        fs::create_dir_all(&windows_runtime_dir).expect("loopback runtime dir should exist");
        let token_path = windows_runtime_dir.join("capability-token");
        fs::write(&token_path, "windows-test-token\n").expect("loopback token should be written");
        let loopback_binding = CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::LoopbackWebSocket,
            endpoint: "ws://127.0.0.1:32145".to_string(),
            pid: 4242,
            runtime_dir: windows_runtime_dir.display().to_string(),
            launched_profile: None,
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: Some(token_path.display().to_string()),
            diagnostic_journal_path: temp_home
                .join(".cutex")
                .join("runtime")
                .join("app-server-journal")
                .join("windows-visible-test.jsonl")
                .display()
                .to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "test".to_string(),
            started_at: "2026-07-13T00:00:00Z".to_string(),
        };
        let loopback_layout =
            cutex::app_server::runtime::AppServerRuntimeLayout::from_binding(&loopback_binding)
                .expect("loopback app-server layout should reconstruct");
        let mut native_record = record.clone();
        native_record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        native_record.alden_session_name = None;
        native_record.alden_pid = None;
        native_record.app_server_runtime = Some(loopback_binding);
        let windows_visible_tui = management_lifecycle::remote_tui_launch_command(
            &native_record,
            &account,
            &loopback_layout,
        )
        .expect("Windows visible remote TUI launch should build");
        assert_eq!(windows_visible_tui.program, "/tmp/cute-codex");
        assert!(windows_visible_tui
            .args
            .windows(2)
            .any(|args| args == ["--remote", "ws://127.0.0.1:32145"]));
        assert!(windows_visible_tui.args.windows(2).any(|args| args
            == [
                "--remote-auth-token-env",
                cutex::app_server::runtime::CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR,
            ]));
        assert_eq!(
            last_launch_env(
                &windows_visible_tui,
                cutex::app_server::runtime::CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR,
            ),
            Some("windows-test-token")
        );
        assert!(!windows_visible_tui
            .args
            .iter()
            .any(|arg| arg == "--server-only" || arg == "--allow-nesting"));
        assert_ne!(windows_visible_tui.program, "/tmp/cute-alden");
        loopback_layout
            .cleanup_files()
            .expect("loopback app-server layout should clean up");
        app_server_layout
            .cleanup_files()
            .expect("app-server layout should clean up");

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        match old_cutex_alden_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_ALDEN_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_ALDEN_BIN_ENV_VAR) },
        }
        restore_env_var("CODEX_THREAD_ID", old_thread_id);
        restore_env_var(CUTEX_AGENT_ID_ENV_VAR, old_agent_id);
        restore_env_var(CUTEX_AGENT_NAME_ENV_VAR, old_agent_name);
        restore_env_var(CUTEX_AGENT_GROUPS_ENV_VAR, old_agent_groups);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_online_launch_host_backend_does_not_require_cute_alden() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        let old_cutex_alden_bin = std::env::var_os(CUTEX_ALDEN_BIN_ENV_VAR);
        let old_thread_id = std::env::var_os("CODEX_THREAD_ID");
        let old_agent_id = std::env::var_os(CUTEX_AGENT_ID_ENV_VAR);
        let old_agent_name = std::env::var_os(CUTEX_AGENT_NAME_ENV_VAR);
        let old_agent_groups = std::env::var_os(CUTEX_AGENT_GROUPS_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex").join("codex-home"))
            .expect("temp codex home should be created");
        fs::write(
            temp_home
                .join(".cutex")
                .join("codex-home")
                .join("session_index.jsonl"),
            "{\"id\":\"019e-alpha\",\"timestamp\":\"2026-06-25T00:00:00Z\"}\n",
        )
        .expect("session index should be written");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
            std::env::remove_var(CUTEX_ALDEN_BIN_ENV_VAR);
            std::env::set_var("CODEX_THREAD_ID", "wrong-current-thread");
            std::env::set_var(CUTEX_AGENT_ID_ENV_VAR, "cutex.wrong.current");
            std::env::set_var(CUTEX_AGENT_NAME_ENV_VAR, "wrong-current-agent");
            std::env::set_var(CUTEX_AGENT_GROUPS_ENV_VAR, "wrong-current-group");
        }
        let mut config = CodezConfig::default();
        config.agent_bus_port = Some(24261);
        config.agent_bus_token = Some("agent-test-token".to_string());
        save_codez_config(&config).expect("config should be saved");

        let mut account = sample_account("deepseek");
        account.source = Some("api-key".to_string());
        write_profile_files(
            &account,
            "{\"OPENAI_API_KEY\":\"test-only\"}\n",
            Some(
                r#"
model = "deepseek-v4-flash"
model_provider = "deepseek"
model_reasoning_effort = "high"
model_catalog_json = "models.json"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
requires_openai_auth = false
"#,
            ),
        )
        .expect("profile files should be written");
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/scgpt".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.thread_name = Some("observer-smoke-host".to_string());
        record.managed_cwd = Some("/home/example/Projects/scgpt-managed".to_string());
        record.agent_groups = vec!["waveline".to_string()];

        let planned = management_lifecycle::session_online_launch_command(&record, &account)
            .expect("host backend launch should build without cute-alden");
        let shell_command = planned.launch.to_shell_command();

        assert_eq!(planned.backend, CutexSessionRuntimeBackend::Host);
        assert_eq!(planned.cwd, "/home/example/Projects/scgpt-managed");
        assert_eq!(record.cwd, "/home/example/Projects/scgpt");
        assert_eq!(planned.launch.program, "/tmp/cute-codex");
        assert!(planned.alden_session_name.is_none());
        assert!(!shell_command.contains("--server-only"));
        assert!(shell_command.contains("'resume'"));
        assert!(shell_command.contains("'019e-alpha'"));
        assert!(!planned.launch.args.iter().any(|arg| arg == "--model"));
        assert!(!planned
            .launch
            .args
            .iter()
            .any(|arg| arg.starts_with("model_reasoning_effort=")));
        assert_eq!(
            last_launch_env(&planned.launch, CODEX_CONFIG_FILE_ENV_VAR),
            Some(
                materialized_account_files(&account)
                    .expect("profile paths")
                    .config_path
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(planned
            .launch
            .envs
            .iter()
            .any(|(key, value)| { key == CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR && value == "1" }));
        assert!(planned
            .launch
            .env_removes
            .iter()
            .any(|key| key == "NO_COLOR"));
        assert!(planned
            .launch
            .envs
            .iter()
            .any(|(key, value)| key == "COLORTERM" && value == "truecolor"));
        assert!(planned
            .launch
            .envs
            .iter()
            .any(|(key, value)| key == "CLICOLOR" && value == "1"));
        assert_eq!(
            last_launch_env(&planned.launch, "CODEX_THREAD_ID"),
            Some("019e-alpha")
        );
        assert_eq!(
            last_launch_env(&planned.launch, CUTEX_AGENT_NAME_ENV_VAR),
            Some("observer-smoke-host")
        );
        let agent_id = last_launch_env(&planned.launch, CUTEX_AGENT_ID_ENV_VAR)
            .expect("session-specific agent id should be exported");
        assert!(
            agent_id.starts_with("cutex.observer-smoke-host.scgpt-managed."),
            "unexpected session-specific agent id: {agent_id}"
        );
        assert_ne!(agent_id, "cutex.wrong.current");
        let effective_groups = last_launch_env(&planned.launch, CUTEX_AGENT_GROUPS_ENV_VAR)
            .expect("session-specific groups should be exported");
        assert!(effective_groups.contains("waveline"));
        assert!(!effective_groups.contains("wrong-current-group"));
        assert_eq!(
            last_launch_env(&planned.launch, CUTEX_AGENT_BUS_URL_ENV_VAR),
            Some("http://127.0.0.1:24261")
        );
        let credential = last_launch_env(&planned.launch, CUTEX_AGENT_BUS_TOKEN_ENV_VAR)
            .expect("per-agent Agent Bus credential should be exported");
        assert!(credential.starts_with(cutex::agent_bus::agent_auth::AGENT_CREDENTIAL_PREFIX));
        assert!(!credential.contains("agent-test-token"));
        assert!(last_launch_env(&planned.launch, CUTEX_AGENT_HOST_ID_ENV_VAR).is_some());
        assert!(last_launch_env(&planned.launch, CUTEX_AGENT_HINT_ENV_VAR)
            .is_some_and(|hint| hint.contains("agent_list")));
        assert!(planned
            .launch
            .env_removes
            .iter()
            .any(|key| key == "CODEX_THREAD_ID"));
        assert!(planned
            .launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_BUS_URL_ENV_VAR));
        assert!(planned
            .launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_BUS_TOKEN_ENV_VAR));
        assert!(planned
            .launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_ID_ENV_VAR));
        assert!(planned
            .launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_HOST_ID_ENV_VAR));
        assert!(planned
            .launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_AGENT_HINT_ENV_VAR));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        match old_cutex_alden_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_ALDEN_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_ALDEN_BIN_ENV_VAR) },
        }
        restore_env_var("CODEX_THREAD_ID", old_thread_id);
        restore_env_var(CUTEX_AGENT_ID_ENV_VAR, old_agent_id);
        restore_env_var(CUTEX_AGENT_NAME_ENV_VAR, old_agent_name);
        restore_env_var(CUTEX_AGENT_GROUPS_ENV_VAR, old_agent_groups);
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_online_runtime_observation_persists_live_agent_binding() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.exposed_to_backend = true;
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        let mut agent = sample_bus_agent(
            "cutex.orchid.session-online-smoke.runtime1",
            "session-online-smoke.abcdef0",
            Some("observer-smoke"),
            Some("abcdef0"),
        );
        agent.session_id = Some("019e-alpha".to_string());

        let outcome = apply_session_online_runtime_observation(
            &mut store,
            "cutex.019e-alpha",
            Some(&agent),
            Some("cutex.orchid.host.cutex.cutex.019e-alpha"),
            CutexSessionRuntimeBackend::CuteAlden,
            4242,
            "devbox",
            "2026-06-25T00:01:00Z",
        )
        .expect("runtime observation should apply")
        .expect("live agent should reconcile");

        assert!(outcome
            .events
            .iter()
            .any(|event| event.event_type == "runtime_endpoint_registered"));
        let record = store
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should remain");
        assert!(matches!(
            record.runtime_backend,
            CutexSessionRuntimeBackend::CuteAlden
        ));
        assert_eq!(record.alden_pid, Some(4242));
        assert_eq!(
            record.alden_session_name.as_deref(),
            Some("cutex.orchid.host.cutex.cutex.019e-alpha")
        );
        assert_eq!(
            record.current_runtime_agent_id.as_deref(),
            Some("cutex.orchid.session-online-smoke.runtime1")
        );
        assert_eq!(record.runtime_generation, 1);
        assert_eq!(record.last_seen_at.as_deref(), Some("2026-06-25T00:01:00Z"));
    }

    #[test]
    fn session_stop_clears_stale_runtime_binding_without_process() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut entry = sample_im_registration("019e-alpha");
        let local_host = current_host_name();
        entry.host_id = local_host.clone();
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            local_host,
            "/home/example/Projects/scgpt".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.alden_pid = Some(u32::MAX);
        record.current_runtime_agent_id = Some("cutex.orchid.scgpt.dead".to_string());
        record.last_runtime_agent_id = record.current_runtime_agent_id.clone();
        record.exposed_to_backend = true;
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        save_cutex_session_store(&store).expect("store should save");

        let result = management_lifecycle::stop_cutex_session_runtime_for_entry(&entry, &[], false)
            .expect("stale runtime should clear");

        assert!(!result.had_runtime);
        assert!(result.stopped);
        assert_eq!(result.detail, "already_offline");
        let loaded = load_cutex_session_store().expect("store should load");
        let record = loaded
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should remain");
        assert_eq!(
            record.runtime_backend,
            CutexSessionRuntimeBackend::CuteAlden
        );
        assert!(record.alden_pid.is_none());
        assert!(record.current_runtime_agent_id.is_none());

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_force_stop_clears_failed_start_claim_and_preserves_legacy_launch_id() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let entry = sample_im_registration("019e-alpha");
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            current_host_name(),
            "/home/example/Projects/scgpt".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.pending_launch_id = Some("legacy-heartbeat-launch".to_string());
        record.app_server_launch_claim_id = Some("failed-app-server-launch".to_string());
        record.current_runtime_agent_id = Some("cutex.orchid.scgpt.failed".to_string());
        record.runtime_generation = 3;
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        save_cutex_session_store(&store).expect("store should save");

        let result = management_lifecycle::stop_cutex_session_runtime_for_entry(&entry, &[], true)
            .expect("force stop should recover a process-free failed start");

        assert!(result.had_runtime);
        assert!(result.stopped);
        assert!(!result.forced);
        assert_eq!(result.detail, "stale_runtime_claim_cleared");
        let loaded = load_cutex_session_store().expect("store should load");
        let record = loaded
            .sessions
            .get("cutex.019e-alpha")
            .expect("record should remain");
        assert_eq!(
            record.pending_launch_id.as_deref(),
            Some("legacy-heartbeat-launch")
        );
        assert!(record.app_server_launch_claim_id.is_none());
        assert!(record.current_runtime_agent_id.is_none());
        assert!(record.app_server_runtime.is_none());

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_stop_refuses_remote_host_runtime() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_hostname = std::env::var_os("HOSTNAME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var("HOSTNAME", "devbox");
        }

        let mut entry = sample_im_registration("019e-alpha");
        entry.host_id = "winbox".to_string();
        let mut record = CutexSessionRecord::new_at(
            "cutex.019e-alpha".to_string(),
            Some("019e-alpha".to_string()),
            "winbox".to_string(),
            "E:\\Projects (Orchid)\\scgpt".to_string(),
            Some("orchid".to_string()),
            "2026-06-25T00:00:00Z".to_string(),
        )
        .expect("record should be created");
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        save_cutex_session_store(&store).expect("store should save");

        let err = management_lifecycle::stop_cutex_session_runtime_for_entry(&entry, &[], false)
            .expect_err("remote host runtime should require a remote manager");
        assert!(format!("{err:#}").contains("remote_runtime_manager_required"));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_hostname {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn session_stop_refuses_live_agent_without_cutex_session_record() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let entry = sample_im_registration("019e-alpha");
        let mut agent = sample_bus_agent(
            "cutex.orchid.scgpt.live",
            "aria-data.abcdef0",
            Some("aria-data"),
            Some("abcdef0"),
        );
        agent.session_id = Some("019e-alpha".to_string());
        agent.host_id = Some(current_host_name());

        let err =
            management_lifecycle::stop_cutex_session_runtime_for_entry(&entry, &[agent], false)
                .expect_err("live agent without durable session should be rejected");

        assert!(format!("{err:#}").contains("cutex session record missing"));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn managed_session_skips_launch_when_profile_disables_it() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        let old_cutex_alden_bin = std::env::var_os(CUTEX_ALDEN_BIN_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
            std::env::set_var(CUTEX_ALDEN_BIN_ENV_VAR, "/tmp/cute-alden");
        }

        let mut account = sample_account("no-session");
        account.session = Some(SessionConfig { enabled: false });
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");

        let direct = codex_launch_command(&account, &[]).expect("launch should build");
        let wrapped =
            maybe_wrap_launch_with_session(&account, &[], LaunchOutput::Human, direct.clone())
                .expect("launch should build");

        assert_eq!(wrapped.program, direct.program);
        assert_eq!(wrapped.args, direct.args);

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        match old_cutex_alden_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_ALDEN_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_ALDEN_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn launch_shell_command_serializes_profile_and_runtime_envs() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
        }

        let mut account = sample_account("test-profile");
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");
        account.plan_type = Some("plus".to_string());
        account.email = Some("test-profile@example.test".to_string());

        let launch =
            codex_launch_command(&account, &["resume".to_string()]).expect("launch should build");
        let shell_command = launch.to_shell_command();

        assert!(shell_command.contains("CODEX_LAUNCH_PROFILE='test-profile'"));
        assert!(shell_command.contains("CODEX_LAUNCH_RUNTIME='host'"));
        assert!(shell_command.contains("CODEX_LAUNCH_PROFILE_SOURCE='official'"));
        assert!(shell_command.contains("CODEX_LAUNCH_PROFILE_TYPE='plus'"));
        assert!(shell_command.contains("CODEX_LAUNCH_PROFILE_EMAIL='test-profile@example.test'"));
        assert!(shell_command.contains("'/tmp/cute-codex' 'resume'"));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn host_launch_command_includes_http_proxy_envs() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
        }
        let mut config = CodezConfig::default();
        config.proxy = Some(
            proxy_config_from_parts(
                true,
                Some("http://127.0.0.1:7890".to_string()),
                Some("localhost,127.0.0.1".to_string()),
                true,
            )
            .expect("proxy config should be valid"),
        );
        save_codez_config(&config).expect("config should be saved");

        let account = sample_account("proxy-http");
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");
        let launch =
            codex_launch_command(&account, &["resume".to_string()]).expect("launch should build");

        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "HTTP_PROXY" && value == "http://127.0.0.1:7890"));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "ALL_PROXY" && value == "http://127.0.0.1:7890"));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "NO_PROXY" && value == "localhost,127.0.0.1"));
        assert!(launch.envs.iter().any(|(key, value)| {
            key == CUTE_CODEX_FORCE_HTTP_TRANSPORT_ENV_VAR && value == "1"
        }));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn host_launch_command_sets_http_and_all_proxy_for_socks_proxy() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        let old_cutex_codex_bin = std::env::var_os(CUTEX_CODEX_BIN_ENV_VAR);
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
            std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, "/tmp/cute-codex");
        }
        let mut config = CodezConfig::default();
        config.proxy = Some(
            proxy_config_from_parts(
                true,
                Some("socks5h://127.0.0.1:7890".to_string()),
                None,
                true,
            )
            .expect("proxy config should be valid"),
        );
        save_codez_config(&config).expect("config should be saved");

        let account = sample_account("proxy-socks");
        write_profile_files(&account, "{\"demo\":true}\n", None)
            .expect("profile files should be written");
        let launch =
            codex_launch_command(&account, &["resume".to_string()]).expect("launch should build");

        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "ALL_PROXY" && value == "socks5h://127.0.0.1:7890"));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "HTTP_PROXY" && value == "socks5h://127.0.0.1:7890"));
        assert!(launch
            .envs
            .iter()
            .any(|(key, value)| key == "HTTPS_PROXY" && value == "socks5h://127.0.0.1:7890"));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_cutex_codex_bin {
            Some(value) => unsafe { std::env::set_var(CUTEX_CODEX_BIN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(CUTEX_CODEX_BIN_ENV_VAR) },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn docker_runtime_rewrites_loopback_proxy_to_host_alias() {
        let proxy = proxy_config_from_parts(
            true,
            Some("socks5h://127.0.0.1:7891".to_string()),
            Some("localhost,127.0.0.1,::1".to_string()),
            true,
        )
        .expect("proxy config should be valid");
        let envs = proxy_envs(
            Some(&proxy),
            Some(&RuntimeConfig::Docker {
                image: "cutex-dev-v2".to_string(),
                user_name: Some("cutex".to_string()),
            }),
        );

        assert!(envs.iter().any(|(key, value)| {
            key == "ALL_PROXY" && value.starts_with("socks5h://host.docker.internal:7891")
        }));
        assert!(envs.iter().any(|(key, value)| {
            key == "HTTP_PROXY" && value.starts_with("socks5h://host.docker.internal:7891")
        }));
        assert!(envs.iter().any(|(key, value)| {
            key == "HTTPS_PROXY" && value.starts_with("socks5h://host.docker.internal:7891")
        }));
    }

    #[test]
    fn account_proxy_scope_label_reports_profile_vs_global_state() {
        let global = CodezConfig {
            proxy: Some(
                proxy_config_from_parts(
                    true,
                    Some("socks5h://127.0.0.1:7891".to_string()),
                    None,
                    true,
                )
                .expect("global proxy should be valid"),
            ),
            ..CodezConfig::default()
        };

        let mut account = sample_account("scope");
        assert_eq!(account_proxy_scope_label(&account, &global), "on(global)");

        account.proxy = Some(
            proxy_config_from_parts(false, None, None, /*force_http_transport*/ true)
                .expect("disabled proxy should be valid"),
        );
        assert_eq!(account_proxy_scope_label(&account, &global), "off(profile)");

        account.proxy = Some(
            proxy_config_from_parts(true, Some("http://127.0.0.1:8080".to_string()), None, true)
                .expect("profile proxy should be valid"),
        );
        assert_eq!(account_proxy_scope_label(&account, &global), "on(profile)");
    }

    #[test]
    fn account_model_provider_reads_model_provider_from_profile_config() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let account = sample_account("provider");
        write_profile_files(
            &account,
            "{\"demo\":true}\n",
            Some(
                r#"
model_provider = "custom"

[model_providers.custom]
base_url = "https://example.test/v1"
"#,
            ),
        )
        .expect("profile files should be written");

        assert_eq!(account_model_provider(&account).as_deref(), Some("custom"));
        assert_eq!(
            account_model_api_base(&account).as_deref(),
            Some("https://example.test/v1")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn api_key_config_toml_includes_named_model_provider() {
        let config = super::super::auth::codex_api_key_config_toml(
            "custom",
            Some("https://api.example.test/v1"),
        );
        let table = parse_toml_table(&config).expect("config should parse");
        assert_eq!(
            table.get("model_provider").and_then(|value| value.as_str()),
            Some("custom")
        );
        let provider = table
            .get("model_providers")
            .and_then(|value| value.as_table())
            .and_then(|providers| providers.get("custom"))
            .and_then(|value| value.as_table())
            .expect("custom provider should exist");
        assert_eq!(
            provider.get("name").and_then(|value| value.as_str()),
            Some("custom")
        );
        assert_eq!(
            provider.get("base_url").and_then(|value| value.as_str()),
            Some("https://api.example.test/v1")
        );
        assert_eq!(
            provider.get("env_key").and_then(|value| value.as_str()),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            provider
                .get("requires_openai_auth")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(
            provider.get("wire_api").and_then(|value| value.as_str()),
            Some("responses")
        );
    }

    #[test]
    fn deepseek_api_key_config_uses_the_supported_profile_preset() {
        let config = super::super::auth::codex_api_key_config_toml("deepseek", None);
        let table = parse_toml_table(&config).expect("config should parse");

        assert_eq!(
            table.get("model").and_then(toml::Value::as_str),
            Some("deepseek-flash")
        );
        assert_eq!(
            table
                .get("model_reasoning_effort")
                .and_then(toml::Value::as_str),
            Some("high")
        );
        assert_eq!(
            table
                .get("model_catalog_json")
                .and_then(toml::Value::as_str),
            Some("models.json")
        );
        assert_eq!(
            table
                .get("forced_login_method")
                .and_then(toml::Value::as_str),
            Some("api")
        );
        assert!(!table.contains_key("preferred_auth_method"));
        let provider = table
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("deepseek"))
            .and_then(toml::Value::as_table)
            .expect("DeepSeek provider should exist");
        assert_eq!(
            provider.get("base_url").and_then(toml::Value::as_str),
            Some("https://api.deepseek.com/")
        );
        assert_eq!(
            provider.get("env_key").and_then(toml::Value::as_str),
            Some("OPENAI_API_KEY")
        );
        assert!(!provider.contains_key("experimental_bearer_token"));
        assert!(!provider.contains_key("request_max_retries"));
        assert!(!provider.contains_key("stream_max_retries"));
    }

    #[test]
    fn account_model_provider_falls_back_to_openai_for_official_auth() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let chatgpt = sample_account("openai-chatgpt-fallback");
        write_profile_files(
            &chatgpt,
            r#"{
  "openai_api_key": null,
  "tokens": { "id_token": "x", "access_token": "y", "refresh_token": "z" }
}"#,
            None,
        )
        .expect("profile files should be written");
        assert_eq!(account_model_provider(&chatgpt).as_deref(), Some("openai"));
        assert_eq!(
            account_model_api_base(&chatgpt).as_deref(),
            Some("https://chatgpt.com/backend-api/codex")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn official_openai_base_uses_chatgpt_even_if_materialized_auth_is_stale_api_key() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let account = sample_account("official-stale-auth");
        write_profile_files(
            &account,
            r#"{ "openai_api_key": "sk-stale", "tokens": null }"#,
            Some("model_provider = \"openai\"\n"),
        )
        .expect("profile files should be written");

        assert_eq!(account_model_provider(&account).as_deref(), Some("openai"));
        assert_eq!(
            account_model_api_base(&account).as_deref(),
            Some("https://chatgpt.com/backend-api/codex")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn account_model_api_base_falls_back_for_openai_by_auth_mode() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let chatgpt = sample_account("openai-chatgpt");
        write_profile_files(
            &chatgpt,
            r#"{
  "openai_api_key": null,
  "tokens": { "id_token": "x", "access_token": "y", "refresh_token": "z" }
}"#,
            Some("model_provider = \"openai\"\n"),
        )
        .expect("profile files should be written");
        assert_eq!(
            account_model_api_base(&chatgpt).as_deref(),
            Some("https://chatgpt.com/backend-api/codex")
        );

        let mut api_key = sample_account("openai-api-key");
        api_key.source = Some("api-key".to_string());
        write_profile_files(
            &api_key,
            r#"{ "openai_api_key": "sk-test", "tokens": null }"#,
            Some("model_provider = \"openai\"\n"),
        )
        .expect("profile files should be written");
        assert_eq!(
            account_model_api_base(&api_key).as_deref(),
            Some("https://api.openai.com/v1")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn account_model_api_base_reads_oss_defaults() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("codez-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(&temp_home).expect("temp home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let ollama = sample_account("ollama");
        write_profile_files(
            &ollama,
            "{\"demo\":true}\n",
            Some("model_provider = \"ollama\"\n"),
        )
        .expect("profile files should be written");
        assert_eq!(
            account_model_api_base(&ollama).as_deref(),
            Some("http://localhost:11434/v1")
        );

        let lmstudio = sample_account("lmstudio");
        write_profile_files(
            &lmstudio,
            "{\"demo\":true}\n",
            Some("model_provider = \"lmstudio\"\n"),
        )
        .expect("profile files should be written");
        assert_eq!(
            account_model_api_base(&lmstudio).as_deref(),
            Some("http://localhost:1234/v1")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn load_store_migrates_v2_store_and_materializes_profile_files() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let accounts_path = temp_home.join(".cutex").join("accounts.json");
        let legacy = r#"
{
  "version": 2,
  "accounts": [
    {
      "id": "legacy-id",
      "name": "legacy",
      "email": "legacy@example.test",
      "plan_type": "plus",
      "source": null,
      "raw_auth_json": "{\"openai_api_key\":\"sk-test\",\"tokens\":null}",
      "raw_config_toml": "model_provider = \"openai\"\n",
      "runtime": { "kind": "host" },
      "proxy": null,
      "last_used_at": null
    }
  ],
  "active_account_id": "legacy-id"
}
"#;
        fs::write(&accounts_path, legacy).expect("legacy accounts.json should be written");

        let store = load_store().expect("store should migrate");
        assert_eq!(store.version, STORE_VERSION);
        assert_eq!(store.accounts.len(), 1);
        assert_eq!(store.accounts[0].name, "legacy");
        assert_eq!(
            temp_home
                .join(".cutex")
                .join("accounts.v2.backup.json")
                .exists(),
            true
        );

        let files = materialized_account_files(&store.accounts[0]).expect("paths should resolve");
        let auth = fs::read_to_string(&files.auth_path).expect("auth should be materialized");
        let config = fs::read_to_string(&files.config_path).expect("config should be materialized");
        let auth_json: serde_json::Value =
            serde_json::from_str(&auth).expect("auth should parse as JSON");
        assert!(
            auth_json.get("OPENAI_API_KEY").is_some() || auth_json.get("openai_api_key").is_some()
        );
        assert!(config.contains("model_provider = \"openai\""));

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn profile_pin_top_and_bottom_reorders_accounts() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut store = AccountsStore::default();
        let alpha = sample_account("alpha");
        let beta = sample_account("beta");
        let gamma = sample_account("gamma");
        store.accounts = vec![alpha.clone(), beta.clone(), gamma.clone()];
        store.active_account_id = Some(beta.id.clone());
        save_store(&store).expect("store should save");

        cmd_profile_pin("gamma", true).expect("pin top should succeed");
        let reloaded = load_store().expect("store should reload");
        assert_eq!(
            reloaded
                .accounts
                .iter()
                .map(|account| account.name.clone())
                .collect::<Vec<_>>(),
            vec!["gamma".to_string(), "alpha".to_string(), "beta".to_string()]
        );

        cmd_profile_pin("gamma", false).expect("pin bottom should succeed");
        let reloaded = load_store().expect("store should reload");
        assert_eq!(
            reloaded
                .accounts
                .iter()
                .map(|account| account.name.clone())
                .collect::<Vec<_>>(),
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn profile_clone_status_line_copies_active_profile_to_all_profiles() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let alpha = sample_account("alpha");
        let beta = sample_account("beta");
        let mut store = AccountsStore::default();
        store.accounts = vec![alpha.clone(), beta.clone()];
        store.active_account_id = Some(alpha.id.clone());
        save_store(&store).expect("store should save");

        write_profile_files(
            &alpha,
            "{\"demo\":true}\n",
            Some(
                r#"
model_provider = "openai"

[tui]
status_line = ["launch-profile", "model-name", "current-dir"]
"#,
            ),
        )
        .expect("alpha files should be written");
        write_profile_files(
            &beta,
            "{\"demo\":true}\n",
            Some(
                r#"
model_provider = "openai"

[tui]
status_line = ["current-dir"]
"#,
            ),
        )
        .expect("beta files should be written");

        cmd_profile_clone_status_line(None).expect("clone should succeed");

        let beta_files = materialized_account_files(&beta).expect("beta paths should resolve");
        let beta_config =
            fs::read_to_string(&beta_files.config_path).expect("beta config should be readable");
        let beta_table = parse_toml_table(&beta_config).expect("beta config should parse");
        let status_line = beta_table
            .get("tui")
            .and_then(|value| value.as_table())
            .and_then(|tui| tui.get("status_line"))
            .and_then(|value| value.as_array())
            .expect("beta status_line should exist")
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(
            status_line,
            vec![
                "launch-profile".to_string(),
                "model-name".to_string(),
                "current-dir".to_string()
            ]
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn profile_copy_duplicates_profile_metadata_and_files() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let mut source = sample_account("source");
        source.email = Some("source@example.test".to_string());
        source.plan_type = Some("pro".to_string());
        source.runtime = RuntimeConfig::Docker {
            image: "cutex-dev-v2".to_string(),
            user_name: Some("devuser".to_string()),
        };
        source.proxy = Some(
            proxy_config_from_parts(
                true,
                Some("socks5h://127.0.0.1:7891".to_string()),
                Some("localhost,127.0.0.1,::1".to_string()),
                true,
            )
            .expect("proxy config should be valid"),
        );

        let mut store = AccountsStore::default();
        store.accounts.push(source.clone());
        store.active_account_id = Some(source.id.clone());
        save_store(&store).expect("store should save");

        let source_config = r#"
model_provider = "custom"
model_catalog_json = "models.json"

[tui]
status_line = ["launch-profile", "current-dir"]

[model_providers.custom]
base_url = "https://old.example/v1"
"#;
        write_profile_files(&source, "{\"demo\":true}\n", Some(source_config))
            .expect("source files should be written");
        let source_files =
            materialized_account_files(&source).expect("source files should resolve");
        fs::write(&source_files.model_catalog_path, r#"{"models":[]}"#)
            .expect("source model catalog should be written");

        cmd_profile_copy("source", "copied", None, None).expect("copy should succeed");

        let reloaded = load_store().expect("store should reload");
        assert_eq!(reloaded.accounts.len(), 2);
        assert_eq!(reloaded.accounts[1].name, "copied");
        assert_eq!(
            reloaded.accounts[1].email.as_deref(),
            Some("source@example.test")
        );
        assert_eq!(reloaded.accounts[1].plan_type.as_deref(), Some("pro"));
        assert_eq!(reloaded.accounts[1].runtime, source.runtime);
        assert_eq!(reloaded.accounts[1].proxy, source.proxy);

        let copied_files =
            materialized_account_files(&reloaded.accounts[1]).expect("copied files should resolve");
        assert_eq!(
            fs::read_to_string(&copied_files.auth_path).expect("copied auth should be readable"),
            fs::read_to_string(&source_files.auth_path).expect("source auth should be readable")
        );
        assert_eq!(
            fs::read_to_string(&copied_files.config_path)
                .expect("copied config should be readable"),
            fs::read_to_string(&source_files.config_path)
                .expect("source config should be readable")
        );
        assert_eq!(
            fs::read_to_string(&copied_files.model_catalog_path)
                .expect("copied model catalog should be readable"),
            fs::read_to_string(&source_files.model_catalog_path)
                .expect("source model catalog should be readable")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn profile_copy_can_override_provider_base_url_for_same_provider() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        fs::create_dir_all(temp_home.join(".cutex")).expect("temp cutex home should be created");
        unsafe {
            std::env::set_var("HOME", &temp_home);
        }

        let source = sample_account("source");
        let mut store = AccountsStore::default();
        store.accounts.push(source.clone());
        save_store(&store).expect("store should save");

        let source_config = r#"
model_provider = "custom"

[model_providers.custom]
base_url = "https://old.example/v1"
"#;
        write_profile_files(&source, "{\"demo\":true}\n", Some(source_config))
            .expect("source files should be written");

        cmd_profile_copy(
            "source",
            "copied",
            None,
            Some("https://new.example/v1".to_string()),
        )
        .expect("copy should succeed");

        let reloaded = load_store().expect("store should reload");
        let copied = reloaded
            .accounts
            .iter()
            .find(|account| account.name == "copied")
            .expect("copied profile should exist");
        let copied_files = materialized_account_files(copied).expect("copied files should resolve");
        let copied_config = fs::read_to_string(&copied_files.config_path)
            .expect("copied config should be readable");
        let copied_table = parse_toml_table(&copied_config).expect("copied config should parse");
        assert_eq!(
            copied_table
                .get("model_provider")
                .and_then(|value| value.as_str()),
            Some("custom")
        );
        assert_eq!(
            copied_table
                .get("model_providers")
                .and_then(|value| value.as_table())
                .and_then(|providers| providers.get("custom"))
                .and_then(|value| value.as_table())
                .and_then(|provider| provider.get("name"))
                .and_then(|value| value.as_str()),
            Some("custom")
        );
        assert_eq!(
            copied_table
                .get("model_providers")
                .and_then(|value| value.as_table())
                .and_then(|providers| providers.get("custom"))
                .and_then(|value| value.as_table())
                .and_then(|provider| provider.get("base_url"))
                .and_then(|value| value.as_str()),
            Some("https://new.example/v1")
        );

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(temp_home);
    }

    #[test]
    fn apply_annotation_updates_and_clears_display_fields() {
        let mut account = sample_account("annotated");
        account.plan_type = Some("unknown".to_string());
        account.email = Some("-".to_string());

        apply_annotation(
            &mut account,
            Some("api".to_string()),
            false,
            Some("target.example".to_string()),
            false,
            Some("portal".to_string()),
            false,
        );

        assert_eq!(account.source.as_deref(), Some("api"));
        assert_eq!(account.plan_type.as_deref(), Some("target.example"));
        assert_eq!(account.email.as_deref(), Some("portal"));

        apply_annotation(&mut account, None, true, None, true, None, true);

        assert!(account.source.is_none());
        assert!(account.plan_type.is_none());
        assert!(account.email.is_none());
    }
}
