//! Provider-specific launch environment construction.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::agent_bus::launch::launch_agent_bus_envs;
use crate::agent_bus::service::{agent_bus_base_url, agent_bus_port};
use crate::config::env::{
    CLAUDE_CONFIG_DIR_ENV_VAR, CODEX_AUTH_FILE_ENV_VAR, CODEX_CONFIG_FILE_ENV_VAR,
    CODEX_CUSTOM_STATUS_ITEMS_FILE_ENV_VAR, CODEX_INSTALL_DIR_ENV_VAR,
    CODEX_LAUNCH_PROFILE_EMAIL_ENV_VAR, CODEX_LAUNCH_PROFILE_ENV_VAR,
    CODEX_LAUNCH_PROFILE_SOURCE_ENV_VAR, CODEX_LAUNCH_PROFILE_TYPE_ENV_VAR,
    CODEX_LAUNCH_RUNTIME_ENV_VAR,
    CUTEX_CLAUDE_USAGE_REFRESH_SECONDS_ENV_VAR,
};
use crate::config::paths::config_dir;
use crate::config::proxy::{effective_proxy_config, proxy_envs};
use crate::config::store::load_codez_config;
use crate::launch::command::LaunchCommand;
use crate::profiles::model::{runtime_label, CliKind, CodezConfig, RuntimeConfig, StoredAccount};

pub struct LaunchEnvContext<'a> {
    pub global_config: &'a CodezConfig,
    pub desktop_notify_url: Option<String>,
    pub agent_bus_url: String,
    pub host_id: String,
}

pub fn default_launch_env_context<'a>(
    global_config: &'a CodezConfig,
    host_id: String,
) -> LaunchEnvContext<'a> {

    LaunchEnvContext {
        global_config,
        desktop_notify_url: None,
        agent_bus_url: agent_bus_base_url(agent_bus_port(global_config)),
        host_id,
    }
}

pub fn profile_launch_envs(
    account: &StoredAccount,
    auth_path: &str,
    config_path: &str,
    custom_status_items_path: &str,
    install_dir: Option<String>,
    api_key_auth_path: Option<&Path>,
    agent_mode: bool,
    agent_groups: &[String],
    context: &LaunchEnvContext<'_>,
) -> Vec<(String, String)> {
    let mut envs = match account.cli_kind {
        // Managed DSH builds a restricted environment in runtime::dsh.
        // This legacy CLI helper must not read auth files or add bus credentials.
        CliKind::Dsh => return Vec::new(),
        // r266: managed Kimi launches build their own environment (kimi-runtime).
        CliKind::Kimi => return Vec::new(),
        CliKind::Claude => claude_launch_envs(account, context.global_config),
        CliKind::Codex => codex_specific_launch_envs(
            account,
            auth_path,
            config_path,
            custom_status_items_path,
            install_dir,
            api_key_auth_path,
        ),
    };

    if agent_mode
        && context.global_config.agent_bus_enabled
        && matches!(account.runtime, RuntimeConfig::Host)
    {
        envs.extend(launch_agent_bus_envs(
            context.global_config,
            account,
            agent_groups,
            context.agent_bus_url.clone(),
            context.host_id.clone(),
        ));
    }

    envs
}

pub struct ApplyProfileLaunchEnvOptions<'a> {
    pub account: &'a StoredAccount,
    pub auth_path: &'a str,
    pub config_path: &'a str,
    pub custom_status_items_path: &'a str,
    pub install_dir: Option<String>,
    pub api_key_auth_path: Option<&'a Path>,
    pub agent_mode: bool,
    pub agent_groups: &'a [String],
    pub context: &'a LaunchEnvContext<'a>,
}

pub fn apply_profile_launch_envs(
    mut launch: LaunchCommand,
    options: ApplyProfileLaunchEnvOptions<'_>,
) -> LaunchCommand {
    for (key, value) in profile_launch_envs(
        options.account,
        options.auth_path,
        options.config_path,
        options.custom_status_items_path,
        options.install_dir,
        options.api_key_auth_path,
        options.agent_mode,
        options.agent_groups,
        options.context,
    ) {
        launch = launch.env(key, value);
    }
    launch
}

pub fn claude_launch_envs(
    account: &StoredAccount,
    global_config: &CodezConfig,
) -> Vec<(String, String)> {
    let claude_config_dir = materialized_claude_config_dir(account);
    let mut envs = vec![
        (
            CLAUDE_CONFIG_DIR_ENV_VAR.to_string(),
            claude_config_dir.to_string_lossy().to_string(),
        ),
        (
            CUTEX_CLAUDE_USAGE_REFRESH_SECONDS_ENV_VAR.to_string(),
            (u64::from(global_config.claude_usage_refresh_minutes) * 60).to_string(),
        ),
        (
            CODEX_LAUNCH_PROFILE_ENV_VAR.to_string(),
            account.name.clone(),
        ),
        (
            CODEX_LAUNCH_RUNTIME_ENV_VAR.to_string(),
            runtime_label(&account.runtime).to_string(),
        ),
    ];

    // No API key in the launch env: a legacy `<config dir>/api_key` file is ignored. The env reaches
    // the agent's Bash tool (and the adapter refuses ANTHROPIC_API_KEY anyway); API accounts pass
    // their key through the adapter's `apiKeyHelper` (claude_accounts).

    let provider_json_path = claude_config_dir.join("provider.json");
    if let Ok(raw) = fs::read_to_string(&provider_json_path) {
        if let Ok(val) = serde_json::from_str::<Value>(&raw) {
            if let Some(url) = val.get("base_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    envs.push(("ANTHROPIC_BASE_URL".to_string(), url.to_string()));
                }
            }
        }
    }

    envs.extend(proxy_envs(
        effective_proxy_config(account, &global_config),
        Some(&account.runtime),
    ));
    envs
}

pub fn codex_specific_launch_envs(
    account: &StoredAccount,
    auth_path: &str,
    config_path: &str,
    custom_status_items_path: &str,
    install_dir: Option<String>,
    api_key_auth_path: Option<&Path>,
) -> Vec<(String, String)> {
    let global_config = load_codez_config();
    let mut envs = vec![
        (
            CODEX_LAUNCH_PROFILE_ENV_VAR.to_string(),
            account.name.clone(),
        ),
        (
            CODEX_LAUNCH_RUNTIME_ENV_VAR.to_string(),
            runtime_label(&account.runtime).to_string(),
        ),
        (
            CODEX_LAUNCH_PROFILE_SOURCE_ENV_VAR.to_string(),
            account.source.as_deref().unwrap_or("unknown").to_string(),
        ),
        (
            CODEX_LAUNCH_PROFILE_TYPE_ENV_VAR.to_string(),
            account
                .plan_type
                .as_deref()
                .unwrap_or("unknown")
                .to_string(),
        ),
        (
            CODEX_LAUNCH_PROFILE_EMAIL_ENV_VAR.to_string(),
            account.email.as_deref().unwrap_or("-").to_string(),
        ),
        (CODEX_AUTH_FILE_ENV_VAR.to_string(), auth_path.to_string()),
        (
            CODEX_CONFIG_FILE_ENV_VAR.to_string(),
            config_path.to_string(),
        ),
        (
            CODEX_CUSTOM_STATUS_ITEMS_FILE_ENV_VAR.to_string(),
            custom_status_items_path.to_string(),
        ),
    ];
    if let Some(install_dir) = install_dir {
        envs.push((CODEX_INSTALL_DIR_ENV_VAR.to_string(), install_dir));
    }
    if account.source.as_deref() == Some("api-key") {
        if let Some(api_key) = api_key_auth_path.and_then(codex_api_key_from_auth_file) {
            envs.push(("OPENAI_API_KEY".to_string(), api_key));
        }
    }
    envs.extend(proxy_envs(
        effective_proxy_config(account, &global_config),
        Some(&account.runtime),
    ));
    envs
}

pub fn materialized_claude_config_dir(account: &StoredAccount) -> PathBuf {
    let base = config_dir().unwrap_or_else(|_| PathBuf::from("."));
    base.join("profiles").join(&account.id).join("claude")
}

pub(crate) fn codex_api_key_from_auth_file(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    json.get("OPENAI_API_KEY")
        .or_else(|| json.get("openai_api_key"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dsh_legacy_launch_does_not_read_auth_or_inject_bus_env() {
        let account: StoredAccount = serde_json::from_value(serde_json::json!({
            "id":"dsh-profile","name":"DSH","cli_kind":"dsh"
        })).unwrap();
        let config = CodezConfig { agent_bus_enabled:true, ..Default::default() };
        let context = LaunchEnvContext {
            global_config:&config, desktop_notify_url:None,
            agent_bus_url:"http://127.0.0.1:1".into(), host_id:"test-host".into(),
        };
        let envs = profile_launch_envs(&account, "/unused/auth", "/unused/config", "/unused/status",
            None, Some(Path::new("/unused/auth")), true, &["test".into()], &context);
        assert!(envs.is_empty(), "DSH environments belong to the managed product runtime");
    }

    use super::*;

    #[test]
    fn claude_launch_env_contains_bounded_usage_refresh_interval() {
        let account: StoredAccount = serde_json::from_value(serde_json::json!({
            "id": "claude-profile",
            "name": "claude",
            "cli_kind": "claude"
        }))
        .expect("claude profile");
        let global_config = CodezConfig {
            claude_usage_refresh_minutes: 2,
            ..CodezConfig::default()
        };

        let envs = claude_launch_envs(&account, &global_config);
        assert!(envs.contains(&(
            CUTEX_CLAUDE_USAGE_REFRESH_SECONDS_ENV_VAR.to_string(),
            "120".to_string()
        )));
    }

    #[test]
    fn claude_launch_env_never_carries_a_legacy_api_key() {
        let account: StoredAccount = serde_json::from_value(serde_json::json!({
            "id": "claude-key-profile",
            "name": "claude-key",
            "cli_kind": "claude"
        }))
        .expect("claude profile");
        let dir = materialized_claude_config_dir(&account);
        fs::create_dir_all(&dir).expect("config dir");
        fs::write(dir.join("api_key"), "sk-fake-legacy-key\n").expect("legacy key file");

        let envs = claude_launch_envs(&account, &CodezConfig::default());
        assert!(envs.iter().all(|(name, value)| name != "ANTHROPIC_API_KEY" && !value.contains("sk-fake-legacy-key")));
    }
}
