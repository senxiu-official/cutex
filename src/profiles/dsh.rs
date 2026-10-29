//! Native DSH profile settings. The credential is a stored key (r265,
//! `profiles::stored_keys`) or an explicit environment variable name.
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{model::StoredAccount, native_models::NativeModels, settings_api::ProfileApiPatch};
use crate::config::global_settings::ConfigValueUpdate;

/// DSH's own default model (pinned `dsh-v0.2.1-rc.3`,
/// `packages/llm/llm-deepseek/src/models.ts`). Profiles that omitted the model
/// before r265 read as this one (`deepseek-v4-flash` is retired upstream and
/// served by Flash); existing agents keep the model they were created with.
pub const DEFAULT_MODEL: &str = "deepseek-flash";
/// The pre-r265 default: not in DSH's catalog, still accepted by the
/// DeepSeek API, so stored values keep working.
pub const LEGACY_MODEL: &str = "deepseek-v4-flash";

/// One model of DSH's built-in DeepSeek catalog.
pub struct CatalogModel {
    pub id: &'static str,
    pub display_name: &'static str,
    pub input_modalities: &'static [&'static str],
}

/// DSH's built-in DeepSeek catalog at the pinned version (what the adapter's
/// `model/list` returns for the official provider). Keep in step with the pin.
pub const CATALOG: &[CatalogModel] = &[
    CatalogModel { id: "deepseek-flash", display_name: "DeepSeek-V4.1-Flash", input_modalities: &["text", "image"] },
    CatalogModel { id: "deepseek-v4-pro", display_name: "DeepSeek-V4-Pro", input_modalities: &["text"] },
];
/// DSH's default effort for DeepSeek models.
pub const DEFAULT_EFFORT: &str = "high";

/// Refuse a model DSH's pinned catalog does not offer (the legacy default stays accepted).
pub fn validate_catalog_model(model: &str) -> anyhow::Result<()> {
    ensure!(
        model == LEGACY_MODEL || CATALOG.iter().any(|entry| entry.id == model),
        "DSH model {model:?} is not in DSH's catalog; use one of: {}",
        CATALOG.iter().map(|entry| entry.id).collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

/// The models a DSH variant / profile offers new agents, for clients
/// (Management `runtimeVariants[].models`), so they do not hard-code them.
pub fn catalog_wire() -> Value {
    Value::Array(CATALOG.iter().map(|entry| json!({
        "id": entry.id, "displayName": entry.display_name, "inputModalities": entry.input_modalities,
        "supportedReasoningEfforts": EFFORTS, "defaultReasoningEffort": DEFAULT_EFFORT,
        "isDefault": entry.id == DEFAULT_MODEL,
    })).collect())
}
pub const DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
const RESERVED_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "TEMP",
    "TMP",
    "TMPDIR",
    "USERNAME",
    "USER",
    "LOGNAME",
    "USERDOMAIN",
    "COMPUTERNAME",
    "HOSTNAME",
    "OS",
    "SHELL",
    "PWD",
    "OLDPWD",
    "TERM",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "TZ",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PYTHONPATH",
    "PYTHONHOME",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "DSH_HOME",
    "DSH_INSTALL_ROOT",
    "DSH_SPIKE_INSTALL_ROOT",
    "DSH_SESSION_LOG_ROOT",
    "DSH_SPIKE_SESSIONS_ROOT",
    "DSH_TELEMETRY_DISABLED",
];
pub const EFFORTS: &[&str] = &["off", "low", "high", "max"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct DshOptions {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub api_key_env: String,
}

impl Default for DshOptions {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            reasoning_effort: None,
            api_key_env: DEFAULT_API_KEY_ENV.into(),
        }
    }
}

impl DshOptions {
    /// Omit additive defaults from legacy Codex/Claude profile persistence.
    pub fn is_default(&self) -> bool {
        self.model == DEFAULT_MODEL
            && self.reasoning_effort.is_none()
            && self.api_key_env == DEFAULT_API_KEY_ENV
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        super::catalog_api::validate_model_id(&self.model)?;
        ensure!(
            self.reasoning_effort
                .as_deref()
                .is_none_or(|effort| EFFORTS.contains(&effort)),
            "DSH reasoningEffort must be off, low, high or max"
        );
        let mut chars = self.api_key_env.bytes();
        ensure!(
            self.api_key_env.len() <= 128
                && chars
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "DSH apiKeyEnv must be an environment variable name"
        );
        let env = self.api_key_env.to_ascii_uppercase();
        ensure!(!["CUTEX_", "DSH_CUTEX_", "NODE_", "LD_", "DYLD_"].iter().any(|prefix| env.starts_with(prefix)) && !RESERVED_ENV.contains(&env.as_str()),
            "DSH apiKeyEnv must name an owner-defined key variable, not a reserved environment variable");
        Ok(())
    }
}

/// Apply runtime defaults without opening Codex/Claude config or auth files.
pub fn apply_patch(account: &mut StoredAccount, patch: &ProfileApiPatch) -> anyhow::Result<()> {
    ensure!(!matches!(patch.allow_session_switch, ConfigValueUpdate::Set(true)), "DSH sessions do not support mid-session profile switching");
    // r265: `apiKey` (write-only stored key) and `credential` are applied by
    // `runtime_variants::apply_dsh_credential`.
    ensure!(patch.private_chatgpt_auth_file.is_none(), "privateChatgptAuthFile does not apply to DSH profiles");
    ensure!(
        matches!(patch.claude_account, ConfigValueUpdate::Unchanged)
            && matches!(patch.claude_allow_write, ConfigValueUpdate::Unchanged)
            && matches!(patch.claude_allow_exec, ConfigValueUpdate::Unchanged)
            && matches!(patch.claude_setting_sources, ConfigValueUpdate::Unchanged)
            && matches!(patch.agent_turn_questions, ConfigValueUpdate::Unchanged)
            && matches!(
                patch.claude_file_checkpointing,
                ConfigValueUpdate::Unchanged
            )
            && patch.claude_ai_connectors.is_none()
            && matches!(patch.claude_compaction, ConfigValueUpdate::Unchanged)
            && matches!(patch.claude_compaction_models, ConfigValueUpdate::Unchanged),
        "Claude settings do not apply to DSH profiles"
    );
    let mut options = match &patch.dsh {
        ConfigValueUpdate::Unchanged => account.options.dsh.clone(),
        ConfigValueUpdate::Clear => DshOptions::default(),
        ConfigValueUpdate::Set(options) => options.clone(),
    };
    let mut config = patch.codex_config.clone();
    match &config.model {
        ConfigValueUpdate::Set(model) => {
            validate_catalog_model(model)?;
            options.model = model.clone();
        }
        ConfigValueUpdate::Clear => options.model = DEFAULT_MODEL.into(),
        ConfigValueUpdate::Unchanged => {}
    }
    match &config.model_reasoning_effort {
        ConfigValueUpdate::Set(effort) => options.reasoning_effort = Some(effort.clone()),
        ConfigValueUpdate::Clear => options.reasoning_effort = None,
        ConfigValueUpdate::Unchanged => {}
    }
    config.model = ConfigValueUpdate::Unchanged;
    config.model_reasoning_effort = ConfigValueUpdate::Unchanged;
    ensure!(
        config.is_empty(),
        "Codex settings do not apply to DSH profiles"
    );
    options.validate()?;
    account.options.dsh = options;
    Ok(())
}

pub fn models(account: &StoredAccount) -> NativeModels {
    let mut rows: Vec<(String, String, Vec<&str>)> = CATALOG
        .iter()
        .map(|entry| (entry.id.to_string(), entry.display_name.to_string(), entry.input_modalities.to_vec()))
        .collect();
    // A stored model outside the catalog (the legacy default) stays listed.
    if rows.iter().all(|(id, _, _)| id != &account.options.dsh.model) {
        rows.push((account.options.dsh.model.clone(), account.options.dsh.model.clone(), vec!["text"]));
    }
    NativeModels {
        rows: rows.into_iter().map(|(id, name, modalities)| json!({
            "slug": id, "display_name": name, "description": "DSH model",
            "supported_reasoning_levels": EFFORTS.iter().map(|effort| json!({"effort":effort,"description":effort})).collect::<Vec<_>>(),
            "default_reasoning_level": account.options.dsh.reasoning_effort.as_deref().unwrap_or(DEFAULT_EFFORT),
            "input_modalities": modalities,
        })).collect(),
        source: json!({"kind":"dsh"}),
    }
}

pub fn models_resource(account: &StoredAccount) -> Value {
    let snapshot = super::codex_profile::CodexProfileConfigSnapshot {
        model: Some(account.options.dsh.model.clone()),
        ..Default::default()
    };
    let rows = models(account)
        .rows
        .iter()
        .map(|row| {
            let mut view = super::catalog_api::model_view(row, &snapshot, &[]);
            view["entryDisplayName"] = json!(view["id"]
                .as_str()
                .and_then(|id| account.model_display_names.get(id)));
            view["effective"]["autoCompactTokenLimitSource"] = json!("notApplicable");
            view
        })
        .collect::<Vec<_>>();
    json!({
        "contractVersion":2, "profile":{"id":account.id,"name":account.name},
        "followsUpstream":false, "catalogOverlay":null, "unavailableModels":[],
        "catalogSupported":false, "modelVariantsSupported":false, "discoverySupported":false,
        "catalogPath":null, "catalogPresent":false, "selectedModel":account.options.dsh.model,
        "modelsSource":{"kind":"dsh"}, "modelsUnavailableReason":null,
        "autoCompactApplies":false, "discovery":null, "models":rows,
        "profileOverrides":{"modelContextWindow":null,"modelAutoCompactTokenLimit":null},
        "unsupportedModelFields":super::catalog_api::UNSUPPORTED_MODEL_FIELDS,
        "precedence":"DSH profile defaults apply to new sessions",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsh_switch_cannot_be_enabled_by_imported_options_or_patch() {
        let mut account: StoredAccount = serde_json::from_value(json!({
            "id":"dsh-switch-fixed", "name":"DSH", "cli_kind":"dsh",
            "options":{"allow_session_switch":true}
        })).unwrap();
        assert!(!super::super::settings_api::allow_session_switch(&account));
        let patch = ProfileApiPatch { allow_session_switch: ConfigValueUpdate::Set(true), ..Default::default() };
        assert!(apply_patch(&mut account, &patch).unwrap_err().to_string().contains("do not support"));
    }
    #[test]
    fn dsh_defaults_wire_and_validation() {
        let options: DshOptions = serde_json::from_value(json!({})).unwrap();
        assert_eq!(options, DshOptions::default());
        assert_eq!(
            serde_json::to_value(&options).unwrap(),
            json!({"model":DEFAULT_MODEL,"apiKeyEnv":DEFAULT_API_KEY_ENV})
        );
        for effort in EFFORTS {
            DshOptions {
                reasoning_effort: Some((*effort).into()),
                ..options.clone()
            }
            .validate()
            .unwrap();
        }
        for env in [
            "",
            "sk-secret-value",
            "9KEY",
            "KEY=value",
            " KEY",
            "A\nB",
            "CUTEX_BUS_TOKEN",
            "cutex_root_token",
            "PATH",
            "Home",
            "USERPROFILE",
            "SystemRoot",
            "NODE_OPTIONS",
            "NODE_EXTRA_CA_CERTS",
            "LD_AUDIT",
            "DSH_CUTEX_CONFIG",
            "DSH_CUTEX_TOKEN",
            "DSH_HOME",
            "DSH_INSTALL_ROOT",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            assert!(DshOptions {
                api_key_env: env.into(),
                ..options.clone()
            }
            .validate()
            .is_err());
        }
        for field in [
            "apiKey",
            "sandbox",
            "approvalPolicy",
            "permissionMode",
            "auth",
        ] {
            assert!(serde_json::from_value::<DshOptions>(json!({(field):"secret"})).is_err());
        }
        assert!(DshOptions {
            reasoning_effort: Some("medium".into()),
            ..options
        }
        .validate()
        .is_err());
    }
}
