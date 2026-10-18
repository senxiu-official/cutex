use anyhow::anyhow;
use anyhow::Context;

use cutex::agent_bus::service::validate_agent_bus_port;
use cutex::agent_bus::service::DEFAULT_AGENT_BUS_PORT;
use cutex::cli::args::{GlobalCommand, ProxyCommand};
use cutex::config::global_settings::{
    apply_global_config_patch, parse_notify_events, parse_notify_user_message_content,
    parse_rate_limit_mode, ConfigValueUpdate, GlobalConfigPatch,
};
use cutex::config::proxy::*;
use cutex::config::store::load_codez_config;
use cutex::config::store::load_codez_config_checked;
use cutex::config::store::save_codez_config;
use cutex::profiles::inspect::session_config_label;
use cutex::profiles::lookup::find_account;
use cutex::profiles::lookup::resolve_configured_default_profile_name;
use cutex::profiles::store::save_store;
use cutex::ui::format::bool_label;
use cutex::ui::format::optional_u64_label;
use cutex::ui::format::proxy_config_label;

use super::account_store::load_store;
use super::profile_settings_presenter;
use super::prompt::*;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const DEFAULT_NOTIFY_EVENTS: &str =
    "task_completed,thinking_too_long,waiting_approval,connection_error,session_exit,session_started,session_startup_idle,user_message_sent,user_message_dispatched,turn_started,turn_completed,turn_interrupted,turn_failed,approval_requested,approval_resolved,thread_closed,context_compacted,rate_limit_warning,rate_limit_prompt_shown";

pub(crate) fn global(command: GlobalCommand) -> anyhow::Result<()> {
    match command {
        GlobalCommand::Show => cmd_global_show(),
        GlobalCommand::Edit => cmd_global_edit(),
        GlobalCommand::Set {
            session_enable,
            default_profile,
            clear_default_profile,
            default_profile_direct_launch,
            proxy_url,
            proxy_no_proxy,
            proxy_clear,
            agent_bus_enable,
            agent_bus_port,
            agent_bus_token,
        } => cmd_global_set(GlobalSetOptions {
            docker_use_sudo: None,
            session_enable,
            default_profile,
            clear_default_profile,
            default_profile_direct_launch,
            proxy_url,
            proxy_no_proxy,
            proxy_clear,
            notify_idle_timeout: None,
            notify_composer_idle_timeout: None,
            notify_approval_timeout: None,
            notify_startup_idle_timeout: None,
            notify_events: None,
            notify_user_message_content: None,
            notify_user_message_preview_chars: None,
            rate_limit_threshold_warning_mode: None,
            rate_limit_model_nudge_mode: None,
            agent_bus_enable,
            agent_bus_port,
            agent_bus_token,
        }),
    }
}

pub(crate) fn proxy(command: ProxyCommand) -> anyhow::Result<()> {
    match command {
        ProxyCommand::Show { profile } => cmd_proxy_show(profile),
        ProxyCommand::Set {
            url,
            no_proxy,
        } => cmd_proxy_set(url, no_proxy),
        ProxyCommand::Clear => cmd_proxy_clear(),
        ProxyCommand::SetProfile {
            profile,
            url,
            no_proxy,
        } => cmd_proxy_set_profile(&profile, url, no_proxy),
        ProxyCommand::DisableProfile { profile } => cmd_proxy_disable_profile(&profile),
        ProxyCommand::ClearProfile { profile } => cmd_proxy_clear_profile(&profile),
    }
}

pub(crate) fn cmd_global_edit() -> anyhow::Result<()> {
    cmd_global_show()?;
    println!("Edit ~/.cutex/config.json; use Cutex Settings for profiles, network and service configuration.");
    Ok(())
}

pub(crate) struct GlobalSetOptions {
    pub(crate) docker_use_sudo: Option<bool>,
    pub(crate) session_enable: Option<bool>,
    pub(crate) default_profile: Option<String>,
    pub(crate) clear_default_profile: bool,
    pub(crate) default_profile_direct_launch: Option<bool>,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_no_proxy: Option<String>,
    pub(crate) proxy_clear: bool,
    pub(crate) notify_idle_timeout: Option<u64>,
    pub(crate) notify_composer_idle_timeout: Option<u64>,
    pub(crate) notify_approval_timeout: Option<u64>,
    pub(crate) notify_startup_idle_timeout: Option<u64>,
    pub(crate) notify_events: Option<String>,
    pub(crate) notify_user_message_content: Option<String>,
    pub(crate) notify_user_message_preview_chars: Option<u64>,
    pub(crate) rate_limit_threshold_warning_mode: Option<String>,
    pub(crate) rate_limit_model_nudge_mode: Option<String>,
    pub(crate) agent_bus_enable: Option<bool>,
    pub(crate) agent_bus_port: Option<u16>,
    pub(crate) agent_bus_token: Option<String>,
}

pub(crate) fn cmd_global_show() -> anyhow::Result<()> {
    let config = load_codez_config_checked()?;
    println!("Default profile: {}", config.default_profile.as_deref().unwrap_or("automatic"));
    println!("Skip default-launch picker: {}", config.default_profile_direct_launch);
    println!("Agent Bus port: {}", config.agent_bus_port.unwrap_or(DEFAULT_AGENT_BUS_PORT));
    println!("Agent Bus token: {}", if config.agent_bus_token.is_some() { "(set)" } else { "(unset)" });
    println!("Config: ~/.cutex/config.json\nTheme: ~/.cutex/theme.json\nNotification labels/styles: ~/.cutex/notifications/config.json");
    Ok(())
}

pub(crate) fn cmd_global_set(options: GlobalSetOptions) -> anyhow::Result<()> {
    let GlobalSetOptions {
        docker_use_sudo,
        session_enable,
        default_profile,
        clear_default_profile,
        default_profile_direct_launch,
        proxy_url,
        proxy_no_proxy,
        proxy_clear,
        notify_idle_timeout,
        notify_composer_idle_timeout,
        notify_approval_timeout,
        notify_startup_idle_timeout,
        notify_events,
        notify_user_message_content,
        notify_user_message_preview_chars,
        rate_limit_threshold_warning_mode,
        rate_limit_model_nudge_mode,
        agent_bus_enable,
        agent_bus_port,
        agent_bus_token,
    } = options;
    if proxy_no_proxy.is_some() && proxy_url.is_none() {
        anyhow::bail!("--proxy-no-proxy requires --proxy-url");
    }
    if let Some(port) = agent_bus_port {
        validate_agent_bus_port(port)?;
    }

    if docker_use_sudo.is_none()
        && session_enable.is_none()
        && default_profile.is_none()
        && !clear_default_profile
        && default_profile_direct_launch.is_none()
        && proxy_url.is_none()
        && !proxy_clear
        && notify_idle_timeout.is_none()
        && notify_composer_idle_timeout.is_none()
        && notify_approval_timeout.is_none()
        && notify_startup_idle_timeout.is_none()
        && notify_events.is_none()
        && notify_user_message_content.is_none()
        && notify_user_message_preview_chars.is_none()
        && rate_limit_threshold_warning_mode.is_none()
        && rate_limit_model_nudge_mode.is_none()
        && agent_bus_enable.is_none()
        && agent_bus_port.is_none()
        && agent_bus_token.is_none()
    {
        anyhow::bail!(
                    "No changes requested. Provide --docker-use-sudo <BOOL>, --session-enable <BOOL>, --default-profile <PROFILE>, --clear-default-profile, --default-profile-direct-launch <BOOL>, --proxy-url <URL>, --proxy-clear, --notify-idle-timeout <SECS>, --notify-composer-idle-timeout <SECS>, --notify-approval-timeout <SECS>, --notify-startup-idle-timeout <SECS>, --notify-events <CSV>, --notify-user-message-content <MODE>, --notify-user-message-preview-chars <CHARS>, --rate-limit-threshold-warning-mode <MODE>, --rate-limit-model-nudge-mode <MODE>, --agent-bus-enable <BOOL>, --agent-bus-port <PORT>, or --agent-bus-token <TOKEN>."
                );
    }

    let mut config = load_codez_config_checked()?;
    let proxy = if proxy_clear {
        ConfigValueUpdate::Clear
    } else if let Some(url) = proxy_url {
        ConfigValueUpdate::Set(proxy_config_from_parts(
            true,
            Some(url),
            proxy_no_proxy,
        )?)
    } else {
        ConfigValueUpdate::Unchanged
    };
    let notify_events_update =
        requested_optional_update(notify_events.map(|events| parse_notify_events(&events)));
    let notify_message_content_update = requested_optional_update(
        notify_user_message_content
            .map(|content| parse_notify_user_message_content(&content))
            .transpose()?,
    );
    let rate_limit_threshold_update = requested_optional_update(
        rate_limit_threshold_warning_mode
            .map(|mode| parse_rate_limit_mode(&mode))
            .transpose()?,
    );
    let rate_limit_model_nudge_update = requested_optional_update(
        rate_limit_model_nudge_mode
            .map(|mode| parse_rate_limit_mode(&mode))
            .transpose()?,
    );
    let agent_bus_token_update =
        requested_optional_update(agent_bus_token.map(|token| parse_optional_string(&token)));
    let default_profile_update = if clear_default_profile {
        ConfigValueUpdate::Clear
    } else if let Some(target) = default_profile {
        let store = load_store()?;
        ConfigValueUpdate::Set(
            resolve_configured_default_profile_name(&store, Some(target))?
                .ok_or_else(|| anyhow!("Default profile cannot be empty"))?,
        )
    } else {
        ConfigValueUpdate::Unchanged
    };
    let changed = apply_global_config_patch(
        &mut config,
        &GlobalConfigPatch {
            docker_use_sudo,
            session_enabled: session_enable,
            default_profile: default_profile_update,
            default_profile_direct_launch,
            proxy,
            notify_service_idle_timeout_secs: requested_value_update(notify_idle_timeout),
            notify_service_composer_idle_timeout_secs: requested_value_update(
                notify_composer_idle_timeout,
            ),
            notify_service_approval_timeout_secs: requested_value_update(notify_approval_timeout),
            notify_service_startup_idle_timeout_secs: requested_value_update(
                notify_startup_idle_timeout,
            ),
            notify_service_events: notify_events_update,
            notify_service_user_message_content: notify_message_content_update,
            notify_service_user_message_preview_chars: requested_value_update(
                notify_user_message_preview_chars,
            ),
            rate_limit_threshold_warning_mode: rate_limit_threshold_update,
            rate_limit_model_nudge_mode: rate_limit_model_nudge_update,
            agent_bus_enabled: agent_bus_enable,
            agent_bus_port: requested_value_update(agent_bus_port),
            agent_bus_token: agent_bus_token_update,
            ..GlobalConfigPatch::default()
        },
    )?;

    if changed {
        save_codez_config(&config)?;
        println!("{GREEN}Updated{RESET} global settings");
    } else {
        println!("{YELLOW}No changes{RESET} global settings already match requested values");
    }
    profile_settings_presenter::print_global_settings(&config);

    Ok(())
}

pub(crate) fn cmd_proxy_show(profile: Option<String>) -> anyhow::Result<()> {
    let global_config = load_codez_config();
    if let Some(profile) = profile {
        let store = load_store()?;
        let account = find_account(&store, &profile)?
            .ok_or_else(|| anyhow!("Account not found: {profile}"))?;
        println!("{BOLD}{CYAN}Proxy for profile{RESET} {}", account.name);
        println!(
            "{DIM}profile{RESET} {}",
            proxy_config_label(account.proxy.as_ref())
        );
        println!(
            "{DIM}global{RESET}  {}",
            proxy_config_label(global_config.proxy.as_ref())
        );
        println!(
            "{DIM}effective{RESET} {}",
            proxy_config_label(effective_proxy_config(account, &global_config))
        );
    } else {
        println!("{BOLD}{CYAN}Global Proxy{RESET}");
        println!("{}", proxy_config_label(global_config.proxy.as_ref()));
    }
    Ok(())
}

pub(crate) fn cmd_proxy_set(
    url: String,
    no_proxy: Option<String>,
) -> anyhow::Result<()> {
    let mut config = load_codez_config_checked()?;
    set_global_proxy_config(&mut config, url, no_proxy)?;
    save_codez_config(&config)?;
    println!(
        "{GREEN}Updated{RESET} global proxy: {}",
        proxy_config_label(config.proxy.as_ref())
    );
    Ok(())
}

pub(crate) fn cmd_proxy_clear() -> anyhow::Result<()> {
    let mut config = load_codez_config_checked()?;
    clear_global_proxy_config(&mut config);
    save_codez_config(&config)?;
    println!("{YELLOW}Cleared{RESET} global proxy");
    Ok(())
}

pub(crate) fn cmd_proxy_set_profile(
    profile: &str,
    url: String,
    no_proxy: Option<String>,
) -> anyhow::Result<()> {
    let mut store = load_store()?;
    let account = store
        .accounts
        .iter_mut()
        .find(|account| account.name == profile || account.id == profile)
        .ok_or_else(|| anyhow!("Account not found: {profile}"))?;
    set_account_proxy_config(account, url, no_proxy)?;
    let name = account.name.clone();
    let label = proxy_config_label(account.proxy.as_ref());
    save_store(&store)?;
    println!("{GREEN}Updated{RESET} proxy for {BOLD}{name}{RESET}: {label}");
    Ok(())
}

pub(crate) fn cmd_proxy_disable_profile(profile: &str) -> anyhow::Result<()> {
    let mut store = load_store()?;
    let account = store
        .accounts
        .iter_mut()
        .find(|account| account.name == profile || account.id == profile)
        .ok_or_else(|| anyhow!("Account not found: {profile}"))?;
    disable_account_proxy_config(account)?;
    let name = account.name.clone();
    save_store(&store)?;
    println!("{YELLOW}Disabled{RESET} proxy for {BOLD}{name}{RESET}");
    Ok(())
}

pub(crate) fn cmd_proxy_clear_profile(profile: &str) -> anyhow::Result<()> {
    let mut store = load_store()?;
    let account = store
        .accounts
        .iter_mut()
        .find(|account| account.name == profile || account.id == profile)
        .ok_or_else(|| anyhow!("Account not found: {profile}"))?;
    clear_account_proxy_config(account);
    let name = account.name.clone();
    save_store(&store)?;
    println!("{YELLOW}Cleared{RESET} proxy override for {BOLD}{name}{RESET}");
    Ok(())
}

fn requested_value_update<T>(value: Option<T>) -> ConfigValueUpdate<T> {
    value.map_or(ConfigValueUpdate::Unchanged, ConfigValueUpdate::Set)
}

fn requested_optional_update<T>(value: Option<Option<T>>) -> ConfigValueUpdate<T> {
    match value {
        None => ConfigValueUpdate::Unchanged,
        Some(Some(value)) => ConfigValueUpdate::Set(value),
        Some(None) => ConfigValueUpdate::Clear,
    }
}
