//! API-key provider presets for new Codex profiles and the stable provider
//! family of a profile.
//!
//! A provider family groups profiles whose conversations can move between
//! each other mid-session (same upstream API, so provider-specific history
//! items stay meaningful): e.g. Z.ai and Zhipu BigModel are both GLM.
//! Custom providers are keyed by their base URL host.

use serde_json::{json, Value};
use toml::value::Table;

use crate::profiles::deepseek;
use crate::profiles::gemini;
use crate::profiles::preset_spec;
use crate::profiles::inspect::{account_model_api_base, account_model_provider};
use crate::profiles::model::{CliKind, StoredAccount};

/// A known API-key provider for `POST /v2/profiles {kind:"apiKey", preset}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiKeyPreset {
    /// Stable preset ID accepted by the API.
    pub id: &'static str,
    /// Accepted alternative spellings.
    pub aliases: &'static [&'static str],
    pub label: &'static str,
    /// `model_provider` / `[model_providers.<id>]` key written to config.toml.
    pub provider_id: &'static str,
    /// `[model_providers.<id>].name`.
    pub provider_name: &'static str,
    /// Default base URL (`None`: the caller must supply one; custom).
    pub base_url: Option<&'static str>,
    /// Default model written to config.toml (`None`: chosen per agent).
    pub default_model: Option<&'static str>,
    /// Provider family ID (see [`family_label`]); custom presets derive it
    /// from the base URL host.
    pub family: Option<&'static str>,
    /// Quota reader a new profile from this preset starts with (on its
    /// default base URL only): the provider's official quota / balance API.
    /// Declared per preset, never inferred from a host or provider name.
    pub quota_reader: Option<crate::profiles::quota::Reader>,
    /// Model list URL for discovery when `<base_url>/models` is not an
    /// OpenAI model list (on the preset's default base URL only). Declared
    /// per preset, never inferred from a host or provider name.
    pub models_url: Option<&'static str>,
}

/// Id of the generic "custom" preset (any OpenAI Responses compatible
/// endpoint): its provider name says nothing about the plan, so a profile
/// created from it gets no `plan_type` label.
pub const CUSTOM_PRESET_ID: &str = "custom";

impl ApiKeyPreset {
    /// Official Anthropic-compatible default, keyed by preset ID so regional
    /// presets sharing a model_provider retain their own address. This is a
    /// creation default, not a stored Claude account or runtime binding.
    pub fn anthropic_base_url(&self) -> Option<&'static str> {
        match self.id {
            "deepseek" => Some(super::interface_urls::DEEPSEEK_ANTHROPIC_BASE_URL),
            "zai" => Some("https://api.z.ai/api/anthropic"),
            "zhipu" => Some("https://open.bigmodel.cn/api/anthropic"),
            "moonshot" => Some("https://api.moonshot.ai/anthropic"),
            "moonshot-cn" => Some("https://api.moonshot.cn/anthropic"),
            "kimi-code-cn" => Some("https://api.kimi.com/coding/"),
            _ => None,
        }
    }
}

pub const API_KEY_PRESETS: &[ApiKeyPreset] = &[
    ApiKeyPreset {
        id: "deepseek",
        aliases: &[],
        label: "DeepSeek",
        provider_id: deepseek::DEEPSEEK_PROVIDER_ID,
        provider_name: deepseek::DEEPSEEK_PROVIDER_NAME,
        base_url: Some(deepseek::DEEPSEEK_BASE_URL),
        default_model: Some(deepseek::DEEPSEEK_DEFAULT_MODEL),
        family: Some("deepseek"),
        quota_reader: Some(crate::profiles::quota::Reader::DeepseekBalance),
        models_url: None,
    },
    ApiKeyPreset {
        id: "zai",
        aliases: &["glm", "z.ai"],
        label: "Z.ai (GLM Coding Plan)",
        provider_id: "zai",
        provider_name: "Z.ai",
        // Responses endpoint (`/api/coding/paas/v4` is Chat Completions).
        base_url: Some("https://api.z.ai/api/v1"),
        default_model: Some("glm-5.3"),
        family: Some("zhipu"),
        quota_reader: Some(crate::profiles::quota::Reader::ZhipuCodingPlan),
        // `/api/v1/models` answers HTTP 200 with a `{code,msg,success}`
        // envelope and no list; the Coding Plan path lists the plan's models.
        models_url: Some("https://api.z.ai/api/coding/paas/v4/models"),
    },
    ApiKeyPreset {
        id: "zhipu",
        aliases: &["bigmodel"],
        label: "Zhipu BigModel (GLM Coding Plan)",
        provider_id: "zhipu",
        provider_name: "Zhipu",
        base_url: Some("https://open.bigmodel.cn/api/v1"),
        default_model: Some("glm-5.3"),
        family: Some("zhipu"),
        quota_reader: Some(crate::profiles::quota::Reader::ZhipuCodingPlan),
        // Same API layout as Z.ai (see the `zai` preset).
        models_url: Some("https://open.bigmodel.cn/api/coding/paas/v4/models"),
    },
    ApiKeyPreset {
        id: "moonshot",
        aliases: &["kimi"],
        label: "Moonshot (Kimi)",
        provider_id: "moonshot",
        provider_name: "Moonshot",
        base_url: Some("https://api.moonshot.ai/v1"),
        default_model: Some("kimi-k3"),
        family: Some("moonshot"),
        quota_reader: Some(crate::profiles::quota::Reader::MoonshotBalance),
        models_url: None,
    },
    ApiKeyPreset {
        // Kimi Open Platform China (separate keys, CNY balance).
        id: "moonshot-cn",
        aliases: &["kimi-cn"],
        label: "Moonshot (Kimi, China)",
        provider_id: "moonshot",
        provider_name: "Moonshot",
        base_url: Some("https://api.moonshot.cn/v1"),
        default_model: Some("kimi-k3"),
        family: Some("moonshot"),
        quota_reader: Some(crate::profiles::quota::Reader::MoonshotCnBalance),
        models_url: None,
    },
    ApiKeyPreset {
        // Kimi Code membership plan, overseas endpoint (kimi.com/code docs
        // "Using Kimi in Codex": Responses API on /coding/v1). Keys come
        // from the Kimi Code Console, not the Open Platform.
        id: "kimi-code",
        aliases: &["kimi-for-coding"],
        label: "Kimi Code (membership plan)",
        provider_id: KIMI_CODE_PROVIDER_ID,
        provider_name: "Kimi Code",
        base_url: Some("https://api.kimi.ai/coding/v1"),
        default_model: Some("kimi-for-coding"),
        family: Some(FAMILY_KIMI_CODE),
        quota_reader: Some(crate::profiles::quota::Reader::KimiCodePlan),
        models_url: None,
    },
    ApiKeyPreset {
        // Kimi Code, China endpoint (same API on api.kimi.com, which the
        // official Kimi CLI uses by default).
        id: "kimi-code-cn",
        aliases: &[],
        label: "Kimi Code (membership plan, China)",
        provider_id: KIMI_CODE_PROVIDER_ID,
        provider_name: "Kimi Code",
        base_url: Some("https://api.kimi.com/coding/v1"),
        default_model: Some("kimi-for-coding"),
        family: Some(FAMILY_KIMI_CODE),
        quota_reader: Some(crate::profiles::quota::Reader::KimiCodePlan),
        models_url: None,
    },
    ApiKeyPreset {
        id: "minimax",
        aliases: &[],
        label: "MiniMax",
        provider_id: "minimax",
        provider_name: "MiniMax",
        base_url: Some("https://api.minimax.io/v1"),
        default_model: Some("MiniMax-M3.1-Flash-Preview"),
        family: Some("minimax"),
        quota_reader: None,
        models_url: None,
    },
    ApiKeyPreset {
        id: "minimax-cn",
        aliases: &[],
        label: "MiniMax (China)",
        provider_id: "minimax",
        provider_name: "MiniMax",
        base_url: Some("https://api.minimaxi.com/v1"),
        default_model: Some("MiniMax-M3.1-Flash-Preview"),
        family: Some("minimax"),
        quota_reader: None,
        models_url: None,
    },
    ApiKeyPreset {
        id: "xai",
        aliases: &["grok", "x.ai"],
        label: "xAI (Grok)",
        provider_id: "xai",
        provider_name: "xAI",
        base_url: Some("https://api.x.ai/v1"),
        default_model: Some("grok-4.7"),
        family: Some("xai"),
        quota_reader: None,
        models_url: None,
    },
    ApiKeyPreset {
        id: "openrouter",
        aliases: &[],
        label: "OpenRouter",
        provider_id: "openrouter",
        provider_name: "OpenRouter",
        base_url: Some("https://openrouter.ai/api/v1"),
        default_model: None,
        family: Some("openrouter"),
        quota_reader: Some(crate::profiles::quota::Reader::OpenrouterKey),
        models_url: None,
    },
    ApiKeyPreset {
        id: "openai",
        aliases: &[],
        label: "OpenAI API",
        // `openai` itself is a built-in Codex provider that cannot be
        // redefined; the API-key route uses its own provider entry.
        provider_id: "openai-api",
        provider_name: "OpenAI",
        base_url: Some("https://api.openai.com/v1"),
        default_model: None,
        family: Some("openai-api"),
        quota_reader: None,
        models_url: None,
    },
    ApiKeyPreset {
        // Gemini served by a third-party OpenAI-Responses-compatible relay
        // (Google's own endpoint is not Responses-compatible). The relay URL
        // is required; the family follows its host (`custom:<host>`). The
        // config disables Codex's built-in web_search (see `gemini`).
        id: "gemini",
        aliases: &["google"],
        label: "Gemini (OpenAI-compatible relay)",
        provider_id: gemini::GEMINI_PROVIDER_ID,
        provider_name: gemini::GEMINI_PROVIDER_NAME,
        base_url: None,
        default_model: Some(gemini::GEMINI_DEFAULT_MODEL),
        family: None,
        quota_reader: None,
        models_url: None,
    },
    ApiKeyPreset {
        id: CUSTOM_PRESET_ID,
        aliases: &[],
        label: "Custom (OpenAI Responses compatible)",
        provider_id: "custom",
        provider_name: "custom",
        base_url: None,
        default_model: None,
        family: None,
        quota_reader: None,
        models_url: None,
    },
];

pub fn find_preset(id: &str) -> Option<&'static ApiKeyPreset> {
    let id = id.trim().to_ascii_lowercase();
    API_KEY_PRESETS
        .iter()
        .find(|preset| preset.id == id || preset.aliases.contains(&id.as_str()))
}

/// The model list URL a preset declares for `base_url` when it is that
/// preset's default base URL (see [`ApiKeyPreset::models_url`]).
pub fn preset_models_url(base_url: &str) -> Option<&'static str> {
    let base = base_url.trim().trim_end_matches('/');
    API_KEY_PRESETS
        .iter()
        .find(|preset| preset.base_url.is_some_and(|default| default.eq_ignore_ascii_case(base)))
        .and_then(|preset| preset.models_url)
}

pub fn presets_json() -> Value {
    Value::Array(
        API_KEY_PRESETS
            .iter()
            .map(|preset| {
                let family = preset.family.map(str::to_string).or_else(|| {
                    preset.base_url.map(|url| family_for(&CliKind::Codex, None, Some(url)).0)
                });
                json!({
                    "id": preset.id,
                    "aliases": preset.aliases,
                    "label": preset.label,
                    "providerId": preset.provider_id,
                    "baseUrl": preset.base_url,
                    "baseUrlRequired": preset.base_url.is_none(),
                    "defaultModel": preset.default_model,
                    "providerFamily": family,
                    "providerFamilyLabel": family.as_deref().map(family_label),
                    // A new profile on the preset's default base URL starts
                    // with this quota reader on (`quotaReader`; declared per
                    // preset, profile-generic-switches).
                    "autoQuota": preset.quota_reader.is_some(),
                    "quotaReader": preset.quota_reader.map(crate::profiles::quota::Reader::as_str),
                    // The preset writes its own model catalog / root
                    // web_search mode (null: native default).
                    "modelCatalog": preset_spec::spec_for_provider(preset.provider_id)
                        .is_some_and(preset_spec::PresetSpec::has_catalog),
                    "webSearch": preset_spec::spec_for_provider(preset.provider_id)
                        .and_then(|spec| spec.web_search),
                    // Root tool_schema_compat the preset writes (null: off).
                    "toolSchemaCompat": preset_spec::spec_for_provider(preset.provider_id)
                        .and_then(|spec| spec.tool_schema_compat),
                    // r265 (profile-runtime-variants, additive): the runtimes a
                    // new API-key profile on this preset could run.
                    "runtimeCompatibility": super::runtime_variants::preset_compatibility_view(preset.provider_id, preset.base_url),
                    // profile-relay-runtimes: the interfaces a new profile
                    // of this preset uses, on its default addresses.
                    "providerInterfaces": crate::profiles::interface_urls::preset_view(preset),
                })
            })
            .collect(),
    )
}

/// Profile-specific config.toml for a third-party API-key Codex profile:
/// a named `[model_providers.<id>]` using the profile's `OPENAI_API_KEY`
/// (the layout `cutex login --api-key` writes), plus the provider's preset
/// (`preset_spec`: model, effort, web_search, local model catalog).
pub fn api_key_profile_config(
    provider_id: &str,
    provider_name: &str,
    base_url: Option<&str>,
) -> anyhow::Result<Table> {
    let mut root = Table::new();
    preset_spec::apply(&mut root, provider_id, provider_name, base_url)?;
    Ok(root)
}

/// The Responses base URL for an official Z.ai / BigModel base URL on the
/// GLM Coding Plan Chat Completions path (`/api/coding/paas/v4`), which a
/// Codex profile (`wire_api = "responses"`) cannot use. `None` for anything
/// else. Used by a `/v2/health` warning; profiles are never rewritten
/// automatically.
pub fn glm_responses_base_url_for(base_url: &str) -> Option<&'static str> {
    let url = url::Url::parse(base_url.trim()).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let path = url.path().trim_end_matches('/');
    if !(path == "/api/coding/paas/v4" || path.starts_with("/api/coding/paas/v4/")) {
        return None;
    }
    match host.as_str() {
        "api.z.ai" => Some("https://api.z.ai/api/v1"),
        "open.bigmodel.cn" | "bigmodel.cn" => Some("https://open.bigmodel.cn/api/v1"),
        _ => None,
    }
}

/// `cutex login --api-key` config: provider ID doubles as display name.
pub fn codex_api_key_config_toml(provider: &str, base_url: Option<&str>) -> String {
    let root = api_key_profile_config(provider, provider, base_url)
        .expect("API-key profile config should build");
    toml::to_string_pretty(&root).expect("API-key profile config should serialize")
}

// ---------------------------------------------------------------------------
// Provider families
// ---------------------------------------------------------------------------

pub const FAMILY_OPENAI_CHATGPT: &str = "openai-chatgpt";
pub const FAMILY_OPENAI_API: &str = "openai-api";
pub const FAMILY_ANTHROPIC: &str = "anthropic";
pub const FAMILY_DEEPSEEK: &str = "deepseek";
pub const FAMILY_ZHIPU: &str = "zhipu";
pub const FAMILY_MOONSHOT: &str = "moonshot";
pub const FAMILY_OPENROUTER: &str = "openrouter";
pub const FAMILY_MINIMAX: &str = "minimax";
pub const FAMILY_XAI: &str = "xai";
/// Kimi Code membership API (api.kimi.com / api.kimi.ai `/coding/v1`): own
/// keys and model IDs, so not the Open Platform's `moonshot` family.
pub const FAMILY_KIMI_CODE: &str = "kimi-code";
/// `model_provider` ID the Kimi Code presets write.
pub const KIMI_CODE_PROVIDER_ID: &str = "kimi-code";
/// Custom families are `custom:<host[:port]>` (or `custom:<provider id>`
/// without a base URL, `custom` with neither).
pub const FAMILY_CUSTOM_PREFIX: &str = "custom";

/// `host` is `domain` or one of its subdomains (no lookalike suffixes).
pub(crate) fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn family_for_host(host: &str) -> Option<&'static str> {
    const HOSTS: &[(&str, &str)] = &[
        ("chatgpt.com", FAMILY_OPENAI_CHATGPT),
        ("openai.com", FAMILY_OPENAI_API),
        ("anthropic.com", FAMILY_ANTHROPIC),
        ("deepseek.com", FAMILY_DEEPSEEK),
        ("z.ai", FAMILY_ZHIPU),
        ("bigmodel.cn", FAMILY_ZHIPU),
        ("moonshot.ai", FAMILY_MOONSHOT),
        ("moonshot.cn", FAMILY_MOONSHOT),
        ("openrouter.ai", FAMILY_OPENROUTER),
        ("minimax.io", FAMILY_MINIMAX),
        ("minimaxi.com", FAMILY_MINIMAX),
        ("x.ai", FAMILY_XAI),
        ("kimi.com", FAMILY_KIMI_CODE),
        ("kimi.ai", FAMILY_KIMI_CODE),
    ];
    HOSTS
        .iter()
        .find(|(domain, _)| host_matches(host, domain))
        .map(|(_, family)| *family)
}

fn family_for_provider_id(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "openai" | "openai-api" => FAMILY_OPENAI_API,
        "anthropic" | "claude" => FAMILY_ANTHROPIC,
        "deepseek" => FAMILY_DEEPSEEK,
        "glm" | "zai" | "z.ai" | "zhipu" | "bigmodel" => FAMILY_ZHIPU,
        "kimi" | "moonshot" => FAMILY_MOONSHOT,
        "openrouter" => FAMILY_OPENROUTER,
        "minimax" => FAMILY_MINIMAX,
        "xai" | "grok" => FAMILY_XAI,
        "kimi-code" => FAMILY_KIMI_CODE,
        _ => return None,
    })
}

/// `host[:port]` of an http(s) base URL, lowercased.
fn base_url_host(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url.trim()).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// Pure family resolution: `(id, label)`. The base URL host wins over the
/// provider ID (a provider named "GLM" pointing at api.z.ai and one named
/// "zai" are the same family); unknown hosts are `custom:<host>`.
pub fn family_for(
    cli_kind: &CliKind,
    model_provider: Option<&str>,
    base_url: Option<&str>,
) -> (String, String) {
    if *cli_kind == CliKind::Dsh {
        return (FAMILY_DEEPSEEK.into(), family_label(FAMILY_DEEPSEEK));
    }
    if *cli_kind == CliKind::Claude {
        let host = base_url.and_then(base_url_host);
        let id = match host.as_deref() {
            None if base_url.is_none() => FAMILY_ANTHROPIC.to_string(),
            Some("api.anthropic.com") => FAMILY_ANTHROPIC.to_string(),
            Some(host) => family_for_host(host)
                .filter(|id| *id != FAMILY_OPENAI_API && *id != FAMILY_OPENAI_CHATGPT)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{FAMILY_CUSTOM_PREFIX}:{host}")),
            None => "custom:invalid-claude-endpoint".to_string(),
        };
        let label = family_label(&id);
        return (id, label);
    }
    let host = base_url.and_then(base_url_host);
    let provider = model_provider.map(|p| p.trim().to_ascii_lowercase()).filter(|p| !p.is_empty());
    let host_name = host.as_deref().map(|h| h.split(':').next().unwrap_or(h).to_string());
    let id = host_name
        .as_deref()
        .and_then(family_for_host)
        .or_else(|| provider.as_deref().and_then(family_for_provider_id))
        .map(str::to_string)
        .unwrap_or_else(|| match (&host, &provider) {
            (Some(host), _) => format!("{FAMILY_CUSTOM_PREFIX}:{host}"),
            (None, Some(provider)) => format!("{FAMILY_CUSTOM_PREFIX}:{provider}"),
            (None, None) => FAMILY_CUSTOM_PREFIX.to_string(),
        });
    let label = family_label(&id);
    (id, label)
}

pub fn family_label(id: &str) -> String {
    match id {
        FAMILY_OPENAI_CHATGPT => "OpenAI (ChatGPT)".to_string(),
        FAMILY_OPENAI_API => "OpenAI API".to_string(),
        FAMILY_ANTHROPIC => "Anthropic (Claude)".to_string(),
        FAMILY_DEEPSEEK => "DeepSeek".to_string(),
        FAMILY_ZHIPU => "Zhipu GLM (Z.ai / BigModel)".to_string(),
        FAMILY_MOONSHOT => "Moonshot (Kimi)".to_string(),
        FAMILY_OPENROUTER => "OpenRouter".to_string(),
        FAMILY_MINIMAX => "MiniMax".to_string(),
        FAMILY_XAI => "xAI (Grok)".to_string(),
        FAMILY_KIMI_CODE => "Kimi Code".to_string(),
        FAMILY_CUSTOM_PREFIX => "Custom".to_string(),
        other => match other.strip_prefix("custom:") {
            Some(key) => format!("Custom ({key})"),
            None => other.to_string(),
        },
    }
}

/// `(providerFamily, providerFamilyLabel)` of a stored profile.
pub fn provider_family(account: &StoredAccount) -> (String, String) {
    if account.cli_kind == CliKind::Dsh {
        return ("deepseek".into(), "DeepSeek".into());
    }
    // r266 Kimi-kind profiles (a Kimi account's subscription login): Kimi Code
    // membership, whatever the account's region (desktop design 049).
    if account.cli_kind == CliKind::Kimi {
        return (FAMILY_KIMI_CODE.into(), family_label(FAMILY_KIMI_CODE));
    }
    if account.cli_kind == CliKind::Claude {
        let id = account
            .claude_account
            .as_deref()
            .unwrap_or(crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID);
        if id == crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID {
            return match crate::runtime::claude::ClaudeDeployment::selected() {
                Ok(deployment) => default_claude_family(&deployment.claude_home),
                Err(_) => {
                    // Before installing an adapter the implicit Claude account
                    // still uses its ordinary settings home. A corrupt/existing
                    // deployment is not evidence that the default home applies.
                    let absent = crate::runtime::claude::deployment_path().ok()
                        .is_some_and(|path| matches!(std::fs::symlink_metadata(path),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound));
                    if absent {
                        crate::config::paths::home_dir().map(|home| default_claude_family(&home.join(".claude")))
                            .unwrap_or_else(unresolved_default_claude_family)
                    } else {
                        unresolved_default_claude_family()
                    }
                }
            };
        }
        let registry = crate::runtime::claude_accounts::load_cached();
        if let Some(claude) = registry.find(id) {
            return family_for(
                &account.cli_kind,
                None,
                claude.api.as_ref().and_then(|api| api.base_url.as_deref()),
            );
        }
        let family = format!("custom:unresolved-claude-account:{id}");
        return (family.clone(), family_label(&family));
    }
    family_for(
        &account.cli_kind,
        account_model_provider(account).as_deref(),
        account_model_api_base(account).as_deref(),
    )
}

fn unresolved_default_claude_family() -> (String, String) {
    let family = "custom:unresolved-default-claude-settings".to_string();
    (family.clone(), family_label(&family))
}

/// Only settings.json is inspected: credentials and auth files are never needed
/// to determine the upstream endpoint of the implicit Claude account.
fn default_claude_family(home: &std::path::Path) -> (String, String) {
    use std::io::Read;
    let path = home.join("settings.json");
    let read = (|| -> anyhow::Result<Option<String>> {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(meta.is_file() && !meta.file_type().is_symlink(), "invalid Claude settings file");
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            anyhow::ensure!(meta.file_attributes() & 0x400 == 0, "linked Claude settings file");
        }
        const MAX_SETTINGS: u64 = 1024 * 1024;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(&path)?;
        let opened = file.metadata()?;
        anyhow::ensure!(opened.is_file(), "invalid opened Claude settings file");
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            anyhow::ensure!(opened.file_attributes() & 0x400 == 0, "linked opened Claude settings file");
        }
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= MAX_SETTINGS, "Claude settings too large");
        let settings: Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(settings.is_object(), "invalid Claude settings object");
        if let Some(env) = settings.get("env") {
            anyhow::ensure!(env.is_object(), "invalid Claude settings env");
        }
        let Some(value) = settings.get("env").and_then(|env| env.get("ANTHROPIC_BASE_URL")) else { return Ok(None); };
        let url = value.as_str().ok_or_else(|| anyhow::anyhow!("invalid Claude endpoint"))?.trim();
        if url.is_empty() { return Ok(None); }
        anyhow::ensure!(url::Url::parse(url).is_ok_and(|url| url.host_str().is_some()), "invalid Claude endpoint");
        Ok(Some(url.to_string()))
    })();
    match read {
        Ok(base_url) => family_for(&CliKind::Claude, None, base_url.as_deref()),
        Err(_) => unresolved_default_claude_family(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstalled_default_account_still_classifies_ordinary_settings_without_credentials() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let account: StoredAccount = serde_json::from_value(serde_json::json!({
            "id":"default-test", "name":"default", "email":null, "plan_type":null,
            "last_used_at":null, "cli_kind":"claude"})).unwrap();
        assert_eq!(provider_family(&account).0, FAMILY_ANTHROPIC);
        let home = crate::config::paths::home_dir().unwrap().join(".claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir(home.join(".credentials.json")).unwrap();
        std::fs::write(home.join("settings.json"), br#"{"env":{"ANTHROPIC_BASE_URL":"https://relay.example.invalid/anthropic"}}"#).unwrap();
        assert_eq!(provider_family(&account).0, "custom:relay.example.invalid");
        let deployment = crate::runtime::claude::deployment_path().unwrap();
        std::fs::create_dir_all(deployment.parent().unwrap()).unwrap();
        std::fs::write(deployment, b"not valid deployment JSON").unwrap();
        assert_eq!(provider_family(&account).0, "custom:unresolved-default-claude-settings");
    }

    #[test]
    fn default_claude_settings_endpoint_controls_family_without_auth_reads() {
        let home = std::env::temp_dir().join(format!("cutex-default-family-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(default_claude_family(&home).0, FAMILY_ANTHROPIC);
        // A directory in place of credentials makes accidental auth reads fail.
        std::fs::create_dir(home.join(".credentials.json")).unwrap();
        for (settings, expected) in [
            (json!({"env":{"ANTHROPIC_BASE_URL":"https://api.z.ai/api/anthropic"}}), FAMILY_ZHIPU),
            (json!({"env":{"ANTHROPIC_BASE_URL":"https://relay.example/anthropic"}}), "custom:relay.example"),
            (json!({"env":{"ANTHROPIC_BASE_URL":"https://api.anthropic.com"}}), FAMILY_ANTHROPIC),
            (json!({"env":{}}), FAMILY_ANTHROPIC),
        ] {
            std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
            assert_eq!(default_claude_family(&home).0, expected);
        }
        std::fs::write(home.join("settings.json"), b"invalid-json").unwrap();
        assert_eq!(default_claude_family(&home).0, "custom:unresolved-default-claude-settings");
        std::fs::remove_dir_all(home).unwrap();
    }

    fn family(provider: Option<&str>, base_url: Option<&str>) -> String {
        family_for(&CliKind::Codex, provider, base_url).0
    }

    #[test]
    fn claude_third_party_endpoints_are_not_anthropic() {
        assert_eq!(family_for(&CliKind::Claude, None, None).0, FAMILY_ANTHROPIC);
        assert_eq!(
            family_for(&CliKind::Claude, None, Some("https://api.anthropic.com")).0,
            FAMILY_ANTHROPIC
        );
        assert_ne!(
            family_for(
                &CliKind::Claude,
                None,
                Some("https://api.z.ai/api/anthropic")
            )
            .0,
            FAMILY_ANTHROPIC
        );
        assert_eq!(
            family_for(
                &CliKind::Claude,
                None,
                Some("https://relay.example/anthropic")
            )
            .0,
            "custom:relay.example"
        );
        assert_ne!(
            family_for(
                &CliKind::Claude,
                None,
                Some("https://api.anthropic.com.evil.example")
            )
            .0,
            FAMILY_ANTHROPIC
        );
    }

    #[test]
    fn families_group_known_providers_and_key_custom_by_host() {
        assert_eq!(family(Some("GLM"), Some("https://api.z.ai/api/coding/paas/v4")), "zhipu");
        assert_eq!(family(Some("zhipu"), Some("https://open.bigmodel.cn/api/coding/paas/v4")), "zhipu");
        assert_eq!(family(Some("zai"), None), "zhipu");
        assert_eq!(family(Some("deepseek"), Some("https://api.deepseek.com/")), "deepseek");
        assert_eq!(family(Some("kimi"), Some("https://api.moonshot.cn/v1")), "moonshot");
        assert_eq!(family(Some("x"), Some("https://openrouter.ai/api/v1")), "openrouter");
        assert_eq!(family(Some("x"), Some("https://api.minimaxi.com/v1")), "minimax");
        assert_eq!(family(Some("x"), Some("https://api.x.ai/v1")), "xai");
        assert_eq!(family(Some("kimi"), Some("https://api.kimi.com/coding/v1")), "kimi-code");
        assert_eq!(family(Some("x"), Some("https://api.kimi.ai/coding/v1")), "kimi-code");
        assert_eq!(family(Some("kimi-code"), None), "kimi-code");
        assert_eq!(family_label("kimi-code"), "Kimi Code");
        assert_eq!(family(Some("zai"), Some("https://api.z.ai/api/v1")), "zhipu");
        assert_eq!(family(Some("openai"), Some("https://chatgpt.com/backend-api/codex")), "openai-chatgpt");
        assert_eq!(family(Some("openai"), Some("https://api.openai.com/v1")), "openai-api");
        assert_eq!(family(Some("openai-api"), None), "openai-api");
        assert_eq!(family(Some("custom"), Some("https://www.relay.example/v1")), "custom:www.relay.example");
        assert_eq!(family(Some("local"), Some("http://127.0.0.1:8080/v1")), "custom:127.0.0.1:8080");
        assert_eq!(family(Some("Local"), None), "custom:local");
        assert_eq!(family(None, None), "custom");
        // An unknown host (proxy) falls back to a known provider ID; lookalike hosts do not match.
        assert_eq!(family(Some("deepseek"), Some("https://proxy.example.com/v1")), "deepseek");
        assert_eq!(family(Some("mine"), Some("https://evil-z.ai.example.com/v1")), "custom:evil-z.ai.example.com");
        assert_eq!(family_for(&CliKind::Claude, Some("zai"), None).0, "anthropic");
        assert_eq!(family_label("custom:example.com"), "Custom (example.com)");
        assert_eq!(family_for(&CliKind::Codex, Some("glm"), None).1, "Zhipu GLM (Z.ai / BigModel)");
    }

    #[test]
    fn glm_chat_completions_base_urls_map_to_responses() {
        assert_eq!(glm_responses_base_url_for("https://api.z.ai/api/coding/paas/v4"), Some("https://api.z.ai/api/v1"));
        assert_eq!(glm_responses_base_url_for("https://open.bigmodel.cn/api/coding/paas/v4/"), Some("https://open.bigmodel.cn/api/v1"));
        for fine in ["https://api.z.ai/api/v1", "https://relay.example.com/api/coding/paas/v4", "not a url", "https://api.z.ai/api/paas/v4"] {
            assert_eq!(glm_responses_base_url_for(fine), None, "{fine}");
        }
    }

    #[test]
    fn preset_models_url_applies_on_the_default_base_url_only() {
        let zai = "https://api.z.ai/api/coding/paas/v4/models";
        assert_eq!(preset_models_url("https://api.z.ai/api/v1"), Some(zai));
        assert_eq!(preset_models_url(" https://api.z.ai/api/v1/ "), Some(zai));
        assert_eq!(preset_models_url("https://open.bigmodel.cn/api/v1"),
            Some("https://open.bigmodel.cn/api/coding/paas/v4/models"));
        for other in ["https://api.z.ai/api/coding/paas/v4", "https://relay.example/api/v1",
            "https://api.deepseek.com", "https://api.moonshot.ai/v1"] {
            assert_eq!(preset_models_url(other), None, "{other}");
        }
    }

    #[test]
    fn presets_resolve_by_id_or_alias_and_build_configs() {
        assert_eq!(find_preset("GLM").unwrap().id, "zai");
        assert_eq!(find_preset("kimi").unwrap().id, "moonshot");
        assert_eq!(find_preset(" DeepSeek ").unwrap().id, "deepseek");
        assert_eq!(find_preset("grok").unwrap().id, "xai");
        assert_eq!(find_preset("kimi-cn").unwrap().id, "moonshot-cn");
        assert!(find_preset("nope").is_none());
        for preset in API_KEY_PRESETS {
            let root = api_key_profile_config(preset.provider_id, preset.provider_name, preset.base_url).unwrap();
            assert_eq!(root["model_provider"].as_str(), Some(preset.provider_id), "{}", preset.id);
            if let (Some(expected), Some(url)) = (preset.family, preset.base_url) {
                assert_eq!(family(Some(preset.provider_id), Some(url)), expected, "{}", preset.id);
            }
        }
        let root = api_key_profile_config("zai", "Z.ai", None).unwrap();
        assert_eq!(root["model_providers"]["zai"]["base_url"].as_str(), Some("https://api.z.ai/api/v1"));
        assert_eq!(root["model"].as_str(), Some("glm-5.3"));
        assert_eq!(root["model_reasoning_effort"].as_str(), Some("max"));
        let provider = root["model_providers"]["zai"].as_table().unwrap();
        assert_eq!(provider["name"].as_str(), Some("Z.ai"));
        assert_eq!(provider["env_key"].as_str(), Some("OPENAI_API_KEY"));
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(false));
        let deepseek = api_key_profile_config("deepseek", "DeepSeek", None).unwrap();
        assert_eq!(deepseek["model"].as_str(), Some(deepseek::DEEPSEEK_DEFAULT_MODEL));
        let presets = presets_json();
        // Every preset with a spec catalog writes it; OpenAI / OpenRouter /
        // custom have no default model.
        let by_id = |id: &str| presets.as_array().unwrap().iter().find(|preset| preset["id"] == id).unwrap().clone();
        for id in ["deepseek", "zai", "zhipu", "moonshot", "moonshot-cn", "kimi-code", "kimi-code-cn", "minimax", "minimax-cn", "xai"] {
            assert_eq!(by_id(id)["modelCatalog"], true, "{id}");
            let preset = find_preset(id).unwrap();
            let root = api_key_profile_config(preset.provider_id, preset.provider_name, preset.base_url).unwrap();
            assert_eq!(root["model"].as_str(), preset.default_model, "{id}");
            assert_eq!(root["model_providers"][preset.provider_id]["base_url"].as_str(), preset.base_url, "{id}");
        }
        for id in ["openai", "openrouter", "custom"] {
            assert_eq!(by_id(id)["defaultModel"], Value::Null, "{id}");
            assert_eq!(by_id(id)["modelCatalog"], false, "{id}");
        }
        // r265: runtimeCompatibility per preset (DeepSeek's official host adds DSH).
        let rows = |id: &str| by_id(id)["runtimeCompatibility"].as_array().unwrap().iter()
            .map(|row| (row["runtime"].as_str().unwrap().to_string(), row["reason"].as_str().map(str::to_string)))
            .collect::<Vec<_>>();
        let deepseek_rows = rows("deepseek");
        assert_eq!(deepseek_rows[0], ("codex".into(), None));
        assert_eq!(deepseek_rows[1], ("claude".into(), Some("anthropicRouteNotYetSupported".into())));
        assert_eq!(deepseek_rows[2].0, "dsh");
        assert_ne!(deepseek_rows[2].1.as_deref(), Some("providerNotDeepseekNative"));
        assert_eq!(rows("zai")[2], ("dsh".into(), Some("providerNotDeepseekNative".into())));
        assert_eq!(rows("custom")[1], ("claude".into(), Some("anthropicRouteRequired".into())));
        assert_eq!(by_id("minimax-cn")["providerFamily"], "minimax");
        assert_eq!(by_id("xai")["providerFamilyLabel"], "xAI (Grok)");
        assert_eq!(presets.as_array().unwrap().len(), API_KEY_PRESETS.len());
        let gemini_index = API_KEY_PRESETS.iter().position(|preset| preset.id == "gemini").unwrap();
        assert_eq!(presets[gemini_index]["label"], "Gemini (OpenAI-compatible relay)");
        assert_eq!(presets[gemini_index]["baseUrlRequired"], true);
        assert_eq!(presets[gemini_index]["providerFamily"], Value::Null);
        assert_eq!(presets[gemini_index]["defaultModel"], gemini::GEMINI_DEFAULT_MODEL);
        assert_eq!(presets[gemini_index]["webSearch"], "disabled");
        assert_eq!(presets[gemini_index]["modelCatalog"], false);
        // profile-search-tool is a per-profile owner switch: no preset sets it.
        assert!(presets.as_array().unwrap().iter().all(|preset| preset.get("searchTool").is_none()));
        assert_eq!(presets[gemini_index + 1]["id"], "custom");
        assert_eq!(presets[gemini_index + 1]["baseUrlRequired"], true);
        assert_eq!(find_preset("Google").unwrap().id, "gemini");
        let gemini = api_key_profile_config("gemini", "Gemini", Some("https://relay.example.com/v1")).unwrap();
        assert_eq!(gemini["web_search"].as_str(), Some("disabled"));
        assert!(gemini.get("tool_schema_compat").is_none());
        assert_eq!(presets[gemini_index]["toolSchemaCompat"], Value::Null);
        assert_eq!(presets[gemini_index + 1]["toolSchemaCompat"], Value::Null);
        assert_eq!(gemini["model_providers"]["gemini"]["base_url"].as_str(), Some("https://relay.example.com/v1"));
        assert_eq!(family(Some("gemini"), Some("https://relay.example.com/v1")), "custom:relay.example.com");
        // DeepSeek and xAI disable the built-in web search; the others keep
        // Codex's default.
        for id in ["deepseek", "xai"] {
            assert_eq!(api_key_profile_config(id, id, None).unwrap()["web_search"].as_str(), Some("disabled"), "{id}");
        }
        // Kimi Code: the official Codex template turns live web search on;
        // both regions share the provider ID and the catalog.
        let kimi_cn = find_preset("kimi-code-cn").unwrap();
        let root = api_key_profile_config(kimi_cn.provider_id, kimi_cn.provider_name, kimi_cn.base_url).unwrap();
        assert_eq!(root["web_search"].as_str(), Some("live"));
        assert_eq!(root["model"].as_str(), Some("kimi-for-coding"));
        assert_eq!(root["model_providers"]["kimi-code"]["base_url"].as_str(), Some("https://api.kimi.com/coding/v1"));
        assert_eq!(root["model_providers"]["kimi-code"]["wire_api"].as_str(), Some("responses"));
        assert!(root.get("model_reasoning_effort").is_none());
        assert_eq!(by_id("kimi-code")["quotaReader"], "kimi_code_plan");
        assert_eq!(by_id("kimi-code")["providerFamilyLabel"], "Kimi Code");
        // A Kimi-kind profile (Kimi account) is Kimi Code too, without any provider fields.
        let kimi: StoredAccount = serde_json::from_value(serde_json::json!({"id":"k","name":"K","cli_kind":"kimi","kimi_account":"ka-a"})).unwrap();
        assert_eq!(provider_family(&kimi), (FAMILY_KIMI_CODE.to_string(), "Kimi Code".to_string()));
        assert_eq!(find_preset("kimi-for-coding").unwrap().id, "kimi-code");
        for (id, name) in [("custom", "custom"), ("zai", "Z.ai"), ("moonshot", "Moonshot"), ("minimax", "MiniMax"), ("openrouter", "OpenRouter")] {
            assert!(api_key_profile_config(id, name, Some("https://x.example/v1")).unwrap().get("web_search").is_none(), "{id}");
        }
        let auto_quota: Vec<(&str, bool)> = presets
            .as_array()
            .unwrap()
            .iter()
            .map(|preset| (preset["id"].as_str().unwrap(), preset["autoQuota"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            auto_quota,
            [
                ("deepseek", true),
                ("zai", true),
                ("zhipu", true),
                ("moonshot", true),
                ("moonshot-cn", true),
                ("kimi-code", true),
                ("kimi-code-cn", true),
                ("minimax", false),
                ("minimax-cn", false),
                ("xai", false),
                ("openrouter", true),
                ("openai", false),
                ("gemini", false),
                ("custom", false),
            ]
        );
    }
}
