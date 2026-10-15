//! DeepSeek Codex profile defaults (DeepSeek's official Codex config). The
//! behaviour itself lives in the data-driven `preset_spec`; this module keeps
//! the DeepSeek names other code refers to.

use toml::value::Table;

use crate::profiles::preset_spec;
use crate::profiles::profile_config::PROFILE_LOCAL_MODEL_CATALOG_FILE;

pub const DEEPSEEK_PROVIDER_ID: &str = "deepseek";
pub const DEEPSEEK_PROVIDER_NAME: &str = "DeepSeek";
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/";
/// DeepSeek V4.1-Flash (image input). Existing profiles on the retired
/// `deepseek-v4-flash` keep it (still accepted, served by Flash); nothing
/// switches them automatically.
pub const DEEPSEEK_DEFAULT_MODEL: &str = "deepseek-flash";
pub const DEEPSEEK_DEFAULT_REASONING: &str = "high";
pub const DEEPSEEK_MODEL_CATALOG_FILE: &str = PROFILE_LOCAL_MODEL_CATALOG_FILE;
pub const DEEPSEEK_API_KEY_ENV: &str = "OPENAI_API_KEY";

// Vendored catalog (`deepseek_models.json`): the rows of DeepSeek's official
// Codex setup script <= 1.2.0 (vendored 2026-08-07) refreshed on 2026-10-14
// for script 1.4.0's model list: `deepseek-flash` (V4.1-Flash, image input,
// `supports_image_detail_original: true`) first, `deepseek-v4-pro`, and the
// retired `deepseek-v4-flash` kept hidden so pinned agents keep metadata.
// The `deepseek-flash` row is derived from the earlier Flash row with the
// documented changes; its display name / description text are ours (the 1.4.0
// script's exact row was not vendored: unverified).

pub fn is_deepseek_provider(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(DEEPSEEK_PROVIDER_ID)
}

fn spec() -> &'static preset_spec::PresetSpec {
    preset_spec::spec_for_provider(DEEPSEEK_PROVIDER_ID).expect("DeepSeek preset spec")
}

/// Whether the DeepSeek preset removes the root config key `key`.
pub fn preset_removes_key(key: &str) -> bool {
    spec().removes_key(key)
}

pub fn model_catalog_json() -> String {
    spec().catalog_json().expect("DeepSeek catalog")
}

pub fn apply_profile_preset(root: &mut Table, base_url: Option<&str>) -> anyhow::Result<()> {
    preset_spec::apply(root, DEEPSEEK_PROVIDER_ID, DEEPSEEK_PROVIDER_NAME, base_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_catalog_is_valid_and_contains_flash_and_future_pro_entries() {
        let catalog: serde_json::Value =
            serde_json::from_str(&model_catalog_json()).expect("catalog should be valid JSON");
        let models = catalog
            .get("models")
            .and_then(serde_json::Value::as_array)
            .expect("catalog should contain models");
        let slugs = models
            .iter()
            .filter_map(|model| model.get("slug").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(slugs, ["deepseek-flash", "deepseek-v4-pro", "deepseek-v4-flash"]);
    }

    #[test]
    fn preset_uses_env_auth_and_leaves_retry_controls_at_codex_defaults() {
        let mut root = Table::new();
        apply_profile_preset(&mut root, None).expect("preset should apply");

        assert_eq!(
            root.get("model").and_then(toml::Value::as_str),
            Some(DEEPSEEK_DEFAULT_MODEL)
        );
        assert_eq!(
            root.get("model_catalog_json").and_then(toml::Value::as_str),
            Some(DEEPSEEK_MODEL_CATALOG_FILE)
        );
        assert!(!root.contains_key("preferred_auth_method"));
        assert_eq!(root.get("web_search").and_then(toml::Value::as_str), Some("disabled"));
        assert_eq!(root.get("model_reasoning_effort").and_then(toml::Value::as_str), Some("high"));
        assert_eq!(root.get("forced_login_method").and_then(toml::Value::as_str), Some("api"));
        let provider = root
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get(DEEPSEEK_PROVIDER_ID))
            .and_then(toml::Value::as_table)
            .expect("DeepSeek provider should exist");
        assert_eq!(
            provider.get("env_key").and_then(toml::Value::as_str),
            Some(DEEPSEEK_API_KEY_ENV)
        );
        assert!(!provider.contains_key("experimental_bearer_token"));
        assert!(!provider.contains_key("request_max_retries"));
        assert!(!provider.contains_key("stream_max_retries"));
        assert!(!provider.contains_key("stream_idle_timeout_ms"));
        assert!(!provider.contains_key("websocket_connect_timeout_ms"));
    }

    #[test]
    fn preset_preserves_unknown_provider_fields_while_removing_secret_conflicts() {
        let mut root = toml::from_str::<toml::Value>(
            r#"
model_provider = "deepseek"
service_tier = "fast"

[model_providers.deepseek]
experimental_bearer_token = "must-not-survive"
future_provider_key = "keep"
request_max_retries = 12
"#,
        )
        .expect("config should parse")
        .as_table()
        .expect("config root should be a table")
        .clone();

        apply_profile_preset(&mut root, Some("https://deepseek.example/")).expect("preset");

        assert!(!root.contains_key("service_tier"));
        let provider = root
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get(DEEPSEEK_PROVIDER_ID))
            .and_then(toml::Value::as_table)
            .expect("provider should exist");
        assert_eq!(
            provider
                .get("future_provider_key")
                .and_then(toml::Value::as_str),
            Some("keep")
        );
        assert_eq!(
            provider
                .get("request_max_retries")
                .and_then(toml::Value::as_integer),
            Some(12)
        );
        assert!(!provider.contains_key("experimental_bearer_token"));
    }
}
