//! r290 P13: shared nominal limit resolution and value-preserving migration.
use super::codex_profile::CodexProfileConfigSnapshot;
use anyhow::{ensure, Context};
use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub struct Limits {
    pub window: Option<i64>,
    pub percent: Option<i64>,
    pub absolute: Option<i64>,
    pub window_source: &'static str,
    pub compact_source: &'static str,
}
fn count(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}
fn scaled(window: i64, percent: i64) -> i64 {
    ((i128::from(window) * i128::from(percent)) / 100) as i64
}
// r290 P13: owner provenance is independent of the merged catalog row.
pub const LIMIT_KEYS: [&str; 3] = [
    "context_window",
    "auto_compact_percent",
    "auto_compact_token_limit",
];
pub fn settings_values(value: &Value) -> Value {
    Value::Object(
        LIMIT_KEYS
            .into_iter()
            .filter_map(|key| {
                value
                    .get(key)
                    .filter(|v| v.is_i64())
                    .map(|v| (key.into(), v.clone()))
            })
            .collect(),
    )
}
pub fn with_settings(row: &Value, catalog: &Value, owner: &Value, preserved: &Value) -> Value {
    let mut row = row.clone();
    row["context_settings"] = serde_json::json!({"catalog":settings_values(catalog),"owner":settings_values(owner),"preserved":settings_values(preserved)});
    row
}
// r290 P13: only the edit record proves an imported/custom field is owner-set.
pub fn custom_provenance(
    catalog: &Value,
    edits: Option<&super::model_discovery::ModelEdits>,
) -> Value {
    let mut catalog = catalog.clone();
    if let Some(rows) = catalog.get_mut("models").and_then(Value::as_array_mut) {
        for row in rows {
            if row.get("context_settings").is_some_and(Value::is_object) {
                continue;
            }
            let keys = edits
                .map(|e| e.pinned_fields(row["slug"].as_str().unwrap_or_default()))
                .unwrap_or_default();
            let mut defaults = settings_values(row);
            let mut owner = serde_json::json!({});
            for key in LIMIT_KEYS {
                if keys.iter().any(|k| k == key) {
                    if let Some(value) = defaults.as_object_mut().unwrap().remove(key) {
                        owner[key] = value;
                    }
                }
            }
            if !owner.as_object().unwrap().is_empty() {
                *row = with_settings(row, &defaults, &owner, &serde_json::json!({}));
            }
        }
    }
    catalog
}
/// Update metadata after an explicit edit without discarding the original catalog.
pub fn edit_settings(row: &mut Value, before: Option<&Value>, keys: &[&str]) {
    let is_new = before.is_none_or(|old| old["slug"] != row["slug"]);
    let reference = before.unwrap_or(row);
    let has_catalog = reference
        .get("context_settings")
        .is_some_and(Value::is_object);
    let mut catalog = if has_catalog {
        layer(reference, "catalog")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(serde_json::json!({}))
    } else {
        settings_values(reference)
    };
    let mut owner = if is_new {
        serde_json::json!({})
    } else {
        layer(reference, "owner")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or(serde_json::json!({}))
    };
    let mut preserved = if is_new {
        serde_json::json!({})
    } else {
        layer(reference, "preserved")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or(serde_json::json!({}))
    };
    if before.is_none() && !has_catalog {
        for key in keys {
            catalog.as_object_mut().unwrap().remove(*key);
        }
    }
    for key in keys.iter().filter(|k| LIMIT_KEYS.contains(k)) {
        if let Some(value) = row.get(*key).filter(|v| v.is_i64()) {
            owner[*key] = value.clone();
        } else {
            owner.as_object_mut().unwrap().remove(*key);
        }
        preserved.as_object_mut().unwrap().remove(*key);
    }
    if keys
        .iter()
        .any(|key| ["auto_compact_percent", "auto_compact_token_limit"].contains(key))
    {
        preserved
            .as_object_mut()
            .unwrap()
            .remove("auto_compact_percent");
        preserved
            .as_object_mut()
            .unwrap()
            .remove("auto_compact_token_limit");
    }
    *row = with_settings(row, &catalog, &owner, &preserved);
}
fn layer<'a>(row: &'a Value, name: &str) -> Option<&'a Value> {
    row.get("context_settings").and_then(|s| s.get(name))
}
fn compact(layer: &Value) -> (Option<i64>, Option<i64>) {
    (
        count(layer, "auto_compact_percent"),
        count(layer, "auto_compact_token_limit"),
    )
}
pub fn resolve(row: &Value, profile: &CodexProfileConfigSnapshot) -> Limits {
    let max = count(row, "max_context_window");
    let pw = profile
        .model_context_window
        .and_then(|v| i64::try_from(v).ok());
    let pa = profile
        .model_auto_compact_token_limit
        .and_then(|v| i64::try_from(v).ok());
    if !profile.model_context_settings_v1 {
        let window = pw
            .map(|w| max.map_or(w, |m| w.min(m)))
            .or(count(row, "context_window"))
            .or(max);
        let absolute = pa.or(count(row, "auto_compact_token_limit"));
        return Limits {
            window,
            percent: None,
            absolute: window
                .map(|w| absolute.map_or(scaled(w, 90), |v| v.min(scaled(w, 90))))
                .or(absolute),
            window_source: if pw.is_some() {
                "profile"
            } else if window.is_some() {
                "catalog"
            } else {
                "unknown"
            },
            compact_source: if pa.is_some() {
                "profile"
            } else if count(row, "auto_compact_token_limit").is_some() {
                "catalog"
            } else if window.is_some() {
                "default90Percent"
            } else {
                "unknown"
            },
        };
    }
    let empty = serde_json::json!({});
    let catalog = if row.get("context_settings").is_some_and(Value::is_object) {
        layer(row, "catalog")
            .filter(|v| v.is_object())
            .unwrap_or(&empty)
    } else {
        row
    };
    let owner = layer(row, "owner").unwrap_or(&empty);
    let preserved = layer(row, "preserved").unwrap_or(&empty);
    let (window, window_source) = [
        (count(preserved, "context_window"), "preserved"),
        (count(owner, "context_window"), "model"),
        (pw, "profile"),
        (count(catalog, "context_window").or(max), "catalog"),
    ]
    .into_iter()
    .find(|(value, _)| value.is_some())
    .unwrap_or((None, "unknown"));
    let window = window.map(|w| max.map_or(w, |m| w.min(m)));
    let pp = profile
        .model_auto_compact_percent
        .and_then(|v| i64::try_from(v).ok());
    let ((percent, absolute), compact_source) = [
        (compact(preserved), "preserved"),
        (compact(owner), "model"),
        ((pp, pa), "profile"),
        (compact(catalog), "catalog"),
    ]
    .into_iter()
    .find(|((pct, abs), _)| pct.is_some() || abs.is_some())
    .unwrap_or((
        (None, None),
        if window.is_some() {
            "default90Percent"
        } else {
            "unknown"
        },
    ));
    let configured = percent
        .and_then(|p| window.map(|w| scaled(w, p)))
        .or(absolute);
    Limits {
        window,
        percent,
        absolute: window
            .map(|w| configured.map_or(scaled(w, 90), |n| n.min(scaled(w, 90))))
            .or(configured),
        window_source,
        compact_source,
    }
}

/// Runtime-only unknown-model budget; never exported as catalog capacity.
pub fn resolve_uncatalogued(profile: &CodexProfileConfigSnapshot) -> Limits {
    if !profile.model_context_settings_v1 {
        return resolve(
            &serde_json::json!({"context_window":272000,"max_context_window":272000}),
            profile,
        );
    }
    let mut limits = resolve(&serde_json::json!({}), profile);
    let budget = limits.window.unwrap_or(272000);
    let configured = limits.percent.map(|p| scaled(budget, p)).or(profile
        .model_auto_compact_token_limit
        .and_then(|v| i64::try_from(v).ok()));
    limits.absolute = Some(configured.map_or(scaled(budget, 90), |n| n.min(scaled(budget, 90))));
    if limits.compact_source == "unknown" {
        limits.compact_source = "default90Percent";
    }
    limits
}
pub fn limits_json(value: &Limits) -> Value {
    serde_json::json!({"contextWindow":value.window,"contextWindowSource":value.window_source,"autoCompactPercent":value.percent,"autoCompactPercentSource":value.compact_source,"autoCompactTokenLimit":value.absolute,"autoCompactTokenLimitSource":value.compact_source})
}
fn clear_layer_fields(row: &mut Value, keys: &[&str]) {
    for name in ["owner", "preserved"] {
        if let Some(object) = row
            .get_mut("context_settings")
            .and_then(|s| s.get_mut(name))
            .and_then(Value::as_object_mut)
        {
            for key in keys {
                object.remove(*key);
            }
        }
    }
}
/// Each field's default is resolved independently, with the actual other field.
pub fn default_values(row: &Value, profile: &CodexProfileConfigSnapshot) -> Value {
    let mut result = serde_json::json!({});
    for name in ["model", "profile"] {
        let mut context = row.clone();
        clear_layer_fields(&mut context, &["context_window"]);
        let mut settings = profile.clone();
        if name == "profile" {
            settings.model_context_window = None;
        }
        let context = resolve(&context, &settings);
        let mut auto = row.clone();
        clear_layer_fields(
            &mut auto,
            &["auto_compact_percent", "auto_compact_token_limit"],
        );
        let mut settings = profile.clone();
        if name == "profile" {
            settings.model_auto_compact_percent = None;
            settings.model_auto_compact_token_limit = None;
        }
        let mut values = limits_json(&resolve(&auto, &settings));
        values["contextWindow"] = serde_json::json!(context.window);
        values["contextWindowSource"] = serde_json::json!(context.window_source);
        result[name] = values;
    }
    result
}

/// Preserve only a genuine owner/Profile conflict, retaining the owner original.
pub fn migrate_catalog(
    catalog: &Value,
    profile: &CodexProfileConfigSnapshot,
) -> anyhow::Result<Value> {
    if profile.model_context_settings_v1 {
        return Ok(catalog.clone());
    }
    let mut next = catalog.clone();
    let mut enabled = profile.clone();
    enabled.model_context_settings_v1 = true;
    for row in next
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("catalog models missing")?
    {
        let old = resolve(row, profile);
        let owner = layer(row, "owner")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let mut preserved = layer(row, "preserved")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        if profile.model_context_window.is_some()
            && count(&owner, "context_window").is_some()
            && resolve(row, &enabled).window != old.window
        {
            preserved["context_window"] = serde_json::json!(old.window);
            row["context_settings"]["preserved"] = preserved.clone();
        }
        let (owner_pct, owner_abs) = compact(&owner);
        if profile.model_auto_compact_token_limit.is_some()
            && (owner_pct.is_some() || owner_abs.is_some())
            && resolve(row, &enabled).absolute != old.absolute
        {
            preserved["auto_compact_token_limit"] = serde_json::json!(old.absolute);
            preserved
                .as_object_mut()
                .unwrap()
                .remove("auto_compact_percent");
            row["context_settings"]["preserved"] = preserved;
        }
        let new = resolve(row, &enabled);
        ensure!(
            old.window == new.window && old.absolute == new.absolute,
            "cannot preserve legacy effective limits for model {}",
            row["slug"]
        );
    }
    Ok(next)
}

/// Runtime support and a stable reason for disabled desktop controls.
pub fn support(account: &super::model::StoredAccount) -> Value {
    use super::model::CliKind;
    let reason = match account.cli_kind {
        CliKind::Claude if crate::runtime::claude::selected_model_context_supported() => None,
        CliKind::Claude => Some("claudeModelContextSettingsV1Required"),
        CliKind::Codex if super::model_variants::context_supported(account) => None,
        CliKind::Codex => Some("nativeModelContextSettingsV2Required"),
        CliKind::Dsh => Some("dshContextSettingsUnsupported"),
        CliKind::Kimi => Some("kimiContextSettingsUnsupported"),
    };
    serde_json::json!({"supported":reason.is_none(),"reason":reason})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn golden_vectors_preserve_every_model_and_are_idempotent() {
        for (pw, pa, rw, max, ra, expected_w, expected_a) in [
            (
                Some(200000),
                Some(150000),
                128000,
                256000,
                Some(90000),
                200000,
                150000,
            ),
            (
                Some(500000),
                Some(400000),
                128000,
                256000,
                None,
                256000,
                230400,
            ),
            (
                None,
                Some(500000),
                128000,
                256000,
                Some(90000),
                128000,
                115200,
            ),
            (
                Some(512000),
                None,
                1000000,
                1000000,
                Some(700000),
                512000,
                460800,
            ),
        ] {
            let p = CodexProfileConfigSnapshot {
                model_context_window: pw,
                model_auto_compact_token_limit: pa,
                ..Default::default()
            };
            let rows: Vec<_> = ["base", "alias", "variant"].into_iter().map(|id| json!({
                "slug": id, "context_window": rw, "max_context_window": max, "auto_compact_token_limit": ra
            })).collect();
            let catalog = json!({"models": rows});
            let original = catalog.clone();
            let migrated = migrate_catalog(&catalog, &p).unwrap();
            let mut enabled = p.clone();
            enabled.model_context_settings_v1 = true;
            for (before, after) in catalog["models"]
                .as_array()
                .unwrap()
                .iter()
                .zip(migrated["models"].as_array().unwrap())
            {
                assert_eq!(
                    (resolve(before, &p).window, resolve(before, &p).absolute),
                    (Some(expected_w), Some(expected_a))
                );
                assert_eq!(
                    (
                        resolve(after, &enabled).window,
                        resolve(after, &enabled).absolute
                    ),
                    (Some(expected_w), Some(expected_a))
                );
            }
            assert_eq!(catalog, original);
            assert_eq!(
                migrated, original,
                "built-ins must not acquire migration pins"
            );
            assert_eq!(migrate_catalog(&migrated, &enabled).unwrap(), migrated);
        }
    }
    #[test]
    fn percent_model_priority_and_legacy_seam() {
        let raw =
            json!({"context_window":128000,"max_context_window":256000,"auto_compact_percent":50});
        let row = with_settings(&raw, &json!({"context_window":256000}), &raw, &json!({}));
        let mut p = CodexProfileConfigSnapshot {
            model_context_window: Some(200000),
            model_auto_compact_token_limit: Some(150000),
            ..Default::default()
        };
        assert_eq!(resolve(&row, &p).absolute, Some(150000));
        p.model_context_settings_v1 = true;
        let limits = resolve(&row, &p);
        assert_eq!(
            (limits.window, limits.absolute, limits.compact_source),
            (Some(128000), Some(64000), "model")
        );
        assert_eq!(
            resolve(
                &with_settings(
                    &json!({}),
                    &json!({}),
                    &json!({"context_window":128000,"auto_compact_percent":100}),
                    &json!({})
                ),
                &p
            )
            .absolute,
            Some(115200)
        );
        p.model_auto_compact_token_limit = None;
        p.model_auto_compact_percent = Some(75);
        assert_eq!(
            resolve(
                &with_settings(
                    &json!({}),
                    &json!({}),
                    &json!({"context_window":128000}),
                    &json!({})
                ),
                &p
            )
            .absolute,
            Some(96000)
        );
    }
    #[test]
    fn percent_math_floors_small_windows_and_does_not_overflow_large_windows() {
        let profile = CodexProfileConfigSnapshot {
            model_context_settings_v1: true,
            ..Default::default()
        };
        assert_eq!(
            resolve(
                &json!({"context_window":1,"auto_compact_percent":50}),
                &profile
            )
            .absolute,
            Some(0)
        );
        assert_eq!(
            resolve(
                &json!({"context_window":i64::MAX,"auto_compact_percent":50}),
                &profile
            )
            .absolute,
            Some(i64::MAX / 2)
        );
    }

    #[test]
    fn context_migration_keeps_unset_fields_inheriting_independently() {
        let mut profile = CodexProfileConfigSnapshot {
            model_context_window: Some(200000),
            ..Default::default()
        };
        let catalog = json!({"models":[{"slug":"window-only","context_window":128000,"max_context_window":256000}]});
        let migrated = migrate_catalog(&catalog, &profile).unwrap();
        assert!(migrated["models"][0]
            .get("auto_compact_token_limit")
            .is_none());
        profile.model_context_settings_v1 = true;
        profile.model_auto_compact_percent = Some(75);
        assert_eq!(
            resolve(&migrated["models"][0], &profile).absolute,
            Some(150000)
        );
        let mut profile = CodexProfileConfigSnapshot {
            model_auto_compact_token_limit: Some(150000),
            ..Default::default()
        };
        let catalog = json!({"models":[{"slug":"auto-only","context_window":null,"max_context_window":256000}]});
        let migrated = migrate_catalog(&catalog, &profile).unwrap();
        assert!(migrated["models"][0]["context_window"].is_null());
        profile.model_context_settings_v1 = true;
        profile.model_context_window = Some(128000);
        let limits = resolve(&migrated["models"][0], &profile);
        assert_eq!(
            (limits.window, limits.absolute),
            (Some(128000), Some(115200))
        );
        // A valid existing absolute value remains the configured value, even when capped.
        let profile = CodexProfileConfigSnapshot {
            model_context_window: Some(200000),
            ..Default::default()
        };
        let catalog = json!({"models":[{"slug":"custom","context_window":500000,"auto_compact_token_limit":190000}]});
        assert_eq!(
            migrate_catalog(&catalog, &profile).unwrap()["models"][0]["auto_compact_token_limit"],
            190000
        );
    }
    #[test]
    fn context_uncatalogued_fallback_keeps_capacity_unknown_and_caps_profile_absolute() {
        let mut profile = CodexProfileConfigSnapshot {
            model_context_settings_v1: true,
            ..Default::default()
        };
        let limits = resolve_uncatalogued(&profile);
        assert_eq!((limits.window, limits.absolute), (None, Some(244800)));
        profile.model_auto_compact_token_limit = Some(500000);
        assert_eq!(resolve_uncatalogued(&profile).absolute, Some(244800));
        // A catalogued row with no window has no uncatalogued-runtime marker.
        assert_eq!(
            resolve(&json!({"slug":"catalogued"}), &profile).absolute,
            Some(500000)
        );
        profile.model_auto_compact_token_limit = None;
        profile.model_auto_compact_percent = Some(75);
        assert_eq!(resolve_uncatalogued(&profile).absolute, Some(204000));
        profile.model_auto_compact_percent = Some(100);
        assert_eq!(resolve_uncatalogued(&profile).absolute, Some(244800));
        profile.model_auto_compact_percent = None;
        profile.model_auto_compact_token_limit = Some(500000);
        profile.model_context_window = Some(500000);
        let limits = resolve_uncatalogued(&profile);
        assert_eq!(
            (limits.window, limits.absolute),
            (Some(500000), Some(450000))
        );
    }
    #[test]
    fn context_owner_conflict_preserves_original_and_restore_exposes_it() {
        let flat = json!({"slug":"owner","context_window":128000,"max_context_window":256000,"auto_compact_token_limit":90000});
        let row = with_settings(&flat, &json!({"context_window":256000}), &flat, &json!({}));
        let mut profile = CodexProfileConfigSnapshot {
            model_context_window: Some(200000),
            model_auto_compact_token_limit: Some(150000),
            ..Default::default()
        };
        let catalog = migrate_catalog(&json!({"models":[row]}), &profile).unwrap();
        let mut row = catalog["models"][0].clone();
        profile.model_context_settings_v1 = true;
        let kept = resolve(&row, &profile);
        assert_eq!(
            (
                kept.window,
                kept.absolute,
                kept.window_source,
                kept.compact_source
            ),
            (Some(200000), Some(150000), "preserved", "preserved")
        );
        assert_eq!(row["context_settings"]["owner"]["context_window"], 128000);
        assert_eq!(
            row["context_settings"]["owner"]["auto_compact_token_limit"],
            90000
        );
        super::super::catalog_api::clear_preserved(&mut row, true, true);
        let restored = resolve(&row, &profile);
        assert_eq!(
            (
                restored.window,
                restored.absolute,
                restored.window_source,
                restored.compact_source
            ),
            (Some(128000), Some(90000), "model", "model")
        );
    }
    #[test]
    fn context_defaults_resolve_each_field_without_desktop_arithmetic() {
        let row = with_settings(
            &json!({"context_window":128000,"max_context_window":1000000}),
            &json!({"context_window":1000000,"auto_compact_token_limit":700000}),
            &json!({"context_window":128000,"auto_compact_percent":50}),
            &json!({}),
        );
        let profile = CodexProfileConfigSnapshot {
            model_context_settings_v1: true,
            model_context_window: Some(200000),
            model_auto_compact_token_limit: Some(150000),
            ..Default::default()
        };
        let defaults = default_values(&row, &profile);
        assert_eq!(defaults["model"]["contextWindow"], 200000);
        assert_eq!(defaults["model"]["autoCompactTokenLimit"], 115200);
        assert_eq!(defaults["profile"]["contextWindow"], 1000000);
        assert_eq!(defaults["profile"]["autoCompactTokenLimit"], 115200);
        assert_eq!(
            defaults["profile"]["autoCompactTokenLimitSource"],
            "catalog"
        );
    }
    #[test]
    fn context_new_variant_does_not_inherit_another_rows_owner_or_preserved_layers() {
        let base = with_settings(
            &json!({"slug":"base","context_window":128000}),
            &json!({"context_window":256000}),
            &json!({"context_window":128000}),
            &json!({"context_window":200000}),
        );
        let mut variant = base.clone();
        variant["slug"] = json!("variant");
        edit_settings(&mut variant, Some(&base), &[]);
        let profile = CodexProfileConfigSnapshot {
            model_context_settings_v1: true,
            ..Default::default()
        };
        let effective = resolve(&variant, &profile);
        assert_eq!(
            (effective.window, effective.window_source),
            (Some(256000), "catalog")
        );
        assert_eq!(variant["context_settings"]["owner"], json!({}));
        assert_eq!(variant["context_settings"]["preserved"], json!({}));
    }
    #[test]
    fn context_catalog_layer_retained_on_owner_only_edits_and_clear() {
        use super::super::catalog_api::{apply_model_patch, ModelPatch};
        use crate::config::global_settings::ConfigValueUpdate;
        let mut row = serde_json::json!({"slug":"catalog","context_window":128000,"auto_compact_token_limit":90000});
        let original = row.clone();
        let patch = ModelPatch {
            auto_compact_percent: ConfigValueUpdate::Set(50),
            ..Default::default()
        };
        apply_model_patch(&mut row, &patch).unwrap();
        edit_settings(
            &mut row,
            Some(&original),
            &super::super::catalog_api::pinned_keys(&patch),
        );
        // Native ignores all flat built-ins once context_settings exists.
        let bytes = serde_json::to_vec(&row).unwrap();
        let mut row: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(row["context_settings"]["catalog"].is_object());
        assert_eq!(row["context_settings"]["catalog"]["context_window"], 128000);
        assert_eq!(
            row["context_settings"]["catalog"]["auto_compact_token_limit"],
            90000
        );
        let profile = CodexProfileConfigSnapshot {
            model_context_settings_v1: true,
            ..Default::default()
        };
        assert_eq!(resolve(&row, &profile).absolute, Some(64000));
        let before = row.clone();
        let clear = ModelPatch {
            auto_compact_percent: ConfigValueUpdate::Clear,
            ..Default::default()
        };
        apply_model_patch(&mut row, &clear).unwrap();
        edit_settings(
            &mut row,
            Some(&before),
            &super::super::catalog_api::pinned_keys(&clear),
        );
        let value = resolve(&row, &profile);
        assert_eq!(
            (value.window, value.absolute, value.compact_source),
            (Some(128000), Some(90000), "catalog")
        );
    }
    #[test]
    fn context_partial_external_metadata_never_reinterprets_flat_values() {
        let mut row = json!({"slug":"external","context_window":128000,"max_context_window":256000,"auto_compact_token_limit":90000,"context_settings":{"owner":{"auto_compact_percent":50}}});
        let profile = CodexProfileConfigSnapshot {
            model_context_settings_v1: true,
            ..Default::default()
        };
        let value = resolve(&row, &profile);
        assert_eq!((value.window, value.absolute), (Some(256000), Some(128000)));
        let before = row.clone();
        edit_settings(&mut row, Some(&before), &[]);
        assert_eq!(row["context_settings"]["catalog"], json!({}));
        assert_eq!(resolve(&row, &profile).absolute, Some(128000));
        row["context_settings"]["owner"] = Value::Null;
        row["context_settings"]["preserved"] = Value::Null;
        let before = row.clone();
        edit_settings(&mut row, Some(&before), &["context_window"]);
        assert!(row["context_settings"]["catalog"].is_object());
    }
}
