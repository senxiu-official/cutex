//! Claude auto-compaction per profile (`claude-compaction`, desktop design
//! 036): `<config>/profiles/<account id>/claude-runtime.json` =
//! `{"auto_compact_window": 100000..1000000, "auto_compact_percent": 1..100}`.
//! The Claude launch sets `CLAUDE_CODE_AUTO_COMPACT_WINDOW` /
//! `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` from it, so a change applies at the
//! profile's agents' next runtime start. No file = Claude Code decides.
//!
//! Claude Code 2.1.301 compacts at `min(E × percent / 100, E − 13000)` with
//! `E = min(window, model context) − min(max output, 20000)`, so "compact at
//! about N tokens" is `window = N + 33000, percent = 100`.
//!
//! The file keeps `deny_unknown_fields`: binaries before this feature read
//! the same file and refuse unknown keys, so nothing else is ever written
//! into it (per-model values would need their own file).
use std::path::PathBuf;

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

use crate::profiles::model::{CliKind, StoredAccount};

pub const WINDOW_RANGE: std::ops::RangeInclusive<u32> = 100_000..=1_000_000;
pub const PERCENT_RANGE: std::ops::RangeInclusive<u8> = 1..=100;
pub const FILE_NAME: &str = "claude-runtime.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCompaction {
    pub auto_compact_window: u32,
    pub auto_compact_percent: u8,
}

impl ClaudeCompaction {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(WINDOW_RANGE.contains(&self.auto_compact_window), "Claude auto_compact_window must be 100000..1000000 tokens");
        ensure!(PERCENT_RANGE.contains(&self.auto_compact_percent), "Claude auto_compact_percent must be 1..100");
        Ok(())
    }

    /// The launch environment of the Claude adapter (inherited by Claude Code).
    pub fn envs(&self) -> [(&'static str, String); 2] {
        [
            ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", self.auto_compact_window.to_string()),
            ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", self.auto_compact_percent.to_string()),
        ]
    }

    /// API shape: `{window, percent}`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"window": self.auto_compact_window, "percent": self.auto_compact_percent})
    }
}

pub fn path_for_id(account_id: &str) -> anyhow::Result<PathBuf> {
    Ok(crate::profiles::materialize::materialized_profiles_dir()?.join(account_id).join(FILE_NAME))
}

/// The profile's file; `None` when there is none. A file that does not parse
/// or validate is an error (the launch refuses it the same way).
pub fn read_id(account_id: &str) -> anyhow::Result<Option<ClaudeCompaction>> {
    let path = path_for_id(account_id)?;
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Read Claude profile runtime {}", path.display())),
    };
    let settings: ClaudeCompaction =
        serde_json::from_slice(&raw).with_context(|| format!("Invalid Claude profile runtime {}", path.display()))?;
    settings.validate().with_context(|| format!("Invalid Claude profile runtime {}", path.display()))?;
    Ok(Some(settings))
}

/// API view of a profile: `(value, error)`; Codex profiles `(None, None)`.
pub fn view(account: &StoredAccount) -> (Option<ClaudeCompaction>, Option<String>) {
    if account.cli_kind != CliKind::Claude {
        return (None, None);
    }
    match read_id(&account.id) {
        Ok(value) => (value, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    }
}

/// Writes (`Some`) or removes (`None`) the file. Returns whether it changed.
pub fn write_id(account_id: &str, value: Option<ClaudeCompaction>) -> anyhow::Result<bool> {
    let path = path_for_id(account_id)?;
    match value {
        Some(value) => {
            value.validate()?;
            if read_id(account_id).ok().flatten() == Some(value) {
                return Ok(false);
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("Create {}", dir.display()))?;
            }
            crate::config::atomic::write_private_pretty_json_atomic(&path, &value, "Claude profile runtime")?;
            Ok(true)
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("Remove {}", path.display())),
        },
    }
}

/// Per-model values (`claude-compaction-models`, desktop design 037):
/// `<config>/profiles/<account id>/claude-compaction-models.json` =
/// `{"<model id>": {"auto_compact_window": …, "auto_compact_percent": …}}`.
/// A separate file, so binaries that only know `claude-runtime.json` keep
/// reading theirs. The launch passes the table to the adapter in
/// `CUTEX_CLAUDE_COMPACTION_MODELS` (`{"<model id>": {"window", "percent"}}`);
/// adapters before claude-r24 ignore it. The adapter applies a model's entry
/// when its Claude Code starts and on every model switch (live on Claude Code
/// 2.1.295+, else by restarting the idle child); models without an entry use
/// `claude-runtime.json`, else Claude Code decides.
pub const MODELS_FILE_NAME: &str = "claude-compaction-models.json";
pub const MODELS_ENV: &str = "CUTEX_CLAUDE_COMPACTION_MODELS";
pub const MAX_MODELS: usize = 32;
pub type ModelTable = std::collections::BTreeMap<String, ClaudeCompaction>;

/// A model ID the table accepts: 1..=256 characters, no whitespace or control.
pub fn valid_model_id(model: &str) -> bool {
    !model.is_empty() && model.chars().count() <= 256 && !model.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn validate_models(table: &ModelTable) -> anyhow::Result<()> {
    ensure!(table.len() <= MAX_MODELS, "at most {MAX_MODELS} models");
    for (model, value) in table {
        ensure!(valid_model_id(model), "invalid model ID {model:?}");
        value.validate().with_context(|| format!("model {model}"))?;
    }
    Ok(())
}

pub fn models_path_for_id(account_id: &str) -> anyhow::Result<PathBuf> {
    Ok(crate::profiles::materialize::materialized_profiles_dir()?.join(account_id).join(MODELS_FILE_NAME))
}

/// The profile's table; `None` when there is no file.
pub fn read_models_id(account_id: &str) -> anyhow::Result<Option<ModelTable>> {
    let path = models_path_for_id(account_id)?;
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Read Claude per-model compaction {}", path.display())),
    };
    let table: ModelTable =
        serde_json::from_slice(&raw).with_context(|| format!("Invalid Claude per-model compaction {}", path.display()))?;
    validate_models(&table).with_context(|| format!("Invalid Claude per-model compaction {}", path.display()))?;
    Ok(Some(table))
}

/// API view: `(table, error)`; Codex profiles `(None, None)`.
pub fn models_view(account: &StoredAccount) -> (Option<ModelTable>, Option<String>) {
    if account.cli_kind != CliKind::Claude {
        return (None, None);
    }
    match read_models_id(&account.id) {
        Ok(value) => (value, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    }
}

/// Writes (`Some`, non-empty) or removes (`None` / empty) the file. Returns whether it changed.
pub fn write_models_id(account_id: &str, value: Option<ModelTable>) -> anyhow::Result<bool> {
    let path = models_path_for_id(account_id)?;
    match value.filter(|table| !table.is_empty()) {
        Some(table) => {
            validate_models(&table)?;
            if read_models_id(account_id).ok().flatten().as_ref() == Some(&table) {
                return Ok(false);
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("Create {}", dir.display()))?;
            }
            crate::config::atomic::write_private_pretty_json_atomic(&path, &table, "Claude per-model compaction")?;
            Ok(true)
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("Remove {}", path.display())),
        },
    }
}

/// API and adapter shape: `{"<model id>": {window, percent}}`.
pub fn models_json(table: &ModelTable) -> serde_json::Value {
    serde_json::Value::Object(table.iter().map(|(model, value)| (model.clone(), value.to_json())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_table_file_api_and_adapter_shapes() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let id = "claude-test-models";
        assert_eq!(read_models_id(id).unwrap(), None);
        let table: ModelTable = [
            ("claude-sonnet-5-5".to_string(), ClaudeCompaction { auto_compact_window: 233_000, auto_compact_percent: 100 }),
            ("claude-opus-5-5".to_string(), ClaudeCompaction { auto_compact_window: 533_000, auto_compact_percent: 100 }),
        ].into();
        assert!(write_models_id(id, Some(table.clone())).unwrap());
        assert!(!write_models_id(id, Some(table.clone())).unwrap(), "same table: no change");
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(models_path_for_id(id).unwrap()).unwrap()).unwrap();
        assert_eq!(raw["claude-opus-5-5"], serde_json::json!({"auto_compact_window":533000,"auto_compact_percent":100}));
        assert_eq!(read_models_id(id).unwrap(), Some(table.clone()));
        assert_eq!(models_json(&table)["claude-sonnet-5-5"], serde_json::json!({"window":233000,"percent":100}));
        assert!(path_for_id(id).unwrap() != models_path_for_id(id).unwrap(), "claude-runtime.json is untouched");
        let bad: ModelTable = [("has space".to_string(), ClaudeCompaction { auto_compact_window: 233_000, auto_compact_percent: 100 })].into();
        assert!(write_models_id(id, Some(bad)).is_err());
        assert!(write_models_id(id, Some(ModelTable::new())).unwrap(), "empty removes the file");
        assert_eq!(read_models_id(id).unwrap(), None);
    }

    #[test]
    fn profile_patch_writes_the_model_table_next_to_claude_runtime_json() {
        use crate::config::global_settings::ConfigValueUpdate;
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        use crate::profiles::settings_api::{update_profile, ProfileApiPatch};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let new = |name: &str, kind| create_profile(&NewProfile { name: name.into(), kind, preset: None, base_url: None, model: None, api_key: None }).unwrap();
        let claude = new("Claude", NewProfileKind::Claude);
        let table: ModelTable = [("claude-sonnet-5-5".to_string(), ClaudeCompaction { auto_compact_window: 233_000, auto_compact_percent: 100 })].into();
        let set = |value: ModelTable| ProfileApiPatch { claude_compaction_models: ConfigValueUpdate::Set(value), ..Default::default() };
        assert!(update_profile(&claude.id, &set(table.clone())).unwrap().1);
        assert_eq!(models_view(&claude), (Some(table.clone()), None));
        assert!(!path_for_id(&claude.id).unwrap().exists(), "claude-runtime.json is not written");
        assert!(!update_profile(&claude.id, &set(table)).unwrap().1, "same table: no change");
        let clear = ProfileApiPatch { claude_compaction_models: ConfigValueUpdate::Clear, ..Default::default() };
        assert!(update_profile(&claude.id, &clear).unwrap().1);
        assert_eq!(models_view(&claude), (None, None));
        std::fs::write(models_path_for_id(&claude.id).unwrap(), br#"{"m":{"auto_compact_window":5,"auto_compact_percent":100}}"#).unwrap();
        assert!(models_view(&claude).1.is_some(), "an invalid file is reported");
        let codex = new("Codex", NewProfileKind::Chatgpt);
        assert!(update_profile(&codex.id, &set(ModelTable::new())).is_err());
    }

    #[test]
    fn file_shape_is_the_one_older_binaries_read() {
        let value = ClaudeCompaction { auto_compact_window: 533_000, auto_compact_percent: 100 };
        let raw = serde_json::to_value(value).unwrap();
        assert_eq!(raw, serde_json::json!({"auto_compact_window":533000,"auto_compact_percent":100}));
        assert!(serde_json::from_str::<ClaudeCompaction>(r#"{"auto_compact_window":500000,"auto_compact_percent":90,"models":{}}"#).is_err());
        assert_eq!(value.to_json(), serde_json::json!({"window":533000,"percent":100}));
    }

    #[test]
    fn ranges() {
        let ok = |w, p| ClaudeCompaction { auto_compact_window: w, auto_compact_percent: p }.validate().is_ok();
        assert!(ok(100_000, 1) && ok(1_000_000, 100));
        assert!(!ok(99_999, 50) && !ok(1_000_001, 50) && !ok(500_000, 0) && !ok(500_000, 101));
    }

    #[test]
    fn profile_patch_writes_reads_and_removes_the_file() {
        use crate::config::global_settings::ConfigValueUpdate;
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        use crate::profiles::settings_api::{update_profile, ProfileApiPatch};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let new = |name: &str, kind| create_profile(&NewProfile { name: name.into(), kind, preset: None, base_url: None, model: None, api_key: None }).unwrap();
        let claude = new("Claude", NewProfileKind::Claude);
        let path = path_for_id(&claude.id).unwrap();
        assert_eq!(view(&claude), (None, None));
        let set = |value| ProfileApiPatch { claude_compaction: ConfigValueUpdate::Set(value), ..Default::default() };
        let value = ClaudeCompaction { auto_compact_window: 533_000, auto_compact_percent: 100 };
        assert!(update_profile(&claude.id, &set(value)).unwrap().1);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
            serde_json::json!({"auto_compact_window":533000,"auto_compact_percent":100}));
        assert_eq!(view(&claude).0, Some(value));
        assert!(!update_profile(&claude.id, &set(value)).unwrap().1, "same value: no change");
        assert!(update_profile(&claude.id, &set(ClaudeCompaction { auto_compact_window: 50_000, auto_compact_percent: 100 })).is_err());
        // A file written by hand (the 10-22 rollout) shows up as is.
        std::fs::write(&path, br#"{"auto_compact_window":500000,"auto_compact_percent":90}"#).unwrap();
        assert_eq!(view(&claude).0, Some(ClaudeCompaction { auto_compact_window: 500_000, auto_compact_percent: 90 }));
        // An invalid file is reported, and a write replaces it.
        std::fs::write(&path, br#"{"auto_compact_window":5}"#).unwrap();
        assert!(view(&claude).1.is_some());
        assert!(update_profile(&claude.id, &set(value)).unwrap().1);
        assert_eq!(view(&claude), (Some(value), None));
        let clear = ProfileApiPatch { claude_compaction: ConfigValueUpdate::Clear, ..Default::default() };
        assert!(update_profile(&claude.id, &clear).unwrap().1);
        assert!(!path.exists());
        assert!(!update_profile(&claude.id, &clear).unwrap().1);
        let codex = new("ChatGPT", NewProfileKind::Chatgpt);
        assert!(update_profile(&codex.id, &set(value)).is_err());
        assert!(!path_for_id(&codex.id).unwrap().exists());
    }
}
