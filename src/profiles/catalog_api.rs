//! Profile model catalog (`model_catalog_json`) view and edits for Management
//! clients (`/v2/profiles/{profile}/models...`).
//!
//! Catalog rows follow cute-codex `ModelInfo` (snake_case JSON). Edits never
//! rewrite the file a running runtime selected. API-key custom profiles write
//! a new immutable `models-<uuid>.json` per change and point the profile's
//! `model_catalog_json` at it through the ordinary profile writer (same
//! pattern as `cutex profile models --apply`). Official profiles store an
//! overlay on the live upstream list instead (`catalog_overlay`), selected as
//! a content-addressed `models-<sha256>.json`.
//!
//! Context precedence in cute-codex (models-manager `with_config_overrides`):
//! profile `model_context_window` replaces the catalog `context_window`
//! (clamped to the catalog `max_context_window`); profile
//! `model_auto_compact_token_limit` replaces the catalog
//! `auto_compact_token_limit`; the effective auto-compact limit is then
//! `min(limit, 90% of the resolved context window)`.

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde_json::{json, Map, Value};

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::global_settings::ConfigValueUpdate;
use crate::config::proxy::effective_proxy_config;
use crate::config::store::load_codez_config;
use crate::profiles::catalog_overlay as overlay;
use crate::profiles::codex_profile::{CodexProfileConfigPatch, CodexProfileConfigSnapshot};
use crate::profiles::inspect::{account_auth_payload, account_model_api_base, account_uses_chatgpt_auth};
use crate::profiles::materialize::materialized_account_files;
use crate::profiles::model::{CliKind, StoredAccount};
use crate::profiles::model_discovery::{self, ModelCatalog};
use crate::profiles::settings_api::{self, ProfileApiPatch};

pub const MAX_MODEL_ID_CHARS: usize = 200;
/// Per-model fields the desktop asked for that cute-codex `ModelInfo` has no
/// key for; requests naming them are rejected rather than invented.
pub const UNSUPPORTED_MODEL_FIELDS: &[&str] = &["maxOutputTokens"];

/// API-key routes retain legacy catalog support; native official edits need a
/// runtime that supports model variants and preserves wire model identities.
pub fn catalog_supported(account: &StoredAccount) -> bool {
    discovery_supported(account) || crate::profiles::model_variants::for_profile(account)
}

pub fn discovery_supported(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex
        && account.source.as_deref() != Some("official")
        && !account_uses_chatgpt_auth(account)
}

pub(crate) fn profile_dir(account: &StoredAccount) -> anyhow::Result<PathBuf> {
    Ok(materialized_account_files(account)?
        .config_path
        .parent()
        .context("profile directory missing")?
        .to_path_buf())
}

fn resolve_catalog_path(account: &StoredAccount, setting: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(setting);
    Ok(if path.is_absolute() { path } else { profile_dir(account)?.join(path) })
}

pub struct CatalogState {
    pub snapshot: CodexProfileConfigSnapshot,
    pub path: Option<PathBuf>,
    pub catalog: Option<Value>,
}

pub fn load(account: &StoredAccount) -> anyhow::Result<CatalogState> {
    let snapshot = settings_api::codex_snapshot(account);
    let path = snapshot
        .model_catalog_json
        .as_deref()
        .map(|setting| resolve_catalog_path(account, setting))
        .transpose()?;
    let catalog = match &path {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<Value>(&bytes)
                    .with_context(|| format!("model catalog is not JSON: {}", path.display()))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("model catalog could not be read"),
        },
        None => None,
    };
    Ok(CatalogState { snapshot, path, catalog })
}

fn discovered_path(account: &StoredAccount) -> anyhow::Result<PathBuf> {
    Ok(profile_dir(account)?.join("discovered-models.json"))
}

pub fn discovered(account: &StoredAccount) -> Option<ModelCatalog> {
    let bytes = std::fs::read(discovered_path(account).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn int(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

/// camelCase view of one catalog row plus the effective limits after the
/// profile-level overrides.
pub fn model_view(row: &Value, snapshot: &CodexProfileConfigSnapshot, discovered: &[String]) -> Value {
    let id = row.get("slug").and_then(Value::as_str).unwrap_or_default();
    let context_window = int(row, "context_window");
    let max_context_window = int(row, "max_context_window");
    let catalog_auto_compact = int(row, "auto_compact_token_limit");
    let profile_window = snapshot.model_context_window.and_then(|v| i64::try_from(v).ok());
    let profile_auto = snapshot.model_auto_compact_token_limit.and_then(|v| i64::try_from(v).ok());
    let resolved_window = match profile_window {
        Some(window) => Some(max_context_window.map_or(window, |max| window.min(max))),
        None => context_window.or(max_context_window),
    };
    let configured_auto = profile_auto.or(catalog_auto_compact);
    let effective_auto = match resolved_window {
        Some(window) => Some(configured_auto.map_or(window * 9 / 10, |limit| limit.min(window * 9 / 10))),
        None => configured_auto,
    };
    let efforts: Vec<Value> = row
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(|levels| levels.iter().filter_map(|level| level.get("effort").cloned()).collect())
        .unwrap_or_default();
    let mut view = json!({
        "id": id,
        "requestModel": row.get("request_model"),
        "displayName": row.get("display_name"),
        "contextWindow": context_window,
        "maxContextWindow": max_context_window,
        "autoCompactTokenLimit": catalog_auto_compact,
        "effectiveContextWindowPercent": int(row, "effective_context_window_percent"),
        "inputModalities": row.get("input_modalities").cloned().unwrap_or(json!(["text", "image"])),
        "supportedReasoningEfforts": efforts,
        "defaultReasoningEffort": row.get("default_reasoning_level"),
        "isDefault": snapshot.model.as_deref() == Some(id),
        "discovered": discovered.iter().any(|model| model == id),
        "effective": {
            "contextWindow": resolved_window,
            "contextWindowSource": if profile_window.is_some() { "profile" } else if resolved_window.is_some() { "catalog" } else { "unknown" },
            "autoCompactTokenLimit": effective_auto,
            "autoCompactTokenLimitSource": if profile_auto.is_some() { "profile" } else if catalog_auto_compact.is_some() { "catalog" } else if effective_auto.is_some() { "default90Percent" } else { "unknown" },
        },
    });
    // Preserve Claude reference provenance and compatibility evidence instead
    // of losing it while adapting native-shaped rows for Desktop.
    for field in ["kind", "family", "source", "description", "accountAvailability", "minCliVersion", "cliCompatibility", "effortSupportKnown"] {
        if let Some(value) = row.get(field) { view[field] = value.clone(); }
    }
    view
}

pub fn models_resource(account: &StoredAccount) -> anyhow::Result<Value> {
    let follows_upstream = overlay::uses_overlay(account);
    if follows_upstream {
        // One-time conversion of a legacy snapshot; retried while upstream is unavailable.
        if let Err(error) = overlay::ensure_migrated(account) {
            eprintln!("cutex-catalog-overlay: migration of {} skipped: {error:#}", account.id);
        }
    }
    let catalog_overlay = if follows_upstream { overlay::load(account)? } else { None };
    let state = load(account)?;
    let discovered = discovered(account);
    let discovered_ids = discovered.as_ref().map(|d| d.models.clone()).unwrap_or_default();
    // Official profiles show the live native list, with this profile's
    // overlay applied once it edited the list. Claude always uses its native
    // aliases; see `native_models`.
    let selected_model = match account.cli_kind {
        CliKind::Claude => claude_selected_model(account),
        CliKind::Codex => state.snapshot.model.clone(),
    };
    let mut unavailable = Vec::new();
    // An emptied overlay (the "migrated" marker) shows the plain native list.
    let edits = catalog_overlay.as_ref().filter(|edits| !edits.entries.is_empty());
    let (rows, models_source, models_unavailable) = if let Some(edits) = edits {
        // Resolve against the complete upstream (hidden rows included), then
        // apply the picker's visibility for display only.
        match crate::profiles::native_models::upstream_for_profile(account) {
            Ok(native) => {
                let resolved = overlay::resolve(&native.rows, edits);
                unavailable = resolved.unavailable;
                let rows = resolved
                    .rows
                    .into_iter()
                    .filter(|row| crate::profiles::native_models::shown_in_picker(row, selected_model.as_deref()))
                    .collect();
                (rows, Some(json!({ "kind": "overlay", "upstream": native.source })), None)
            }
            // Upstream unavailable: show the catalog the profile last launched with.
            Err(reason) => {
                let rows = state.catalog.as_ref().and_then(|c| c.get("models")).and_then(Value::as_array).cloned().unwrap_or_default();
                (rows, Some(json!({ "kind": "overlay", "upstream": Value::Null })), Some(reason))
            }
        }
    } else if state.catalog.is_some() || discovery_supported(account) {
        let rows = state
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.get("models"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        (rows, state.catalog.is_some().then(|| json!({ "kind": "catalog" })), None)
    } else {
        match crate::profiles::native_models::for_profile(account, selected_model.as_deref()) {
            Ok(models) => (models.rows, Some(models.source), None),
            Err(reason) => (Vec::new(), None, Some(reason)),
        }
    };
    let mut models: Vec<Value> =
        rows.iter().map(|row| model_view(row, &state.snapshot, &discovered_ids)).collect();
    if account.cli_kind == CliKind::Claude {
        for model in &mut models {
            model["isDefault"] = json!(model["id"].as_str() == selected_model.as_deref());
            model["defaultReasoningEffort"] = claude_default_effort(
                model, account.options.claude.effort.map(|effort| effort.as_str()));
        }
    }
    // profile-model-display-names: this profile's own name for each entry
    // (display only; `displayName` stays the catalog / native name).
    for model in &mut models {
        let name = model["id"].as_str().and_then(|id| account.model_display_names.get(id)).cloned();
        model["entryDisplayName"] = json!(name);
    }
    for model in &mut unavailable {
        let name = model["id"].as_str().and_then(|id| account.model_display_names.get(id)).cloned();
        model["entryDisplayName"] = json!(name);
    }
    let variants_supported = crate::profiles::model_variants::for_profile(account);
    Ok(json!({
        // profile-catalog-overlay: official lists follow upstream; edits are
        // an overlay, entries whose (base) model left upstream are listed in
        // `unavailableModels` {id, requestModel, displayName, reason}.
        "followsUpstream": follows_upstream,
        "catalogOverlay": catalog_overlay.as_ref().map(|edits| json!({
            "entries": edits.entries.len(),
            "migratedFrom": edits.migrated_from,
        })),
        "unavailableModels": unavailable,
        "contractVersion": 2,
        "profile": { "id": account.id, "name": account.name },
        "catalogSupported": discovery_supported(account) || variants_supported,
        "modelVariantsSupported": variants_supported,
        "discoverySupported": discovery_supported(account),
        "catalogPath": state.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "catalogPresent": state.catalog.is_some(),
        "selectedModel": selected_model,
        "modelsSource": models_source,
        "modelsUnavailableReason": models_unavailable,
        "profileOverrides": {
            "modelContextWindow": state.snapshot.model_context_window,
            "modelAutoCompactTokenLimit": state.snapshot.model_auto_compact_token_limit,
        },
        "discovery": discovered.as_ref().map(|d| json!({
            "sourceUrl": d.source_url,
            "fetchedAt": d.fetched_at,
            "models": d.models,
        })),
        "models": models,
        "unsupportedModelFields": UNSUPPORTED_MODEL_FIELDS,
        "precedence": "profile modelContextWindow replaces catalog contextWindow (clamped to maxContextWindow); profile modelAutoCompactTokenLimit replaces catalog autoCompactTokenLimit; effective auto-compact = min(limit, 90% of resolved context window)",
    }))
}

fn claude_default_effort(model: &Value, configured: Option<&str>) -> Value {
    json!(configured.filter(|effort| model["supportedReasoningEfforts"].as_array()
        .is_some_and(|levels| levels.iter().any(|level| level.as_str() == Some(*effort)))))
}

/// Claude default model: the profile's new-session default, else its own default.
fn claude_selected_model(account: &StoredAccount) -> Option<String> {
    crate::profiles::settings_api::claude_effective_model(&load_codez_config(), account)
        .filter(|model| !model.trim().is_empty())
}

/// Write a new immutable catalog file and select it for the profile.
fn select_catalog(account: &StoredAccount, catalog: &Value) -> anyhow::Result<()> {
    validate_catalog(catalog)?;
    crate::profiles::materialize::ensure_materialized_account_dir(&materialized_account_files(account)?)?;
    let path = profile_dir(account)?.join(format!("models-{}.json", uuid::Uuid::new_v4()));
    write_private_pretty_json_atomic(&path, catalog, "selected model catalog")?;
    let canonical = path.canonicalize()?.to_string_lossy().into_owned();
    let patch = ProfileApiPatch {
        codex_config: CodexProfileConfigPatch {
            model_catalog_json: ConfigValueUpdate::Set(canonical),
            ..CodexProfileConfigPatch::default()
        },
        ..ProfileApiPatch::default()
    };
    if let Err(error) = settings_api::update_profile(&account.id, &patch) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn validate_catalog(catalog: &Value) -> anyhow::Result<()> {
    let rows = catalog
        .get("models")
        .and_then(Value::as_array)
        .context("catalog must contain a models array")?;
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        let slug = row.get("slug").and_then(Value::as_str).context("catalog row without slug")?;
        ensure!(seen.insert(slug), "duplicate catalog model {slug}");
        for key in ["display_name", "supported_reasoning_levels", "shell_type", "visibility", "truncation_policy"] {
            ensure!(row.get(key).is_some(), "catalog model {slug} lacks {key}");
        }
    }
    crate::profiles::model_variants::validate_bindings(rows)?;
    Ok(())
}

/// The stored provider key of an API-key profile (model discovery, the
/// `web_search` cutex tool). Never logged or returned to clients.
pub(crate) fn api_key(account: &StoredAccount) -> anyhow::Result<String> {
    account_auth_payload(account)
        .and_then(|auth| {
            auth.get("OPENAI_API_KEY")
                .or_else(|| auth.get("openai_api_key"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_string)
        })
        .context("model discovery requires an API-key profile with a stored key")
}

/// Re-fetch `/models` with the profile's key and effective proxy; cache it in
/// `discovered-models.json`; with `apply`, merge into the catalog (existing
/// rows keep their metadata; new IDs get the native template) and select it.
pub fn discover(account: &StoredAccount, apply: bool) -> anyhow::Result<()> {
    ensure!(discovery_supported(account), "model discovery requires an API-key custom profile");
    let base = account_model_api_base(account).context("profile has no API base URL")?;
    let key = api_key(account)?;
    let global = load_codez_config();
    let proxy = effective_proxy_config(account, &global);
    let catalog = model_discovery::fetch(
        &base,
        &key,
        proxy.map(|p| if p.enabled { p.url.as_deref() } else { None }),
        proxy.and_then(|p| p.no_proxy.as_deref()),
    )?;
    crate::profiles::materialize::ensure_materialized_account_dir(&materialized_account_files(account)?)?;
    write_private_pretty_json_atomic(&discovered_path(account)?, &catalog, "discovered models")?;
    if apply {
        let state = load(account)?;
        let preset = model_discovery::preset(&catalog, state.catalog.as_ref(), state.snapshot.model.as_deref());
        select_catalog(account, &preset)?;
    }
    Ok(())
}

/// Native template row for a model ID unknown to the catalog.
pub(super) fn template_row(id: &str) -> Value {
    let single = ModelCatalog { models: vec![id.to_string()], ..ModelCatalog::default() };
    model_discovery::preset(&single, None, None)["models"][0].clone()
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModelPatch {
    pub request_model: ConfigValueUpdate<String>,
    pub display_name: ConfigValueUpdate<String>,
    pub context_window: ConfigValueUpdate<u64>,
    pub max_context_window: ConfigValueUpdate<u64>,
    pub auto_compact_token_limit: ConfigValueUpdate<u64>,
    pub effective_context_window_percent: ConfigValueUpdate<u64>,
    pub input_modalities: ConfigValueUpdate<Vec<String>>,
    pub supported_reasoning_efforts: ConfigValueUpdate<Vec<String>>,
    pub default_reasoning_effort: ConfigValueUpdate<String>,
}

fn set_or_clear<T: serde::Serialize>(row: &mut Map<String, Value>, key: &str, update: &ConfigValueUpdate<T>, clear: Value) {
    match update {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(value) => {
            row.insert(key.to_string(), json!(value));
        }
        ConfigValueUpdate::Clear => {
            if clear.is_null() {
                row.remove(key);
            } else {
                row.insert(key.to_string(), clear);
            }
        }
    }
}

/// Apply a per-model patch to a catalog row (pure; validated).
pub fn apply_model_patch(row: &mut Value, patch: &ModelPatch) -> anyhow::Result<()> {
    let object = row.as_object_mut().context("catalog row must be an object")?;
    set_or_clear(object, "display_name", &patch.display_name, Value::Null);
    if matches!(patch.display_name, ConfigValueUpdate::Clear) {
        let slug = object.get("slug").cloned().unwrap_or(Value::Null);
        object.insert("display_name".into(), slug);
    }
    set_or_clear(object, "context_window", &patch.context_window, Value::Null);
    set_or_clear(object, "max_context_window", &patch.max_context_window, Value::Null);
    set_or_clear(object, "auto_compact_token_limit", &patch.auto_compact_token_limit, Value::Null);
    set_or_clear(object, "effective_context_window_percent", &patch.effective_context_window_percent, Value::Null);
    set_or_clear(object, "input_modalities", &patch.input_modalities, Value::Null);
    match &patch.supported_reasoning_efforts {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Clear => {
            object.insert("supported_reasoning_levels".into(), json!([]));
        }
        ConfigValueUpdate::Set(efforts) => {
            let previous = object
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let levels = efforts
                .iter()
                .map(|effort| {
                    previous
                        .iter()
                        .find(|level| level.get("effort").and_then(Value::as_str) == Some(effort))
                        .cloned()
                        .unwrap_or_else(|| json!({ "effort": effort, "description": effort }))
                })
                .collect::<Vec<_>>();
            object.insert("supported_reasoning_levels".into(), Value::Array(levels));
        }
    }
    set_or_clear(object, "default_reasoning_level", &patch.default_reasoning_effort, Value::Null);
    // Consistency checks on the resulting row.
    let window = object.get("context_window").and_then(Value::as_i64);
    let max_window = object.get("max_context_window").and_then(Value::as_i64);
    let auto = object.get("auto_compact_token_limit").and_then(Value::as_i64);
    if let (Some(window), Some(max)) = (window, max_window) {
        ensure!(window <= max, "contextWindow ({window}) cannot exceed maxContextWindow ({max})");
    }
    if let (Some(auto), Some(window)) = (auto, window.or(max_window)) {
        ensure!(auto < window, "autoCompactTokenLimit ({auto}) must be smaller than the context window ({window})");
    }
    let efforts: Vec<&str> = object
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(|levels| levels.iter().filter_map(|l| l.get("effort").and_then(Value::as_str)).collect())
        .unwrap_or_default();
    if let Some(default) = object.get("default_reasoning_level").and_then(Value::as_str) {
        ensure!(
            efforts.is_empty() || efforts.contains(&default),
            "defaultReasoningEffort {default} is not in supportedReasoningEfforts"
        );
    }
    Ok(())
}

fn rows_mut(catalog: &mut Value) -> anyhow::Result<&mut Vec<Value>> {
    catalog
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("catalog must contain a models array")
}

pub fn validate_model_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        !id.trim().is_empty() && id.trim() == id && id.chars().count() <= MAX_MODEL_ID_CHARS
            && !id.chars().any(char::is_control),
        "model id must be 1-{MAX_MODEL_ID_CHARS} visible characters"
    );
    Ok(())
}

/// The `ModelInfo` keys a patch sets or clears explicitly (an official
/// overlay stores a changed one even while it equals upstream).
fn pinned_keys(patch: &ModelPatch) -> Vec<&'static str> {
    let touched = |unchanged: bool, key: &'static str| (!unchanged).then_some(key);
    use ConfigValueUpdate::Unchanged;
    [
        touched(matches!(patch.display_name, Unchanged), "display_name"),
        touched(matches!(patch.context_window, Unchanged), "context_window"),
        touched(matches!(patch.max_context_window, Unchanged), "max_context_window"),
        touched(matches!(patch.auto_compact_token_limit, Unchanged), "auto_compact_token_limit"),
        touched(matches!(patch.effective_context_window_percent, Unchanged), "effective_context_window_percent"),
        touched(matches!(patch.input_modalities, Unchanged), "input_modalities"),
        touched(matches!(patch.supported_reasoning_efforts, Unchanged), "supported_reasoning_levels"),
        touched(matches!(patch.default_reasoning_effort, Unchanged), "default_reasoning_level"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Create-or-update one model row. Returns whether the row was created.
pub fn update_model(account: &StoredAccount, id: &str, patch: &ModelPatch) -> anyhow::Result<bool> {
    ensure!(catalog_supported(account), "this profile does not use a custom model catalog");
    validate_model_id(id)?;
    let state = load(account)?;
    if matches!(patch.request_model, ConfigValueUpdate::Set(_)) {
        ensure!(crate::profiles::model_variants::for_profile(account), "the selected native runtime does not support model variants");
    }
    if let ConfigValueUpdate::Set(target) = &patch.request_model {
        if !discovery_supported(account) {
            // The complete upstream: a base the picker hides is still a model.
            let native = crate::profiles::native_models::upstream_for_profile(account)
                .map_err(anyhow::Error::msg)?;
            ensure!(native.rows.iter().any(|row| row["slug"].as_str() == Some(target)),
                "official configurations require a requestModel from the native catalog");
        }
    }
    if overlay::uses_overlay(account) {
        let pinned = pinned_keys(patch);
        let change = overlay::Change::Patch { request_model: &patch.request_model, pinned: &pinned };
        return overlay::edit(account, id, change, |catalog, upstream| {
            let rows = rows_mut(catalog)?;
            // Editing an entry this profile hid shows it again.
            if !matches!(patch.request_model, ConfigValueUpdate::Set(_))
                && !rows.iter().any(|row| row["slug"].as_str() == Some(id))
            {
                if let Some(base) = upstream.iter().find(|row| row["slug"].as_str() == Some(id)) {
                    rows.push(base.clone());
                }
            }
            crate::profiles::model_variants::patch_row(rows, id, patch, true)
        });
    }
    let mut catalog = crate::profiles::model_variants::catalog_for_edit(account, &state)?;
    let before = catalog.clone();
    let rows = rows_mut(&mut catalog)?;
    let created = crate::profiles::model_variants::patch_row(rows, id, patch, !discovery_supported(account))?;
    if catalog != before {
        select_catalog(account, &catalog)?;
    }
    Ok(created)
}

/// "Smart" reset: rebuild the row from the best known source — the bundled
/// DeepSeek catalog row for DeepSeek profiles, else the native template — then
/// re-apply the reviewed vendor metadata overlays.
pub fn reset_model(account: &StoredAccount, id: &str) -> anyhow::Result<()> {
    ensure!(catalog_supported(account), "this profile does not use a custom model catalog");
    validate_model_id(id)?;
    let state = load(account)?;
    if overlay::uses_overlay(account) {
        // Back to the live upstream row (a configuration keeps its identity).
        return overlay::edit(account, id, overlay::Change::Reset, |catalog, _| {
            crate::profiles::model_variants::reset_catalog_row(account, &state, id, catalog)
        });
    }
    let mut catalog = crate::profiles::model_variants::catalog_for_edit(account, &state)?;
    crate::profiles::model_variants::reset_catalog_row(account, &state, id, &mut catalog)?;
    select_catalog(account, &catalog)
}

fn remove_row(catalog: &mut Value, id: &str) -> anyhow::Result<()> {
    let rows = rows_mut(catalog)?;
    ensure!(!rows.iter().any(|row| row["request_model"].as_str() == Some(id)), "cannot remove a base model while configurations reference it");
    let before = rows.len();
    rows.retain(|row| row.get("slug").and_then(Value::as_str) != Some(id));
    ensure!(rows.len() != before, "model {id} is not in the catalog");
    Ok(())
}

pub fn remove_model(account: &StoredAccount, id: &str) -> anyhow::Result<()> {
    ensure!(catalog_supported(account), "this profile does not use a custom model catalog");
    let state = load(account)?;
    ensure!(state.snapshot.model.as_deref() != Some(id), "cannot remove the profile's selected model");
    if overlay::uses_overlay(account) {
        // An entry whose model left upstream is only in the overlay; an
        // upstream entry becomes hidden for this profile.
        if overlay::remove_unavailable(account, id)? {
            return Ok(());
        }
        return overlay::edit(account, id, overlay::Change::Remove, |catalog, _| remove_row(catalog, id));
    }
    let mut catalog = crate::profiles::model_variants::catalog_for_edit(account, &state)?;
    remove_row(&mut catalog, id)?;
    select_catalog(account, &catalog)
}

pub fn path_display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_patch_maps_to_model_info_keys_and_validates() {
        let mut row = template_row("glm-5.3");
        let patch = ModelPatch {
            context_window: ConfigValueUpdate::Set(200_000),
            auto_compact_token_limit: ConfigValueUpdate::Set(150_000),
            input_modalities: ConfigValueUpdate::Set(vec!["text".into()]),
            supported_reasoning_efforts: ConfigValueUpdate::Set(vec!["low".into(), "high".into()]),
            default_reasoning_effort: ConfigValueUpdate::Set("high".into()),
            ..ModelPatch::default()
        };
        apply_model_patch(&mut row, &patch).unwrap();
        assert_eq!(row["context_window"], 200_000);
        assert_eq!(row["auto_compact_token_limit"], 150_000);
        assert_eq!(row["input_modalities"], json!(["text"]));
        assert_eq!(row["supported_reasoning_levels"][1]["effort"], "high");
        assert_eq!(row["default_reasoning_level"], "high");
        validate_catalog(&json!({ "models": [row.clone()] })).unwrap();

        let bad_default = ModelPatch {
            default_reasoning_effort: ConfigValueUpdate::Set("max".into()),
            ..ModelPatch::default()
        };
        assert!(apply_model_patch(&mut row.clone(), &bad_default).is_err());
        let bad_auto = ModelPatch {
            auto_compact_token_limit: ConfigValueUpdate::Set(200_000),
            ..ModelPatch::default()
        };
        assert!(apply_model_patch(&mut row.clone(), &bad_auto).is_err());
        let clear = ModelPatch { context_window: ConfigValueUpdate::Clear, ..ModelPatch::default() };
        apply_model_patch(&mut row, &clear).unwrap();
        assert!(row.get("context_window").is_none());
    }

    #[test]
    fn effective_limits_follow_codex_precedence() {
        let row = json!({ "slug": "m", "context_window": 1_000_000, "max_context_window": 1_000_000 });
        let mut snapshot = CodexProfileConfigSnapshot::default();
        let view = model_view(&row, &snapshot, &[]);
        assert_eq!(view["effective"]["contextWindow"], 1_000_000);
        assert_eq!(view["effective"]["autoCompactTokenLimit"], 900_000);
        assert_eq!(view["effective"]["autoCompactTokenLimitSource"], "default90Percent");
        snapshot.model_context_window = Some(200_000);
        snapshot.model_auto_compact_token_limit = Some(190_000);
        let view = model_view(&row, &snapshot, &[]);
        assert_eq!(view["effective"]["contextWindow"], 200_000);
        assert_eq!(view["effective"]["contextWindowSource"], "profile");
        assert_eq!(view["effective"]["autoCompactTokenLimit"], 180_000);
        snapshot.model_context_window = Some(2_000_000);
        assert_eq!(model_view(&row, &snapshot, &[])["effective"]["contextWindow"], 1_000_000);
    }

    #[test]
    fn claude_reference_metadata_survives_desktop_model_view() {
        let row = json!({"slug": "reference-id", "kind": "pinned", "family": "family", "source": "reference",
            "description": "Reference only", "accountAvailability": "unknown", "effortSupportKnown": false,
            "minCliVersion": "2.1.100", "cliCompatibility": {"state": "unknown", "reason": "cliVersionUnknown"}});
        let view = model_view(&row, &CodexProfileConfigSnapshot::default(), &[]);
        for key in ["kind", "family", "source", "description", "accountAvailability", "effortSupportKnown", "minCliVersion", "cliCompatibility"] {
            assert_eq!(view[key], row[key], "{key}");
        }
        assert_eq!(view["supportedReasoningEfforts"], json!([]));
    }

    #[test]
    fn claude_profile_effort_must_be_supported_by_the_exact_model() {
        let older = json!({"supportedReasoningEfforts": ["low", "medium", "high", "max"]});
        assert_eq!(claude_default_effort(&older, Some("high")), "high");
        assert_eq!(claude_default_effort(&older, Some("xhigh")), Value::Null);
        assert_eq!(claude_default_effort(&older, None), Value::Null);
        assert_eq!(claude_default_effort(&json!({"supportedReasoningEfforts": []}), Some("high")), Value::Null);
        assert_eq!(claude_default_effort(&json!({}), Some("high")), Value::Null);
    }
}
