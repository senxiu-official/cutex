//! Typed inspection and mutation for profile-scoped Codex model settings.

use toml::value::Table;

use crate::config::global_settings::ConfigValueUpdate;
use crate::profiles::deepseek;
use crate::profiles::profile_config::parse_toml_table;

pub const DEFAULT_REQUEST_MAX_RETRIES: u64 = 4;
pub const DEFAULT_STREAM_MAX_RETRIES: u64 = 5;
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
pub const MAX_PROVIDER_RETRIES: u64 = 100;
/// Values cute-codex accepts for `model_reasoning_summary` (its
/// `ReasoningSummary`); unset means the model catalog default.
pub const MODEL_REASONING_SUMMARY_VALUES: &[&str] = &["auto", "concise", "detailed", "none"];
/// Nested key cute-codex reads to register the `update_plan` tool
/// (`[tools.update_plan] enabled = true`); unset means disabled.
pub const UPDATE_PLAN_ENABLED_PATH: [&str; 3] = ["tools", "update_plan", "enabled"];
/// Values cute-codex accepts for the root `web_search` key (its
/// `WebSearchMode`); unset means the native default ("cached").
pub const WEB_SEARCH_VALUES: &[&str] = &["disabled", "cached", "indexed", "live"];
/// Values cute-codex (r53+) accepts for the root `tool_schema_compat` key (its
/// `ToolSchemaCompat`); unset means "off" (tool schemas sent unchanged).
/// "strict" gives every tool-parameter `enum` one `type` shared by all of its
/// values, for providers whose function declarations refuse mixed, typeless
/// or nullable enums. Older natives ignore the key.
pub const TOOL_SCHEMA_COMPAT_VALUES: &[&str] = &["off", "strict"];
/// Provider-table path of the native r53 switch that sends base instructions
/// in the top-level Responses `instructions` field instead of a `developer`
/// input message (D4). Unset = native default (false). Natives before r53
/// ignore the `capabilities` block.
pub const PROVIDER_INSTRUCTIONS_FIELD_PATH: [&str; 2] = ["capabilities", "instructions_field"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexProfileConfigSnapshot {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub forced_login_method: Option<String>,
    pub model_reasoning_effort: Option<String>,
    pub model_reasoning_summary: Option<String>,
    pub update_plan_enabled: Option<bool>,
    pub web_search: Option<String>,
    pub tool_schema_compat: Option<String>,
    pub model_catalog_json: Option<String>,
    pub model_context_window: Option<u64>,
    pub model_auto_compact_token_limit: Option<u64>,
    pub provider_name: Option<String>,
    pub provider_base_url: Option<String>,
    pub provider_env_key: Option<String>,
    pub provider_wire_api: Option<String>,
    pub request_max_retries: Option<u64>,
    pub stream_max_retries: Option<u64>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub requires_openai_auth: Option<bool>,
    pub supports_websockets: Option<bool>,
    pub provider_instructions_field: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexProfileConfigPatch {
    pub apply_deepseek_preset: bool,
    pub model: ConfigValueUpdate<String>,
    pub model_provider: ConfigValueUpdate<String>,
    pub forced_login_method: ConfigValueUpdate<String>,
    pub model_reasoning_effort: ConfigValueUpdate<String>,
    pub model_reasoning_summary: ConfigValueUpdate<String>,
    pub update_plan_enabled: ConfigValueUpdate<bool>,
    pub web_search: ConfigValueUpdate<String>,
    pub tool_schema_compat: ConfigValueUpdate<String>,
    pub model_catalog_json: ConfigValueUpdate<String>,
    pub model_context_window: ConfigValueUpdate<u64>,
    pub model_auto_compact_token_limit: ConfigValueUpdate<u64>,
    pub provider_name: ConfigValueUpdate<String>,
    pub provider_base_url: ConfigValueUpdate<String>,
    pub provider_env_key: ConfigValueUpdate<String>,
    pub provider_wire_api: ConfigValueUpdate<String>,
    pub request_max_retries: ConfigValueUpdate<u64>,
    pub stream_max_retries: ConfigValueUpdate<u64>,
    pub stream_idle_timeout_ms: ConfigValueUpdate<u64>,
    pub websocket_connect_timeout_ms: ConfigValueUpdate<u64>,
    pub requires_openai_auth: ConfigValueUpdate<bool>,
    pub supports_websockets: ConfigValueUpdate<bool>,
    pub provider_instructions_field: ConfigValueUpdate<bool>,
}

impl CodexProfileConfigPatch {
    pub fn is_empty(&self) -> bool {
        !self.apply_deepseek_preset
            && matches!(self.model, ConfigValueUpdate::Unchanged)
            && matches!(self.model_provider, ConfigValueUpdate::Unchanged)
            && matches!(self.forced_login_method, ConfigValueUpdate::Unchanged)
            && matches!(self.model_reasoning_effort, ConfigValueUpdate::Unchanged)
            && matches!(self.model_reasoning_summary, ConfigValueUpdate::Unchanged)
            && matches!(self.update_plan_enabled, ConfigValueUpdate::Unchanged)
            && matches!(self.web_search, ConfigValueUpdate::Unchanged)
            && matches!(self.tool_schema_compat, ConfigValueUpdate::Unchanged)
            && matches!(self.model_catalog_json, ConfigValueUpdate::Unchanged)
            && matches!(self.model_context_window, ConfigValueUpdate::Unchanged)
            && matches!(self.model_auto_compact_token_limit, ConfigValueUpdate::Unchanged)
            && matches!(self.provider_name, ConfigValueUpdate::Unchanged)
            && matches!(self.provider_base_url, ConfigValueUpdate::Unchanged)
            && matches!(self.provider_env_key, ConfigValueUpdate::Unchanged)
            && matches!(self.provider_wire_api, ConfigValueUpdate::Unchanged)
            && matches!(self.request_max_retries, ConfigValueUpdate::Unchanged)
            && matches!(self.stream_max_retries, ConfigValueUpdate::Unchanged)
            && matches!(self.stream_idle_timeout_ms, ConfigValueUpdate::Unchanged)
            && matches!(
                self.websocket_connect_timeout_ms,
                ConfigValueUpdate::Unchanged
            )
            && matches!(self.requires_openai_auth, ConfigValueUpdate::Unchanged)
            && matches!(self.supports_websockets, ConfigValueUpdate::Unchanged)
            && matches!(self.provider_instructions_field, ConfigValueUpdate::Unchanged)
    }
}

pub fn inspect_codex_profile_config(
    config_toml: Option<&str>,
) -> anyhow::Result<CodexProfileConfigSnapshot> {
    let root = config_toml
        .map(parse_toml_table)
        .transpose()?
        .unwrap_or_default();
    let model_provider = optional_string(&root, "model_provider")?;
    let provider = model_provider
        .as_deref()
        .and_then(|provider_id| selected_provider_table(&root, provider_id));

    Ok(CodexProfileConfigSnapshot {
        model: optional_string(&root, "model")?,
        model_provider,
        forced_login_method: optional_string(&root, "forced_login_method")?,
        model_reasoning_effort: optional_string(&root, "model_reasoning_effort")?,
        model_reasoning_summary: optional_string(&root, "model_reasoning_summary")?,
        update_plan_enabled: optional_nested_bool(&root, &UPDATE_PLAN_ENABLED_PATH)?,
        web_search: optional_string(&root, "web_search")?,
        tool_schema_compat: optional_string(&root, "tool_schema_compat")?,
        model_catalog_json: optional_string(&root, "model_catalog_json")?,
        model_context_window: optional_provider_u64(Some(&root), "model_context_window")?,
        model_auto_compact_token_limit: optional_provider_u64(
            Some(&root),
            "model_auto_compact_token_limit",
        )?,
        provider_name: optional_provider_string(provider, "name")?,
        provider_base_url: optional_provider_string(provider, "base_url")?,
        provider_env_key: optional_provider_string(provider, "env_key")?,
        provider_wire_api: optional_provider_string(provider, "wire_api")?,
        request_max_retries: optional_provider_u64(provider, "request_max_retries")?,
        stream_max_retries: optional_provider_u64(provider, "stream_max_retries")?,
        stream_idle_timeout_ms: optional_provider_u64(provider, "stream_idle_timeout_ms")?,
        websocket_connect_timeout_ms: optional_provider_u64(
            provider,
            "websocket_connect_timeout_ms",
        )?,
        requires_openai_auth: optional_provider_bool(provider, "requires_openai_auth")?,
        supports_websockets: optional_provider_bool(provider, "supports_websockets")?,
        provider_instructions_field: provider
            .map(|provider| optional_nested_bool(provider, &PROVIDER_INSTRUCTIONS_FIELD_PATH))
            .transpose()?
            .flatten(),
    })
}

pub fn apply_codex_profile_config_patch(
    existing_config_toml: Option<&str>,
    patch: &CodexProfileConfigPatch,
) -> anyhow::Result<(Option<String>, bool)> {
    if patch.is_empty() {
        return Ok((existing_config_toml.map(str::to_string), false));
    }

    let mut root = existing_config_toml
        .map(parse_toml_table)
        .transpose()?
        .unwrap_or_default();
    let before = root.clone();

    if patch.apply_deepseek_preset {
        deepseek::apply_profile_preset(&mut root, None)?;
    }

    let previous_provider = optional_string(&root, "model_provider")?;
    apply_optional_string(&mut root, "model", &patch.model, false)?;
    apply_optional_string(
        &mut root,
        "forced_login_method",
        &patch.forced_login_method,
        false,
    )?;
    apply_optional_string(
        &mut root,
        "model_reasoning_effort",
        &patch.model_reasoning_effort,
        false,
    )?;
    validate_reasoning_summary_update(&patch.model_reasoning_summary)?;
    apply_optional_string(
        &mut root,
        "model_reasoning_summary",
        &patch.model_reasoning_summary,
        false,
    )?;
    apply_nested_bool(&mut root, &UPDATE_PLAN_ENABLED_PATH, &patch.update_plan_enabled)?;
    if let ConfigValueUpdate::Set(value) = &patch.web_search {
        anyhow::ensure!(
            WEB_SEARCH_VALUES.contains(&value.as_str()),
            "unsupported web_search mode `{value}` (expected one of {})",
            WEB_SEARCH_VALUES.join(", ")
        );
    }
    apply_optional_string(&mut root, "web_search", &patch.web_search, false)?;
    if let ConfigValueUpdate::Set(value) = &patch.tool_schema_compat {
        anyhow::ensure!(
            TOOL_SCHEMA_COMPAT_VALUES.contains(&value.as_str()),
            "unsupported tool_schema_compat `{value}` (expected one of {})",
            TOOL_SCHEMA_COMPAT_VALUES.join(", ")
        );
    }
    apply_optional_string(&mut root, "tool_schema_compat", &patch.tool_schema_compat, false)?;
    apply_optional_string(
        &mut root,
        "model_catalog_json",
        &patch.model_catalog_json,
        false,
    )?;
    apply_token_count_update(&mut root, "model_context_window", &patch.model_context_window)?;
    apply_token_count_update(
        &mut root,
        "model_auto_compact_token_limit",
        &patch.model_auto_compact_token_limit,
    )?;
    apply_model_provider_update(
        &mut root,
        previous_provider.as_deref(),
        &patch.model_provider,
    )?;

    let effective_provider = optional_string(&root, "model_provider")?;
    if matches!(patch.model_reasoning_summary, ConfigValueUpdate::Set(_))
        && !reasoning_summary_supported(effective_provider.as_deref())
    {
        anyhow::bail!(
            "model_reasoning_summary is not supported on DeepSeek profiles (the DeepSeek preset removes it)"
        );
    }
    let provider_patch_requested = provider_patch_requested(patch);
    if provider_patch_requested {
        let provider_id = effective_provider.as_deref().ok_or_else(|| {
            anyhow::anyhow!("Set a model provider before editing provider options")
        })?;
        if is_builtin_model_provider(provider_id) {
            anyhow::bail!(
                "Provider options cannot override built-in provider `{provider_id}`; set a custom provider ID first"
            );
        }
        let provider = ensure_selected_provider_table(&mut root, provider_id)?;
        apply_optional_string(provider, "name", &patch.provider_name, true)?;
        apply_optional_string(provider, "base_url", &patch.provider_base_url, false)?;
        apply_optional_string(provider, "env_key", &patch.provider_env_key, false)?;
        validate_wire_api_update(&patch.provider_wire_api)?;
        apply_optional_string(provider, "wire_api", &patch.provider_wire_api, false)?;
        apply_retry_update(provider, "request_max_retries", &patch.request_max_retries)?;
        apply_retry_update(provider, "stream_max_retries", &patch.stream_max_retries)?;
        apply_optional_u64(
            provider,
            "stream_idle_timeout_ms",
            &patch.stream_idle_timeout_ms,
        )?;
        apply_optional_u64(
            provider,
            "websocket_connect_timeout_ms",
            &patch.websocket_connect_timeout_ms,
        )?;
        apply_optional_bool(
            provider,
            "requires_openai_auth",
            &patch.requires_openai_auth,
        );
        apply_optional_bool(provider, "supports_websockets", &patch.supports_websockets);
        apply_nested_bool(
            provider,
            &PROVIDER_INSTRUCTIONS_FIELD_PATH,
            &patch.provider_instructions_field,
        )?;
    }

    let changed = root != before;
    let rendered = (!root.is_empty())
        .then(|| toml::to_string_pretty(&root))
        .transpose()?;
    Ok((rendered, changed))
}

pub fn is_builtin_model_provider(provider_id: &str) -> bool {
    matches!(
        provider_id.trim().to_ascii_lowercase().as_str(),
        "openai" | "ollama" | "lmstudio" | "amazon-bedrock"
    )
}

/// Whether a profile on `provider` (its `model_provider`) may set
/// `model_reasoning_summary`: the DeepSeek preset removes that key, so
/// DeepSeek profiles do not offer it.
pub fn reasoning_summary_supported(provider: Option<&str>) -> bool {
    !(provider.is_some_and(deepseek::is_deepseek_provider)
        && deepseek::preset_removes_key("model_reasoning_summary"))
}

fn provider_patch_requested(patch: &CodexProfileConfigPatch) -> bool {
    !matches!(patch.provider_name, ConfigValueUpdate::Unchanged)
        || !matches!(patch.provider_base_url, ConfigValueUpdate::Unchanged)
        || !matches!(patch.provider_env_key, ConfigValueUpdate::Unchanged)
        || !matches!(patch.provider_wire_api, ConfigValueUpdate::Unchanged)
        || !matches!(patch.request_max_retries, ConfigValueUpdate::Unchanged)
        || !matches!(patch.stream_max_retries, ConfigValueUpdate::Unchanged)
        || !matches!(patch.stream_idle_timeout_ms, ConfigValueUpdate::Unchanged)
        || !matches!(
            patch.websocket_connect_timeout_ms,
            ConfigValueUpdate::Unchanged
        )
        || !matches!(patch.requires_openai_auth, ConfigValueUpdate::Unchanged)
        || !matches!(patch.supports_websockets, ConfigValueUpdate::Unchanged)
        || !matches!(patch.provider_instructions_field, ConfigValueUpdate::Unchanged)
}

fn apply_model_provider_update(
    root: &mut Table,
    previous_provider: Option<&str>,
    update: &ConfigValueUpdate<String>,
) -> anyhow::Result<()> {
    let next_provider = match update {
        ConfigValueUpdate::Unchanged => return Ok(()),
        ConfigValueUpdate::Set(value) => Some(required_string(value, "Model provider")?),
        ConfigValueUpdate::Clear => None,
    };
    if previous_provider == next_provider.as_deref() {
        return Ok(());
    }

    let previous_table = previous_provider.and_then(|provider| take_provider_table(root, provider));
    match next_provider {
        Some(provider) => {
            root.insert(
                "model_provider".to_string(),
                toml::Value::String(provider.clone()),
            );
            if !is_builtin_model_provider(&provider) {
                let mut moved = previous_table
                    .and_then(|value| value.as_table().cloned())
                    .unwrap_or_default();
                if let Some(existing) =
                    take_provider_table(root, &provider).and_then(|value| value.as_table().cloned())
                {
                    moved.extend(existing);
                }
                if !moved.is_empty() {
                    insert_provider_table(root, &provider, moved)?;
                }
            }
        }
        None => {
            root.remove("model_provider");
        }
    }
    remove_empty_provider_section(root);
    Ok(())
}

fn apply_optional_string(
    table: &mut Table,
    key: &str,
    update: &ConfigValueUpdate<String>,
    required_when_set: bool,
) -> anyhow::Result<()> {
    match update {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Clear => {
            table.remove(key);
        }
        ConfigValueUpdate::Set(value) => {
            let value = if required_when_set {
                required_string(value, key)?
            } else {
                optional_string_value(value)
                    .ok_or_else(|| anyhow::anyhow!("{key} cannot be empty when set"))?
            };
            table.insert(key.to_string(), toml::Value::String(value));
        }
    }
    Ok(())
}

fn apply_token_count_update(
    table: &mut Table,
    key: &str,
    update: &ConfigValueUpdate<u64>,
) -> anyhow::Result<()> {
    if let ConfigValueUpdate::Set(value) = update {
        anyhow::ensure!(*value > 0, "{key} must be a positive token count");
    }
    apply_optional_u64(table, key, update)
}

fn apply_retry_update(
    table: &mut Table,
    key: &str,
    update: &ConfigValueUpdate<u64>,
) -> anyhow::Result<()> {
    if let ConfigValueUpdate::Set(value) = update {
        if *value > MAX_PROVIDER_RETRIES {
            anyhow::bail!("{key} must be between 0 and {MAX_PROVIDER_RETRIES}");
        }
    }
    apply_optional_u64(table, key, update)?;
    Ok(())
}

fn apply_optional_u64(
    table: &mut Table,
    key: &str,
    update: &ConfigValueUpdate<u64>,
) -> anyhow::Result<()> {
    match update {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Clear => {
            table.remove(key);
        }
        ConfigValueUpdate::Set(value) => {
            let value = i64::try_from(*value)
                .map_err(|_| anyhow::anyhow!("{key} is too large for a TOML integer"))?;
            table.insert(key.to_string(), toml::Value::Integer(value));
        }
    }
    Ok(())
}

fn apply_optional_bool(table: &mut Table, key: &str, update: &ConfigValueUpdate<bool>) {
    match update {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Clear => {
            table.remove(key);
        }
        ConfigValueUpdate::Set(value) => {
            table.insert(key.to_string(), toml::Value::Boolean(*value));
        }
    }
}

fn validate_wire_api_update(update: &ConfigValueUpdate<String>) -> anyhow::Result<()> {
    if let ConfigValueUpdate::Set(value) = update {
        if value.trim() != "responses" {
            anyhow::bail!("wire_api only supports `responses` in cute-codex 0.144.1");
        }
    }
    Ok(())
}

fn validate_reasoning_summary_update(update: &ConfigValueUpdate<String>) -> anyhow::Result<()> {
    if let ConfigValueUpdate::Set(value) = update {
        if !MODEL_REASONING_SUMMARY_VALUES.contains(&value.trim()) {
            anyhow::bail!(
                "model_reasoning_summary must be one of {}",
                MODEL_REASONING_SUMMARY_VALUES.join(", ")
            );
        }
    }
    Ok(())
}

fn optional_string(table: &Table, key: &str) -> anyhow::Result<Option<String>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("config.toml key `{key}` must be a string"))?;
    Ok(optional_string_value(value))
}

/// Reads a boolean at a nested table path (`a.b.key`); absent = `None`.
fn optional_nested_bool(root: &Table, path: &[&str]) -> anyhow::Result<Option<bool>> {
    let (key, parents) = path.split_last().expect("nested path");
    let mut table = root;
    for (depth, parent) in parents.iter().enumerate() {
        match table.get(*parent) {
            None => return Ok(None),
            Some(toml::Value::Table(next)) => table = next,
            Some(_) => anyhow::bail!(
                "config.toml key `{}` must be a table",
                parents[..=depth].join(".")
            ),
        }
    }
    match table.get(*key) {
        None => Ok(None),
        Some(value) => value.as_bool().map(Some).ok_or_else(|| {
            anyhow::anyhow!("config.toml key `{}` must be a boolean", path.join("."))
        }),
    }
}

/// Sets or removes a boolean at a nested table path. Clearing drops the
/// parent tables that become empty because of it (and only those).
fn apply_nested_bool(
    root: &mut Table,
    path: &[&str],
    update: &ConfigValueUpdate<bool>,
) -> anyhow::Result<()> {
    let (key, parents) = path.split_last().expect("nested path");
    match update {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(value) => {
            let mut table = root;
            for (depth, parent) in parents.iter().enumerate() {
                table = table
                    .entry(parent.to_string())
                    .or_insert_with(|| toml::Value::Table(Table::new()))
                    .as_table_mut()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "config.toml key `{}` must be a table",
                            parents[..=depth].join(".")
                        )
                    })?;
            }
            table.insert(key.to_string(), toml::Value::Boolean(*value));
        }
        ConfigValueUpdate::Clear => {
            remove_nested_key(root, path);
        }
    }
    Ok(())
}

/// Removes `path`; returns whether `table` became empty because of it.
fn remove_nested_key(table: &mut Table, path: &[&str]) -> bool {
    match path {
        [] => false,
        [key] => table.remove(*key).is_some() && table.is_empty(),
        [parent, rest @ ..] => {
            let emptied = table
                .get_mut(*parent)
                .and_then(toml::Value::as_table_mut)
                .is_some_and(|child| remove_nested_key(child, rest));
            if emptied {
                table.remove(*parent);
            }
            emptied && table.is_empty()
        }
    }
}

fn optional_provider_string(provider: Option<&Table>, key: &str) -> anyhow::Result<Option<String>> {
    provider
        .map(|provider| optional_string(provider, key))
        .transpose()
        .map(Option::flatten)
}

fn optional_provider_u64(provider: Option<&Table>, key: &str) -> anyhow::Result<Option<u64>> {
    let Some(value) = provider.and_then(|provider| provider.get(key)) else {
        return Ok(None);
    };
    let value = value
        .as_integer()
        .ok_or_else(|| anyhow::anyhow!("provider key `{key}` must be an integer"))?;
    u64::try_from(value)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("provider key `{key}` cannot be negative"))
}

fn optional_provider_bool(provider: Option<&Table>, key: &str) -> anyhow::Result<Option<bool>> {
    let Some(value) = provider.and_then(|provider| provider.get(key)) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("provider key `{key}` must be a boolean"))
}

fn selected_provider_table<'a>(root: &'a Table, provider_id: &str) -> Option<&'a Table> {
    root.get("model_providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get(provider_id))
        .and_then(toml::Value::as_table)
}

fn ensure_selected_provider_table<'a>(
    root: &'a mut Table,
    provider_id: &str,
) -> anyhow::Result<&'a mut Table> {
    let providers = root
        .entry("model_providers".to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml key `model_providers` must be a table"))?;
    providers
        .entry(provider_id.to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("config.toml key `model_providers.{provider_id}` must be a table")
        })
}

fn take_provider_table(root: &mut Table, provider_id: &str) -> Option<toml::Value> {
    let providers = root
        .get_mut("model_providers")
        .and_then(toml::Value::as_table_mut)?;
    providers.remove(provider_id)
}

fn insert_provider_table(
    root: &mut Table,
    provider_id: &str,
    provider: Table,
) -> anyhow::Result<()> {
    let providers = root
        .entry("model_providers".to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml key `model_providers` must be a table"))?;
    providers.insert(provider_id.to_string(), toml::Value::Table(provider));
    Ok(())
}

fn remove_empty_provider_section(root: &mut Table) {
    let remove = root
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .is_some_and(Table::is_empty);
    if remove {
        root.remove("model_providers");
    }
}

fn optional_string_value(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value != "-").then(|| value.to_string())
}

fn required_string(value: &str, label: &str) -> anyhow::Result<String> {
    optional_string_value(value).ok_or_else(|| anyhow::anyhow!("{label} cannot be empty"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_reads_all_supported_provider_scalars() {
        let config = r#"
model = "deepseek-v4-flash"
model_provider = "deepseek"
forced_login_method = "api"
model_reasoning_effort = "high"
model_catalog_json = "models.json"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
request_max_retries = 8
stream_max_retries = 9
stream_idle_timeout_ms = 400000
websocket_connect_timeout_ms = 20000
requires_openai_auth = false
supports_websockets = false
future_provider_key = "preserved"
"#;

        let snapshot = inspect_codex_profile_config(Some(config)).expect("inspect config");

        assert_eq!(snapshot.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(snapshot.request_max_retries, Some(8));
        assert_eq!(snapshot.stream_max_retries, Some(9));
        assert_eq!(snapshot.stream_idle_timeout_ms, Some(400000));
        assert_eq!(snapshot.websocket_connect_timeout_ms, Some(20000));
        assert_eq!(snapshot.supports_websockets, Some(false));
    }

    #[test]
    fn patch_updates_and_clears_scalars_without_losing_unknown_provider_fields() {
        let existing = r#"
model = "old"
model_provider = "custom"

[model_providers.custom]
name = "Custom"
request_max_retries = 7
future_provider_key = "preserved"
"#;
        let patch = CodexProfileConfigPatch {
            model: ConfigValueUpdate::Set("new-model".to_string()),
            request_max_retries: ConfigValueUpdate::Clear,
            stream_max_retries: ConfigValueUpdate::Set(12),
            supports_websockets: ConfigValueUpdate::Set(true),
            ..CodexProfileConfigPatch::default()
        };

        let (updated, changed) = apply_codex_profile_config_patch(Some(existing), &patch)
            .expect("profile patch should apply");
        assert!(changed);
        let updated = updated.expect("updated config");
        let table = parse_toml_table(&updated).expect("updated config should parse");
        let provider = selected_provider_table(&table, "custom").expect("custom provider");
        assert!(!provider.contains_key("request_max_retries"));
        assert_eq!(
            provider
                .get("stream_max_retries")
                .and_then(toml::Value::as_integer),
            Some(12)
        );
        assert_eq!(
            provider
                .get("future_provider_key")
                .and_then(toml::Value::as_str),
            Some("preserved")
        );
    }

    #[test]
    fn provider_rename_moves_unknown_fields_and_builtin_switch_removes_custom_table() {
        let existing = r#"
model_provider = "before"

[model_providers.before]
name = "Before"
future_provider_key = "preserved"
"#;
        let rename = CodexProfileConfigPatch {
            model_provider: ConfigValueUpdate::Set("after".to_string()),
            provider_name: ConfigValueUpdate::Set("After".to_string()),
            ..CodexProfileConfigPatch::default()
        };
        let (renamed, _) =
            apply_codex_profile_config_patch(Some(existing), &rename).expect("rename provider");
        let renamed = renamed.expect("renamed config");
        let renamed_table = parse_toml_table(&renamed).expect("renamed config should parse");
        let provider = selected_provider_table(&renamed_table, "after").expect("renamed provider");
        assert_eq!(
            provider
                .get("future_provider_key")
                .and_then(toml::Value::as_str),
            Some("preserved")
        );
        assert!(selected_provider_table(&renamed_table, "before").is_none());

        let builtin = CodexProfileConfigPatch {
            model_provider: ConfigValueUpdate::Set("openai".to_string()),
            ..CodexProfileConfigPatch::default()
        };
        let (builtin, _) = apply_codex_profile_config_patch(Some(&renamed), &builtin)
            .expect("switch to built-in provider");
        let builtin = parse_toml_table(&builtin.expect("builtin config")).expect("parse builtin");
        assert!(builtin.get("model_providers").is_none());
    }

    #[test]
    fn context_window_keys_are_root_level_positive_counts() {
        let patch = CodexProfileConfigPatch {
            model_context_window: ConfigValueUpdate::Set(200_000),
            model_auto_compact_token_limit: ConfigValueUpdate::Set(150_000),
            ..CodexProfileConfigPatch::default()
        };
        let (updated, changed) =
            apply_codex_profile_config_patch(Some("model = \"m\"\n"), &patch).unwrap();
        assert!(changed);
        let snapshot = inspect_codex_profile_config(updated.as_deref()).unwrap();
        assert_eq!(snapshot.model_context_window, Some(200_000));
        assert_eq!(snapshot.model_auto_compact_token_limit, Some(150_000));
        let clear = CodexProfileConfigPatch {
            model_context_window: ConfigValueUpdate::Clear,
            ..CodexProfileConfigPatch::default()
        };
        let (cleared, _) = apply_codex_profile_config_patch(updated.as_deref(), &clear).unwrap();
        assert_eq!(inspect_codex_profile_config(cleared.as_deref()).unwrap().model_context_window, None);
        let zero = CodexProfileConfigPatch {
            model_context_window: ConfigValueUpdate::Set(0),
            ..CodexProfileConfigPatch::default()
        };
        assert!(apply_codex_profile_config_patch(None, &zero).is_err());
    }

    #[test]
    fn retry_limits_and_wire_api_are_validated() {
        let existing = r#"
model_provider = "custom"
[model_providers.custom]
name = "Custom"
"#;
        let retries = CodexProfileConfigPatch {
            request_max_retries: ConfigValueUpdate::Set(101),
            ..CodexProfileConfigPatch::default()
        };
        assert!(apply_codex_profile_config_patch(Some(existing), &retries).is_err());

        let wire = CodexProfileConfigPatch {
            provider_wire_api: ConfigValueUpdate::Set("chat".to_string()),
            ..CodexProfileConfigPatch::default()
        };
        assert!(apply_codex_profile_config_patch(Some(existing), &wire).is_err());
    }

    #[test]
    fn deepseek_preset_is_atomic_and_leaves_retry_defaults_unset() {
        let patch = CodexProfileConfigPatch {
            apply_deepseek_preset: true,
            ..CodexProfileConfigPatch::default()
        };

        let (updated, changed) =
            apply_codex_profile_config_patch(None, &patch).expect("apply DeepSeek preset");
        assert!(changed);
        let snapshot = inspect_codex_profile_config(updated.as_deref()).expect("inspect preset");
        assert_eq!(snapshot.model.as_deref(), Some(deepseek::DEEPSEEK_DEFAULT_MODEL));
        assert_eq!(snapshot.model_provider.as_deref(), Some("deepseek"));
        assert_eq!(snapshot.request_max_retries, None);
        assert_eq!(snapshot.stream_max_retries, None);
    }

    fn summary_patch(update: ConfigValueUpdate<String>) -> CodexProfileConfigPatch {
        CodexProfileConfigPatch {
            model_reasoning_summary: update,
            ..CodexProfileConfigPatch::default()
        }
    }

    #[test]
    fn reasoning_summary_round_trips_and_is_validated() {
        let set = summary_patch(ConfigValueUpdate::Set("detailed".to_string()));
        assert!(!set.is_empty());
        let (updated, changed) =
            apply_codex_profile_config_patch(Some("model = \"gpt-5.5\"\n"), &set).unwrap();
        assert!(changed);
        let updated = updated.expect("updated config");
        assert!(
            updated.contains("model_reasoning_summary = \"detailed\""),
            "{updated}"
        );
        let snapshot = inspect_codex_profile_config(Some(&updated)).unwrap();
        assert_eq!(
            snapshot.model_reasoning_summary.as_deref(),
            Some("detailed")
        );
        assert_eq!(snapshot.model.as_deref(), Some("gpt-5.5"));
        let (_, changed) = apply_codex_profile_config_patch(Some(&updated), &set).unwrap();
        assert!(!changed, "setting the same value again is a no-op");

        let (cleared, changed) = apply_codex_profile_config_patch(
            Some(&updated),
            &summary_patch(ConfigValueUpdate::Clear),
        )
        .unwrap();
        assert!(changed);
        let cleared = inspect_codex_profile_config(cleared.as_deref()).unwrap();
        assert_eq!(cleared.model_reasoning_summary, None);
        assert_eq!(cleared.model.as_deref(), Some("gpt-5.5"));

        for value in MODEL_REASONING_SUMMARY_VALUES {
            let patch = summary_patch(ConfigValueUpdate::Set(format!(" {value} ")));
            let (next, _) = apply_codex_profile_config_patch(None, &patch).unwrap();
            let snapshot = inspect_codex_profile_config(next.as_deref()).unwrap();
            assert_eq!(snapshot.model_reasoning_summary.as_deref(), Some(*value));
        }
        for bad in ["verbose", "Auto", "", "-"] {
            let patch = summary_patch(ConfigValueUpdate::Set(bad.to_string()));
            assert!(
                apply_codex_profile_config_patch(None, &patch).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn reasoning_summary_is_refused_on_deepseek_profiles() {
        let preset = CodexProfileConfigPatch {
            apply_deepseek_preset: true,
            ..CodexProfileConfigPatch::default()
        };
        let deepseek = apply_codex_profile_config_patch(None, &preset)
            .unwrap()
            .0
            .expect("DeepSeek config");
        let set = summary_patch(ConfigValueUpdate::Set("auto".to_string()));
        let error = apply_codex_profile_config_patch(Some(&deepseek), &set).unwrap_err();
        assert!(error.to_string().contains("DeepSeek"), "{error:#}");
        // The preset and a summary in one patch: the preset would strip it.
        let preset_and_summary = CodexProfileConfigPatch {
            apply_deepseek_preset: true,
            ..set.clone()
        };
        assert!(
            apply_codex_profile_config_patch(Some("model = \"m\"\n"), &preset_and_summary).is_err()
        );

        // Clearing never writes the key, and removes a stale one.
        let clear = summary_patch(ConfigValueUpdate::Clear);
        let (_, changed) = apply_codex_profile_config_patch(Some(&deepseek), &clear).unwrap();
        assert!(!changed);
        let stale = format!("model_reasoning_summary = \"auto\"\n{deepseek}");
        let (cleaned, changed) = apply_codex_profile_config_patch(Some(&stale), &clear).unwrap();
        assert!(changed);
        let cleaned = inspect_codex_profile_config(cleaned.as_deref()).unwrap();
        assert_eq!(cleaned.model_reasoning_summary, None);
        assert_eq!(cleaned.model_provider.as_deref(), Some("deepseek"));

        // Leaving DeepSeek in the same patch makes it settable.
        let switch = CodexProfileConfigPatch {
            model_provider: ConfigValueUpdate::Set("openai".to_string()),
            ..set
        };
        let (switched, _) = apply_codex_profile_config_patch(Some(&deepseek), &switch).unwrap();
        let switched = inspect_codex_profile_config(switched.as_deref()).unwrap();
        assert_eq!(switched.model_reasoning_summary.as_deref(), Some("auto"));

        assert!(!reasoning_summary_supported(Some(" DeepSeek ")));
        assert!(reasoning_summary_supported(Some("openai")));
        assert!(reasoning_summary_supported(None));
    }

    fn update_plan_patch(update: ConfigValueUpdate<bool>) -> CodexProfileConfigPatch {
        CodexProfileConfigPatch {
            update_plan_enabled: update,
            ..CodexProfileConfigPatch::default()
        }
    }

    #[test]
    fn update_plan_enabled_round_trips_as_nested_tools_table() {
        let set = update_plan_patch(ConfigValueUpdate::Set(true));
        assert!(!set.is_empty());
        let (updated, changed) =
            apply_codex_profile_config_patch(Some("model = \"gpt-5.5\"\n"), &set).unwrap();
        assert!(changed);
        let updated = updated.expect("updated config");
        let root = parse_toml_table(&updated).unwrap();
        assert_eq!(
            root["tools"]["update_plan"]["enabled"].as_bool(),
            Some(true),
            "{updated}"
        );
        let snapshot = inspect_codex_profile_config(Some(&updated)).unwrap();
        assert_eq!(snapshot.update_plan_enabled, Some(true));
        assert_eq!(snapshot.model.as_deref(), Some("gpt-5.5"));
        let (_, changed) = apply_codex_profile_config_patch(Some(&updated), &set).unwrap();
        assert!(!changed, "setting the same value again is a no-op");

        let (disabled, changed) = apply_codex_profile_config_patch(
            Some(&updated),
            &update_plan_patch(ConfigValueUpdate::Set(false)),
        )
        .unwrap();
        assert!(changed);
        let disabled = inspect_codex_profile_config(disabled.as_deref()).unwrap();
        assert_eq!(disabled.update_plan_enabled, Some(false));

        // Clearing removes the key and the tables it emptied.
        let (cleared, changed) = apply_codex_profile_config_patch(
            Some(&updated),
            &update_plan_patch(ConfigValueUpdate::Clear),
        )
        .unwrap();
        assert!(changed);
        let cleared = cleared.expect("model remains");
        let root = parse_toml_table(&cleared).unwrap();
        assert!(!root.contains_key("tools"), "{cleared}");
        assert_eq!(
            inspect_codex_profile_config(Some(&cleared)).unwrap().update_plan_enabled,
            None
        );
        let (_, changed) = apply_codex_profile_config_patch(
            Some(&cleared),
            &update_plan_patch(ConfigValueUpdate::Clear),
        )
        .unwrap();
        assert!(!changed);

        // Sibling tool settings survive a clear; only empty tables go.
        let siblings = "[tools]\nweb_search = true\n\n[tools.update_plan]\nenabled = true\nnote = \"keep\"\n";
        let (kept, _) = apply_codex_profile_config_patch(
            Some(siblings),
            &update_plan_patch(ConfigValueUpdate::Clear),
        )
        .unwrap();
        let root = parse_toml_table(&kept.unwrap()).unwrap();
        assert_eq!(root["tools"]["web_search"].as_bool(), Some(true));
        assert_eq!(root["tools"]["update_plan"]["note"].as_str(), Some("keep"));
        assert!(root["tools"]["update_plan"].get("enabled").is_none());
        let (kept, _) = apply_codex_profile_config_patch(
            Some("[tools]\nweb_search = true\n[tools.update_plan]\nenabled = true\n"),
            &update_plan_patch(ConfigValueUpdate::Clear),
        )
        .unwrap();
        let root = parse_toml_table(&kept.unwrap()).unwrap();
        assert_eq!(root["tools"]["web_search"].as_bool(), Some(true));
        assert!(root["tools"].get("update_plan").is_none());

        // Clearing the only key leaves no config at all.
        let (empty, _) = apply_codex_profile_config_patch(
            Some("[tools.update_plan]\nenabled = true\n"),
            &update_plan_patch(ConfigValueUpdate::Clear),
        )
        .unwrap();
        assert_eq!(empty, None);

        // Malformed shapes are reported, not overwritten.
        for bad in ["tools = 1\n", "[tools]\nupdate_plan = true\n", "[tools.update_plan]\nenabled = \"yes\"\n"] {
            assert!(inspect_codex_profile_config(Some(bad)).is_err(), "{bad}");
        }
        assert!(apply_codex_profile_config_patch(Some("tools = 1\n"), &set).is_err());
    }

    #[test]
    fn update_plan_enabled_survives_and_is_settable_on_deepseek_profiles() {
        let preset = CodexProfileConfigPatch {
            apply_deepseek_preset: true,
            update_plan_enabled: ConfigValueUpdate::Set(true),
            ..CodexProfileConfigPatch::default()
        };
        let deepseek = apply_codex_profile_config_patch(None, &preset)
            .unwrap()
            .0
            .expect("DeepSeek config");
        let snapshot = inspect_codex_profile_config(Some(&deepseek)).unwrap();
        assert_eq!(snapshot.model_provider.as_deref(), Some("deepseek"));
        assert_eq!(snapshot.update_plan_enabled, Some(true));
        // Re-applying the preset keeps it.
        let reapplied = apply_codex_profile_config_patch(
            Some(&deepseek),
            &CodexProfileConfigPatch {
                apply_deepseek_preset: true,
                ..CodexProfileConfigPatch::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(
            inspect_codex_profile_config(reapplied.as_deref())
                .unwrap()
                .update_plan_enabled,
            Some(true)
        );
        assert!(!deepseek::preset_removes_key("tools"));
    }

    #[test]
    fn provider_instructions_field_is_a_nested_provider_switch_accepted_by_launch() {
        let base = "cutex_provider_mode = \"selected_profile_v2\"\nmodel = \"m\"\nmodel_provider = \"relay\"\n\n[model_providers.relay]\nname = \"relay\"\nbase_url = \"https://relay.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nenv_key = \"OPENAI_API_KEY\"\n";
        assert_eq!(inspect_codex_profile_config(Some(base)).unwrap().provider_instructions_field, None);
        let patch = |update| CodexProfileConfigPatch {
            provider_instructions_field: update,
            ..CodexProfileConfigPatch::default()
        };
        let (on, changed) =
            apply_codex_profile_config_patch(Some(base), &patch(ConfigValueUpdate::Set(true))).unwrap();
        assert!(changed);
        let on = on.unwrap();
        let root = parse_toml_table(&on).unwrap();
        assert_eq!(
            root["model_providers"]["relay"]["capabilities"]["instructions_field"].as_bool(),
            Some(true),
            "{on}"
        );
        assert_eq!(inspect_codex_profile_config(Some(&on)).unwrap().provider_instructions_field, Some(true));
        // The launch projection accepts and forwards it.
        let provider = crate::launch::selected_profile::Config::parse(&on)
            .unwrap()
            .model_providers
            .remove("relay")
            .unwrap();
        assert_eq!(provider.capabilities.and_then(|c| c.instructions_field), Some(true));

        let (cleared, changed) =
            apply_codex_profile_config_patch(Some(&on), &patch(ConfigValueUpdate::Clear)).unwrap();
        assert!(changed);
        let cleared = cleared.unwrap();
        assert!(!cleared.contains("capabilities"), "{cleared}");
        assert_eq!(inspect_codex_profile_config(Some(&cleared)).unwrap().provider_instructions_field, None);

        // Like every provider option, it needs a custom provider.
        assert!(apply_codex_profile_config_patch(Some("model = \"m\"\n"), &patch(ConfigValueUpdate::Set(true))).is_err());
    }
}
