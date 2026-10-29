//! The Claude template references the one globally installed subscription runtime.
use anyhow::{ensure, Context};
use cutex::profiles::model::{CliKind, StoredAccount};

pub(super) fn add(name: &str) -> anyhow::Result<()> {
    ensure!(
        !name.trim().is_empty() && name.trim() == name && !name.chars().any(char::is_control),
        "Valid profile name required"
    );
    let mut store = super::account_store::load_store()?;
    cutex::profiles::lookup::ensure_unique_name(&store, name, None)?;
    let account = cutex::profiles::lifecycle::new_claude_account(name);
    store.accounts.push(account);
    cutex::profiles::store::save_store(&store)?;
    println!("Created Claude Code profile {name}; uses the shared official login.");
    if cutex::runtime::claude::ClaudeDeployment::selected().is_err() {
        println!(
            "Claude runtime is not installed on this host; install it before starting a session."
        );
    }
    Ok(())
}

pub(super) fn account(name: &str) -> anyhow::Result<Option<StoredAccount>> {
    let store = super::account_store::load_store_read_only()?;
    Ok(cutex::profiles::lookup::find_account(&store, name)?
        .filter(|a| a.cli_kind == CliKind::Claude)
        .cloned())
}

pub(super) fn create(
    account: &StoredAccount,
    name: &str,
    cwd: &std::path::Path,
    managed: bool,
    defaults: cutex::profiles::appearance::ClaudeDefaults,
    tools: (Option<bool>, Option<bool>),
) -> anyhow::Result<cutex::session::model::CutexSessionRecord> {
    create_with_creator(account, name, cwd, managed, defaults, tools,
        cutex::session::model::SessionCreatedBy::Owner)
}

pub(super) fn create_with_creator(
    account: &StoredAccount,
    name: &str,
    cwd: &std::path::Path,
    managed: bool,
    defaults: cutex::profiles::appearance::ClaudeDefaults,
    tools: (Option<bool>, Option<bool>),
    creator: cutex::session::model::SessionCreatedBy,
) -> anyhow::Result<cutex::session::model::CutexSessionRecord> {
    // `tools`: write / exec set for this session; None follows the profile
    // (`defaults.allow_*` are the profile's rules, resolved at each start).
    cutex::runtime::claude::create_profile_with_creator(
        name,
        cwd,
        &defaults.model,
        tools.0,
        tools.1,
        Some(account.name.clone()),
        managed,
        defaults.effort,
        Some(creator),
    )
}

pub(super) fn run(account: &StoredAccount, args: &[String], managed: bool) -> anyhow::Result<()> {
    ensure!(args.is_empty() && account.default_cli_args.is_empty(),"Claude profiles do not accept Codex CLI arguments; use Profile defaults or attach an existing Claude session");
    ensure!(
        matches!(account.runtime, cutex::profiles::model::RuntimeConfig::Host),
        "Claude runtime runs on its owning host; connect using SSH"
    );
    ensure!(!managed, "Use Alt+M to create a named Claude Agent");
    let name = format!("Session {}", &uuid::Uuid::new_v4().to_string()[..8]);
    let mut defaults = account.options.claude.clone();
    if let Some(model) = cutex::config::store::load_codez_config_checked()?
        .new_session_defaults
        .get(&account.name)
        .and_then(|d| d.model.clone())
    {
        defaults.model = model;
    }
    let r = create(account, &name, &std::env::current_dir()?, false, defaults, (None, None))?;
    println!("Created Cutex Session: {}", r.cutex_session_id);
    super::claude_runtime::lifecycle(&r.cutex_session_id, true).with_context(|| {
        format!(
            "Session {} exists; retry Online for this identity",
            r.cutex_session_id
        )
    })?;
    cutex::runtime::claude::attach(&r.cutex_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changing_profile_color_updates_frontend_without_invalidating_runtime_review() {
        let home = super::super::test_home::IsolatedTestHome::new("profile-presentation").unwrap();
        add("Claude main").unwrap();
        let config = cutex::profiles::model::CodezConfig::default();
        let catalog = cutex::profiles::materialize::custom_status_items_catalog_json(&config)
            .unwrap()
            .unwrap();
        let path = home.root().join("catalog.json");
        cutex::config::atomic::write_private_bytes_atomic(&path, catalog.as_bytes()).unwrap();
        let order = vec!["cutex_profile".to_owned()];
        let reviewed = cutex::launch::selected_status::Status::review(&path, &order, "Claude main")
            .unwrap()
            .unwrap();
        let mut accounts = super::super::account_store::load_store().unwrap();
        accounts.accounts[0].options.style =
            Some(cutex::profiles::appearance::TextStyle::color("#123456"));
        cutex::profiles::store::save_store(&accounts).unwrap();
        reviewed.validate_projection(&order, "Claude main").unwrap();
        let rendered = reviewed.materialize_current_display().unwrap();
        let payload: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rendered).unwrap()).unwrap();
        // Since 1b979ed the session display is always welcome + profile, so the
        // profile item is looked up by id rather than by position.
        let profile = payload["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["id"]
                    .as_str()
                    .is_some_and(|id| cutex::launch::selected_status::canonical_id(id) == "cutex_profile")
            })
            .expect("profile item in session display");
        assert_eq!(profile["style"]["fg"], "#123456");
        assert_eq!(profile["text"], "Claude main");
    }

    #[test]
    fn shared_login_profiles_create_distinct_agent_and_session_identities_without_auth_copies() {
        let home = super::super::test_home::IsolatedTestHome::new("claude-profile").unwrap();
        let adapter = home.root().join("adapter");
        std::fs::create_dir_all(adapter.join("src/cute_claude")).unwrap();
        std::fs::create_dir_all(adapter.join("scripts")).unwrap();
        for file in ["src/cute_claude/__main__.py", "scripts/connect_tui.py"] {
            std::fs::write(adapter.join(file), "").unwrap();
        }
        let binary = home.root().join("binary");
        std::fs::write(&binary, "").unwrap();
        let login = home.root().join("shared-login");
        std::fs::create_dir(&login).unwrap();
        cutex::runtime::claude::ClaudeDeployment {
            python: binary.clone(),
            adapter_source: adapter,
            claude_cli: binary.clone(),
            claude_home: login.clone(),
            tui_binary: binary,
        }
        .install()
        .unwrap();
        add("Claude main").unwrap();
        add("Claude second").unwrap();
        assert!(add("Claude main").is_err());
        let profile = account("Claude main").unwrap().unwrap();
        let agent = create(
            &profile,
            "worker",
            home.root(),
            true,
            profile.options.claude.clone(),
            (None, None),
        )
        .unwrap();
        let session = create(
            &profile,
            "chat",
            home.root(),
            false,
            profile.options.claude.clone(),
            (None, None),
        )
        .unwrap();
        assert!(agent.agent_enabled);
        assert!(!agent.is_owned_session());
        assert!(!session.agent_enabled);
        assert!(session.is_owned_session());
        assert!(session.formal_agent_name.is_none());
        assert_ne!(agent.app_server_thread_id(), session.app_server_thread_id());
        for r in [&agent, &session] {
            assert_eq!(r.profile.as_deref(), Some("Claude main"));
            assert_eq!(
                r.claude_session.as_ref().unwrap().deployment.claude_home,
                login
            );
            assert!(r.runtime_pid.is_none());
            assert!(r.codex_session_id.is_none());
        }
        let files = cutex::profiles::materialize::materialized_account_files(&profile).unwrap();
        assert!(!files.auth_path.exists());
        assert_eq!(
            cutex::config::new_session::inherited("Claude main")
                .unwrap()
                .model
                .as_deref(),
            Some("sonnet")
        );
    }
}
