use anyhow::{anyhow, Context};

use cutex::agent_bus::model::AgentGroupUpdateMode;
use cutex::cli::args::{SessionGroupsCommand, SessionProfileCommand, SessionQuickCommand};
use cutex::platform::host::current_host_name;
use cutex::session::model::CutexSessionQuickActionMode;
use cutex::session::projection::runtime_backend_short_label;
use cutex::session::service::{
    adopt_cutex_session, cutex_session_key_for_user_id, expose_cutex_session, hide_cutex_session,
    normalize_cutex_session_managed_cwd_path, persist_cutex_session_store_and_im_record,
    set_cutex_session_profile_by_key, set_cutex_session_quick_action, unmanage_cutex_session,
    update_cutex_session_groups, CutexSessionAdoptOptions, CutexSessionEnsureSeed,
};
use cutex::session::store::load_cutex_session_store;
use cutex::ui::format::{bool_label, compact_home_path};

use super::agent_bus_runtime;
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

pub(crate) fn cmd_session_show(id: &str) -> anyhow::Result<()> {
    super::session_reconcile::mirror_im_registry_into_cutex_session_store(
        &cutex::im::registry::load_im_registry()?,
    )?;
    let store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, id)
        .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
    let record = store
        .sessions
        .get(&key)
        .ok_or_else(|| anyhow!("cutex session disappeared while showing: {key}"))?;
    println!("{}", serde_json::to_string_pretty(record)?);
    Ok(())
}

pub(crate) fn cmd_session_adopt(
    id: &str,
    name: Option<String>,
    cwd: Option<String>,
    current_cwd: bool,
    groups: Vec<String>,
    expose_to_im: bool,
    pin: bool,
) -> anyhow::Result<()> {
    if current_cwd && cwd.is_some() {
        anyhow::bail!("Use either --cwd or --current-cwd, not both");
    }
    let managed_cwd = if current_cwd {
        Some(
            std::env::current_dir()
                .context("Failed to determine current directory")?
                .display()
                .to_string(),
        )
    } else {
        cwd.map(|path| normalize_cutex_session_managed_cwd_path(&path))
            .transpose()?
    };
    if cutex::launch::local_deployment::LocalDeployment::selected()?.is_some() {
        let current = load_cutex_session_store()?;
        let key = cutex_session_key_for_user_id(&current, id);
        let existing = key.as_ref().and_then(|key| current.sessions.get(key));
        if existing.is_none_or(|record| !cutex::session::metadata::cutex_session_is_managed(record))
        {
            let native = existing
                .and_then(|record| record.codex_session_id.as_deref())
                .unwrap_or(id);
            let formal = name
                .clone()
                .or_else(|| existing.map(cutex::session::metadata::cutex_session_display_name))
                .unwrap_or_else(|| native.to_string());
            let source_cwd = existing
                .map(|record| record.cwd.clone())
                .unwrap_or_else(|| cutex_session_ensure_seed().cwd);
            super::light_new::adopt_saved(native, &formal, &source_cwd)?;
        }
    }
    let mut store = load_cutex_session_store()?;
    let outcome = adopt_cutex_session(
        &mut store,
        id,
        cutex_session_ensure_seed(),
        CutexSessionAdoptOptions {
            display_name: name.as_deref(),
            managed_cwd,
            groups,
            expose_to_im,
            pin,
        },
    )?;
    persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
    let _ = agent_bus_runtime::maybe_patch_live_agent_groups(
        &outcome.session_id,
        &outcome.groups,
        AgentGroupUpdateMode::Set,
    );
    println!(
        "{GREEN}Adopted{RESET} {BOLD}{}{RESET} as a managed cutex session",
        outcome.display_name
    );
    println!(
        "  {DIM}session{RESET} {}  {DIM}backend{RESET} {}  {DIM}im{RESET} {}",
        outcome.session_id,
        runtime_backend_short_label(outcome.runtime_backend),
        bool_label(outcome.im_visible)
    );
    println!(
        "  {DIM}launch cwd{RESET} {}",
        compact_home_path(&outcome.launch_cwd)
    );
    Ok(())
}

fn cutex_session_ensure_seed() -> CutexSessionEnsureSeed {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    CutexSessionEnsureSeed {
        host_id: current_host_name(),
        cwd,
        profile: None,
    }
}

pub(crate) fn cmd_session_expose(
    id: &str,
    name: Option<&str>,
    groups: Vec<String>,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let outcome = expose_cutex_session(&mut store, id, cutex_session_ensure_seed(), name, groups)?;
    persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
    let _ = agent_bus_runtime::maybe_patch_live_agent_groups(
        &outcome.session_id,
        &outcome.groups,
        AgentGroupUpdateMode::Set,
    );
    println!(
        "{GREEN}Exposed{RESET} cutex session {BOLD}{}{RESET}",
        outcome.session_id
    );
    Ok(())
}

pub(crate) fn cmd_session_hide(id: &str) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let outcome = hide_cutex_session(&mut store, id)?;
    persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
    println!(
        "{YELLOW}Hid{RESET} cutex session {BOLD}{}{RESET}",
        outcome.session_id
    );
    Ok(())
}

pub(crate) fn cmd_session_unmanage(id: &str) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let outcome = unmanage_cutex_session(&mut store, id)?;
    persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
    println!(
        "{YELLOW}Unmanaged{RESET} cutex session {BOLD}{}{RESET}; history and any running runtime were left untouched",
        outcome.session_id
    );
    Ok(())
}

pub(crate) fn cmd_session_quick(command: SessionQuickCommand) -> anyhow::Result<()> {
    let (id, mode) = match command {
        SessionQuickCommand::Pin { id } => (id, CutexSessionQuickActionMode::Pinned),
        SessionQuickCommand::Hide { id } => (id, CutexSessionQuickActionMode::Hidden),
        SessionQuickCommand::Auto { id } => (id, CutexSessionQuickActionMode::Auto),
    };
    cmd_session_quick_set(&id, mode)
}

pub(crate) fn cmd_session_quick_set(
    id: &str,
    mode: CutexSessionQuickActionMode,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let outcome = set_cutex_session_quick_action(&mut store, id, mode)?;
    persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
    println!(
        "{GREEN}Updated{RESET} quick action for {BOLD}{session_id}{RESET}: {}",
        mode.label(),
        session_id = outcome.session_id
    );
    Ok(())
}

pub(crate) fn cmd_session_groups(command: SessionGroupsCommand) -> anyhow::Result<()> {
    let (id, groups, mode) = match command {
        SessionGroupsCommand::Set { id, groups } => (id, groups, AgentGroupUpdateMode::Set),
        SessionGroupsCommand::Add { id, groups } => (id, groups, AgentGroupUpdateMode::Add),
        SessionGroupsCommand::Remove { id, groups } => (id, groups, AgentGroupUpdateMode::Remove),
    };
    let mut store = load_cutex_session_store()?;
    let outcome = update_cutex_session_groups(&mut store, &id, groups, mode)?;
    let groups_label = outcome.groups.join(",");
    persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
    let _ = agent_bus_runtime::maybe_patch_live_agent_groups(
        &outcome.session_id,
        &outcome.groups,
        AgentGroupUpdateMode::Set,
    );
    println!(
        "{GREEN}Updated{RESET} cutex session groups for {BOLD}{session_id}{RESET}: {groups_label}",
        session_id = outcome.session_id
    );
    Ok(())
}

pub(crate) fn cmd_session_profile(command: SessionProfileCommand) -> anyhow::Result<()> {
    let (id, requested_profile) = match command {
        SessionProfileCommand::Set { id, profile } => (id, Some(profile)),
        SessionProfileCommand::Clear { id } => (id, None),
    };
    let mut store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, &id)
        .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
    // Resolve the target record before touching the local profile catalog. A
    // direct CLI invocation must never stamp this host's profile name onto a
    // remote session.
    let record = store
        .sessions
        .get(&key)
        .ok_or_else(|| anyhow!("cutex session disappeared while updating profile: {key}"))?;
    let current_host = current_host_name();
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, &current_host) {
        anyhow::bail!(
            "remote_runtime_manager_required: session host_id={} current_host={} cutex_session_id={}",
            record.host_id,
            current_host,
            record.cutex_session_id
        );
    }
    cutex::runtime::claude_accounts::check_profile_change(record, requested_profile.as_deref())
        .map_err(|mismatch| anyhow!("{}: {mismatch}", cutex::runtime::claude_accounts::MISMATCH_CODE))?;
    let mut claude_model = None;
    let profile = if record.claude_session.is_some() {
        requested_profile.as_deref()
            .map(|requested| super::launch::resolve_claude_profile_for_session(record, requested))
            .transpose()?
            .map(|(name, model)| { claude_model = Some(model); name })
    } else if record.explicit_launch.is_some() {
        let mut candidate = record.clone();
        candidate.profile = requested_profile.clone();
        cutex::launch::stock::current_configuration(&candidate)?;
        requested_profile
    } else {
        requested_profile
            .as_deref()
            .map(super::launch::resolve_launch_profile_override)
            .transpose()?
            .map(|resolved| resolved.account.name)
    };
    let outcome = set_cutex_session_profile_by_key(&mut store, &key, profile.clone())?;
    if let Some(model) = claude_model {
        let record = store.sessions.get_mut(&key).expect("profile-updated session");
        record.claude_session.as_mut().expect("Claude session").model = model.clone();
        record.model_defaults = Some(model);
    }
    persist_cutex_session_store_and_im_record(&store, &key)?;
    match profile {
        Some(profile) => println!("{GREEN}Configured{RESET} profile {BOLD}{profile}{RESET} for {}", outcome.session_id),
        None => println!("{GREEN}Cleared{RESET} configured profile for {}; next launch follows the global default", outcome.session_id),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_app::test_home::IsolatedTestHome;
    use cutex::profiles::materialize::materialized_account_files;
    use cutex::profiles::model::{AccountsStore, CliKind, RuntimeConfig, StoredAccount};
    use cutex::profiles::store::save_store;
    use cutex::session::model::CutexSessionRecord;
    use cutex::session::store::save_cutex_session_store;
    use std::fs;

    #[test]
    fn claude_profile_commands_and_management_share_account_gate_and_persist_model() {
        let home = IsolatedTestHome::new("claude-profile-mutations").unwrap();
        let mut same = cutex::profiles::lifecycle::new_claude_account("Claude next");
        same.options.claude.model = "opus".into();
        let mut other = cutex::profiles::lifecycle::new_claude_account("Other account");
        other.claude_account = Some("ca-other".into());
        save_store(&AccountsStore { version: 3, accounts: vec![same.clone(), other, profile("Codex")], active_account_id: None }).unwrap();
        write_adapter_features(&home.root().join("adapter"), true);
        // No materialized auth.json/config.toml exist for either Claude profile.
        for management in [false, true] {
            let id = if management { "cutex.claude-profile-http" } else { "cutex.claude-profile-cli" };
            save_session(id, current_host_name());
            let mut store = load_cutex_session_store().unwrap();
            let record = store.sessions.get_mut(id).unwrap();
            record.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
            record.claude_session = Some(cutex::runtime::claude::ClaudeSession {
                deployment: cutex::runtime::claude::ClaudeDeployment {
                    python: home.root().join("python"), adapter_source: home.root().join("adapter"),
                    claude_cli: home.root().join("cli"), claude_home: home.root().join("login"), tui_binary: home.root().join("tui"),
                },
                thread_id: "claude-thread".into(), state_dir: home.root().join(id), model: "sonnet".into(),
                effort: None, instructions: String::new(), allow_write: false, allow_exec: false, write_explicit: false, exec_explicit: false,
                native_session_id: None, connection_id: None,
            });
            save_cutex_session_store(&store).unwrap();
            let set = |target: &str| -> Result<(), String> {
                if management {
                    super::super::management_context::mutate_management_v2_session(id, "cutex/session/profile/set",
                        serde_json::json!({"profile":target}))
                        .map(|result| { assert_eq!(result["model"], "opus"); })
                        .map_err(|e| format!("{}: {}", e.code, e.message))
                } else {
                    cmd_session_profile(SessionProfileCommand::Set { id: id.into(), profile: target.into() })
                        .map_err(|e| format!("{e:#}"))
                }
            };
            set(&same.id).unwrap();
            let updated = load_cutex_session_store().unwrap().sessions[id].clone();
            assert_eq!(updated.profile.as_deref(), Some("Claude next"));
            assert_eq!(updated.claude_session.as_ref().unwrap().model, "opus");
            assert_eq!(updated.model_defaults.as_deref(), Some("opus"));
            for (target, expected) in [("Other account", "claude_account_mismatch"), ("Codex", "Claude profile"), ("missing", "not found")] {
                let error = set(target).unwrap_err();
                assert!(error.contains(expected), "{error}");
                assert!(!error.contains("auth.json"), "{error}");
                assert_eq!(load_cutex_session_store().unwrap().sessions[id], updated);
            }
            if management {
                let mut config = cutex::config::store::load_codez_config();
                config.new_session_defaults.insert(same.name.clone(), cutex::profiles::model::NewSessionDefaults {
                    model: Some("haiku".into()), reasoning: None,
                });
                cutex::config::store::save_codez_config(&config).unwrap();
                let result = super::super::management_context::mutate_management_v2_session(id, "cutex/session/profile/switch",
                    serde_json::json!({"profile":same.id, "model":"stale-codex-model"})).unwrap();
                assert_eq!(result["model"], "haiku");
                assert_eq!(load_cutex_session_store().unwrap().sessions[id].claude_session.as_ref().unwrap().model, "haiku");
            }
        }
    }

    fn write_adapter_features(adapter: &std::path::Path, launch_model_update: bool) {
        let dir = adapter.join("src/cute_claude");
        fs::create_dir_all(&dir).unwrap();
        let features = if launch_model_update { serde_json::json!({"launchModelUpdate": true}) } else { serde_json::json!({}) };
        fs::write(dir.join("features.json"), serde_json::to_vec(&serde_json::json!({"version": 1, "features": features})).unwrap()).unwrap();
    }

    /// A session pinned to an adapter without `launchModelUpdate` would be
    /// refused by thread/ensure (-32009) after a launch model change: the
    /// profile changes, the model stays.
    #[test]
    fn claude_profile_switch_keeps_the_model_for_adapters_without_launch_model_update() {
        let home = IsolatedTestHome::new("claude-profile-old-adapter").unwrap();
        let mut next = cutex::profiles::lifecycle::new_claude_account("Claude next");
        next.options.claude.model = "opus".into();
        save_store(&AccountsStore { version: 3, accounts: vec![next.clone()], active_account_id: None }).unwrap();
        write_adapter_features(&home.root().join("adapter"), false);
        for management in [false, true] {
            let id = if management { "cutex.claude-old-http" } else { "cutex.claude-old-cli" };
            save_session(id, current_host_name());
            let mut store = load_cutex_session_store().unwrap();
            let record = store.sessions.get_mut(id).unwrap();
            record.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
            record.claude_session = Some(cutex::runtime::claude::ClaudeSession {
                deployment: cutex::runtime::claude::ClaudeDeployment {
                    python: home.root().join("python"), adapter_source: home.root().join("adapter"),
                    claude_cli: home.root().join("cli"), claude_home: home.root().join("login"), tui_binary: home.root().join("tui"),
                },
                thread_id: "claude-thread".into(), state_dir: home.root().join(id), model: "sonnet".into(),
                effort: None, instructions: String::new(), allow_write: false, allow_exec: false, write_explicit: false, exec_explicit: false,
                native_session_id: None, connection_id: None,
            });
            save_cutex_session_store(&store).unwrap();
            if management {
                let result = super::super::management_context::mutate_management_v2_session(id, "cutex/session/profile/set",
                    serde_json::json!({"profile": next.id})).unwrap();
                assert_eq!(result["model"], "sonnet");
            } else {
                cmd_session_profile(SessionProfileCommand::Set { id: id.into(), profile: next.id.clone() }).unwrap();
            }
            let updated = load_cutex_session_store().unwrap().sessions[id].clone();
            assert_eq!(updated.profile.as_deref(), Some("Claude next"));
            assert_eq!(updated.claude_session.as_ref().unwrap().model, "sonnet");
        }
    }

    fn profile(name: &str) -> StoredAccount {
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
            source: Some("test".to_string()),
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

    fn save_resolvable_profile(account: &StoredAccount) {
        save_store(&AccountsStore {
            version: 3,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        })
        .expect("save profile store");
        let files = materialized_account_files(account).expect("profile files");
        fs::create_dir_all(files.auth_path.parent().expect("profile parent"))
            .expect("create profile directory");
        fs::write(&files.auth_path, "{}\n").expect("write profile auth");
        fs::write(&files.config_path, "model = \"test\"\n").expect("write profile config");
    }

    fn save_session(id: &str, host_id: String) {
        let record = CutexSessionRecord::new_at(
            id.to_string(),
            Some("thread-profile-command".to_string()),
            host_id,
            "/tmp/profile-command".to_string(),
            None,
            "2026-08-15T00:00:00Z".to_string(),
        )
        .expect("session record");
        let mut store = load_cutex_session_store().expect("load session store");
        store.sessions.insert(id.to_string(), record);
        save_cutex_session_store(&store).expect("save session store");
    }

    #[test]
    fn profile_command_uses_real_store_for_set_clear_and_rejects_unknown_or_remote_targets() {
        let _home = IsolatedTestHome::new("csm").expect("create isolated HOME");
        let local_id = "cutex.profile-command-local";
        let remote_id = "cutex.profile-command-remote";
        let account = profile("alpha");
        save_resolvable_profile(&account);
        let local_host = current_host_name();
        save_session(local_id, local_host.clone());
        save_session(remote_id, format!("remote-{local_host}"));

        cmd_session_profile(SessionProfileCommand::Set {
            id: local_id.to_string(),
            profile: "alpha".to_string(),
        })
        .expect("set configured profile");
        assert_eq!(
            load_cutex_session_store().expect("load store").sessions[local_id]
                .profile
                .as_deref(),
            Some("alpha")
        );

        cmd_session_profile(SessionProfileCommand::Clear {
            id: local_id.to_string(),
        })
        .expect("clear configured profile");
        assert_eq!(
            load_cutex_session_store().expect("reload store").sessions[local_id].profile,
            None
        );

        let before_unknown = load_cutex_session_store()
            .expect("load before unknown profile")
            .sessions[local_id]
            .clone();
        assert!(cmd_session_profile(SessionProfileCommand::Set {
            id: local_id.to_string(),
            profile: "missing".to_string(),
        })
        .is_err());
        assert_eq!(
            load_cutex_session_store()
                .expect("reload after unknown")
                .sessions[local_id],
            before_unknown
        );

        let before_remote = load_cutex_session_store()
            .expect("load before remote")
            .sessions[remote_id]
            .clone();
        assert!(cmd_session_profile(SessionProfileCommand::Set {
            id: remote_id.to_string(),
            profile: "alpha".to_string(),
        })
        .is_err());
        assert_eq!(
            load_cutex_session_store()
                .expect("reload after remote")
                .sessions[remote_id],
            before_remote
        );
    }
}
