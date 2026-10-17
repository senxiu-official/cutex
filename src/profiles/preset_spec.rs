//! Data-driven API-key preset behaviour, keyed by `model_provider` ID.
//!
//! Per provider: default base URL, default model and reasoning effort, the
//! root config keys the preset sets (`web_search`, `forced_login_method`) or
//! removes, and the model catalog it writes at creation (vendored by the
//! vendor, or built from [`known_models`]). One code path for every preset
//! (`apply`) replaces the earlier DeepSeek-only special case.
//!
//! Sources: cutex-desktop `docs/research/profile-presets-upgrade-20261014.md`
//! (provider docs fetched 2026-10-14).

use toml::value::Table;

use crate::profiles::known_models;
use crate::profiles::profile_config::{remove_model_provider_entry, PROFILE_LOCAL_MODEL_CATALOG_FILE};

/// Where a preset's model catalog comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetCatalog {
    /// No catalog: native's bundled catalog (OpenAI) or discovery (relays).
    None,
    /// A vendor-published catalog shipped with cutex.
    Vendored(&'static str),
    /// Built from the known-model table, in this order.
    Known(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetSpec {
    pub provider_id: &'static str,
    pub provider_name: &'static str,
    /// Written when the caller gives no base URL.
    pub default_base_url: Option<&'static str>,
    pub default_model: Option<&'static str>,
    /// Root `model_reasoning_effort`.
    pub reasoning_effort: Option<&'static str>,
    /// Root `web_search` (native `WebSearchMode`).
    pub web_search: Option<&'static str>,
    /// Root `tool_schema_compat` (native `ToolSchemaCompat`, r53+).
    pub tool_schema_compat: Option<&'static str>,
    /// Root `forced_login_method = "api"`.
    pub forced_login_api: bool,
    pub catalog: PresetCatalog,
}

/// Root keys a preset with a catalog removes (DeepSeek's published list):
/// they override or conflict with the catalog's per-model metadata.
pub const CONFLICTING_KEYS: &[&str] = &[
    "base_instructions",
    "compact_prompt",
    "experimental_compact_prompt_file",
    "experimental_use_unified_exec_tool",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
    "model_context_window",
    "model_instructions_file",
    "model_reasoning_summary",
    "model_verbosity",
    "openai_base_url",
    "oss_provider",
    "plan_mode_reasoning_effort",
    "profile",
    "service_tier",
];

const DEEPSEEK_MODELS_JSON: &str = include_str!("deepseek_models.json");

pub const SPECS: &[PresetSpec] = &[
    // DeepSeek's official Codex config (setup script / Codex integration
    // page): high effort, API login, web_search disabled (the Responses API
    // supports no built-in tools), vendored catalog.
    PresetSpec {
        provider_id: "deepseek",
        provider_name: "DeepSeek",
        default_base_url: Some("https://api.deepseek.com/"),
        default_model: Some("deepseek-flash"),
        reasoning_effort: Some("high"),
        web_search: Some("disabled"),
        tool_schema_compat: None,
        forced_login_api: true,
        catalog: PresetCatalog::Vendored(DEEPSEEK_MODELS_JSON),
    },
    // Z.ai Coding Plan: the Responses endpoint is /api/v1 (/api/coding/paas/v4
    // is Chat Completions). Official Codex template default effort max.
    // Web search: neither documented nor disabled by Z.ai (unverified), left
    // at the native default.
    PresetSpec {
        provider_id: "zai",
        provider_name: "Z.ai",
        default_base_url: Some("https://api.z.ai/api/v1"),
        default_model: Some("glm-5.3"),
        reasoning_effort: Some("max"),
        web_search: None,
        tool_schema_compat: None,
        forced_login_api: false,
        catalog: PresetCatalog::Known(&["glm-5.3", "glm-5.3-flash", "glm-5.2"]),
    },
    // BigModel (China): same API on open.bigmodel.cn/api/v1.
    PresetSpec {
        provider_id: "zhipu",
        provider_name: "Zhipu",
        default_base_url: Some("https://open.bigmodel.cn/api/v1"),
        default_model: Some("glm-5.3"),
        reasoning_effort: Some("max"),
        web_search: None,
        tool_schema_compat: None,
        forced_login_api: false,
        catalog: PresetCatalog::Known(&["glm-5.3", "glm-5-turbo", "glm-5.3-flash"]),
    },
    // Kimi Open Platform (global; the `moonshot-cn` preset shares this
    // provider ID with api.moonshot.cn). K3 defaults to max itself; K2.x take
    // no effort, so no root effort is written. web_search is supported.
    PresetSpec {
        provider_id: "moonshot",
        provider_name: "Moonshot",
        default_base_url: Some("https://api.moonshot.ai/v1"),
        default_model: Some("kimi-k3"),
        reasoning_effort: None,
        web_search: None,
        tool_schema_compat: None,
        forced_login_api: false,
        catalog: PresetCatalog::Known(&["kimi-k3", "kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"]),
    },
    // Kimi Code membership plan (kimi.com/code/docs "Using Kimi in Codex" and
    // "Model Configuration", fetched 2026-10-16): Responses API on
    // `/coding/v1` (overseas api.kimi.ai, China api.kimi.com, shared by the
    // `kimi-code` / `kimi-code-cn` presets), official template
    // `web_search = "live"`. Default `kimi-for-coding`: the stable ID every
    // coding tier has (legacy Andante has no K3). Each model has its own
    // default effort, so no root effort is written.
    PresetSpec {
        provider_id: crate::profiles::providers::KIMI_CODE_PROVIDER_ID,
        provider_name: "Kimi Code",
        default_base_url: Some("https://api.kimi.ai/coding/v1"),
        default_model: Some("kimi-for-coding"),
        reasoning_effort: None,
        web_search: Some("live"),
        tool_schema_compat: None,
        forced_login_api: false,
        catalog: PresetCatalog::Known(&["kimi-for-coding", "k3-256k", "k3", "kimi-for-coding-highspeed"]),
    },
    // MiniMax Token Plan Codex guide (global; `minimax-cn` shares the ID with
    // api.minimaxi.com). Web search supported.
    PresetSpec {
        provider_id: "minimax",
        provider_name: "MiniMax",
        default_base_url: Some("https://api.minimax.io/v1"),
        default_model: Some("MiniMax-M3.1-Flash-Preview"),
        reasoning_effort: None,
        web_search: None,
        tool_schema_compat: None,
        forced_login_api: false,
        catalog: PresetCatalog::Known(&["MiniMax-M3.1-Flash-Preview", "MiniMax-M3"]),
    },
    // xAI Responses API. Whether native's web_search tool shape is accepted
    // is unverified, so it stays disabled until a smoke test.
    PresetSpec {
        provider_id: "xai",
        provider_name: "xAI",
        default_base_url: Some("https://api.x.ai/v1"),
        default_model: Some("grok-4.7"),
        reasoning_effort: None,
        web_search: Some("disabled"),
        tool_schema_compat: None,
        forced_login_api: false,
        catalog: PresetCatalog::Known(&["grok-4.7", "grok-4.6", "grok-4.3"]),
    },
    // Gemini through a relay (see `gemini`): Gemini refuses built-in
    // web_search mixed with function tools.
    PresetSpec {
        provider_id: crate::profiles::gemini::GEMINI_PROVIDER_ID,
        provider_name: crate::profiles::gemini::GEMINI_PROVIDER_NAME,
        default_base_url: None,
        default_model: Some(crate::profiles::gemini::GEMINI_DEFAULT_MODEL),
        reasoning_effort: None,
        web_search: Some(crate::profiles::gemini::GEMINI_WEB_SEARCH),
        // Google function declarations refuse mixed / typeless / nullable
        // enums, which MCP tool schemas often carry.
        tool_schema_compat: Some(crate::profiles::gemini::GEMINI_TOOL_SCHEMA_COMPAT),
        forced_login_api: false,
        catalog: PresetCatalog::None,
    },
];

pub fn spec_for_provider(provider_id: &str) -> Option<&'static PresetSpec> {
    let provider_id = provider_id.trim();
    SPECS.iter().find(|spec| spec.provider_id.eq_ignore_ascii_case(provider_id))
}

impl PresetSpec {
    /// The catalog JSON this preset writes (`None`: no catalog).
    pub fn catalog_json(&self) -> Option<String> {
        match self.catalog {
            PresetCatalog::None => None,
            PresetCatalog::Vendored(json) => Some(json.to_string()),
            PresetCatalog::Known(slugs) => Some(known_models::catalog_json(slugs)),
        }
    }

    pub fn has_catalog(&self) -> bool {
        self.catalog != PresetCatalog::None
    }

    /// Whether applying this preset removes root key `key`.
    pub fn removes_key(&self, key: &str) -> bool {
        self.has_catalog() && CONFLICTING_KEYS.contains(&key)
    }
}

/// The preset catalog of a provider ID (`None`: the provider has none).
pub fn catalog_for_provider(provider_id: &str) -> Option<String> {
    spec_for_provider(provider_id).and_then(PresetSpec::catalog_json)
}

/// One catalog row of a provider's preset catalog.
pub fn catalog_row_for_provider(provider_id: &str, slug: &str) -> Option<serde_json::Value> {
    let catalog: serde_json::Value = serde_json::from_str(&catalog_for_provider(provider_id)?).ok()?;
    catalog["models"].as_array()?.iter().find(|row| row["slug"].as_str() == Some(slug)).cloned()
}

fn string(value: &str) -> toml::Value {
    toml::Value::String(value.to_string())
}

/// Write the API-key provider layout for `provider_id` into `root` (a
/// `[model_providers.<id>]` table reading the key from `OPENAI_API_KEY`,
/// Responses wire API), plus the provider's preset keys when it has a spec.
/// Unknown keys of an existing provider table survive; secrets inside it do
/// not. Re-applying is idempotent.
pub fn apply(root: &mut Table, provider_id: &str, provider_name: &str, base_url: Option<&str>) -> anyhow::Result<()> {
    let spec = spec_for_provider(provider_id);
    // A known provider keeps its canonical ID (`DeepSeek` → `deepseek`).
    let provider_id = spec.map_or(provider_id.trim(), |spec| spec.provider_id);
    let previous_provider = root.get("model_provider").and_then(toml::Value::as_str).map(str::to_string);
    if let Some(previous) = previous_provider.as_deref() {
        if previous != provider_id {
            remove_model_provider_entry(root, previous);
        }
    }
    if spec.is_some_and(PresetSpec::has_catalog) {
        for key in CONFLICTING_KEYS {
            root.remove(*key);
        }
        root.remove("preferred_auth_method");
        root.insert("model_catalog_json".into(), string(PROFILE_LOCAL_MODEL_CATALOG_FILE));
    }
    root.insert("model_provider".into(), string(provider_id));
    if let Some(spec) = spec {
        if let Some(model) = spec.default_model {
            root.insert("model".into(), string(model));
        }
        if let Some(effort) = spec.reasoning_effort {
            root.insert("model_reasoning_effort".into(), string(effort));
        }
        if let Some(mode) = spec.web_search {
            root.insert("web_search".into(), string(mode));
        }
        if let Some(mode) = spec.tool_schema_compat {
            root.insert("tool_schema_compat".into(), string(mode));
        }
        if spec.forced_login_api {
            root.insert("forced_login_method".into(), string("api"));
        }
    }

    let name = spec.map(|spec| spec.provider_name).unwrap_or(provider_name).trim().to_string();
    let base_url = base_url.or(spec.and_then(|spec| spec.default_base_url));
    let providers = root
        .entry("model_providers".to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml key `model_providers` must be a table"))?;
    let provider = providers
        .entry(provider_id.to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml key `model_providers.{provider_id}` must be a table"))?;
    for conflicting in ["auth", "aws", "experimental_bearer_token"] {
        provider.remove(conflicting);
    }
    provider.insert("name".into(), string(&name));
    if let Some(url) = base_url {
        provider.insert("base_url".into(), string(url));
    }
    provider.insert("env_key".into(), string("OPENAI_API_KEY"));
    provider.insert("wire_api".into(), string("responses"));
    provider.insert("requires_openai_auth".into(), toml::Value::Boolean(false));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn every_catalog_is_a_valid_native_catalog_with_its_default_model() {
        for spec in SPECS {
            let Some(raw) = spec.catalog_json() else { continue };
            let catalog: Value = serde_json::from_str(&raw).unwrap();
            known_models::tests::assert_native_model_rows(&catalog);
            let slugs: Vec<&str> = catalog["models"].as_array().unwrap().iter()
                .map(|row| row["slug"].as_str().unwrap()).collect();
            let default = spec.default_model.unwrap();
            assert!(slugs.contains(&default), "{}: {default}", spec.provider_id);
            // The root effort is one the default model offers (or the model
            // takes any / none).
            if let Some(effort) = spec.reasoning_effort {
                let row = catalog_row_for_provider(spec.provider_id, default).unwrap();
                let levels: Vec<&str> = row["supported_reasoning_levels"].as_array().unwrap().iter()
                    .map(|level| level["effort"].as_str().unwrap()).collect();
                assert!(levels.is_empty() || levels.contains(&effort), "{}", spec.provider_id);
            }
        }
    }

    #[test]
    fn deepseek_catalog_offers_flash_pro_and_hides_the_retired_name() {
        let row = |slug| catalog_row_for_provider("deepseek", slug).unwrap();
        assert_eq!(row("deepseek-flash")["input_modalities"], serde_json::json!(["text", "image"]));
        assert_eq!(row("deepseek-flash")["supports_image_detail_original"], true);
        assert_eq!(row("deepseek-v4-pro")["input_modalities"], serde_json::json!(["text"]));
        assert_eq!(row("deepseek-v4-flash")["visibility"], "hide");
        assert_eq!(row("deepseek-flash")["visibility"], "list");
    }

    #[test]
    fn apply_writes_layout_and_preset_keys_idempotently() {
        let mut root = Table::new();
        root.insert("service_tier".into(), string("fast"));
        root.insert("model_provider".into(), string("old"));
        root.insert("model_providers".into(), toml::from_str::<toml::Value>("[old]\nname = \"x\"").unwrap());
        apply(&mut root, "zai", "ignored", None).unwrap();
        assert_eq!(root["model"].as_str(), Some("glm-5.3"));
        assert_eq!(root["model_reasoning_effort"].as_str(), Some("max"));
        assert_eq!(root["model_catalog_json"].as_str(), Some(PROFILE_LOCAL_MODEL_CATALOG_FILE));
        assert!(!root.contains_key("service_tier"));
        assert!(!root.contains_key("web_search"));
        let providers = root["model_providers"].as_table().unwrap();
        assert!(!providers.contains_key("old"));
        assert_eq!(providers["zai"]["base_url"].as_str(), Some("https://api.z.ai/api/v1"));
        assert_eq!(providers["zai"]["wire_api"].as_str(), Some("responses"));
        assert_eq!(providers["zai"]["name"].as_str(), Some("Z.ai"));
        let before = root.clone();
        apply(&mut root, "zai", "Z.ai", None).unwrap();
        assert_eq!(root, before);

        // Unknown providers get the plain layout, nothing else.
        let mut custom = Table::new();
        apply(&mut custom, "custom", "custom", Some("https://relay.example/v1")).unwrap();
        for key in ["model", "model_catalog_json", "web_search", "tool_schema_compat", "model_reasoning_effort", "forced_login_method"] {
            assert!(!custom.contains_key(key), "{key}");
        }
        assert_eq!(spec_for_provider(" XAI ").unwrap().provider_id, "xai");
        assert!(spec_for_provider("openrouter").is_none());
    }
}
