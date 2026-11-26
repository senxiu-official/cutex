//! One reasoning-effort vocabulary for agent creation (research §8.2).
//!
//! The allowed values come, in order, from the model catalog's
//! `supported_reasoning_levels`, then the Claude effort list, then the full
//! native list; `default` (or an empty value) is always allowed and means the
//! profile / model default. There is no mapping between vocabularies: an
//! unsupported value is rejected with the allowed list.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EffortSource {
    Session,
    Model,
    Profile,
    Catalog,
    ClaudeCodeDefault,
    CutexDefault,
    Runtime,
    Unsupported,
    Unreported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedEffort {
    pub source: EffortSource,
    pub value: String,
    pub reason: SkippedEffortReason,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkippedEffortReason {
    UnsupportedByModel,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffortResolution {
    pub configured: Option<String>,
    pub value: Option<String>,
    pub source: EffortSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_version: Option<String>,
    #[serde(default)]
    pub skipped: Vec<SkippedEffort>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEffort {
    pub requested_model: String,
    pub resolved_model: Option<String>,
    pub resolution: EffortResolution,
}

pub fn resolve(
    configured: Option<&str>,
    owner_model: Option<&str>,
    profile: Option<&str>,
    builtin: Option<(&str, EffortSource)>,
    supported: Option<&[String]>,
) -> Result<EffortResolution, String> {
    use EffortSource::*;
    let configured = configured.filter(|value| !is_default(value));
    let owner_model = owner_model.filter(|value| !is_default(value));
    let profile = profile.filter(|value| !is_default(value));
    let skipped: Vec<_> = [(owner_model, Model), (profile, Profile)]
        .into_iter().filter_map(|(value, source)| {
            let value = value?;
            let supported = supported?;
            (!supported.iter().any(|level| level == value)).then(|| SkippedEffort {
                source,
                value: value.to_owned(),
                reason: SkippedEffortReason::UnsupportedByModel,
            })
        }).collect();
    let result = move |value: Option<&str>, source| EffortResolution {
        configured: configured.map(str::to_owned),
        value: value.map(str::to_owned),
        source,
        source_version: (source == ClaudeCodeDefault).then(|| CLAUDE_CODE_DEFAULT_VERSION.to_owned()),
        skipped,
    };
    if let Some(chosen) = configured {
        if let Some(supported) = supported {
            check(chosen, supported, "for the selected model")?;
        }
        return Ok(result(Some(chosen), Session));
    }
    let Some(supported) = supported else {
        // An existing owner setting is a chosen value even when the gateway
        // cannot describe its capabilities. Do not invent a runtime default.
        return Ok(match (owner_model, profile) {
            (Some(value), _) => result(Some(value), Model),
            (None, Some(value)) => result(Some(value), Profile),
            (None, None) => result(None, Unreported),
        });
    };
    if supported.is_empty() {
        return Ok(result(None, Unsupported));
    }
    for (value, source) in owner_model.map(|v| (v, Model))
        .into_iter().chain(profile.map(|v| (v, Profile))).chain(builtin)
    {
        if supported.iter().any(|level| level == value) {
            return Ok(result(Some(value), source));
        }
    }
    // Order of provider vocabularies is a stable tie-break: prefer the
    // lower level when it is equally close to medium.
    let rank = |level: &str| NATIVE_EFFORTS.iter().position(|entry| *entry == level);
    let medium = rank("medium").unwrap();
    let nearest = supported.iter().filter_map(|level| rank(level).map(|i| (i.abs_diff(medium), i, level)))
        .min().map(|(_, _, level)| level.as_str());
    Ok(result(nearest, if nearest.is_some() { CutexDefault } else { Unreported }))
}

pub(crate) fn profile_model_efforts(account: &super::model::StoredAccount) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let path = profile_model_efforts_path(account)?;
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(error) => Err(error.into()),
    }
}

fn profile_model_efforts_path(account: &super::model::StoredAccount) -> anyhow::Result<std::path::PathBuf> {
    Ok(super::materialize::materialized_profiles_dir()?.join(&account.id).join("model-efforts.json"))
}

pub(crate) fn update_profile_model_effort(
    account: &super::model::StoredAccount, id: &str, update: &crate::config::global_settings::ConfigValueUpdate<String>,
) -> anyhow::Result<()> {
    use crate::config::global_settings::ConfigValueUpdate;
    use anyhow::{Context, ensure};
    ensure!(matches!(account.cli_kind, super::model::CliKind::Claude | super::model::CliKind::Codex),
        "this runtime does not support per-model effort");
    super::catalog_api::validate_model_id(id)?;
    let path = profile_model_efforts_path(account)?;
    if let ConfigValueUpdate::Set(value) = update {
        let view = super::catalog_api::models_resource(account)?;
        let row = view["models"].as_array().into_iter().flatten()
            .find(|row| row["id"] == id).context("model missing from catalog")?;
        let levels = row["supportedReasoningEfforts"].as_array().context("model effort is unreported")?;
        let trustworthy = account.cli_kind != super::model::CliKind::Claude
            || uses_claude_reference(account) || row["effortSupportSource"] == "sdkHandshake";
        ensure!(trustworthy && levels.iter().any(|offered| offered == value.as_str()),
            "reasoning effort {value} is not offered by {id}");
    }
    let _lock = crate::config::store::config_write_lock(&path)?;
    let mut table = profile_model_efforts(account)?;
    match update {
        ConfigValueUpdate::Unchanged => return Ok(()),
        ConfigValueUpdate::Clear => { table.remove(id); }
        ConfigValueUpdate::Set(value) => { table.insert(id.to_owned(), value.to_owned()); }
    }
    crate::config::atomic::write_private_pretty_json_atomic(&path, &table, "profile model efforts")
}

pub(crate) fn resolve_claude(
    profile: Option<&super::model::StoredAccount>,
    requested_model: &str,
    configured: Option<&str>,
    handshake: Option<&super::claude_models::ModelHandshake>,
) -> anyhow::Result<SessionEffort> {
    let target = handshake.and_then(|sdk| sdk.resolved_models.get(requested_model)).map(String::as_str);
    let levels = handshake.and_then(|sdk| sdk.supported_effort_levels.get(requested_model)
        .or_else(|| target.and_then(|id| sdk.supported_effort_levels.get(id))));
    let settings = profile.map(profile_model_efforts).transpose()?.unwrap_or_default();
    let owner = settings.get(requested_model).or_else(|| target.and_then(|id| settings.get(id)));
    let profile_value = profile.and_then(|account| account.options.claude.effort).map(|value| value.as_str());
    let target_or_fixed = target.unwrap_or(requested_model);
    let builtin = claude_builtin_default(target_or_fixed);
    let reference = profile.filter(|account| uses_claude_reference(account))
        .and_then(|account| super::native_models::for_profile(account, Some(target_or_fixed)).ok())
        .and_then(|catalog| catalog.rows.into_iter().find(|row| row["slug"] == target_or_fixed
            && row["source"] == "reference" && row["effortSupportKnown"] == true))
        .map(|row| crate::launch::selected_profile::row_efforts(&row));
    // Reference data proves a fixed official model's supported levels. It
    // cannot establish which model an unobserved alias or gateway will use.
    let levels = levels.map(Vec::as_slice).or_else(|| {
        (builtin.is_some() || owner.is_some() || profile_value.is_some()).then(|| reference.as_deref()).flatten()
    });
    let resolution = resolve(configured, owner.map(String::as_str), profile_value, builtin, levels)
        .map_err(anyhow::Error::msg)?;
    Ok(SessionEffort {
        requested_model: requested_model.to_owned(),
        resolved_model: target.map(str::to_owned),
        resolution,
    })
}

pub(crate) fn uses_claude_reference(profile: &super::model::StoredAccount) -> bool {
    use crate::runtime::claude_accounts::{ClaudeAccountKind, load_cached};
    match profile.claude_account.as_deref() {
        None | Some("default") => true,
        Some(id) => load_cached().find(id).is_some_and(|account| account.kind == ClaudeAccountKind::Subscription),
    }
}

pub const CLAUDE_CODE_DEFAULT_VERSION: &str = "2.1.321";
pub fn claude_builtin_default(model: &str) -> Option<(&'static str, EffortSource)> {
    // Claude Code's model configuration guide, verified with the r52 CLI.
    // Versioned IDs only. Aliases require a concrete SDK handshake target.
    let id = model.strip_suffix("[1m]").unwrap_or(model);
    let level = match id {
        "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-haiku-5-5" | "claude-fable-5-2" => "medium",
        "claude-opus-4-7" => "xhigh",
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-6" | "claude-sonnet-5"
        | "claude-sonnet-4-6" | "claude-fable-5-1" | "claude-fable-5" => "high",
        _ => return None,
    };
    Some((level, EffortSource::ClaudeCodeDefault))
}

/// Every effort the native runtime knows (`ReasoningEffort`).
pub const NATIVE_EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "persistent",
];

/// The value that selects the profile / model default.
pub const DEFAULT: &str = "default";

/// Efforts the catalog lists for `model`; `None` when the catalog has no
/// such model or an empty effort list (the API-key launch then accepts any
/// native effort, `selected_profile::validate_model`).
pub fn catalog_efforts(catalog: &serde_json::Value, model: &str) -> Option<Vec<String>> {
    let row = catalog["models"]
        .as_array()?
        .iter()
        .find(|row| row["slug"].as_str() == Some(model))?;
    let efforts: Vec<String> = row["supported_reasoning_levels"]
        .as_array()?
        .iter()
        .filter_map(|level| level["effort"].as_str().or_else(|| level.as_str()))
        .map(str::to_string)
        .collect();
    (!efforts.is_empty()).then_some(efforts)
}

/// The allowed values (always starting with `default`).
pub fn allowed(catalog: Option<Vec<String>>, claude: bool) -> Vec<String> {
    let listed = match catalog {
        Some(efforts) => efforts,
        None if claude => super::native_models::CLAUDE_EFFORTS.iter().map(|e| e.to_string()).collect(),
        None => NATIVE_EFFORTS.iter().map(|e| e.to_string()).collect(),
    };
    std::iter::once(DEFAULT.to_string())
        .chain(listed.into_iter().filter(|effort| effort != DEFAULT))
        .collect()
}

/// Whether `value` selects the default (`default` or empty).
pub fn is_default(value: &str) -> bool {
    let value = value.trim();
    value.is_empty() || value == DEFAULT
}

/// `Ok` when `value` is allowed; otherwise the `invalid_reasoning` detail
/// with the allowed list.
pub fn check(value: &str, allowed: &[String], context: &str) -> Result<(), String> {
    if is_default(value) || allowed.iter().any(|effort| effort == value) {
        return Ok(());
    }
    Err(format!(
        "reasoning {value:?} is not supported {context}; allowed: {}; omit it or use \"default\" for the profile default",
        serde_json::to_string(allowed).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolved_effort_honors_precedence_and_evidence() {
        use EffortSource::*;
        let levels = ["low", "medium", "high"].map(str::to_owned);
        let builtin = claude_builtin_default("claude-opus-5-5").unwrap();
        assert_eq!(resolve(Some("low"), Some("high"), Some("medium"), Some(builtin), Some(&levels)).unwrap().source, Session);
        assert_eq!(resolve(None, Some("high"), Some("low"), Some(builtin), Some(&levels)).unwrap().source, Model);
        assert_eq!(resolve(None, None, Some("low"), Some(builtin), Some(&levels)).unwrap().source, Profile);
        let standard = resolve(None, None, None, Some(builtin), Some(&levels)).unwrap();
        assert_eq!((standard.value.as_deref(), standard.source), (Some("medium"), ClaudeCodeDefault));
        assert_eq!(resolve(None, None, None, None, None).unwrap().source, Unreported);
        assert_eq!(resolve(None, None, Some("high"), None, None).unwrap().source, Profile);
        assert_eq!(resolve(None, None, None, None, Some(&[])).unwrap().source, Unsupported);
        assert!(resolve(Some("max"), None, None, None, Some(&levels)).is_err());
        let nearest = resolve(None, None, None, None, Some(&["high".into(), "low".into()])).unwrap();
        assert_eq!((nearest.value.as_deref(), nearest.source), (Some("low"), CutexDefault));
    }

    #[test]
    fn unsupported_saved_values_include_source_and_reason() {
        let levels = ["medium".to_owned()];
        let result = resolve(None, Some("high"), Some("max"), None, Some(&levels)).unwrap();
        assert_eq!(result.value.as_deref(), Some("medium"));
        assert_eq!(json!(result.skipped), json!([
            {"source":"model","value":"high","reason":"unsupportedByModel"},
            {"source":"profile","value":"max","reason":"unsupportedByModel"}
        ]));
        let result = resolve(None, Some("high"), Some("max"), None, Some(&[])).unwrap();
        assert_eq!(result.source, EffortSource::Unsupported);
        assert_eq!(result.skipped.len(), 2);
        assert!(resolve(None, Some("high"), None, None, None).unwrap().skipped.is_empty());
    }

    #[test]
    fn persisted_effort_accepts_future_fields_at_every_level() {
        let saved: SessionEffort = serde_json::from_value(json!({
            "requestedModel":"opus","resolvedModel":null,"futureSessionField":true,
            "resolution":{
                "configured":null,"value":"medium","source":"profile","futureResolutionField":[],
                "skipped":[{"source":"model","value":"high","reason":"unsupportedByModel","futureSkippedField":{}}]
            }
        })).unwrap();
        assert_eq!(saved.requested_model, "opus");
        assert_eq!(saved.resolution.value.as_deref(), Some("medium"));
        assert_eq!(saved.resolution.skipped[0].value, "high");
    }

    #[test]
    fn an_inherited_alias_uses_the_default_of_its_current_sdk_target() {
        use crate::profiles::claude_models::ModelHandshake;
        let mut handshake = ModelHandshake {
            supported_effort_levels: [("opus".into(), vec!["low".into(), "medium".into(), "high".into()])].into(),
            ..Default::default()
        };
        handshake.resolved_models.insert("opus".into(), "claude-opus-5-5".into());
        let first = resolve_claude(None, "opus", None, Some(&handshake)).unwrap();
        assert_eq!(first.resolution.value.as_deref(), Some("medium"));
        assert_eq!(first.resolution.source, EffortSource::ClaudeCodeDefault);
        assert!(first.resolution.configured.is_none());
        handshake.resolved_models.insert("opus".into(), "claude-opus-5".into());
        let next = resolve_claude(None, "opus", None, Some(&handshake)).unwrap();
        assert_eq!(next.resolved_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(next.resolution.value.as_deref(), Some("high"));
        assert!(next.resolution.configured.is_none());
    }

    #[test]
    fn catalog_then_claude_then_native() {
        let catalog = json!({"models":[{"slug":"glm","supported_reasoning_levels":[{"effort":"high"},{"effort":"max"}]},
            {"slug":"plain","supported_reasoning_levels":[]}]});
        assert_eq!(allowed(catalog_efforts(&catalog, "glm"), false), ["default", "high", "max"]);
        assert_eq!(catalog_efforts(&catalog, "plain"), None);
        assert_eq!(catalog_efforts(&catalog, "other"), None);
        assert_eq!(allowed(None, true), ["default", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(allowed(None, false).len(), NATIVE_EFFORTS.len() + 1);
    }

    #[test]
    fn check_rejects_with_the_allowed_list_and_never_maps() {
        let claude = allowed(None, true);
        assert!(check("max", &claude, "").is_ok());
        assert!(check("default", &claude, "").is_ok());
        assert!(check("", &claude, "").is_ok());
        let error = check("minimal", &claude, "for model opus").unwrap_err();
        assert!(error.contains("\"minimal\" is not supported for model opus"), "{error}");
        assert!(error.contains(r#"allowed: ["default","low","medium","high","xhigh","max"]"#), "{error}");
        assert!(check("MAX", &claude, "").is_err());
    }
}
