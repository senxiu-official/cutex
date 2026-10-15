//! Notification levels (cutex-desktop `docs/research/notification-levels.md`,
//! capability `notification-levels`; cutex doc `docs/NOTIFICATION-LEVELS.md`).
//!
//! Two owner tables, both with presets:
//!
//! 1. **event → level**: every notification event (a registered id such as
//!    `ownerNotify.urgent` or `delegation.pendingReview`) has one of four
//!    levels (`urgent`, `needsYou`, `inform`, `record`);
//! 2. **level → methods**: every level has its methods (`needsYou`, `mark`,
//!    `desktop`, `phone`, `sound`, `bigAlert`).
//!
//! The registry (levels, methods, presets, events) is data
//! (`registry.json`). Modules contribute events additively with
//! [`register_event`] or a JSON file in `<cutex config dir>/notification-events/`
//! ([`load_contributed_events`]); unknown event ids in the owner's settings
//! are kept, so a module that registers later finds its configuration.
//!
//! Until the owner picks other presets both tables are `default`. The former
//! `legacy` presets (cutex r216 behaviour, methods derived from
//! `ownerNotify.delivery` / `bigAlertSound`) were removed on 2026-10-15; a
//! home that stored them is moved to `default` once
//! ([`crate::config::notification_levels::run_legacy_migration_on_startup`]).
//! [`Evaluator`] turns a set of matching events into a [`Decision`] (level +
//! methods + owner_notify channels); owner_notify creation and the host
//! summary use it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::notification_levels::NotificationLevelSettings;
use crate::config::owner_notify::OwnerNotifySettings;

/// Capability name (`GET /v2/capabilities`).
pub const CAPABILITY: &str = "notification-levels";
/// `GET|PATCH /v2/notification-levels`.
pub const PATH: &str = "/v2/notification-levels";
/// The preset id of both tables used when none (or an unknown one) is stored.
pub const DEFAULT_PRESET: &str = "default";
/// The removed preset id of both tables (cutex r216 behaviour; stored by
/// older binaries, migrated to `default` once, treated as `default`).
pub const LEGACY: &str = "legacy";
/// Project override key for agents outside any project.
pub const INDEPENDENT_PROJECT: &str = "__independent__";
/// Methods that are owner_notify channels, in channel order.
pub const CHANNEL_METHODS: &[&str] = &["desktop", "phone", "bigAlert"];
/// Methods a mute override removes.
const MUTED_METHODS: &[&str] = &["desktop", "phone", "sound", "bigAlert"];

const BUILTIN: &str = include_str!("registry.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    Record,
    Inform,
    NeedsYou,
    Urgent,
}

impl Level {
    /// Highest first (display order).
    pub const ALL: [Level; 4] = [Level::Urgent, Level::NeedsYou, Level::Inform, Level::Record];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "urgent" => Some(Self::Urgent),
            "needsYou" => Some(Self::NeedsYou),
            "inform" => Some(Self::Inform),
            "record" => Some(Self::Record),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Urgent => "urgent",
            Self::NeedsYou => "needsYou",
            Self::Inform => "inform",
            Self::Record => "record",
        }
    }
}

/// Localised text: `{"en": …, "zh-CN": …}`.
pub type Text = BTreeMap<String, String>;
/// One level's methods: method id → on.
pub type Row = BTreeMap<String, bool>;
/// level id → row.
pub type Matrix = BTreeMap<String, Row>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelSpec {
    pub id: String,
    pub title: Text,
    #[serde(default)]
    pub description: Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodSpec {
    pub id: String,
    pub title: Text,
    #[serde(default)]
    pub description: Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPresetSpec {
    pub id: String,
    pub title: Text,
    #[serde(default)]
    pub description: Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodPresetSpec {
    pub id: String,
    pub title: Text,
    #[serde(default)]
    pub description: Text,
    pub matrix: Matrix,
}

/// One notification event (registry data; modules add their own).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventSpec {
    /// Dotted id, e.g. `ownerNotify.urgent` (`^[a-z][A-Za-z0-9]*(\.[a-zA-Z0-9]+)+$`).
    pub id: String,
    /// Contributing module (`ownerNotify`, `delegation`, … or a plugin id).
    pub module: String,
    pub title: Text,
    #[serde(default)]
    pub description: Text,
    /// Level per event preset id; `default` is required. A `legacy` level
    /// (contributions written for older cutex) is dropped on registration.
    pub levels: BTreeMap<String, Level>,
    /// Who applies the decision today: `cutex` (owner_notify delivery,
    /// summary needs) and / or `client` (desktop / phone using the resolved
    /// tables). Empty = registered only.
    #[serde(default)]
    pub applied_by: Vec<String>,
    /// Capabilities the decision depends on that do not exist yet
    /// (e.g. `turn-triggers`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
}

impl EventSpec {
    pub fn level_in(&self, preset: &str) -> Level {
        self.levels
            .get(preset)
            .or_else(|| self.levels.get("default"))
            .copied()
            .unwrap_or(Level::Record)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub version: u32,
    pub levels: Vec<LevelSpec>,
    pub methods: Vec<MethodSpec>,
    /// level → method → the only allowed value.
    pub fixed_cells: Matrix,
    pub event_presets: Vec<EventPresetSpec>,
    pub method_presets: Vec<MethodPresetSpec>,
    pub events: Vec<EventSpec>,
}

pub fn valid_event_id(id: &str) -> bool {
    let mut parts = id.split('.');
    let first = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.collect();
    id.len() <= 128
        && first.starts_with(|c: char| c.is_ascii_lowercase())
        && first.chars().all(|c| c.is_ascii_alphanumeric())
        && !rest.is_empty()
        && rest.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn valid_module(module: &str) -> bool {
    !module.is_empty()
        && module.len() <= 64
        && module.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl Registry {
    pub fn builtin() -> Self {
        let registry: Registry = serde_json::from_str(BUILTIN).expect("notification_levels/registry.json is valid");
        registry
    }

    pub fn event(&self, id: &str) -> Option<&EventSpec> {
        self.events.iter().find(|event| event.id == id)
    }

    pub fn has_method(&self, id: &str) -> bool {
        self.methods.iter().any(|method| method.id == id)
    }

    pub fn has_event_preset(&self, id: &str) -> bool {
        self.event_presets.iter().any(|preset| preset.id == id)
    }

    pub fn method_preset(&self, id: &str) -> Option<&MethodPresetSpec> {
        self.method_presets.iter().find(|preset| preset.id == id)
    }

    /// Check one contributed event against the registry.
    pub fn check_event(&self, spec: &EventSpec) -> Result<(), String> {
        if !valid_event_id(&spec.id) {
            return Err(format!("event id {:?}: dotted ascii id like module.event", spec.id));
        }
        if self.event(&spec.id).is_some() {
            return Err(format!("event {} is already registered", spec.id));
        }
        if !valid_module(&spec.module) {
            return Err(format!("event {}: invalid module {:?}", spec.id, spec.module));
        }
        if !spec.levels.contains_key("default") {
            return Err(format!("event {}: levels.default is required", spec.id));
        }
        if let Some(preset) = spec.levels.keys().find(|preset| !self.has_event_preset(preset)) {
            return Err(format!("event {}: unknown event preset {preset}", spec.id));
        }
        if spec.title.is_empty() {
            return Err(format!("event {}: title is required", spec.id));
        }
        if let Some(by) = spec.applied_by.iter().find(|by| !matches!(by.as_str(), "cutex" | "client")) {
            return Err(format!("event {}: appliedBy {by} is not cutex or client", spec.id));
        }
        Ok(())
    }

    /// Add an event (additive registration; duplicates are refused).
    pub fn add_event(&mut self, mut spec: EventSpec) -> Result<(), String> {
        spec.levels.remove(LEGACY);
        self.check_event(&spec)?;
        self.events.push(spec);
        Ok(())
    }

    /// The fixed value of a cell, if any.
    pub fn fixed(&self, level: &str, method: &str) -> Option<bool> {
        self.fixed_cells.get(level).and_then(|row| row.get(method)).copied()
    }

    /// A full row with every registered method (missing = off) and the
    /// fixed cells applied.
    fn complete_row(&self, level: &str, row: Option<&Row>) -> Row {
        self.methods
            .iter()
            .map(|method| {
                let value = self
                    .fixed(level, &method.id)
                    .unwrap_or_else(|| row.and_then(|row| row.get(&method.id)).copied().unwrap_or(false));
                (method.id.clone(), value)
            })
            .collect()
    }

    /// The matrix of a method preset. Unknown presets give `None`.
    pub fn preset_matrix(&self, preset: &str) -> Option<Matrix> {
        let base = &self.method_preset(preset)?.matrix;
        Some(
            Level::ALL
                .iter()
                .map(|level| (level.as_str().to_string(), self.complete_row(level.as_str(), base.get(level.as_str()))))
                .collect(),
        )
    }
}

// ---------------------------------------------------------------- global registry

fn global() -> &'static RwLock<Arc<Registry>> {
    static REGISTRY: OnceLock<RwLock<Arc<Registry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = Registry::builtin();
        if !cfg!(test) {
            if let Ok(dir) = crate::config::paths::config_dir() {
                for error in load_contributed_events(&mut registry, &dir.join("notification-events")) {
                    eprintln!("notification levels: {error}");
                }
            }
        }
        RwLock::new(Arc::new(registry))
    })
}

/// The process registry (built-ins + contributed events).
pub fn registry() -> Arc<Registry> {
    global().read().map(|registry| Arc::clone(&registry)).unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
}

/// Register a module's event in this process (additive; a second
/// registration of the same id is refused). Modules that run in both the
/// management and the Agent Bus process register in both.
pub fn register_event(spec: EventSpec) -> Result<(), String> {
    let mut guard = global().write().map_err(|_| "notification registry lock poisoned".to_string())?;
    let mut next = (**guard).clone();
    next.add_event(spec)?;
    *guard = Arc::new(next);
    Ok(())
}

/// Load `*.json` event contributions from `dir` (each file: one event object
/// or `{"events": [...]}`), sorted by file name. Returns the problems
/// (a bad file or event is skipped, never fatal).
pub fn load_contributed_events(registry: &mut Registry, dir: &std::path::Path) -> Vec<String> {
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return errors;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let parsed = std::fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string()));
        let value = match parsed {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let items = match value.get("events") {
            Some(Value::Array(items)) => items.clone(),
            _ => vec![value],
        };
        for item in items {
            match serde_json::from_value::<EventSpec>(item) {
                Ok(spec) => {
                    if let Err(error) = registry.add_event(spec) {
                        errors.push(format!("{}: {error}", path.display()));
                    }
                }
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            }
        }
    }
    errors
}

// ---------------------------------------------------------------- evaluation

/// Where an event comes from (overrides).
#[derive(Clone, Copy, Debug, Default)]
pub struct Scope<'a> {
    /// cutexSessionId the event is about.
    pub agent: Option<&'a str>,
    /// Its project: `Some(None)` = independent agent, `None` = unknown.
    pub project: Option<Option<&'a str>>,
}

/// What to do for one event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub level: Level,
    /// The event that decided the level (the highest of the matching ones).
    pub event: String,
    /// Methods that are on (registry order).
    pub actions: Vec<String>,
    /// owner_notify channels (`desktop` / `phone` / `bigAlert`).
    pub channels: Vec<String>,
    /// An agent or project mute applied.
    pub muted: bool,
}

impl Decision {
    pub fn has(&self, method: &str) -> bool {
        self.actions.iter().any(|action| action == method)
    }

    pub fn needs_you(&self) -> bool {
        self.has("needsYou")
    }
}

/// The resolved tables plus overrides.
#[derive(Clone, Debug)]
pub struct Evaluator {
    registry: Arc<Registry>,
    event_preset: String,
    event_levels: BTreeMap<String, Level>,
    matrix: Matrix,
    method_preset: String,
    owner: OwnerNotifySettings,
    project_overrides: BTreeMap<String, String>,
}

impl Default for Evaluator {
    /// Default tables with default owner_notify settings.
    fn default() -> Self {
        Self::new(registry(), &NotificationLevelSettings::default(), &OwnerNotifySettings::default())
    }
}

impl Evaluator {
    pub fn new(registry: Arc<Registry>, settings: &NotificationLevelSettings, owner: &OwnerNotifySettings) -> Self {
        let resolved = settings.resolve(&registry, owner);
        Self {
            event_preset: resolved.event_preset,
            event_levels: resolved.event_levels,
            matrix: resolved.matrix,
            method_preset: resolved.method_preset,
            owner: owner.clone(),
            project_overrides: resolved.project_overrides,
            registry,
        }
    }

    /// Default tables for these owner_notify settings (agent overrides).
    pub fn with_owner(owner: &OwnerNotifySettings) -> Self {
        Self::new(registry(), &NotificationLevelSettings::default(), owner)
    }

    /// From the current config (defaults when it cannot be read).
    pub fn current() -> Self {
        Self::current_with_owner().1
    }

    /// The current effective owner_notify settings and the evaluator built
    /// from the same config read (defaults when unreadable).
    pub fn current_with_owner() -> (OwnerNotifySettings, Self) {
        match crate::config::store::load_codez_config_checked() {
            Ok(config) => {
                let owner = config.owner_notify.effective();
                let evaluator = Self::new(registry(), &config.notification_levels, &owner);
                (owner, evaluator)
            }
            Err(_) => (OwnerNotifySettings::default(), Self::default()),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn owner_settings(&self) -> &OwnerNotifySettings {
        &self.owner
    }

    pub fn event_preset(&self) -> &str {
        &self.event_preset
    }

    pub fn matrix(&self) -> &Matrix {
        &self.matrix
    }

    pub fn event_levels(&self) -> &BTreeMap<String, Level> {
        &self.event_levels
    }

    /// The level of one event (`record` for unknown ids).
    pub fn level(&self, event: &str) -> Level {
        self.event_levels.get(event).copied().unwrap_or(Level::Record)
    }

    pub fn method_on(&self, level: Level, method: &str) -> bool {
        self.matrix.get(level.as_str()).and_then(|row| row.get(method)).copied().unwrap_or(false)
    }

    fn muted(&self, scope: Scope<'_>) -> bool {
        let agent = scope
            .agent
            .is_some_and(|agent| self.owner.agent_overrides.get(agent).is_some_and(|mode| mode == "mute"));
        let project = match scope.project {
            Some(project) => self
                .project_overrides
                .get(project.unwrap_or(INDEPENDENT_PROJECT))
                .is_some_and(|mode| mode == "mute"),
            None => false,
        };
        agent || project
    }

    /// Decide for the events that match one occurrence (the highest level
    /// wins; on a tie the first listed event).
    pub fn decide(&self, events: &[&str], scope: Scope<'_>) -> Decision {
        let mut chosen: Option<(&str, Level)> = None;
        for event in events {
            let level = self.level(event);
            if chosen.is_none_or(|(_, best)| level > best) {
                chosen = Some((event, level));
            }
        }
        let (event, mut level) = chosen.unwrap_or(("", Level::Record));
        let muted = self.muted(scope);
        if muted && level == Level::Inform {
            level = Level::Record;
        }
        let mut actions: Vec<String> = self
            .registry
            .methods
            .iter()
            .filter(|method| self.method_on(level, &method.id))
            .map(|method| method.id.clone())
            .collect();
        let mut channels: Vec<String> = CHANNEL_METHODS
            .iter()
            .filter(|channel| actions.iter().any(|action| action == *channel))
            .map(|channel| channel.to_string())
            .collect();
        if muted {
            actions.retain(|action| !MUTED_METHODS.contains(&action.as_str()));
            channels.clear();
        }
        Decision { level, event: event.to_string(), actions, channels, muted }
    }

    /// The resolved tables as JSON (`effective` of the route).
    pub fn effective_view(&self) -> Value {
        json!({
            "eventPreset": self.event_preset,
            "methodPreset": self.method_preset,
            "eventLevels": self.event_levels.iter().map(|(id, level)| (id.clone(), json!(level))).collect::<serde_json::Map<_, _>>(),
            "methods": self.matrix,
        })
    }
}

/// The owner-facing view of the registry plus the owner's settings
/// (`GET /v2/notification-levels`).
pub fn view(registry: &Registry, settings: &NotificationLevelSettings, owner: &OwnerNotifySettings) -> Value {
    let evaluator = Evaluator::new(Arc::new(registry.clone()), settings, owner);
    let resolved = settings.resolve(registry, owner);
    let event_presets: Vec<Value> = registry
        .event_presets
        .iter()
        .map(|preset| {
            json!({
                "id": preset.id, "title": preset.title, "description": preset.description,
                "levels": registry.events.iter().map(|event| (event.id.clone(), json!(event.level_in(&preset.id)))).collect::<serde_json::Map<_, _>>(),
            })
        })
        .collect();
    let method_presets: Vec<Value> = registry
        .method_presets
        .iter()
        .map(|preset| {
            json!({
                "id": preset.id, "title": preset.title, "description": preset.description,
                "methods": registry.preset_matrix(&preset.id),
            })
        })
        .collect();
    let events: Vec<Value> = registry
        .events
        .iter()
        .map(|event| {
            let level = evaluator.level(&event.id);
            json!({
                "id": event.id, "module": event.module, "title": event.title, "description": event.description,
                "levels": event.levels, "appliedBy": event.applied_by, "requires": event.requires,
                "level": level,
                "overridden": settings.event_levels.contains_key(&event.id),
            })
        })
        .collect();
    let unknown: BTreeSet<&String> =
        settings.event_levels.keys().filter(|id| registry.event(id).is_none()).collect();
    json!({
        "contractVersion": 1,
        "registryVersion": registry.version,
        "levels": registry.levels,
        "methods": registry.methods,
        "fixedCells": registry.fixed_cells,
        "eventPresets": event_presets,
        "methodPresets": method_presets,
        "events": events,
        "settings": settings.view(),
        "effective": {
            "eventPreset": resolved.event_preset_effective,
            "methodPreset": resolved.method_preset_effective,
            "eventLevels": evaluator.event_levels.iter().map(|(id, level)| (id.clone(), json!(level))).collect::<serde_json::Map<_, _>>(),
            "methods": evaluator.matrix,
            // Always false since the legacy preset was removed (kept for
            // clients that read it).
            "legacyNeedsYouRequiresChannel": false,
        },
        // Stored levels of events no module registered (kept for later).
        "unregisteredEventLevels": unknown.into_iter().map(|id| (id.clone(), json!(settings.event_levels[id]))).collect::<serde_json::Map<_, _>>(),
    })
}

/// Resolved settings (see [`NotificationLevelSettings::resolve`]).
#[derive(Clone, Debug)]
pub struct Resolved {
    pub event_preset: String,
    pub event_preset_effective: String,
    pub event_levels: BTreeMap<String, Level>,
    pub method_preset: String,
    pub method_preset_effective: String,
    pub matrix: Matrix,
    pub project_overrides: BTreeMap<String, String>,
}

/// Map of event levels for a preset.
pub fn preset_event_levels(registry: &Registry, preset: &str) -> BTreeMap<String, Level> {
    registry.events.iter().map(|event| (event.id.clone(), event.level_in(preset))).collect()
}

/// Ids of presets whose table equals `table`; the configured one first.
pub(crate) fn matching_preset<T: PartialEq>(configured: &str, candidates: &[(String, T)], table: &T) -> String {
    if candidates.iter().any(|(id, value)| id == configured && value == table) {
        return configured.to_string();
    }
    candidates
        .iter()
        .find(|(_, value)| value == table)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| "custom".to_string())
}

#[cfg(test)]
mod tests;
