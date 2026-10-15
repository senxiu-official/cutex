//! Notification level settings (`config.json` `notification_levels`;
//! `GET|PATCH /v2/notification-levels`, also `/v2/settings`
//! `notificationLevels`). Both tables are a preset plus the owner's changed
//! cells; absent (or the removed `legacy`) = `default`.
//!
//! The `legacy` presets (cutex r216 behaviour) were removed on 2026-10-15:
//! the one-time migration `notification-presets-no-legacy`
//! ([`run_legacy_migration_on_startup`]) stores `default` explicitly for a
//! table that was absent or `legacy`, so an older binary after a rollback
//! reads `default` too. Presets are always stored explicitly from then on.
//!
//! Older cutex (r216 and before) ignores the key and drops it on its next
//! config save.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::owner_notify::OwnerNotifySettings;
use crate::notification_levels::{
    matching_preset, preset_event_levels, Level, Matrix, Registry, Resolved, DEFAULT_PRESET, LEGACY,
};

pub const MAX_EVENT_LEVELS: usize = 1000;
pub const MAX_PROJECT_OVERRIDES: usize = 500;
pub const PROJECT_OVERRIDES: &[&str] = &["mute"];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NotificationLevelSettings {
    /// Table 1 preset (`default` | a contributed one); None = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_preset: Option<String>,
    /// eventId → level, on top of the preset. Ids no module registers are
    /// kept (a module may register them later).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub event_levels: BTreeMap<String, String>,
    /// Table 2 preset (`quiet` | `default` | `all`); None = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method_preset: Option<String>,
    /// level → method → on, on top of the preset (fixed cells never stored).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub methods: Matrix,
    /// projectId | `__independent__` → `mute`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub project_overrides: BTreeMap<String, String>,
}

impl NotificationLevelSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn event_preset_id<'a>(&'a self, registry: &Registry) -> &'a str {
        match self.event_preset.as_deref() {
            Some(preset) if registry.has_event_preset(preset) => preset,
            _ => DEFAULT_PRESET,
        }
    }

    fn method_preset_id<'a>(&'a self, registry: &Registry) -> &'a str {
        match self.method_preset.as_deref() {
            Some(preset) if registry.method_preset(preset).is_some() => preset,
            _ => DEFAULT_PRESET,
        }
    }

    /// Effective tables; invalid hand edits fall back cell by cell.
    pub fn resolve(&self, registry: &Registry, _owner: &OwnerNotifySettings) -> Resolved {
        let event_preset = self.event_preset_id(registry).to_string();
        let mut event_levels = preset_event_levels(registry, &event_preset);
        for (id, level) in &self.event_levels {
            if let (Some(slot), Some(level)) = (event_levels.get_mut(id), Level::parse(level)) {
                *slot = level;
            }
        }
        let event_candidates: Vec<(String, std::collections::BTreeMap<String, Level>)> = registry
            .event_presets
            .iter()
            .map(|preset| (preset.id.clone(), preset_event_levels(registry, &preset.id)))
            .collect();
        let event_preset_effective = matching_preset(&event_preset, &event_candidates, &event_levels);

        let method_preset = self.method_preset_id(registry).to_string();
        let mut matrix = registry.preset_matrix(&method_preset).unwrap_or_default();
        for (level, row) in &self.methods {
            let Some(target) = matrix.get_mut(level) else { continue };
            for (method, value) in row {
                if registry.has_method(method) && registry.fixed(level, method).is_none() {
                    target.insert(method.clone(), *value);
                }
            }
        }
        let method_candidates: Vec<(String, Matrix)> = registry
            .method_presets
            .iter()
            .filter_map(|preset| registry.preset_matrix(&preset.id).map(|matrix| (preset.id.clone(), matrix)))
            .collect();
        let method_preset_effective = matching_preset(&method_preset, &method_candidates, &matrix);
        Resolved {
            event_preset,
            event_preset_effective,
            event_levels,
            method_preset,
            method_preset_effective,
            matrix,
            project_overrides: self
                .project_overrides
                .iter()
                .filter(|(_, mode)| PROJECT_OVERRIDES.contains(&mode.as_str()))
                .map(|(project, mode)| (project.clone(), mode.clone()))
                .collect(),
        }
    }

    /// Stored settings as the API shows them.
    pub fn view(&self) -> Value {
        json!({
            "eventPreset": self.event_preset.as_deref().filter(|preset| *preset != LEGACY).unwrap_or(DEFAULT_PRESET),
            "eventLevels": self.event_levels,
            "methodPreset": self.method_preset.as_deref().filter(|preset| *preset != LEGACY).unwrap_or(DEFAULT_PRESET),
            "methods": self.methods,
            "projectOverrides": self.project_overrides,
        })
    }

    /// Apply a partial update (`PATCH` body, see docs/NOTIFICATION-LEVELS.md):
    ///
    /// ```json
    /// {"events": {"preset": "default", "levels": {"ownerNotify.info": "inform", "x.y": null}},
    ///  "methods": {"preset": "quiet", "cells": {"inform": {"desktop": true, "phone": null}}},
    ///  "projectOverrides": {"proj-a": "mute", "proj-b": null}}
    /// ```
    ///
    /// A preset clears that table's changed cells (applied before the cells
    /// of the same request); a cell equal to the preset is not stored; `null`
    /// removes one; fixed cells are ignored. `projectOverrides` merges per
    /// key (`null` deletes).
    pub fn patched(&self, value: &Value, registry: &Registry, _owner: &OwnerNotifySettings) -> Result<Self, String> {
        let object = value.as_object().ok_or("notificationLevels: must be an object")?;
        if let Some(key) = object.keys().find(|key| !["events", "methods", "projectOverrides"].contains(&key.as_str())) {
            return Err(format!("notificationLevels: unsupported field {key}"));
        }
        let mut next = self.clone();
        if let Some(events) = object.get("events") {
            let events = object_of(events, "events")?;
            if let Some(key) = events.keys().find(|key| !["preset", "levels"].contains(&key.as_str())) {
                return Err(format!("notificationLevels.events: unsupported field {key}"));
            }
            if let Some(preset) = events.get("preset") {
                let preset = preset.as_str().ok_or("notificationLevels.events.preset: must be a string")?;
                if !registry.has_event_preset(preset) {
                    return Err(format!("notificationLevels.events.preset: unknown preset {preset}"));
                }
                next.event_preset = Some(preset.to_string());
                next.event_levels.clear();
            }
            if let Some(levels) = events.get("levels") {
                let preset = next.event_preset_id(registry).to_string();
                for (id, value) in object_of(levels, "events.levels")? {
                    let Some(event) = registry.event(id) else {
                        return Err(format!("notificationLevels.events.levels: unknown event {id}"));
                    };
                    match value {
                        Value::Null => {
                            next.event_levels.remove(id);
                        }
                        Value::String(level) => {
                            let level = Level::parse(level).ok_or_else(|| {
                                format!("notificationLevels.events.levels.{id}: must be urgent, needsYou, inform, record or null")
                            })?;
                            if level == event.level_in(&preset) {
                                next.event_levels.remove(id);
                            } else {
                                next.event_levels.insert(id.clone(), level.as_str().to_string());
                            }
                        }
                        _ => return Err(format!("notificationLevels.events.levels.{id}: must be a level or null")),
                    }
                }
            }
            if next.event_levels.len() > MAX_EVENT_LEVELS {
                return Err(format!("notificationLevels.events.levels: at most {MAX_EVENT_LEVELS} entries"));
            }
        }
        if let Some(methods) = object.get("methods") {
            let methods = object_of(methods, "methods")?;
            if let Some(key) = methods.keys().find(|key| !["preset", "cells"].contains(&key.as_str())) {
                return Err(format!("notificationLevels.methods: unsupported field {key}"));
            }
            if let Some(preset) = methods.get("preset") {
                let preset = preset.as_str().ok_or("notificationLevels.methods.preset: must be a string")?;
                if registry.method_preset(preset).is_none() {
                    return Err(format!("notificationLevels.methods.preset: unknown preset {preset}"));
                }
                next.method_preset = Some(preset.to_string());
                next.methods.clear();
            }
            if let Some(cells) = methods.get("cells") {
                let base = registry.preset_matrix(next.method_preset_id(registry)).unwrap_or_default();
                for (level, row) in object_of(cells, "methods.cells")? {
                    if Level::parse(level).is_none() {
                        return Err(format!("notificationLevels.methods.cells: unknown level {level}"));
                    }
                    for (method, value) in object_of(row, &format!("methods.cells.{level}"))? {
                        if !registry.has_method(method) {
                            return Err(format!("notificationLevels.methods.cells.{level}: unknown method {method}"));
                        }
                        let stored = next.methods.entry(level.clone()).or_default();
                        match value {
                            Value::Null => {
                                stored.remove(method);
                            }
                            Value::Bool(on) => {
                                let preset_value = base.get(level).and_then(|row| row.get(method)).copied();
                                if registry.fixed(level, method).is_some() || preset_value == Some(*on) {
                                    stored.remove(method);
                                } else {
                                    stored.insert(method.clone(), *on);
                                }
                            }
                            _ => {
                                return Err(format!(
                                    "notificationLevels.methods.cells.{level}.{method}: must be a boolean or null"
                                ))
                            }
                        }
                    }
                }
                next.methods.retain(|_, row| !row.is_empty());
            }
        }
        if let Some(overrides) = object.get("projectOverrides") {
            for (project, value) in object_of(overrides, "projectOverrides")? {
                if project.is_empty() || project.len() > 200 || project.chars().any(char::is_control) {
                    return Err("notificationLevels.projectOverrides: keys must be project ids or __independent__".into());
                }
                match value {
                    Value::Null => {
                        next.project_overrides.remove(project);
                    }
                    Value::String(mode) if PROJECT_OVERRIDES.contains(&mode.as_str()) => {
                        next.project_overrides.insert(project.clone(), mode.clone());
                    }
                    _ => return Err(format!("notificationLevels.projectOverrides.{project}: must be mute or null")),
                }
            }
            if next.project_overrides.len() > MAX_PROJECT_OVERRIDES {
                return Err(format!("notificationLevels.projectOverrides: at most {MAX_PROJECT_OVERRIDES} entries"));
            }
        }
        Ok(next)
    }
}

fn object_of<'a>(value: &'a Value, field: &str) -> Result<&'a Map<String, Value>, String> {
    value.as_object().ok_or_else(|| format!("notificationLevels.{field}: must be an object"))
}

/// The current settings (defaults when the config is unreadable).
pub fn current() -> NotificationLevelSettings {
    crate::config::store::load_codez_config_checked()
        .map(|config| config.notification_levels)
        .unwrap_or_default()
}

// ------------------------------------------------- migration: no legacy preset

/// Schema of `archive/notification-presets-no-legacy/migration.json`.
pub const LEGACY_MIGRATION_SCHEMA: &str = "cutex/notification-presets-no-legacy-migration/v1";
pub const LEGACY_MIGRATION_ARCHIVE: &str = "archive/notification-presets-no-legacy";
const LEGACY_MIGRATION_RECORD: &str = "migration.json";

/// `cutex management serve` calls this at start. Never fails or panics.
pub fn run_legacy_migration_on_startup(component: &str) {
    let result = std::panic::catch_unwind(|| {
        let cutex_dir = crate::config::paths::config_dir()?;
        run_legacy_migration_at(&cutex_dir, chrono::Utc::now())
    });
    match result {
        Ok(Ok(Some(written))) if !written.is_empty() => eprintln!(
            "cutex notification-presets-no-legacy: {} -> default",
            written.join(", ")
        ),
        Ok(Ok(_)) => {}
        Ok(Err(error)) => eprintln!(
            "cutex notification-presets-no-legacy migration: {component}: not run on this start: {error:#}"
        ),
        Err(_) => eprintln!(
            "cutex notification-presets-no-legacy migration: {component}: panicked, not run on this start"
        ),
    }
}

/// One-time: in `config.json` `notification_levels`, every table whose
/// preset is absent or `legacy` gets `default` stored explicitly. The
/// owner's changed cells are kept (they now apply on top of `default`). The
/// record names the previous block and the owner_notify delivery / sound
/// settings the legacy methods were derived from. Returns `None` when the
/// record existed, else the tables written (`eventPreset` / `methodPreset`).
/// A home without `config.json` is only recorded (absent = default).
pub fn run_legacy_migration_at(
    cutex_dir: &std::path::Path,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Option<Vec<String>>> {
    use anyhow::Context;
    let archive = cutex_dir.join(LEGACY_MIGRATION_ARCHIVE);
    let record_path = archive.join(LEGACY_MIGRATION_RECORD);
    if record_path.exists() {
        return Ok(None);
    }
    let config_path = cutex_dir.join("config.json");
    let mut previous = Value::Null;
    let mut owner_notify = Value::Null;
    let mut written = Vec::new();
    match std::fs::read(&config_path) {
        Ok(raw) => {
            let mut config: Value = serde_json::from_slice(&raw)
                .with_context(|| format!("parse {}", config_path.display()))?;
            let root = config.as_object_mut().context("config.json is not an object")?;
            owner_notify = root.get("owner_notify").cloned().unwrap_or(Value::Null);
            let levels = root.entry("notification_levels").or_insert_with(|| json!({}));
            if levels.is_null() {
                *levels = json!({});
            }
            previous = levels.clone();
            let levels = levels.as_object_mut().context("notification_levels is not an object")?;
            for key in ["eventPreset", "methodPreset"] {
                let current = levels.get(key).and_then(Value::as_str);
                if current.is_none() || current == Some(LEGACY) {
                    levels.insert(key.to_string(), json!(DEFAULT_PRESET));
                    written.push(key.to_string());
                }
            }
            if !written.is_empty() {
                crate::config::atomic::write_private_pretty_json_atomic(&config_path, &config, "config file")?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", config_path.display())),
    }
    crate::management::client_tokens::ensure_private_dir(&cutex_dir.join("archive"))?;
    crate::management::client_tokens::ensure_private_dir(&archive)?;
    let record = json!({
        "schema": LEGACY_MIGRATION_SCHEMA,
        "at": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "previousNotificationLevels": previous,
        "previousOwnerNotify": owner_notify,
        "written": written,
    });
    crate::config::atomic::write_private_pretty_json_atomic(&record_path, &record, "notification-presets-no-legacy record")?;
    Ok(Some(written))
}
