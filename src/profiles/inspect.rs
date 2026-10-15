//! Read-only profile inspection helpers used by CLI presenters and launch code.

use serde_json::Value;
use toml::value::Table;

use crate::config::proxy::effective_proxy_config;
use crate::config::text::read_optional_text;
use crate::profiles::materialize::materialized_account_files;
use crate::profiles::model::CliKind;
use crate::profiles::model::CodezConfig;
use crate::profiles::model::SessionConfig;
use crate::profiles::model::StoredAccount;
use crate::profiles::profile_config::parse_toml_table;

pub fn account_proxy_scope_label(account: &StoredAccount, global_config: &CodezConfig) -> String {
    let effective = effective_proxy_config(account, global_config);
    match (account.proxy.as_ref(), effective) {
        (Some(proxy), _) if !proxy.enabled => "off(profile)".to_string(),
        (Some(proxy), Some(_)) if proxy.enabled => "on(profile)".to_string(),
        (None, Some(proxy)) if proxy.enabled => "on(global)".to_string(),
        (None, Some(_)) => "off(global)".to_string(),
        (None, None) => "off".to_string(),
        _ => "off".to_string(),
    }
}

pub fn account_session_scope_label(account: &StoredAccount, global_config: &CodezConfig) -> String {
    match account.session.as_ref() {
        Some(config) if config.enabled => "on(profile)".to_string(),
        Some(_) => "off(profile)".to_string(),
        None if global_config.session.enabled => "on(global)".to_string(),
        None => "off(global)".to_string(),
    }
}

pub fn session_config_label(config: &SessionConfig) -> &'static str {
    if config.enabled {
        "enabled"
    } else {
        "disabled"
    }
}

pub fn account_model_provider(account: &StoredAccount) -> Option<String> {
    let provider_from_config = account_profile_config_table(account).and_then(|table| {
        table
            .get("model_provider")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    });
    if provider_from_config.is_some() {
        return provider_from_config;
    }

    if is_official_codex_account(account) {
        return Some("openai".to_string());
    }

    if account_uses_openai_auth(account) {
        return Some("openai".to_string());
    }

    None
}

pub fn account_profile_config_table(account: &StoredAccount) -> Option<Table> {
    let files = materialized_account_files(account).ok()?;
    let raw = read_optional_text(&files.config_path).ok().flatten()?;
    parse_toml_table(&raw).ok()
}

pub fn account_auth_payload(account: &StoredAccount) -> Option<Value> {
    let files = materialized_account_files(account).ok()?;
    let raw = read_optional_text(&files.auth_path).ok().flatten()?;
    serde_json::from_str::<Value>(&raw).ok()
}

pub fn account_model_api_base(account: &StoredAccount) -> Option<String> {
    let provider = account_model_provider(account)?;
    let explicit_base_url = account_profile_config_table(account).and_then(|table| {
        table
            .get("model_providers")
            .and_then(|value| value.as_table())
            .and_then(|providers| providers.get(provider.as_str()))
            .and_then(|value| value.as_table())
            .and_then(|provider_table| provider_table.get("base_url"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    });
    if explicit_base_url.is_some() {
        return explicit_base_url;
    }

    default_provider_api_base(&provider, account)
}

pub fn default_provider_api_base(provider: &str, account: &StoredAccount) -> Option<String> {
    if provider.eq_ignore_ascii_case("openai") {
        let uses_chatgpt_auth = account_uses_chatgpt_auth(account);
        return Some(if uses_chatgpt_auth {
            "https://chatgpt.com/backend-api/codex".to_string()
        } else {
            "https://api.openai.com/v1".to_string()
        });
    }

    if provider.eq_ignore_ascii_case("ollama") {
        return Some(default_oss_provider_base_url(11434));
    }

    if provider.eq_ignore_ascii_case("lmstudio") {
        return Some(default_oss_provider_base_url(1234));
    }

    None
}

fn default_oss_provider_base_url(default_port: u16) -> String {
    if let Ok(base_url) = std::env::var("CODEX_OSS_BASE_URL") {
        if !base_url.trim().is_empty() {
            return base_url;
        }
    }

    let port = std::env::var("CODEX_OSS_PORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(default_port);
    format!("http://localhost:{port}/v1")
}

pub fn account_uses_chatgpt_auth(account: &StoredAccount) -> bool {
    if is_official_codex_account(account) {
        return true;
    }

    account_auth_payload(account)
        .and_then(|json| {
            json.get("tokens")
                .and_then(|value| value.as_object())
                .cloned()
        })
        .is_some()
}

pub fn is_official_codex_account(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex && account.source.as_deref() == Some("official")
}

pub fn account_uses_openai_auth(account: &StoredAccount) -> bool {
    account_auth_payload(account).is_some_and(|json| {
        json.get("tokens")
            .and_then(|value| value.as_object())
            .is_some()
            || json.get("OPENAI_API_KEY").is_some()
            || json.get("openai_api_key").is_some()
    })
}

pub fn account_uses_api_key_auth(account: &StoredAccount) -> bool {
    account_auth_payload(account).is_some_and(|json| {
        json.get("OPENAI_API_KEY")
            .or_else(|| json.get("openai_api_key"))
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|key| !key.is_empty())
    })
}

pub fn account_status_line_len(account: &StoredAccount) -> Option<usize> {
    let table = account_profile_config_table(account)?;
    table
        .get("tui")
        .and_then(|value| value.as_table())
        .and_then(|tui| tui.get("status_line"))
        .and_then(|value| value.as_array())
        .map(Vec::len)
}
