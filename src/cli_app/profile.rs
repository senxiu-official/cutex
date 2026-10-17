use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{anyhow, Context};
use cutex::cli::args::ProfileCommand;
use cutex::config::atomic::{write_private_bytes_atomic, write_private_pretty_json_atomic};
use cutex::config::global_settings::ConfigValueUpdate;
use cutex::config::proxy::{effective_proxy_config, proxy_config_from_parts};
use cutex::config::store::{
    load_codez_config, load_quick_state, save_codez_config, save_quick_state,
};
use cutex::config::text::{read_optional_text, write_optional_text_if_changed};
use cutex::launch::docker::{
    default_docker_user_name, docker_user_name, normalize_docker_user_name,
};
use cutex::profiles::codex_profile::apply_codex_profile_config_patch;
use cutex::profiles::inspect::{
    account_model_provider, account_proxy_scope_label, account_uses_api_key_auth,
};
use cutex::profiles::lookup::{ensure_unique_name, find_account};
use cutex::profiles::materialize::{
    custom_status_items_catalog_json, ensure_materialized_account_dir, ensure_materialized_account_files,
    materialized_account_files, set_materialized_file_permissions, sync_active_codex_home_files,
};
use cutex::profiles::model::{
    runtime_label, AccountsStore, CodezConfig, QuickRunState, RuntimeConfig, SessionConfig,
    StoredAccount,
};
use cutex::profiles::profile_config::{
    build_copied_profile_config, extract_profile_config_toml, merge_and_write_config_toml,
    normalize_profile_config_for_account, parse_toml_table, profile_uses_local_model_catalog,
    read_profile_specific_config_table,
};
use cutex::profiles::references::{
    remove_all_profile_references, rename_all_profile_references, ProfileReferenceChanges,
};
use cutex::profiles::store::save_store;
use cutex::session::model::CutexSessionStore;
use cutex::session::service::persist_cutex_session_store_and_im_record;
use cutex::session::store::load_cutex_session_store;
use cutex::ui::format::{bool_label, proxy_config_label};
use toml::value::Table;
use uuid::Uuid;

use super::account_store::{detect_source_label_for_account_files, load_store};
use super::profile_settings::{
    apply_profile_settings_patch, ProfileApiKeyUpdate, ProfileSettingsPatch,
};
use super::profile_settings_presenter;
use super::prompt::{
    checkbox, cli_args_label, parse_cli_args_value, prompt_cli_args, prompt_line,
    prompt_optional_string, read_wizard_choice, wizard_value,
};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

pub(crate) fn list() -> anyhow::Result<()> {
    cmd_profile_list()
}

pub(crate) fn profile_list() -> anyhow::Result<()> {
    cmd_profile_list()
}

pub(crate) fn current() -> anyhow::Result<()> {
    cmd_profile_show(None)
}

pub(crate) fn active_profile_name() -> Option<String> {
    let store = load_store().ok()?;
    let active_id = store.active_account_id.as_deref()?;
    store
        .accounts
        .iter()
        .find(|account| account.id == active_id)
        .map(|account| account.name.clone())
}

pub(crate) fn profile_show(target: Option<&str>) -> anyhow::Result<()> {
    cmd_profile_show(target)
}

pub(crate) fn use_profile(target: &str) -> anyhow::Result<()> {
    cmd_use(target)
}

pub(crate) fn rename(target: &str, new_name: &str) -> anyhow::Result<()> {
    cmd_rename(target, new_name)
}

pub(crate) fn remove(target: &str) -> anyhow::Result<()> {
    cmd_remove(target)
}

pub(crate) fn annotate(
    target: &str,
    source: Option<String>,
    clear_source: bool,
    plan: Option<String>,
    clear_plan: bool,
    email: Option<String>,
    clear_email: bool,
) -> anyhow::Result<()> {
    cmd_annotate(
        target,
        source,
        clear_source,
        plan,
        clear_plan,
        email,
        clear_email,
    )
}

pub(crate) fn runtime(
    target: &str,
    host: bool,
    docker_image: Option<String>,
    docker_user_name: Option<String>,
) -> anyhow::Result<()> {
    cmd_runtime(target, host, docker_image, docker_user_name)
}

fn cmd_profile_list() -> anyhow::Result<()> {
    let store = load_store()?;
    profile_settings_presenter::print_profile_list(&store);
    Ok(())
}

fn cmd_profile_show(target: Option<&str>) -> anyhow::Result<()> {
    let store = load_store()?;
    let global_config = load_codez_config();
    let account = match target {
        Some(target) => {
            find_account(&store, target)?.ok_or_else(|| anyhow!("Account not found: {target}"))?
        }
        None => {
            let Some(active_id) = store.active_account_id.as_ref() else {
                println!("No active account. Use `cutex use <name>` to select one.");
                return Ok(());
            };
            let Some(account) = store
                .accounts
                .iter()
                .find(|candidate| &candidate.id == active_id)
            else {
                println!("No active account. Use `cutex use <name>` to select one.");
                return Ok(());
            };
            account
        }
    };

    profile_settings_presenter::print_profile_details(&store, account, &global_config);
    Ok(())
}

fn cmd_use(target: &str) -> anyhow::Result<()> {
    let account = activate_account(target)?;

    println!(
        "{GREEN}Switched{RESET} active profile to {BOLD}{}{RESET}",
        account.name
    );
    Ok(())
}

pub(crate) fn activate_account(target: &str) -> anyhow::Result<StoredAccount> {
    let mut store = load_store()?;
    let account_id = find_account(&store, target)?
        .map(|account| account.id.clone())
        .ok_or_else(|| anyhow!("Account not found: {target}"))?;

    let account = store
        .accounts
        .iter()
        .find(|account| account.id == account_id)
        .cloned()
        .ok_or_else(|| anyhow!("Account not found after sync: {target}"))?;

    switch_to_account(&account)?;

    store.active_account_id = Some(account.id.clone());
    if let Some(acc) = store.accounts.iter_mut().find(|a| a.id == account.id) {
        acc.last_used_at = Some(chrono::Utc::now());
    }
    save_store(&store)?;

    Ok(account)
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileRenameResult {
    pub(crate) old_name: String,
    pub(crate) account: StoredAccount,
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileRemoveResult {
    pub(crate) removed: StoredAccount,
    pub(crate) active: Option<StoredAccount>,
}

fn switch_to_account(account: &StoredAccount) -> anyhow::Result<()> {
    let files = ensure_materialized_account_files(account)?;
    sync_active_codex_home_files(account, &files)
}

/// `cutex profile copy`: settings only, like the Management duplicate. A
/// ChatGPT login is never copied (two profiles sharing one rotating refresh
/// token log each other out); an API key only with `--with-api-key`. The
/// copy's account identity (email, ChatGPT plan) is cleared.
pub(crate) fn cmd_profile_copy(
    source: &str,
    name: &str,
    provider: Option<String>,
    provider_base_url: Option<String>,
    with_api_key: bool,
) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("Profile name cannot be empty");
    }

    let mut store = load_store()?;
    ensure_unique_name(&store, name, None)?;

    let source_index = store
        .accounts
        .iter()
        .position(|account| account.name == source || account.id == source)
        .ok_or_else(|| anyhow!("Account not found: {source}"))?;
    let source_account = store.accounts[source_index].clone();

    let mut copied_account = source_account.clone();
    copied_account.id = Uuid::new_v4().to_string();
    copied_account.name = name.to_string();
    copied_account.last_used_at = None;
    copied_account.email = None;
    if copied_account.source.as_deref() != Some("api-key") {
        copied_account.plan_type = None;
        copied_account.plan_label = None;
    }

    let key_copied = copy_profile_account_files(
        &source_account,
        &copied_account,
        provider,
        provider_base_url,
        with_api_key,
    )?;

    // Without auth.json the files may say nothing ("custom"): keep the
    // source's label then.
    if let Some(source_label) = detect_source_label_for_account_files(&copied_account) {
        if key_copied || source_label != "custom" {
            copied_account.source = Some(source_label);
        }
    }

    let copied_name = copied_account.name.clone();
    store.accounts.insert(source_index + 1, copied_account);
    save_store(&store)?;

    println!(
        "{GREEN}Copied{RESET} profile {BOLD}{}{RESET} -> {BOLD}{}{RESET}",
        source_account.name, copied_name
    );
    if key_copied {
        println!("API key copied (--with-api-key).");
    } else {
        println!(
            "{YELLOW}No credentials copied:{RESET} log the copy in (desktop: Settings → Profiles) \
             or set its API key (`cutex profile edit {copied_name}`); `--with-api-key` copies an \
             API key, a ChatGPT login is never copied."
        );
    }
    cmd_profile_show(Some(copied_name.as_str()))
}

/// The API-key `auth.json` of a source profile (no ChatGPT tokens).
fn api_key_auth(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw).is_ok_and(|auth| {
        auth.get("tokens").is_none_or(serde_json::Value::is_null)
            && auth
                .get("OPENAI_API_KEY")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|key| !key.trim().is_empty())
    })
}

/// Returns whether an API key was copied.
fn copy_profile_account_files(
    source_account: &StoredAccount,
    target_account: &StoredAccount,
    provider: Option<String>,
    provider_base_url: Option<String>,
    with_api_key: bool,
) -> anyhow::Result<bool> {
    let source_files = cutex::profiles::materialize::refresh_materialized_account_config(source_account)?;
    let target_files = materialized_account_files(target_account)?;

    let auth_contents = read_optional_text(&source_files.auth_path)?;
    let api_key = if with_api_key {
        match auth_contents {
            Some(raw) if api_key_auth(&raw) => Some(raw),
            Some(_) => anyhow::bail!(
                "Source profile '{}' uses a ChatGPT login, which is never copied (two profiles sharing \
                 one refresh token log each other out); copy without --with-api-key and log the copy in",
                source_account.name
            ),
            None => anyhow::bail!("Source profile '{}' has no API key to copy", source_account.name),
        }
    } else {
        None
    };
    ensure_materialized_account_dir(&target_files)?;
    if let Some(raw) = &api_key {
        write_optional_text_if_changed(&target_files.auth_path, Some(raw))?;
    }

    let source_config_contents = read_optional_text(&source_files.config_path)?;
    let copied_config = build_copied_profile_config(
        source_account,
        source_config_contents,
        provider,
        provider_base_url,
    )?;
    write_optional_text_if_changed(&target_files.config_path, copied_config.as_deref())?;

    let copied_model_catalog = if copied_config
        .as_deref()
        .map(profile_uses_local_model_catalog)
        .transpose()?
        .unwrap_or(false)
    {
        match read_optional_text(&source_files.model_catalog_path)? {
            Some(contents) => Some(contents),
            None if copied_config.as_deref().is_some_and(|config| {
                parse_toml_table(config)
                    .ok()
                    .and_then(|table| {
                        table
                            .get("model_provider")
                            .and_then(toml::Value::as_str)
                            .map(str::to_string)
                    })
                    .and_then(|provider| cutex::profiles::preset_spec::catalog_for_provider(&provider))
                    .is_some()
            }) =>
            {
                copied_config
                    .as_deref()
                    .and_then(|config| parse_toml_table(config).ok())
                    .and_then(|table| table.get("model_provider").and_then(toml::Value::as_str).map(str::to_string))
                    .and_then(|provider| cutex::profiles::preset_spec::catalog_for_provider(&provider))
            }
            None => anyhow::bail!(
                "Source profile '{}' references models.json but it is missing at {}",
                source_account.name,
                source_files.model_catalog_path.display()
            ),
        }
    } else {
        None
    };
    write_optional_text_if_changed(
        &target_files.model_catalog_path,
        copied_model_catalog.as_deref(),
    )?;

    let codez_config = load_codez_config();
    let custom_status_items_json = custom_status_items_catalog_json(&codez_config)?;
    write_optional_text_if_changed(
        &target_files.custom_status_items_path,
        custom_status_items_json.as_deref(),
    )?;
    set_materialized_file_permissions(&target_files)?;
    Ok(api_key.is_some())
}

pub(crate) fn cmd_profile_pin(target: &str, to_top: bool) -> anyhow::Result<()> {
    let mut store = load_store()?;
    let index = store
        .accounts
        .iter()
        .position(|account| account.name == target || account.id == target)
        .ok_or_else(|| anyhow!("Account not found: {target}"))?;

    let destination_index = if to_top { 0 } else { store.accounts.len() - 1 };
    if index == destination_index {
        let account = &store.accounts[index];
        println!(
            "{YELLOW}No changes{RESET} profile {BOLD}{}{RESET} is already at the {}",
            account.name,
            if to_top { "top" } else { "bottom" }
        );
        return Ok(());
    }

    let account = store.accounts.remove(index);
    let account_name = account.name.clone();
    if to_top {
        store.accounts.insert(0, account);
    } else {
        store.accounts.push(account);
    }
    save_store(&store)?;
    println!(
        "{GREEN}Moved{RESET} profile {BOLD}{}{RESET} to the {}",
        account_name,
        if to_top { "top" } else { "bottom" }
    );
    Ok(())
}

pub(crate) fn cmd_profile_clone_status_line(from: Option<&str>) -> anyhow::Result<()> {
    let store = load_store()?;
    if store.accounts.is_empty() {
        anyhow::bail!(
            "No accounts configured. Use `cutex add --from-auth <path> --name <name>` to add one."
        );
    }

    let source_account = match from {
        Some(target) => {
            find_account(&store, target)?.ok_or_else(|| anyhow!("Account not found: {target}"))?
        }
        None => {
            let active_id = store
                .active_account_id
                .as_ref()
                .ok_or_else(|| anyhow!("No active profile. Use `cutex use <name>` first."))?;
            store
                .accounts
                .iter()
                .find(|account| account.id.as_str() == active_id.as_str())
                .ok_or_else(|| anyhow!("Active profile not found: {active_id}"))?
        }
    };

    let source_files = materialized_account_files(source_account)?;
    let source_config = read_optional_text(&source_files.config_path)?.ok_or_else(|| {
        anyhow!(
            "Source profile has no config.toml: {}",
            source_files.config_path.display()
        )
    })?;
    let source_table = parse_toml_table(&source_config).with_context(|| {
        format!(
            "Failed to parse source profile config.toml: {}",
            source_files.config_path.display()
        )
    })?;
    let source_status_line = source_table
        .get("tui")
        .and_then(|value| value.as_table())
        .and_then(|tui| tui.get("status_line"))
        .and_then(|value| value.as_array())
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "Source profile '{}' has no [tui].status_line in {}",
                source_account.name,
                source_files.config_path.display()
            )
        })?;

    let global_config = load_codez_config();
    for account in &store.accounts {
        let files = materialized_account_files(account)?;
        let mut profile_table = read_profile_specific_config_table(&files.config_path)?;
        let tui_entry = profile_table
            .entry("tui".to_string())
            .or_insert_with(|| toml::Value::Table(Table::new()));
        let tui_table = tui_entry
            .as_table_mut()
            .ok_or_else(|| anyhow!("config.toml key `tui` must be a table"))?;
        tui_table.insert(
            "status_line".to_string(),
            toml::Value::Array(source_status_line.clone()),
        );

        let profile_config_toml = toml::to_string_pretty(&profile_table)?;
        merge_and_write_config_toml(
            &files.config_path,
            Some(profile_config_toml.as_str()),
            effective_proxy_config(account, &global_config)
                .map(|proxy| proxy.enabled)
                .unwrap_or(false),
        )?;
        set_materialized_file_permissions(&files)?;
    }

    println!(
        "{GREEN}Cloned{RESET} [tui].status_line from {BOLD}{}{RESET} to {} profiles",
        source_account.name,
        store.accounts.len()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_profile_set(
    target: &str,
    name: Option<String>,
    source: Option<String>,
    clear_source: bool,
    plan: Option<String>,
    clear_plan: bool,
    email: Option<String>,
    clear_email: bool,
    default_cli_args: Option<String>,
    clear_default_cli_args: bool,
    agent_name: Option<String>,
    clear_agent_name: bool,
    host: bool,
    docker_image: Option<String>,
    docker_user_name: Option<String>,
    proxy_url: Option<String>,
    proxy_no_proxy: Option<String>,
    proxy_force_http_transport: Option<bool>,
    proxy_disable: bool,
    proxy_inherit: bool,
    session_enable: bool,
    session_disable: bool,
    session_inherit: bool,
) -> anyhow::Result<()> {
    if proxy_no_proxy.is_some() && proxy_url.is_none() {
        anyhow::bail!("--proxy-no-proxy requires --proxy-url");
    }
    if proxy_force_http_transport.is_some() && proxy_url.is_none() {
        anyhow::bail!("--proxy-force-http requires --proxy-url");
    }

    let metadata_requested = source.is_some()
        || clear_source
        || plan.is_some()
        || clear_plan
        || email.is_some()
        || clear_email;
    let default_cli_args_requested = default_cli_args.is_some() || clear_default_cli_args;
    let agent_name_requested = agent_name.is_some() || clear_agent_name;
    let runtime_requested = host || docker_image.is_some();
    let proxy_requested = proxy_inherit || proxy_disable || proxy_url.is_some();
    let session_requested = session_enable || session_disable || session_inherit;

    if name.is_none()
        && !metadata_requested
        && !default_cli_args_requested
        && !agent_name_requested
        && !runtime_requested
        && !proxy_requested
        && !session_requested
    {
        anyhow::bail!(
            "No changes requested. Provide at least one of --name, metadata flags, default CLI args, agent name, runtime flags, proxy flags, or session flags."
        );
    }

    let patch = ProfileSettingsPatch {
        options: None,
        name,
        source: requested_optional_update(source, clear_source),
        plan_type: requested_optional_update(plan, clear_plan),
        quota_preset: ConfigValueUpdate::Unchanged,
        email: requested_optional_update(email, clear_email),
        runtime: if let Some(image) = docker_image {
            Some(RuntimeConfig::Docker {
                image,
                user_name: Some(normalize_docker_user_name(docker_user_name)?),
            })
        } else if host {
            Some(RuntimeConfig::Host)
        } else {
            None
        },
        proxy: if proxy_inherit {
            ConfigValueUpdate::Clear
        } else if proxy_disable {
            ConfigValueUpdate::Set(proxy_config_from_parts(false, None, None, true)?)
        } else if let Some(url) = proxy_url {
            ConfigValueUpdate::Set(proxy_config_from_parts(
                true,
                Some(url),
                proxy_no_proxy,
                proxy_force_http_transport.unwrap_or(true),
            )?)
        } else {
            ConfigValueUpdate::Unchanged
        },
        session: if session_inherit {
            ConfigValueUpdate::Clear
        } else if session_enable {
            ConfigValueUpdate::Set(SessionConfig { enabled: true })
        } else if session_disable {
            ConfigValueUpdate::Set(SessionConfig { enabled: false })
        } else {
            ConfigValueUpdate::Unchanged
        },
        default_cli_args: if clear_default_cli_args {
            Some(Vec::new())
        } else {
            default_cli_args
                .as_deref()
                .map(parse_cli_args_value)
                .transpose()?
        },
        agent_name: if clear_agent_name {
            ConfigValueUpdate::Clear
        } else if let Some(agent_name) = agent_name {
            ConfigValueUpdate::Set(agent_name)
        } else {
            ConfigValueUpdate::Unchanged
        },
        api_key: ProfileApiKeyUpdate::Unchanged,
        codex_config: Default::default(),
    };
    let result = update_profile_settings(target, &patch)?;

    if !result.changed {
        println!(
            "{YELLOW}No changes{RESET} for profile {BOLD}{}{RESET}",
            result.account.name
        );
        return Ok(());
    }

    println!(
        "{GREEN}Updated{RESET} profile {BOLD}{}{RESET}",
        result.account.name
    );
    profile_show(Some(result.account.name.as_str()))
}

fn requested_optional_update<T>(value: Option<T>, clear: bool) -> ConfigValueUpdate<T> {
    if clear {
        ConfigValueUpdate::Clear
    } else if let Some(value) = value {
        ConfigValueUpdate::Set(value)
    } else {
        ConfigValueUpdate::Unchanged
    }
}

#[derive(Debug)]
pub(super) struct ProfileSettingsUpdateResult {
    pub(super) account: StoredAccount,
    pub(super) changed: bool,
}

pub(super) fn update_profile_settings(
    target: &str,
    patch: &ProfileSettingsPatch,
) -> anyhow::Result<ProfileSettingsUpdateResult> {
    let mut store = load_store()?;
    let account_id = find_account(&store, target)?
        .map(|account| account.id.clone())
        .ok_or_else(|| anyhow!("Account not found: {target}"))?;

    if let Some(new_name) = patch.name.as_deref() {
        ensure_unique_name(&store, new_name, Some(&account_id))?;
    }

    let (account_changed, old_name, account) = {
        let account = store
            .accounts
            .iter_mut()
            .find(|account| account.id == account_id)
            .ok_or_else(|| anyhow!("Account not found after lookup: {target}"))?;
        let old_name = account.name.clone();
        let changed = apply_profile_settings_patch(account, patch)?;
        (changed, old_name, account.clone())
    };

    let files = materialized_account_files(&account)?;
    if !patch.api_key.is_unchanged()
        && (account.cli_kind != cutex::profiles::model::CliKind::Codex
            || account.source.as_deref() != Some("api-key"))
    {
        anyhow::bail!("API key editing is only available for Codex API-key profiles");
    }
    let original_auth_file = read_optional_text(&files.auth_path)?;
    let original_config_file = read_optional_text(&files.config_path)?;
    let original_model_catalog_file = read_optional_text(&files.model_catalog_path)?;
    let current_profile_config = original_config_file
        .as_deref()
        .map(extract_profile_config_toml)
        .transpose()?
        .flatten();
    let (next_profile_config, config_changed) = if patch.codex_config.is_empty() {
        (current_profile_config, false)
    } else {
        if account.cli_kind != cutex::profiles::model::CliKind::Codex {
            anyhow::bail!("Codex model settings are unavailable for a Claude profile");
        }
        let api_key_will_be_configured = match patch.api_key {
            ProfileApiKeyUpdate::Replace(_) => true,
            ProfileApiKeyUpdate::Clear => false,
            ProfileApiKeyUpdate::Unchanged => account_uses_api_key_auth(&account),
        };
        if patch.codex_config.apply_deepseek_preset
            && (account.source.as_deref() != Some("api-key") || !api_key_will_be_configured)
        {
            anyhow::bail!(
                "DeepSeek preset requires an API-key profile with a stored OPENAI_API_KEY"
            );
        }
        apply_codex_profile_config_patch(current_profile_config.as_deref(), &patch.codex_config)?
    };
    let next_auth = match &patch.api_key {
        ProfileApiKeyUpdate::Unchanged => None,
        ProfileApiKeyUpdate::Replace(api_key) => Some(serde_json::json!({
            "OPENAI_API_KEY": api_key,
        })),
        ProfileApiKeyUpdate::Clear => Some(serde_json::json!({
            "OPENAI_API_KEY": null,
        })),
    };
    let current_auth = original_auth_file
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let auth_changed = next_auth
        .as_ref()
        .is_some_and(|next| current_auth.as_ref() != Some(next));
    let changed = account_changed || config_changed || auth_changed;
    if !changed {
        return Ok(ProfileSettingsUpdateResult { account, changed });
    }

    let reference_changes = (old_name != account.name)
        .then(|| prepare_renamed_profile_references(&old_name, &account.name))
        .transpose()?;
    let normalized_profile_config = config_changed
        .then(|| normalize_profile_config_for_account(&account, next_profile_config))
        .transpose()?;

    if auth_changed {
        ensure_materialized_account_dir(&files)?;
        write_private_pretty_json_atomic(
            &files.auth_path,
            next_auth
                .as_ref()
                .expect("changed API key update should have an auth payload"),
            "profile auth.json",
        )?;
    }

    if config_changed {
        let global_config = load_codez_config();
        let materialized = (|| -> anyhow::Result<()> {
            merge_and_write_config_toml(
                &files.config_path,
                normalized_profile_config
                    .as_ref()
                    .and_then(|config| config.as_deref()),
                effective_proxy_config(&account, &global_config)
                    .map(|proxy| proxy.enabled)
                    .unwrap_or(false),
            )?;
            // Re-applying the preset also refreshes its vendored catalog
            // (an older one may lack the preset's new default model).
            if patch.codex_config.apply_deepseek_preset {
                write_optional_text_if_changed(
                    &files.model_catalog_path,
                    Some(&cutex::profiles::deepseek::model_catalog_json()),
                )?;
            }
            ensure_materialized_account_files(&account)?;
            Ok(())
        })();
        if let Err(error) = materialized {
            write_optional_text_if_changed(&files.config_path, original_config_file.as_deref())?;
            write_optional_text_if_changed(
                &files.model_catalog_path,
                original_model_catalog_file.as_deref(),
            )?;
            if auth_changed {
                restore_profile_auth_file(&files.auth_path, original_auth_file.as_deref())?;
            }
            return Err(error);
        }
    }

    if account_changed {
        if let Err(error) = save_store(&store) {
            if config_changed {
                write_optional_text_if_changed(
                    &files.config_path,
                    original_config_file.as_deref(),
                )?;
                write_optional_text_if_changed(
                    &files.model_catalog_path,
                    original_model_catalog_file.as_deref(),
                )?;
            }
            if auth_changed {
                restore_profile_auth_file(&files.auth_path, original_auth_file.as_deref())?;
            }
            return Err(error);
        }
    }
    if let Some(reference_changes) = reference_changes {
        reference_changes.persist()?;
    }

    Ok(ProfileSettingsUpdateResult { account, changed })
}

fn restore_profile_auth_file(path: &Path, original: Option<&str>) -> anyhow::Result<()> {
    match original {
        Some(original) => write_private_bytes_atomic(path, original.as_bytes()),
        None if path.exists() => fs::remove_file(path)
            .with_context(|| format!("Failed to remove restored auth file: {}", path.display())),
        None => Ok(()),
    }
}

fn profile_edit_target_id(
    store: &AccountsStore,
    target: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if let Some(target) = target {
        return Ok(Some(
            find_account(store, target)?
                .map(|account| account.id.clone())
                .ok_or_else(|| anyhow!("Account not found: {target}"))?,
        ));
    }

    choose_profile_for_edit(store)
}

fn choose_profile_for_edit(store: &AccountsStore) -> anyhow::Result<Option<String>> {
    if store.accounts.is_empty() {
        anyhow::bail!("No profiles configured. Use `cutex login` to create one.");
    }

    let default_index = store
        .active_account_id
        .as_ref()
        .and_then(|active_id| {
            store
                .accounts
                .iter()
                .position(|account| &account.id == active_id)
        })
        .unwrap_or(0);

    println!();
    println!("{BOLD}{CYAN}Choose Profile{RESET}");
    for (idx, account) in store.accounts.iter().enumerate() {
        let active = store.active_account_id.as_deref() == Some(account.id.as_str());
        let marker = if active {
            format!("{GREEN}●{RESET}")
        } else {
            format!("{DIM}○{RESET}")
        };
        let provider = account_model_provider(account).unwrap_or_else(|| "-".to_string());
        println!(
            "  {BOLD}{:>2}{RESET}. {marker} {CYAN}{}{RESET}  {DIM}{} / {} / {}{RESET}",
            idx + 1,
            account.name,
            account.cli_kind,
            account.source.as_deref().unwrap_or("-"),
            provider,
        );
    }

    loop {
        print!(
            "Select profile number [{BOLD}{}{RESET}], or q to quit: ",
            default_index + 1
        );
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let input = line.trim();
        if input.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        let choice = if input.is_empty() {
            default_index + 1
        } else {
            input
                .parse::<usize>()
                .with_context(|| format!("Invalid profile selection: {input}"))?
        };
        if choice == 0 || choice > store.accounts.len() {
            eprintln!("{YELLOW}warning:{RESET} profile selection out of range: {choice}");
            continue;
        }
        return Ok(Some(store.accounts[choice - 1].id.clone()));
    }
}

fn session_override_label(session: Option<&SessionConfig>) -> &'static str {
    match session {
        Some(SessionConfig { enabled: true }) => "enabled override",
        Some(SessionConfig { enabled: false }) => "disabled override",
        None => "inherit global",
    }
}

pub(crate) fn cmd_profile_edit(target: Option<&str>) -> anyhow::Result<()> {
    let initial_store = load_store()?;
    let Some(account_id) = profile_edit_target_id(&initial_store, target)? else {
        println!("Done.");
        return Ok(());
    };

    loop {
        let store = load_store()?;
        let global_config = load_codez_config();
        let account = store
            .accounts
            .iter()
            .find(|account| account.id == account_id)
            .ok_or_else(|| anyhow!("Profile disappeared while editing"))?
            .clone();
        let is_host = matches!(account.runtime, RuntimeConfig::Host);
        let docker_image = match &account.runtime {
            RuntimeConfig::Docker { image, .. } => Some(image.as_str()),
            RuntimeConfig::Host => None,
        };
        let docker_user_name = match &account.runtime {
            RuntimeConfig::Docker { user_name, .. } => user_name.as_deref(),
            RuntimeConfig::Host => None,
        };
        let proxy_enabled = account.proxy.as_ref().is_some_and(|proxy| proxy.enabled);
        let proxy_disabled = account.proxy.as_ref().is_some_and(|proxy| !proxy.enabled);
        let proxy_inherit = account.proxy.is_none();
        let session_inherit = account.session.is_none();
        let session_enabled = account
            .session
            .as_ref()
            .is_some_and(|session| session.enabled);
        let session_disabled = account
            .session
            .as_ref()
            .is_some_and(|session| !session.enabled);

        println!();
        println!(
            "{BOLD}{CYAN}Profile Wizard{RESET} {BOLD}{}{RESET}",
            account.name
        );
        println!("{DIM}Boolean rows toggle immediately. Text rows prompt for a new value. Use `-` to clear optional values.{RESET}");
        println!(
            "  1.     name                                  {}",
            wizard_value(&account.name)
        );
        println!(
            "  2.     source                                {}",
            wizard_value(account.source.as_deref().unwrap_or("-"))
        );
        println!(
            "  3.     plan                                  {}",
            wizard_value(account.plan_label.as_deref().unwrap_or("-"))
        );
        println!(
            "        account plan (read-only)             {}",
            wizard_value(
                cutex::profiles::plan_label::account_plan(&account)
                    .as_deref()
                    .or(account.plan_type.as_deref())
                    .unwrap_or("-")
            )
        );
        println!(
            "  4.     email                                 {}",
            wizard_value(account.email.as_deref().unwrap_or("-"))
        );
        println!(
            "  5.     default cli args                      {}",
            wizard_value(cli_args_label(&account.default_cli_args))
        );
        println!(
            "  6. {} runtime host                          {}",
            checkbox(is_host),
            wizard_value(runtime_description(&account.runtime))
        );
        println!(
            "  7.     docker image                          {}",
            wizard_value(docker_image.unwrap_or("-"))
        );
        println!(
            "  8.     docker user name                      {}",
            wizard_value(docker_user_name.unwrap_or("-"))
        );
        println!(
            "  9. {} proxy inherit global                   {}",
            checkbox(proxy_inherit),
            wizard_value(account_proxy_scope_label(&account, &global_config))
        );
        println!(
            " 10. {} proxy disabled override                {}",
            checkbox(proxy_disabled),
            wizard_value(proxy_config_label(account.proxy.as_ref()))
        );
        println!(
            " 11. {} proxy enabled override                 {}",
            checkbox(proxy_enabled),
            wizard_value(proxy_config_label(account.proxy.as_ref()))
        );
        println!(
            " 12.     proxy url                             {}",
            wizard_value(
                account
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.url.as_deref())
                    .unwrap_or("-")
            )
        );
        println!(
            " 13.     proxy no_proxy                        {}",
            wizard_value(
                account
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.no_proxy.as_deref())
                    .unwrap_or("-")
            )
        );
        println!(
            " 14. {} proxy force_http                       {}",
            checkbox(
                account
                    .proxy
                    .as_ref()
                    .is_some_and(|proxy| proxy.force_http_transport)
            ),
            account
                .proxy
                .as_ref()
                .map(|proxy| bool_label(proxy.force_http_transport))
                .map(wizard_value)
                .unwrap_or_else(|| wizard_value("-"))
        );
        println!(
            " 15. {} session inherit global                 {}",
            checkbox(session_inherit),
            wizard_value(session_override_label(account.session.as_ref()))
        );
        println!(
            " 16. {} session enabled override               {}",
            checkbox(session_enabled),
            wizard_value(session_override_label(account.session.as_ref()))
        );
        println!(
            " 17. {} session disabled override              {}",
            checkbox(session_disabled),
            wizard_value(session_override_label(account.session.as_ref()))
        );
        println!(" 18.     show profile details");

        let Some(choice) = read_wizard_choice(18)? else {
            println!("Done.");
            return Ok(());
        };

        let mut store = load_store()?;
        let account_index = store
            .accounts
            .iter()
            .position(|candidate| candidate.id == account_id)
            .ok_or_else(|| anyhow!("Profile disappeared while editing"))?;
        let mut renamed: Option<(String, String)> = None;

        match choice {
            1 => {
                let current_name = store.accounts[account_index].name.clone();
                let name = prompt_line("Profile name", &current_name)?;
                let name = name.trim();
                if name.is_empty() {
                    anyhow::bail!("Profile name cannot be empty");
                }
                ensure_unique_name(&store, name, Some(&account_id))?;
                if store.accounts[account_index].name != name {
                    renamed = Some((store.accounts[account_index].name.clone(), name.to_string()));
                    store.accounts[account_index].name = name.to_string();
                }
            }
            2 => {
                store.accounts[account_index].source = prompt_optional_string(
                    "Profile source",
                    store.accounts[account_index].source.as_deref(),
                )?;
            }
            3 => {
                store.accounts[account_index].plan_label = prompt_optional_string(
                    "Profile plan override",
                    store.accounts[account_index].plan_label.as_deref(),
                )?;
            }
            4 => {
                store.accounts[account_index].email = prompt_optional_string(
                    "Profile email",
                    store.accounts[account_index].email.as_deref(),
                )?;
            }
            5 => {
                let next_args = prompt_cli_args(
                    "Default CLI args",
                    &store.accounts[account_index].default_cli_args,
                )?;
                store.accounts[account_index].default_cli_args = next_args;
            }
            6 => {
                store.accounts[account_index].runtime = RuntimeConfig::Host;
            }
            7 => {
                let current_image = match &store.accounts[account_index].runtime {
                    RuntimeConfig::Docker { image, .. } => image.as_str(),
                    RuntimeConfig::Host => "cutex-base",
                };
                let image = prompt_line("Docker image", current_image)?;
                if image.trim().is_empty() || image.trim() == "-" {
                    store.accounts[account_index].runtime = RuntimeConfig::Host;
                } else {
                    let user_name = match &store.accounts[account_index].runtime {
                        RuntimeConfig::Docker { user_name, .. } => user_name.clone(),
                        RuntimeConfig::Host => None,
                    };
                    store.accounts[account_index].runtime = RuntimeConfig::Docker {
                        image: image.trim().to_string(),
                        user_name: Some(normalize_docker_user_name(user_name)?),
                    };
                }
            }
            8 => {
                let current_user_name = match &store.accounts[account_index].runtime {
                    RuntimeConfig::Docker { user_name, .. } => user_name.as_deref().unwrap_or(""),
                    RuntimeConfig::Host => "",
                };
                let value = prompt_line("Docker user name", current_user_name)?;
                match &mut store.accounts[account_index].runtime {
                    RuntimeConfig::Docker { user_name, .. } => {
                        *user_name = Some(normalize_docker_user_name(Some(value))?);
                    }
                    RuntimeConfig::Host => {
                        println!("{YELLOW}Set Docker image first.{RESET}");
                        continue;
                    }
                }
            }
            9 => {
                store.accounts[account_index].proxy = None;
            }
            10 => {
                store.accounts[account_index].proxy = Some(proxy_config_from_parts(
                    false, None, None, /*force_http_transport*/ true,
                )?);
            }
            11 => {
                let url = store.accounts[account_index]
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.url.clone())
                    .unwrap_or_else(|| "socks5h://127.0.0.1:7890".to_string());
                store.accounts[account_index].proxy = Some(proxy_config_from_parts(
                    true,
                    Some(url),
                    store.accounts[account_index]
                        .proxy
                        .as_ref()
                        .and_then(|proxy| proxy.no_proxy.clone()),
                    store.accounts[account_index]
                        .proxy
                        .as_ref()
                        .map(|proxy| proxy.force_http_transport)
                        .unwrap_or(true),
                )?);
            }
            12 => {
                let current_url = store.accounts[account_index]
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.url.as_deref());
                let url = prompt_optional_string("Profile proxy URL", current_url)?;
                store.accounts[account_index].proxy = url
                    .map(|url| {
                        proxy_config_from_parts(
                            true,
                            Some(url),
                            store.accounts[account_index]
                                .proxy
                                .as_ref()
                                .and_then(|proxy| proxy.no_proxy.clone()),
                            store.accounts[account_index]
                                .proxy
                                .as_ref()
                                .map(|proxy| proxy.force_http_transport)
                                .unwrap_or(true),
                        )
                    })
                    .transpose()?;
            }
            13 => {
                let Some(proxy) = store.accounts[account_index].proxy.as_mut() else {
                    println!("{YELLOW}Enable profile proxy first.{RESET}");
                    continue;
                };
                proxy.no_proxy =
                    prompt_optional_string("Profile proxy NO_PROXY", proxy.no_proxy.as_deref())?;
            }
            14 => {
                let Some(proxy) = store.accounts[account_index].proxy.as_mut() else {
                    println!("{YELLOW}Enable profile proxy first.{RESET}");
                    continue;
                };
                proxy.force_http_transport = !proxy.force_http_transport;
            }
            15 => {
                store.accounts[account_index].session = None;
            }
            16 => {
                store.accounts[account_index].session = Some(SessionConfig { enabled: true });
            }
            17 => {
                store.accounts[account_index].session = Some(SessionConfig { enabled: false });
            }
            18 => {
                profile_settings_presenter::print_profile_details(
                    &store,
                    &store.accounts[account_index],
                    &global_config,
                );
                continue;
            }
            _ => unreachable!(),
        }

        let reference_changes = renamed
            .as_ref()
            .map(|(old_name, new_name)| prepare_renamed_profile_references(old_name, new_name))
            .transpose()?;
        save_store(&store)?;
        if let Some(reference_changes) = reference_changes {
            reference_changes.persist()?;
        }
        println!("{GREEN}Saved.{RESET}");
    }
}

fn runtime_description(runtime: &RuntimeConfig) -> String {
    match runtime {
        RuntimeConfig::Host => "host".to_string(),
        RuntimeConfig::Docker { image, user_name } => format!(
            "docker image={} user={}",
            image,
            docker_user_name(user_name.as_deref()).unwrap_or_else(|_| default_docker_user_name())
        ),
    }
}

pub(crate) fn cmd_rename(target: &str, new_name: &str) -> anyhow::Result<()> {
    let result = rename_profile(target, new_name)?;

    println!(
        "{GREEN}Renamed{RESET} profile {BOLD}{}{RESET} -> {BOLD}{}{RESET}",
        result.old_name, result.account.name
    );
    Ok(())
}

pub(crate) fn rename_profile(target: &str, new_name: &str) -> anyhow::Result<ProfileRenameResult> {
    if new_name.trim().is_empty() {
        anyhow::bail!("Profile name cannot be empty");
    }
    let mut store = load_store()?;
    let account_id = find_account(&store, target)?
        .map(|account| account.id.clone())
        .ok_or_else(|| anyhow!("Account not found: {target}"))?;

    ensure_unique_name(&store, new_name, Some(&account_id))?;

    let old_name = {
        let account = store
            .accounts
            .iter_mut()
            .find(|account| account.id == account_id)
            .ok_or_else(|| anyhow!("Account not found after lookup: {target}"))?;
        let old_name = account.name.clone();
        account.name = new_name.to_string();
        old_name
    };

    let reference_changes = prepare_renamed_profile_references(&old_name, new_name)?;
    save_store(&store)?;
    reference_changes.persist()?;

    let account = store
        .accounts
        .iter()
        .find(|account| account.id == account_id)
        .cloned()
        .ok_or_else(|| anyhow!("Account not found after rename: {target}"))?;
    Ok(ProfileRenameResult { old_name, account })
}

pub(crate) fn cmd_remove(target: &str) -> anyhow::Result<()> {
    let result = remove_profile(target)?;

    println!(
        "{YELLOW}Removed{RESET} profile {BOLD}{}{RESET}",
        result.removed.name
    );
    if let Some(active) = result.active {
        println!("Current active profile: {}", active.name);
    }

    Ok(())
}

pub(crate) fn remove_profile(target: &str) -> anyhow::Result<ProfileRemoveResult> {
    let mut store = load_store()?;
    let account = find_account(&store, target)?
        .cloned()
        .ok_or_else(|| anyhow!("Account not found: {target}"))?;

    store
        .accounts
        .retain(|candidate| candidate.id != account.id);

    if store.active_account_id.as_deref() == Some(account.id.as_str()) {
        store.active_account_id = store.accounts.first().map(|next| next.id.clone());
    }
    let reference_changes = prepare_removed_profile_references(&account.name)?;
    save_store(&store)?;
    reference_changes.persist()?;

    let active = store.active_account_id.as_ref().and_then(|active_id| {
        store
            .accounts
            .iter()
            .find(|candidate| &candidate.id == active_id)
            .cloned()
    });
    Ok(ProfileRemoveResult {
        removed: account,
        active,
    })
}

struct ProfileReferencePersistence {
    state: QuickRunState,
    config: CodezConfig,
    sessions: CutexSessionStore,
    changes: ProfileReferenceChanges,
}

impl ProfileReferencePersistence {
    fn persist(&self) -> anyhow::Result<()> {
        for key in &self.changes.session_keys {
            persist_cutex_session_store_and_im_record(&self.sessions, key)?;
        }
        if self.changes.global_config_changed {
            save_codez_config(&self.config)?;
        }
        if self.changes.quick_state_changed {
            save_quick_state(&self.state)?;
        }
        Ok(())
    }
}

fn prepare_renamed_profile_references(
    old_name: &str,
    new_name: &str,
) -> anyhow::Result<ProfileReferencePersistence> {
    let mut state = load_quick_state();
    let mut config = load_codez_config();
    let mut sessions = load_cutex_session_store()?;
    let changes =
        rename_all_profile_references(&mut state, &mut config, &mut sessions, old_name, new_name)?;
    Ok(ProfileReferencePersistence {
        state,
        config,
        sessions,
        changes,
    })
}

fn prepare_removed_profile_references(
    removed_name: &str,
) -> anyhow::Result<ProfileReferencePersistence> {
    let mut state = load_quick_state();
    let mut config = load_codez_config();
    let mut sessions = load_cutex_session_store()?;
    let changes =
        remove_all_profile_references(&mut state, &mut config, &mut sessions, removed_name)?;
    Ok(ProfileReferencePersistence {
        state,
        config,
        sessions,
        changes,
    })
}

pub(crate) fn cmd_annotate(
    target: &str,
    source: Option<String>,
    clear_source: bool,
    plan: Option<String>,
    clear_plan: bool,
    email: Option<String>,
    clear_email: bool,
) -> anyhow::Result<()> {
    if !(source.is_some()
        || clear_source
        || plan.is_some()
        || clear_plan
        || email.is_some()
        || clear_email)
    {
        anyhow::bail!(
            "Specify at least one of --source, --clear-source, --plan, --clear-plan, --email, or --clear-email"
        );
    }

    let mut store = load_store()?;
    let account = store
        .accounts
        .iter_mut()
        .find(|account| account.name == target || account.id == target)
        .ok_or_else(|| anyhow!("Account not found: {target}"))?;

    apply_annotation(
        account,
        source,
        clear_source,
        plan,
        clear_plan,
        email,
        clear_email,
    )?;

    let name = account.name.clone();
    save_store(&store)?;

    println!("{GREEN}Updated{RESET} metadata for {BOLD}{}{RESET}", name);
    Ok(())
}

pub(crate) fn cmd_runtime(
    target: &str,
    host: bool,
    docker_image: Option<String>,
    docker_user_name: Option<String>,
) -> anyhow::Result<()> {
    let mut store = load_store()?;
    let runtime = if let Some(image) = docker_image {
        RuntimeConfig::Docker {
            image,
            user_name: Some(normalize_docker_user_name(docker_user_name)?),
        }
    } else if host {
        RuntimeConfig::Host
    } else {
        anyhow::bail!("Specify either --host or --docker-image <IMAGE>");
    };

    let account_name = {
        let account = store
            .accounts
            .iter_mut()
            .find(|account| account.name == target || account.id == target)
            .ok_or_else(|| anyhow!("Account not found: {target}"))?;
        account.runtime = runtime.clone();
        account.name.clone()
    };

    save_store(&store)?;
    println!(
        "{GREEN}Updated{RESET} runtime for {BOLD}{}{RESET} to {}",
        account_name,
        runtime_label(&runtime)
    );
    Ok(())
}

fn apply_annotation(
    account: &mut StoredAccount,
    source: Option<String>,
    clear_source: bool,
    plan: Option<String>,
    clear_plan: bool,
    email: Option<String>,
    clear_email: bool,
) -> anyhow::Result<()> {
    if clear_source {
        account.source = None;
    } else if let Some(source) = source {
        account.source = Some(source);
    }

    if clear_plan {
        account.plan_label = None;
    } else if let Some(plan) = plan {
        let plan = plan.trim();
        anyhow::ensure!(
            !plan.is_empty() && plan.len() <= 64 && !plan.chars().any(char::is_control),
            "Plan label must be 1–64 visible characters"
        );
        account.plan_label = Some(plan.to_string());
    }

    if clear_email {
        account.email = None;
    } else if let Some(email) = email {
        account.email = Some(email);
    }
    Ok(())
}

pub(crate) fn run_command(command: ProfileCommand) -> anyhow::Result<()> {
    match command {
        ProfileCommand::Models { target, refresh, apply } => cmd_profile_models(&target, refresh, apply),
        ProfileCommand::List => cmd_profile_list(),
        ProfileCommand::Show { target } => cmd_profile_show(target.as_deref()),
        ProfileCommand::Edit { target } => cmd_profile_edit(target.as_deref()),
        ProfileCommand::Use { target } => cmd_use(&target),
        ProfileCommand::Rename { target, name } => cmd_rename(&target, &name),
        ProfileCommand::Remove { target } => cmd_remove(&target),
        ProfileCommand::Copy {
            source,
            name,
            provider,
            provider_base_url,
            with_api_key,
        } => cmd_profile_copy(&source, &name, provider, provider_base_url, with_api_key),
        ProfileCommand::CloneStatusLine { from } => cmd_profile_clone_status_line(from.as_deref()),
        ProfileCommand::PinTop { target } => cmd_profile_pin(&target, true),
        ProfileCommand::PinBottom { target } => cmd_profile_pin(&target, false),
        ProfileCommand::Set {
            target,
            name,
            source,
            clear_source,
            plan,
            clear_plan,
            email,
            clear_email,
            default_cli_args,
            clear_default_cli_args,
            agent_name,
            clear_agent_name,
            host,
            docker_image,
            docker_user_name,
            proxy_url,
            proxy_no_proxy,
            proxy_force_http_transport,
            proxy_disable,
            proxy_inherit,
            session_enable,
            session_disable,
            session_inherit,
        } => cmd_profile_set(
            &target,
            name,
            source,
            clear_source,
            plan,
            clear_plan,
            email,
            clear_email,
            default_cli_args,
            clear_default_cli_args,
            agent_name,
            clear_agent_name,
            host,
            docker_image,
            docker_user_name,
            proxy_url,
            proxy_no_proxy,
            proxy_force_http_transport,
            proxy_disable,
            proxy_inherit,
            session_enable,
            session_disable,
            session_inherit,
        ),
    }
}

fn cmd_profile_models(target: &str, refresh: bool, apply: bool) -> anyhow::Result<()> {
    use cutex::profiles::model_discovery::{self, ModelCatalog};
    let store = load_store()?;
    let account = find_account(&store, target)?.context("Profile not found")?;
    let files = ensure_materialized_account_files(account)?;
    let directory = files.config_path.parent().context("profile directory missing")?;
    let mut config: toml::Value = toml::from_str(&fs::read_to_string(&files.config_path)?)?;
    let provider = config.get("model_provider").and_then(toml::Value::as_str).unwrap_or("openai");
    let base = config.get("model_providers").and_then(|v| v.get(provider))
        .and_then(|v| v.get("base_url")).and_then(toml::Value::as_str)
        .unwrap_or("https://api.openai.com/v1");
    let source = model_discovery::models_url(base)?.to_string();
    let cached_path = directory.join("discovered-models.json");
    let cached = fs::read(&cached_path).ok().and_then(|bytes| serde_json::from_slice::<ModelCatalog>(&bytes).ok())
        .filter(|catalog| catalog.source_url == source);
    let catalog = if refresh || cached.is_none() {
        let auth: serde_json::Value = serde_json::from_slice(&fs::read(&files.auth_path)?)
            .map_err(|_| anyhow!("Invalid profile credential file"))?;
        let key = auth.get("OPENAI_API_KEY").and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty()).context("API model discovery requires an API-key profile; subscriptions use native model discovery")?;
        let global = load_codez_config();
        let proxy = effective_proxy_config(account, &global)
            .map(|proxy| if proxy.enabled { proxy.url.as_deref() } else { None });
        let no_proxy = effective_proxy_config(account, &global).and_then(|proxy| proxy.no_proxy.as_deref());
        let catalog = model_discovery::fetch(base, key, proxy, no_proxy)?;
        write_private_pretty_json_atomic(&cached_path, &catalog, "discovered models")?;
        catalog
    } else { cached.unwrap() };
    let existing_path = config.get("model_catalog_json").and_then(toml::Value::as_str)
        .map(|path| { let path=std::path::PathBuf::from(path); if path.is_absolute() { path } else { directory.join(path) } });
    let existing = existing_path.and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let preset = model_discovery::preset(&catalog, existing.as_ref(), config.get("model").and_then(toml::Value::as_str));
    let preset_path = directory.join("discovered-preset.json");
    write_private_pretty_json_atomic(&preset_path, &preset, "discovered model preset")?;
    if apply {
        // Runtime receipts retain their existing immutable catalog. Updating a
        // profile must not overwrite the file a running occurrence has selected.
        let immutable = directory.join(format!("models-{}.json", uuid::Uuid::new_v4()));
        write_private_pretty_json_atomic(&immutable, &preset, "selected model catalog")?;
        config.as_table_mut().context("profile config table missing")?.insert("model_catalog_json".into(),
            toml::Value::String(immutable.canonicalize()?.to_string_lossy().into_owned()));
        write_private_bytes_atomic(&files.config_path, toml::to_string(&config)?.as_bytes())?;
    }
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "profile":account.name,"source":catalog.source_url,"fetched_at":catalog.fetched_at,
        "models":catalog.models,"preset":preset_path,"applied":apply
    }))?);
    Ok(())
}
