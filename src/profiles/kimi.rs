//! r266 Kimi Code runtime facts for Profiles (docs/development/r266-kimi-profiles.md):
//! Kimi-native API-key detection, variant defaults, the Kimi account model
//! catalog and the hooks the kimi-runtime component (K1) registers.
//!
//! Nothing here reads or returns a key or token.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{bail, ensure};
use serde_json::{json, Value};

use super::model::StoredAccount;
use super::runtime_variants::Status;

pub const RUNTIME: &str = "kimi";
pub const FEATURE_PROFILES: &str = "kimi-runtime-profiles";
pub const FEATURE_ACCOUNTS: &str = "kimi-accounts";
/// Kimi thinking levels a variant / launch may name (`off` = thinking off).
pub const EFFORTS: &[&str] = &["off", "low", "medium", "high", "max"];
/// The provider `kimi login` provisions in an account's `config.toml`.
pub const MANAGED_PROVIDER: &str = "managed:kimi-code";
/// Kimi Code membership default model (Kimi Code presets).
pub const KIMI_CODE_DEFAULT_MODEL: &str = "kimi-for-coding";
pub const MAX_MODEL_CHARS: usize = 128;

// ---------------------------------------------------------------------------
// Hooks registered by the kimi-runtime component (K1)
// ---------------------------------------------------------------------------

/// How to run the pinned Kimi Code CLI: `program args… login`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KimiCli {
    pub program: PathBuf,
    /// Arguments before the subcommand (e.g. the `main.mjs` path for a Node payload).
    pub args: Vec<String>,
    /// Extra non-secret environment (e.g. `NODE_OPTIONS`).
    pub env: Vec<(String, String)>,
}

type ProbeFn = fn() -> (Status, Option<&'static str>);
type LocatorFn = fn() -> anyhow::Result<KimiCli>;

static COMPONENT_PROBE: RwLock<Option<ProbeFn>> = RwLock::new(None);
static CLI_LOCATOR: RwLock<Option<LocatorFn>> = RwLock::new(None);

/// K1: whether the `kimi-runtime` component can run Kimi on this host now.
pub fn set_component_probe(probe: ProbeFn) {
    *COMPONENT_PROBE.write().unwrap_or_else(|e| e.into_inner()) = Some(probe);
}

/// K1: where the pinned Kimi CLI is (used by Kimi account login).
pub fn set_cli_locator(locator: LocatorFn) {
    *CLI_LOCATOR.write().unwrap_or_else(|e| e.into_inner()) = Some(locator);
}

/// `(status, reason)` of the Kimi runtime on this host.
pub fn component_status() -> (Status, Option<&'static str>) {
    if !cfg!(any(windows, target_os = "linux")) {
        return (Status::ComponentNotInstalled, Some("platformUnsupported"));
    }
    match *COMPONENT_PROBE.read().unwrap_or_else(|e| e.into_inner()) {
        Some(probe) => probe(),
        None => (Status::ComponentNotInstalled, Some("componentNotInstalled")),
    }
}

/// Debug builds inside a private test home only: test overrides never work in a
/// release build or against the real home.
pub(crate) fn test_override(name: &str) -> Option<String> {
    if !cfg!(debug_assertions) || std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_none() {
        return None;
    }
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

pub const TEST_CLI_ENV: &str = "CUTEX_KIMI_TEST_CLI";
pub const TEST_OAUTH_HOST_ENV: &str = "CUTEX_KIMI_TEST_OAUTH_HOST";
pub const TEST_BASE_URL_ENV: &str = "CUTEX_KIMI_TEST_BASE_URL";

/// The Kimi CLI for a login: the test override (JSON array `[program, args…]`),
/// else the component's locator.
pub fn cli() -> anyhow::Result<KimiCli> {
    if let Some(raw) = test_override(TEST_CLI_ENV) {
        let parts: Vec<String> = serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("{TEST_CLI_ENV} must be a JSON array of strings"))?;
        let (program, args) = parts.split_first().ok_or_else(|| anyhow::anyhow!("{TEST_CLI_ENV} is empty"))?;
        return Ok(KimiCli { program: PathBuf::from(program), args: args.to_vec(), env: Vec::new() });
    }
    match *CLI_LOCATOR.read().unwrap_or_else(|e| e.into_inner()) {
        Some(locator) => locator(),
        None => bail!("the kimi-runtime component is not installed on this host"),
    }
}

// ---------------------------------------------------------------------------
// Kimi-native API-key profiles
// ---------------------------------------------------------------------------

pub fn is_kimi_provider(provider: &str) -> bool {
    matches!(provider, "moonshot" | "kimi" | "kimi-code")
}

/// Kimi's documented model family of an API-key profile, by provider id
/// (never by URL text: a relay URL names neither).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFamily {
    /// `kimi-code` presets: the Kimi Code membership catalog.
    KimiCode,
    /// `moonshot` presets: the Moonshot open-platform catalog.
    Moonshot,
}

pub fn model_family(provider: &str) -> Option<ModelFamily> {
    match provider {
        super::providers::KIMI_CODE_PROVIDER_ID => Some(ModelFamily::KimiCode),
        "moonshot" => Some(ModelFamily::Moonshot),
        _ => None,
    }
}

/// The model family of a Kimi API-key profile (`None`: provider `kimi`, or
/// not a Kimi API-key profile).
pub fn account_model_family(account: &StoredAccount) -> Option<ModelFamily> {
    is_native_api_key(account).then(|| super::interface_urls::provider_id(account)).flatten().and_then(|provider| model_family(&provider))
}

/// A Codex API-key profile of a Kimi / Moonshot provider, on any address
/// (owner decision 2026-10-28: official base URLs are only defaults; a relay
/// keeps Kimi Code). Names, model ids and hosts never count.
pub fn is_native_api_key(account: &StoredAccount) -> bool {
    super::interface_urls::is_kimi_api_key(account)
}

/// Default model of a `kimi` variant on an API-key profile: Kimi Code's
/// membership model, else the profile's own (Moonshot) model.
pub fn api_key_default_model(account: &StoredAccount) -> Option<String> {
    if !is_native_api_key(account) {
        return None;
    }
    if account_model_family(account) == Some(ModelFamily::KimiCode) {
        return Some(KIMI_CODE_DEFAULT_MODEL.to_string());
    }
    super::settings_api::codex_snapshot(account).model.filter(|model| !model.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

pub fn validate_model(model: &str) -> anyhow::Result<()> {
    ensure!(
        !model.is_empty()
            && model.chars().count() <= MAX_MODEL_CHARS
            && !model.chars().any(|c| c.is_control() || c.is_whitespace()),
        "model must be 1-{MAX_MODEL_CHARS} characters without spaces"
    );
    Ok(())
}

pub fn validate_effort(effort: &str) -> anyhow::Result<()> {
    ensure!(EFFORTS.contains(&effort), "Kimi reasoningEffort must be one of {}", EFFORTS.join(", "));
    Ok(())
}

/// Defaults of a `kimi` variant. `catalog` (Kimi accounts) restricts the model
/// to the account's provisioned aliases once the account has one.
pub fn validate_defaults(defaults: &super::runtime_variants::VariantDefaults, catalog: Option<&Catalog>) -> anyhow::Result<()> {
    if let Some(model) = &defaults.model {
        validate_model(model)?;
        if let Some(catalog) = catalog.filter(|catalog| !catalog.models.is_empty()) {
            ensure!(catalog.resolve(model).is_some(), "model {model} is not in the Kimi account's model list");
        }
    }
    if let Some(effort) = &defaults.reasoning_effort {
        validate_effort(effort)?;
    }
    Ok(())
}

/// PATCH of a Kimi-kind profile: `kimiAccount`, `model` / `modelReasoningEffort`
/// (the primary variant's defaults); Codex / Claude / DSH settings are refused.
pub fn apply_profile_patch(account: &mut StoredAccount, patch: &super::settings_api::ProfileApiPatch) -> anyhow::Result<()> {
    use crate::config::global_settings::ConfigValueUpdate as U;
    ensure!(!matches!(patch.allow_session_switch, U::Set(true)), "Kimi sessions do not support mid-session profile switching");
    ensure!(patch.private_chatgpt_auth_file.is_none() && patch.search_tool.is_none(), "Codex settings do not apply to Kimi profiles");
    ensure!(
        matches!(patch.claude_account, U::Unchanged)
            && matches!(patch.claude_allow_write, U::Unchanged)
            && matches!(patch.claude_allow_exec, U::Unchanged)
            && matches!(patch.claude_setting_sources, U::Unchanged)
            && matches!(patch.agent_turn_questions, U::Unchanged)
            && matches!(patch.claude_file_checkpointing, U::Unchanged)
            && patch.claude_ai_connectors.is_none()
            && matches!(patch.claude_compaction, U::Unchanged)
            && matches!(patch.claude_compaction_models, U::Unchanged),
        "Claude settings do not apply to Kimi profiles"
    );
    match &patch.kimi_account {
        U::Unchanged => {}
        U::Clear => account.variants.kimi_account = None,
        U::Set(id) => {
            super::kimi_accounts::validate_id(id)?;
            ensure!(super::kimi_accounts::load()?.find(id).is_some(), "kimiAccount: no Kimi account {id} on this host");
            account.variants.kimi_account = Some(id.clone());
        }
    }
    let mut defaults = account.variants.kimi_defaults.clone().unwrap_or_default();
    let mut config = patch.codex_config.clone();
    match &config.model {
        U::Set(model) => defaults.model = Some(model.trim().to_string()),
        U::Clear => defaults.model = None,
        U::Unchanged => {}
    }
    match &config.model_reasoning_effort {
        U::Set(effort) => defaults.reasoning_effort = Some(effort.trim().to_string()),
        U::Clear => defaults.reasoning_effort = None,
        U::Unchanged => {}
    }
    config.model = U::Unchanged;
    config.model_reasoning_effort = U::Unchanged;
    ensure!(config.is_empty(), "Codex settings do not apply to Kimi profiles");
    let model_changed = !matches!(patch.codex_config.model, U::Unchanged);
    let catalog = profile_catalog(account);
    validate_defaults(&defaults, model_changed.then_some(&catalog))?;
    account.variants.kimi_defaults = (defaults != Default::default()).then_some(defaults);
    Ok(())
}

// ---------------------------------------------------------------------------
// Kimi account model catalog (what `kimi login` provisioned; non-secret)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogModel {
    /// Kimi model alias (`kimi-code/<id>`), what a session selects.
    pub alias: String,
    pub model: String,
    pub display_name: Option<String>,
    pub capabilities: Vec<String>,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub max_context_size: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    pub models: Vec<CatalogModel>,
    pub default_model: Option<String>,
}

impl Catalog {
    /// A model by alias or by its upstream id.
    pub fn resolve(&self, name: &str) -> Option<&CatalogModel> {
        self.models
            .iter()
            .find(|model| model.alias == name)
            .or_else(|| self.models.iter().find(|model| model.model == name))
    }

    pub fn wire(&self) -> Value {
        let default = self.default_model.as_deref();
        Value::Array(
            self.models
                .iter()
                .map(|model| {
                    let mut modalities = vec!["text"];
                    if model.capabilities.iter().any(|cap| cap == "image_in") {
                        modalities.push("image");
                    }
                    if model.capabilities.iter().any(|cap| cap == "video_in") {
                        modalities.push("video");
                    }
                    let thinking = model.capabilities.iter().any(|cap| cap == "thinking");
                    let always = model.capabilities.iter().any(|cap| cap == "always_thinking");
                    let mut efforts: Vec<String> = Vec::new();
                    if thinking && !always {
                        efforts.push("off".into());
                    }
                    if thinking {
                        if model.efforts.is_empty() {
                            efforts.push("high".into());
                        } else {
                            efforts.extend(model.efforts.iter().filter(|e| EFFORTS.contains(&e.as_str())).cloned());
                        }
                    }
                    json!({
                        "id": model.alias,
                        "displayName": model.display_name.clone().unwrap_or_else(|| model.model.clone()),
                        "inputModalities": modalities,
                        "supportedReasoningEfforts": efforts,
                        "defaultReasoningEffort": if thinking { model.default_effort.clone() } else { None },
                        "isDefault": Some(model.alias.as_str()) == default,
                    })
                })
                .collect(),
        )
    }
}

fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The managed models of a Kimi `config.toml` (lenient: unknown shapes are skipped).
pub fn catalog_from_config(config: &toml::Table) -> Catalog {
    let mut models = Vec::new();
    if let Some(table) = config.get("models").and_then(toml::Value::as_table) {
        for (alias, entry) in table {
            let Some(entry) = entry.as_table() else { continue };
            if entry.get("provider").and_then(toml::Value::as_str) != Some(MANAGED_PROVIDER) {
                continue;
            }
            let model = entry.get("model").and_then(toml::Value::as_str).unwrap_or(alias).to_string();
            models.push(CatalogModel {
                alias: alias.clone(),
                model,
                display_name: entry.get("display_name").and_then(toml::Value::as_str).map(str::to_string),
                capabilities: strings(entry.get("capabilities")),
                efforts: strings(entry.get("support_efforts")),
                default_effort: entry.get("default_effort").and_then(toml::Value::as_str).map(str::to_string),
                max_context_size: entry.get("max_context_size").and_then(toml::Value::as_integer),
            });
        }
    }
    let default_model = config
        .get("default_model")
        .and_then(toml::Value::as_str)
        .filter(|alias| models.iter().any(|model| model.alias == *alias))
        .map(str::to_string);
    Catalog { models, default_model }
}

/// Read a Kimi home's `config.toml` (missing = empty).
pub fn read_config(home: &Path) -> anyhow::Result<toml::Table> {
    let path = home.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|_| anyhow::anyhow!("Kimi config.toml of the account is not valid TOML")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(error) => bail!("read Kimi account config: {}", error.kind()),
    }
}

/// Model rows of a Kimi-kind profile (its account's catalog).
pub fn profile_catalog(account: &StoredAccount) -> Catalog {
    account
        .variants
        .kimi_account
        .as_deref()
        .and_then(|id| super::kimi_accounts::account_home(id).ok())
        .and_then(|home| read_config(&home).ok())
        .map(|config| catalog_from_config(&config))
        .unwrap_or_default()
}

/// The selected model of a Kimi-kind profile: its default, else the account's.
pub fn profile_selected_model(account: &StoredAccount, catalog: &Catalog) -> Option<String> {
    account
        .variants
        .kimi_defaults
        .as_ref()
        .and_then(|defaults| defaults.model.clone())
        .or_else(|| catalog.default_model.clone())
}

/// `native_models` rows (Codex `ModelInfo` shape) of a Kimi-kind profile.
pub fn native_models(account: &StoredAccount) -> super::native_models::NativeModels {
    let catalog = profile_catalog(account);
    let rows = catalog
        .wire()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            let efforts = row["supportedReasoningEfforts"].as_array().cloned().unwrap_or_default();
            json!({
                "slug": row["id"], "display_name": row["displayName"], "description": "Kimi Code model",
                "supported_reasoning_levels": efforts.iter().map(|effort| json!({"effort": effort, "description": effort})).collect::<Vec<_>>(),
                "default_reasoning_level": row["defaultReasoningEffort"],
                "input_modalities": row["inputModalities"],
            })
        })
        .collect();
    super::native_models::NativeModels { rows, source: json!({"kind": "kimi"}) }
}

/// `GET /v2/profiles/{p}/models` of a Kimi-kind profile (same envelope as DSH).
pub fn models_resource(account: &StoredAccount) -> Value {
    let catalog = profile_catalog(account);
    let selected = profile_selected_model(account, &catalog);
    let snapshot = super::codex_profile::CodexProfileConfigSnapshot { model: selected.clone(), ..Default::default() };
    let rows = native_models(account)
        .rows
        .iter()
        .map(|row| {
            let mut view = super::catalog_api::model_view(row, &snapshot, &[]);
            view["entryDisplayName"] = json!(view["id"].as_str().and_then(|id| account.model_display_names.get(id)));
            view["effective"]["autoCompactTokenLimitSource"] = json!("notApplicable");
            view
        })
        .collect::<Vec<_>>();
    let unavailable = if account.variants.kimi_account.is_none() {
        Some("kimiAccountMissing")
    } else if catalog.models.is_empty() {
        Some("kimiAccountNotLoggedIn")
    } else {
        None
    };
    json!({
        "contractVersion":2, "profile":{"id":account.id,"name":account.name},
        "followsUpstream":false, "catalogOverlay":null, "unavailableModels":[],
        "catalogSupported":false, "modelVariantsSupported":false, "discoverySupported":false,
        "catalogPath":null, "catalogPresent":false, "selectedModel":selected,
        "modelsSource":{"kind":"kimi"}, "modelsUnavailableReason":unavailable,
        "autoCompactApplies":false, "discovery":null, "models":rows,
        "profileOverrides":{"modelContextWindow":null,"modelAutoCompactTokenLimit":null},
        "unsupportedModelFields":super::catalog_api::UNSUPPORTED_MODEL_FIELDS,
        "precedence":"Kimi profile defaults apply to new sessions",
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Relay URLs (2026-10-28): Kimi Code follows the provider id; the
    /// model family too, never the URL text.
    #[test]
    fn kimi_follows_the_provider_id_not_the_host() {
        assert!(is_kimi_provider("moonshot") && is_kimi_provider("kimi") && is_kimi_provider("kimi-code"));
        assert!(!is_kimi_provider("openai") && !is_kimi_provider("custom") && !is_kimi_provider("deepseek"));
        assert_eq!(model_family("kimi-code"), Some(ModelFamily::KimiCode));
        assert_eq!(model_family("moonshot"), Some(ModelFamily::Moonshot));
        assert_eq!(model_family("kimi"), None);

        let _home = super::super::lifecycle::tests::TestHome::new();
        let create = |name: &str, preset: &str, base_url: Option<&str>| {
            super::super::lifecycle::create_profile(&super::super::lifecycle::NewProfile {
                name: name.into(),
                kind: super::super::lifecycle::NewProfileKind::ApiKey,
                preset: Some(super::super::providers::find_preset(preset).unwrap()),
                base_url: base_url.map(str::to_string),
                model: Some("my-model".into()),
                api_key: None,
            })
            .unwrap()
        };
        let relay_profile = create("relay_profile", "moonshot", Some("https://relay.example/v1"));
        assert!(is_native_api_key(&relay_profile));
        assert_eq!(account_model_family(&relay_profile), Some(ModelFamily::Moonshot));
        assert_eq!(api_key_default_model(&relay_profile).as_deref(), Some("my-model"));
        let coding = create("coding relay", "kimi-code", Some("https://relay.example/v1"));
        assert!(is_native_api_key(&coding));
        assert_eq!(api_key_default_model(&coding).as_deref(), Some(KIMI_CODE_DEFAULT_MODEL));
        let custom = create("custom", "custom", Some("https://api.kimi.com/coding/v1"));
        assert!(!is_native_api_key(&custom), "a custom provider on Kimi's host is not Kimi");
        assert_eq!(api_key_default_model(&custom), None);
    }

    #[test]
    fn defaults_are_validated() {
        use super::super::runtime_variants::VariantDefaults;
        let ok = VariantDefaults { model: Some("kimi-for-coding".into()), reasoning_effort: Some("max".into()) };
        validate_defaults(&ok, None).unwrap();
        assert!(validate_defaults(&VariantDefaults { model: Some("a b".into()), reasoning_effort: None }, None).is_err());
        assert!(validate_defaults(&VariantDefaults { model: None, reasoning_effort: Some("xhigh".into()) }, None).is_err());
        let catalog = catalog_from_config(&toml::from_str(SAMPLE).unwrap());
        validate_defaults(&VariantDefaults { model: Some("kimi-code/kimi-for-coding".into()), reasoning_effort: None }, Some(&catalog)).unwrap();
        validate_defaults(&VariantDefaults { model: Some("kimi-for-coding".into()), reasoning_effort: None }, Some(&catalog)).unwrap();
        assert!(validate_defaults(&VariantDefaults { model: Some("gpt-5".into()), reasoning_effort: None }, Some(&catalog)).is_err());
    }

    pub(crate) const SAMPLE: &str = r#"
default_model = "kimi-code/kimi-for-coding"
[providers."managed:kimi-code"]
type = "kimi"
api_key = ""
base_url = "http://127.0.0.1:9/coding/v1"
[providers."managed:kimi-code".oauth]
storage = "file"
key = "oauth/kimi-code-env-0123456789abcdef"
oauth_host = "http://127.0.0.1:9"
[models."kimi-code/kimi-for-coding"]
provider = "managed:kimi-code"
model = "kimi-for-coding"
max_context_size = 262144
capabilities = [ "thinking", "image_in", "tool_use" ]
display_name = "Kimi For Coding"
support_efforts = [ "low", "high", "max" ]
default_effort = "high"
[models."kimi-code/kimi-k3-fast"]
provider = "managed:kimi-code"
model = "kimi-k3-fast"
max_context_size = 131072
capabilities = [ "tool_use" ]
[models."mine"]
provider = "other"
model = "x"
max_context_size = 1
[thinking]
enabled = true
[services.moonshot_search]
base_url = "http://127.0.0.1:9/coding/v1/search"
api_key = ""
[services.moonshot_search.oauth]
storage = "file"
key = "oauth/kimi-code-env-0123456789abcdef"
"#;

    #[test]
    fn catalog_reads_managed_models_only() {
        let catalog = catalog_from_config(&toml::from_str(SAMPLE).unwrap());
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.default_model.as_deref(), Some("kimi-code/kimi-for-coding"));
        let wire = catalog.wire();
        assert_eq!(wire[0]["id"], "kimi-code/kimi-for-coding");
        assert_eq!(wire[0]["inputModalities"], json!(["text", "image"]));
        assert_eq!(wire[0]["supportedReasoningEfforts"], json!(["off", "low", "high", "max"]));
        assert_eq!(wire[0]["defaultReasoningEffort"], "high");
        assert_eq!(wire[0]["isDefault"], true);
        assert_eq!(wire[1]["supportedReasoningEfforts"], json!([]));
        assert_eq!(wire[1]["defaultReasoningEffort"], Value::Null);
        assert!(catalog_from_config(&toml::Table::new()).models.is_empty());
    }
}
