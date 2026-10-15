//! Profile-scoped Codex `config.toml` extraction, merge, and normalization.

use std::path::Path;

use anyhow::Context;
use toml::value::Table;

use crate::config::text::read_optional_text;
use crate::config::text::write_optional_text;
use crate::profiles::inspect::account_model_provider;
use crate::profiles::inspect::default_provider_api_base;
use crate::profiles::model::CliKind;
use crate::profiles::model::StoredAccount;

pub const PROFILE_CONFIG_SCALAR_KEYS: &[&str] = &[
    "cutex_provider_mode",
    "cli_auth_credentials_store",
    "forced_login_method",
    "model",
    "model_provider",
    "model_catalog_json",
    "model_context_window",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
    "model_reasoning_effort",
    "model_reasoning_summary",
    "model_supports_reasoning_summaries",
    "model_verbosity",
    "plan_mode_reasoning_effort",
    "review_model",
    "service_tier",
    // Native `WebSearchMode`; Gemini relay profiles set "disabled".
    "web_search",
    // Native `ToolSchemaCompat` (r53+): "off" | "strict".
    "tool_schema_compat",
];
pub const PROFILE_CONFIG_TABLE_KEYS: &[&str] = &["shell_environment_policy"];
pub const DEFAULT_CUTEX_STATUS_LINE: [&str; 7] = [
    "cutex_welcome",
    "cutex_profile",
    "model-with-reasoning",
    "current-dir",
    "context-used",
    "weekly-limit",
    "cutex_notification",
];
pub const PROFILE_CONFIG_TUI_KEYS: &[&str] = &[
    "status_line",
    "status_line_use_colors",
    "session_picker_provider_filter",
];
/// Profile-scoped keys inside shared native tables; only these leaves move,
/// sibling keys of the same tables stay shared.
pub const PROFILE_CONFIG_NESTED_KEYS: &[&[&str]] =
    &[&crate::profiles::codex_profile::UPDATE_PLAN_ENABLED_PATH];
pub const PROFILE_LOCAL_MODEL_CATALOG_FILE: &str = "models.json";
pub const PROFILE_ROUTING_ENV_EXCLUDE_PATTERNS: [&str; 2] =
    ["CODEX_AUTH_FILE", "CODEX_CONFIG_FILE"];
pub const TOOL_PROXY_ENV_EXCLUDE_PATTERNS: [&str; 13] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "PIP_PROXY",
    "NPM_CONFIG_PROXY",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "YARN_PROXY",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "BUNDLE_HTTP_PROXY",
    "BUNDLE_HTTPS_PROXY",
];

pub fn extract_profile_config_toml(config_toml: &str) -> anyhow::Result<Option<String>> {
    let root = parse_toml_table(config_toml)?;
    let mut profile = Table::new();

    for &key in PROFILE_CONFIG_SCALAR_KEYS {
        copy_toml_key(&root, &mut profile, key);
    }
    for &key in PROFILE_CONFIG_TABLE_KEYS {
        copy_toml_key(&root, &mut profile, key);
    }
    for &key in PROFILE_CONFIG_TUI_KEYS {
        copy_toml_nested_key(&root, &mut profile, &["tui", key]);
    }
    for &path in PROFILE_CONFIG_NESTED_KEYS {
        copy_toml_nested_key(&root, &mut profile, path);
    }

    if let Some(model_provider) = root.get("model_provider").and_then(|value| value.as_str()) {
        if let Some(provider_value) = root
            .get("model_providers")
            .and_then(|value| value.as_table())
            .and_then(|providers| providers.get(model_provider))
            .cloned()
        {
            let providers = profile
                .entry("model_providers".to_string())
                .or_insert_with(|| toml::Value::Table(Table::new()));
            if let Some(providers_table) = providers.as_table_mut() {
                providers_table.insert(model_provider.to_string(), provider_value);
            }
        }
    }

    if profile.is_empty() {
        return Ok(None);
    }

    Ok(Some(toml::to_string_pretty(&profile)?))
}

pub fn merge_and_write_config_toml(
    path: &Path,
    profile_config_toml: Option<&str>,
    exclude_tool_proxy_envs: bool,
) -> anyhow::Result<()> {
    let existing = read_optional_text(path)?;
    let mut merged = existing
        .as_deref()
        .map(parse_toml_table)
        .transpose()?
        .unwrap_or_default();

    strip_profile_config_keys(&mut merged);

    if let Some(profile_config_toml) = profile_config_toml {
        let profile = parse_toml_table(profile_config_toml)?;

        for &key in PROFILE_CONFIG_SCALAR_KEYS {
            if let Some(value) = profile.get(key).cloned() {
                merged.insert(key.to_string(), value);
            }
        }
        for &key in PROFILE_CONFIG_TABLE_KEYS {
            copy_toml_key(&profile, &mut merged, key);
        }
        for &key in PROFILE_CONFIG_TUI_KEYS {
            copy_toml_nested_key(&profile, &mut merged, &["tui", key]);
        }
        for &path in PROFILE_CONFIG_NESTED_KEYS {
            copy_toml_nested_key(&profile, &mut merged, path);
        }

        if let Some(provider_name) = profile
            .get("model_provider")
            .and_then(|value| value.as_str())
        {
            let provider_value = profile
                .get("model_providers")
                .and_then(|value| value.as_table())
                .and_then(|providers| providers.get(provider_name))
                .cloned();

            if let Some(provider_value) = provider_value {
                let providers = merged
                    .entry("model_providers".to_string())
                    .or_insert_with(|| toml::Value::Table(Table::new()));
                let providers_table = providers.as_table_mut().ok_or_else(|| {
                    anyhow::anyhow!("config.toml key `model_providers` must be a table")
                })?;
                providers_table.insert(provider_name.to_string(), provider_value);
            }
        }
    }

    reconcile_tool_proxy_env_excludes(&mut merged, exclude_tool_proxy_envs)?;

    if merged.is_empty() {
        return write_optional_text(path, None);
    }

    write_optional_text(path, Some(&toml::to_string_pretty(&merged)?))
}

pub fn parse_toml_table(contents: &str) -> anyhow::Result<Table> {
    let value: toml::Value = toml::from_str(contents).context("Failed to parse config.toml")?;
    value
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("config.toml root must be a table"))
}

pub fn profile_model_catalog_setting(contents: &str) -> anyhow::Result<Option<String>> {
    let root = parse_toml_table(contents)?;
    let Some(value) = root.get("model_catalog_json") else {
        return Ok(None);
    };
    let value = value.as_str().ok_or_else(|| {
        anyhow::anyhow!("config.toml key `model_catalog_json` must be a string path")
    })?;
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("config.toml key `model_catalog_json` cannot be empty");
    }
    Ok(Some(value.to_string()))
}

pub fn profile_uses_local_model_catalog(contents: &str) -> anyhow::Result<bool> {
    Ok(
        profile_model_catalog_setting(contents)?.is_some_and(|value| {
            matches!(
                value.replace('\\', "/").as_str(),
                PROFILE_LOCAL_MODEL_CATALOG_FILE | "./models.json"
            )
        }),
    )
}

pub fn read_profile_specific_config_table(path: &Path) -> anyhow::Result<Table> {
    let Some(existing) = read_optional_text(path)? else {
        return Ok(Table::new());
    };
    let extracted = extract_profile_config_toml(&existing)?.and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    });
    match extracted {
        Some(value) => parse_toml_table(&value),
        None => Ok(Table::new()),
    }
}

pub fn build_copied_profile_config(
    source_account: &StoredAccount,
    source_config_contents: Option<String>,
    provider: Option<String>,
    provider_base_url: Option<String>,
) -> anyhow::Result<Option<String>> {
    let provider = provider
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let provider_base_url = provider_base_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if provider.is_none() && provider_base_url.is_none() {
        return Ok(source_config_contents);
    }

    let mut config_table = match source_config_contents.as_deref() {
        Some(contents) => parse_toml_table(contents)?,
        None => Table::new(),
    };

    let target_provider = provider.or_else(|| {
        if provider_base_url.is_some() {
            account_model_provider(source_account)
        } else {
            None
        }
    });
    let Some(target_provider) = target_provider else {
        anyhow::bail!(
            "Unable to determine which provider to update. Pass --provider <ID> or copy from a profile that already has a provider."
        );
    };

    let previous_provider = config_table
        .get("model_provider")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    if previous_provider.as_deref() != Some(target_provider.as_str()) {
        config_table.insert(
            "model_provider".to_string(),
            toml::Value::String(target_provider.clone()),
        );
        if let Some(previous_provider) = previous_provider.as_deref() {
            remove_model_provider_entry(&mut config_table, previous_provider);
        }
    }

    if let Some(base_url) = provider_base_url {
        let providers_value = config_table
            .entry("model_providers".to_string())
            .or_insert_with(|| toml::Value::Table(Table::new()));
        let providers_table = providers_value
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("config.toml key `model_providers` must be a table"))?;
        let provider_value = providers_table
            .entry(target_provider.clone())
            .or_insert_with(|| toml::Value::Table(Table::new()));
        let provider_table = provider_value.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "config.toml key `model_providers.{}` must be a table",
                target_provider
            )
        })?;
        ensure_model_provider_name(provider_table, &target_provider);
        if source_account.source.as_deref() == Some("api-key") {
            ensure_model_provider_uses_api_key_env(provider_table);
        }
        provider_table.insert("base_url".to_string(), toml::Value::String(base_url));
    } else {
        let has_provider_config = config_table
            .get("model_providers")
            .and_then(|value| value.as_table())
            .and_then(|providers| providers.get(target_provider.as_str()))
            .is_some();
        let has_default_provider_base =
            default_provider_api_base(&target_provider, source_account).is_some();
        if !has_provider_config && !has_default_provider_base {
            anyhow::bail!(
                "Provider `{}` requires --provider-base-url because the source profile does not define it and cutex has no built-in default for it",
                target_provider
            );
        }
    }

    Ok(Some(toml::to_string_pretty(&config_table)?))
}

fn strip_profile_config_keys(root: &mut Table) {
    let provider_to_remove = root
        .get("model_provider")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    for &key in PROFILE_CONFIG_SCALAR_KEYS {
        root.remove(key);
    }
    for &key in PROFILE_CONFIG_TABLE_KEYS {
        root.remove(key);
    }
    for &key in PROFILE_CONFIG_TUI_KEYS {
        remove_toml_nested_key(root, &["tui", key]);
    }
    for &path in PROFILE_CONFIG_NESTED_KEYS {
        remove_toml_nested_key(root, path);
    }

    if let Some(provider_name) = provider_to_remove {
        if let Some(providers) = root
            .get_mut("model_providers")
            .and_then(|value| value.as_table_mut())
        {
            providers.remove(&provider_name);
            if providers.is_empty() {
                root.remove("model_providers");
            }
        }
    }
}

fn reconcile_tool_proxy_env_excludes(
    root: &mut Table,
    exclude_tool_proxy_envs: bool,
) -> anyhow::Result<()> {
    if exclude_tool_proxy_envs && !root.contains_key("shell_environment_policy") {
        root.insert(
            "shell_environment_policy".to_string(),
            toml::Value::Table(Table::new()),
        );
    }

    let remove_shell_policy = {
        let Some(policy_value) = root.get_mut("shell_environment_policy") else {
            return Ok(());
        };
        let policy = policy_value.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!("config.toml key `shell_environment_policy` must be a table")
        })?;

        let mut exclude_values = match policy.remove("exclude") {
            Some(toml::Value::Array(values)) => values,
            Some(_) => {
                anyhow::bail!("config.toml key `shell_environment_policy.exclude` must be an array")
            }
            None => Vec::new(),
        };

        exclude_values.retain(|value| {
            value
                .as_str()
                .map(|pattern| !is_managed_tool_proxy_env_exclude_pattern(pattern))
                .unwrap_or(true)
        });

        if exclude_tool_proxy_envs {
            for pattern in TOOL_PROXY_ENV_EXCLUDE_PATTERNS {
                let already_present = exclude_values.iter().any(|value| {
                    value
                        .as_str()
                        .map(|existing| existing.eq_ignore_ascii_case(pattern))
                        .unwrap_or(false)
                });
                if !already_present {
                    exclude_values.push(toml::Value::String(pattern.to_string()));
                }
            }
        }

        if exclude_values.is_empty() {
            policy.remove("exclude");
        } else {
            policy.insert("exclude".to_string(), toml::Value::Array(exclude_values));
        }

        policy.is_empty()
    };

    if remove_shell_policy {
        root.remove("shell_environment_policy");
    }

    Ok(())
}

fn is_managed_tool_proxy_env_exclude_pattern(pattern: &str) -> bool {
    TOOL_PROXY_ENV_EXCLUDE_PATTERNS
        .iter()
        .any(|managed| managed.eq_ignore_ascii_case(pattern))
}

fn copy_toml_key(source: &Table, target: &mut Table, key: &str) {
    if let Some(value) = source.get(key).cloned() {
        target.insert(key.to_string(), value);
    }
}

/// Copies the value at a nested path (`section[.subsection].key`, at least two
/// segments), creating the parent tables in `target` as needed.
fn copy_toml_nested_key(source: &Table, target: &mut Table, path: &[&str]) {
    let Some((key, parents)) = path.split_last().filter(|(_, parents)| !parents.is_empty())
    else {
        return;
    };

    let mut source_table = source;
    for parent in parents {
        match source_table.get(*parent).and_then(|value| value.as_table()) {
            Some(table) => source_table = table,
            None => return,
        }
    }
    let Some(value) = source_table.get(*key).cloned() else {
        return;
    };

    let mut target_table = target;
    for parent in parents {
        let Some(table) = target_table
            .entry(parent.to_string())
            .or_insert_with(|| toml::Value::Table(Table::new()))
            .as_table_mut()
        else {
            return;
        };
        target_table = table;
    }
    target_table.insert(key.to_string(), value);
}

/// Removes the value at a nested path (at least two segments) and drops the
/// parent tables on that path that are left empty.
fn remove_toml_nested_key(root: &mut Table, path: &[&str]) {
    fn remove(table: &mut Table, path: &[&str]) {
        match path {
            [] => {}
            [key] => {
                table.remove(*key);
            }
            [section, rest @ ..] => {
                let remove_section = table
                    .get_mut(*section)
                    .and_then(|value| value.as_table_mut())
                    .map(|child| {
                        remove(child, rest);
                        child.is_empty()
                    })
                    .unwrap_or(false);
                if remove_section {
                    table.remove(*section);
                }
            }
        }
    }
    if path.len() >= 2 {
        remove(root, path);
    }
}

pub fn normalize_profile_config_for_account(
    account: &StoredAccount,
    profile_config_toml: Option<String>,
) -> anyhow::Result<Option<String>> {
    if account.cli_kind != CliKind::Codex {
        return Ok(profile_config_toml);
    }

    let mut root = profile_config_toml
        .as_deref()
        .map(parse_toml_table)
        .transpose()?
        .unwrap_or_default();

    ensure_profile_routing_env_excludes(&mut root)?;
    ensure_default_status_line(&mut root)?;

    if account.source.as_deref() == Some("api-key") {
        if let Some(provider_name) = root
            .get("model_provider")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        {
            if let Some(provider_table) = root
                .get_mut("model_providers")
                .and_then(|value| value.as_table_mut())
                .and_then(|providers| providers.get_mut(&provider_name))
                .and_then(|value| value.as_table_mut())
            {
                ensure_model_provider_name(provider_table, &provider_name);
                ensure_model_provider_uses_api_key_env(provider_table);
            }
        }
    }

    if root.is_empty() {
        Ok(None)
    } else {
        Ok(Some(toml::to_string_pretty(&root)?))
    }
}

fn ensure_profile_routing_env_excludes(root: &mut Table) -> anyhow::Result<()> {
    let policy = root
        .entry("shell_environment_policy".to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("config.toml key `shell_environment_policy` must be a table")
        })?;

    let mut exclude_values = match policy.remove("exclude") {
        Some(toml::Value::Array(values)) => values,
        Some(_) => {
            anyhow::bail!("config.toml key `shell_environment_policy.exclude` must be an array")
        }
        None => Vec::new(),
    };

    exclude_values.retain(|value| {
        value
            .as_str()
            .map(|pattern| {
                !PROFILE_ROUTING_ENV_EXCLUDE_PATTERNS
                    .iter()
                    .any(|managed| managed.eq_ignore_ascii_case(pattern))
            })
            .unwrap_or(true)
    });
    exclude_values.extend(
        PROFILE_ROUTING_ENV_EXCLUDE_PATTERNS
            .iter()
            .map(|pattern| toml::Value::String((*pattern).to_string())),
    );
    policy.insert("exclude".to_string(), toml::Value::Array(exclude_values));
    Ok(())
}

fn ensure_default_status_line(root: &mut Table) -> anyhow::Result<()> {
    let tui = root
        .entry("tui".to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()));
    let tui_table = tui
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml key `tui` must be a table"))?;

    tui_table
        .entry("status_line".to_string())
        .or_insert_with(default_status_line_value);
    tui_table
        .entry("status_line_use_colors".to_string())
        .or_insert(toml::Value::Boolean(true));

    Ok(())
}

fn default_status_line_value() -> toml::Value {
    toml::Value::Array(
        DEFAULT_CUTEX_STATUS_LINE
            .iter()
            .map(|item| toml::Value::String((*item).to_string()))
            .collect(),
    )
}

pub fn remove_model_provider_entry(root: &mut Table, provider_name: &str) {
    let remove_section = root
        .get_mut("model_providers")
        .and_then(|value| value.as_table_mut())
        .map(|providers| {
            providers.remove(provider_name);
            providers.is_empty()
        })
        .unwrap_or(false);
    if remove_section {
        root.remove("model_providers");
    }
}

pub fn ensure_model_provider_name(provider_table: &mut Table, provider_name: &str) {
    let has_non_empty_name = provider_table
        .get("name")
        .and_then(|value| value.as_str())
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !has_non_empty_name {
        provider_table.insert(
            "name".to_string(),
            toml::Value::String(provider_name.to_string()),
        );
    }
}

pub fn ensure_model_provider_uses_api_key_env(provider_table: &mut Table) {
    provider_table.insert(
        "env_key".to_string(),
        toml::Value::String("OPENAI_API_KEY".to_string()),
    );
    provider_table.insert(
        "requires_openai_auth".to_string(),
        toml::Value::Boolean(false),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::RuntimeConfig;

    fn sample_account(source: Option<&str>) -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: "acct-test".to_string(),
            name: "test".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: source.map(str::to_string),
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
    fn normalization_excludes_profile_routing_env_from_tool_shells() {
        let existing = r#"
[shell_environment_policy]
inherit = "all"
exclude = ["KEEP_ME", "codex_auth_file", "CODEX_CONFIG_FILE"]
"#;

        let normalized = normalize_profile_config_for_account(
            &sample_account(Some("api-key")),
            Some(existing.to_string()),
        )
        .expect("profile config should normalize")
        .expect("Codex profile config should remain materialized");
        let table = parse_toml_table(&normalized).expect("normalized config should parse");
        let policy = table
            .get("shell_environment_policy")
            .and_then(toml::Value::as_table)
            .expect("shell environment policy should exist");
        let excludes = policy
            .get("exclude")
            .and_then(toml::Value::as_array)
            .expect("profile routing excludes should exist")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>();

        assert_eq!(
            excludes,
            vec!["KEEP_ME", "CODEX_AUTH_FILE", "CODEX_CONFIG_FILE"]
        );
        assert_eq!(
            policy.get("inherit").and_then(toml::Value::as_str),
            Some("all")
        );
    }

    #[test]
    fn normalization_does_not_add_codex_routing_policy_to_claude_profiles() {
        let mut account = sample_account(None);
        account.cli_kind = CliKind::Claude;
        let existing = Some("model = \"claude-test\"\n".to_string());

        assert_eq!(
            normalize_profile_config_for_account(&account, existing.clone())
                .expect("Claude profile config should pass through"),
            existing
        );
    }

    #[test]
    fn extraction_keeps_model_settings_and_the_complete_selected_provider() {
        let source = r#"
model = "deepseek-v4-flash"
model_provider = "deepseek"
forced_login_method = "api"
model_reasoning_effort = "high"
model_catalog_json = "models.json"
service_tier = "default"
unrelated_global_key = "do-not-copy"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
request_max_retries = 8
stream_max_retries = 9
stream_idle_timeout_ms = 450000
websocket_connect_timeout_ms = 20000
supports_websockets = false
future_provider_key = "preserve-me"

[model_providers.deepseek.query_params]
region = "test"
"#;

        let extracted = extract_profile_config_toml(source)
            .expect("profile config should parse")
            .expect("profile config should be present");
        let table = parse_toml_table(&extracted).expect("extracted config should parse");

        for key in [
            "model",
            "model_provider",
            "forced_login_method",
            "model_reasoning_effort",
            "model_catalog_json",
            "service_tier",
        ] {
            assert!(table.contains_key(key), "missing profile key {key}");
        }
        assert!(!table.contains_key("unrelated_global_key"));
        let provider = table
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("deepseek"))
            .and_then(toml::Value::as_table)
            .expect("selected provider should be copied");
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
            Some("preserve-me")
        );
        assert!(provider.get("query_params").is_some());
    }

    #[test]
    fn extraction_and_merge_preserve_selected_profile_mode_without_global_leakage() {
        let source = r#"
cutex_provider_mode = "selected_profile_v2"
cli_auth_credentials_store = "file"
model = "glm-5.3"
model_provider = "GLM"
model_reasoning_effort = "max"
unrelated_global_key = "do-not-copy"

[model_providers.GLM]
name = "GLM"
base_url = "https://api.example.test/"
env_key = "OPENAI_API_KEY"
requires_openai_auth = false
wire_api = "responses"
"#;

        let extracted = extract_profile_config_toml(source)
            .expect("profile config should parse")
            .expect("profile config should be present");
        let extracted_table = parse_toml_table(&extracted).expect("extracted config should parse");
        assert_eq!(
            extracted_table
                .get("cutex_provider_mode")
                .and_then(toml::Value::as_str),
            Some("selected_profile_v2")
        );
        assert!(!extracted_table.contains_key("unrelated_global_key"));

        let materialized =
            std::env::temp_dir().join(format!("cutex-profile-mode-{}.toml", uuid::Uuid::new_v4()));
        struct RemoveFileOnDrop(std::path::PathBuf);
        impl Drop for RemoveFileOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _guard = RemoveFileOnDrop(materialized.clone());
        merge_and_write_config_toml(&materialized, Some(&extracted), false)
            .expect("materialized config should merge");
        let merged =
            std::fs::read_to_string(&materialized).expect("materialized config should be readable");
        let merged_table = parse_toml_table(&merged).expect("merged config should parse");

        assert_eq!(
            merged_table
                .get("cutex_provider_mode")
                .and_then(toml::Value::as_str),
            Some("selected_profile_v2")
        );
        assert_eq!(
            merged_table.get("model").and_then(toml::Value::as_str),
            Some("glm-5.3")
        );
        assert!(!merged_table.contains_key("unrelated_global_key"));

        let selected = crate::launch::selected_profile::Config::parse(&merged)
            .expect("durable stock selected-profile config should parse");
        assert_eq!(selected.cutex_provider_mode, "selected_profile_v2");
    }

    #[test]
    fn stripping_removes_the_complete_previous_profile_scope() {
        let source = r#"
model = "old-model"
model_provider = "old-provider"
model_reasoning_effort = "xhigh"
model_catalog_json = "old-models.json"
unrelated_global_key = "keep"

[shell_environment_policy]
inherit = "all"

[model_providers.old-provider]
name = "Old"

[model_providers.shared]
name = "Shared"

[tui]
status_line = ["model"]
animations = true
"#;
        let mut table = parse_toml_table(source).expect("config should parse");

        strip_profile_config_keys(&mut table);

        for key in PROFILE_CONFIG_SCALAR_KEYS {
            assert!(!table.contains_key(*key), "profile key leaked: {key}");
        }
        assert!(!table.contains_key("shell_environment_policy"));
        assert_eq!(
            table
                .get("model_providers")
                .and_then(toml::Value::as_table)
                .and_then(|providers| providers.get("shared"))
                .and_then(toml::Value::as_table)
                .and_then(|provider| provider.get("name"))
                .and_then(toml::Value::as_str),
            Some("Shared")
        );
        assert!(table
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("old-provider"))
            .is_none());
        assert_eq!(
            table
                .get("tui")
                .and_then(toml::Value::as_table)
                .and_then(|tui| tui.get("animations"))
                .and_then(toml::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn copied_profile_config_returns_source_without_overrides() {
        let source = Some("model_provider = \"custom\"\n".to_string());

        let copied = build_copied_profile_config(&sample_account(None), source.clone(), None, None)
            .expect("copy config should succeed");

        assert_eq!(copied, source);
    }

    #[test]
    fn copied_profile_config_replaces_provider_and_removes_old_provider_table() {
        let source = r#"
model_provider = "custom"

[model_providers.custom]
base_url = "https://old.example/v1"
"#;

        let copied = build_copied_profile_config(
            &sample_account(None),
            Some(source.to_string()),
            Some("openai".to_string()),
            None,
        )
        .expect("copy config should succeed")
        .expect("copied config should be present");
        let table = parse_toml_table(&copied).expect("copied config should parse");

        assert_eq!(
            table.get("model_provider").and_then(|value| value.as_str()),
            Some("openai")
        );
        assert!(
            table
                .get("model_providers")
                .and_then(|value| value.as_table())
                .and_then(|providers| providers.get("custom"))
                .is_none(),
            "old provider table should be removed"
        );
    }

    #[test]
    fn copied_profile_config_sets_api_key_provider_base() {
        let copied = build_copied_profile_config(
            &sample_account(Some("api-key")),
            None,
            Some("custom".to_string()),
            Some("https://new.example/v1".to_string()),
        )
        .expect("copy config should succeed")
        .expect("copied config should be present");
        let table = parse_toml_table(&copied).expect("copied config should parse");
        let provider_table = table
            .get("model_providers")
            .and_then(|value| value.as_table())
            .and_then(|providers| providers.get("custom"))
            .and_then(|value| value.as_table())
            .expect("custom provider table should exist");

        assert_eq!(
            provider_table
                .get("base_url")
                .and_then(|value| value.as_str()),
            Some("https://new.example/v1")
        );
        assert_eq!(
            provider_table.get("name").and_then(|value| value.as_str()),
            Some("custom")
        );
        assert_eq!(
            provider_table
                .get("env_key")
                .and_then(|value| value.as_str()),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            provider_table
                .get("requires_openai_auth")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn copied_profile_config_requires_base_for_unknown_provider_without_config() {
        let error = build_copied_profile_config(
            &sample_account(None),
            None,
            Some("custom".to_string()),
            None,
        )
        .expect_err("unknown provider without config should fail");

        assert!(
            error.to_string().contains("requires --provider-base-url"),
            "unexpected error: {error:#}"
        );
    }
}
