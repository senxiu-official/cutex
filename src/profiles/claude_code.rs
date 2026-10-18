//! Claude Code's own settings sources, external MCP and claude.ai connectors
//! of a Claude profile (`claude-code-sources`).
//!
//! The Claude runtime starts Claude Code with every settings source off
//! unless the profile turns one on:
//!
//! - `project`: the agent's workspace — CLAUDE.md, `.claude/settings.json`
//!   (permissions, hooks), `.claude/{rules,agents,commands,skills}`.
//! - `user`: the Claude account home the session runs on (its
//!   `CLAUDE_CONFIG_DIR`), never the OS user's `~/.claude`.
//!
//! `claude_ai_connectors` lets a subscription login load its claude.ai
//! connectors (Claude Code then runs without `--strict-mcp-config`).
//!
//! Absent (`setting_sources: None`) keeps the behaviour profiles had before
//! this setting: no sources. Claude profiles created from now on start with
//! `project` (owner decision, claude-agent-gaps Q1). Read at the next runtime
//! launch. Stored top level on the profile (`claude_code`), not in `options`
//! (`deny_unknown_fields` in older binaries).

use serde::Deserialize;
use serde::Serialize;

use super::model::CliKind;
use super::model::StoredAccount;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    Project,
    User,
}

impl SettingSource {
    pub const ALL: [SettingSource; 2] = [SettingSource::Project, SettingSource::User];

    pub fn as_str(self) -> &'static str {
        match self {
            SettingSource::Project => "project",
            SettingSource::User => "user",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == value)
    }
}

/// Sources of a profile without an explicit choice (profiles that predate
/// the setting): none, as before.
pub const DEFAULT_SETTING_SOURCES: &[SettingSource] = &[];
/// Sources a newly created Claude profile is stored with.
pub const NEW_PROFILE_SETTING_SOURCES: &[SettingSource] = &[SettingSource::Project];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCodeOptions {
    /// Source names; names this cutex does not know (a newer writer's) are
    /// kept on save and ignored at launch, so the store always loads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting_sources: Option<Vec<String>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub claude_ai_connectors: bool,
    /// Fields a newer cutex wrote (kept on save).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ClaudeCodeOptions {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// What a profile created now is stored with.
    pub fn for_new_profile(cli_kind: &CliKind) -> Self {
        match cli_kind {
            CliKind::Claude => Self { setting_sources: Some(store_names(NEW_PROFILE_SETTING_SOURCES)), ..Self::default() },
            _ => Self::default(),
        }
    }
}

/// Normalised (sorted, distinct) source list.
pub fn normalize(mut sources: Vec<SettingSource>) -> Vec<SettingSource> {
    sources.sort();
    sources.dedup();
    sources
}

/// The stored form of a source list.
pub fn store_names(sources: &[SettingSource]) -> Vec<String> {
    normalize(sources.to_vec()).into_iter().map(|source| source.as_str().to_string()).collect()
}

/// Known sources of a stored list.
pub fn parse_stored(names: &[String]) -> Vec<SettingSource> {
    normalize(names.iter().filter_map(|name| SettingSource::parse(name)).collect())
}

/// Effective settings sources of a Claude profile; `None` for Codex.
pub fn setting_sources(account: &StoredAccount) -> Option<Vec<SettingSource>> {
    (account.cli_kind == CliKind::Claude).then(|| match &account.claude_code.setting_sources {
        Some(names) => parse_stored(names),
        None => DEFAULT_SETTING_SOURCES.to_vec(),
    })
}

/// Whether a Claude profile loads claude.ai connectors; `None` for Codex.
pub fn claude_ai_connectors(account: &StoredAccount) -> Option<bool> {
    (account.cli_kind == CliKind::Claude).then_some(account.claude_code.claude_ai_connectors)
}

pub fn names(sources: &[SettingSource]) -> Vec<&'static str> {
    sources.iter().map(|source| source.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_shape_and_defaults() {
        assert!(ClaudeCodeOptions::default().is_empty());
        assert_eq!(serde_json::to_value(ClaudeCodeOptions::default()).unwrap(), serde_json::json!({}));
        let new = ClaudeCodeOptions::for_new_profile(&CliKind::Claude);
        assert_eq!(serde_json::to_value(&new).unwrap(), serde_json::json!({"setting_sources": ["project"]}));
        assert!(ClaudeCodeOptions::for_new_profile(&CliKind::Codex).is_empty());
        let newer: ClaudeCodeOptions = serde_json::from_value(serde_json::json!({
            "setting_sources": ["user", "project", "user"], "claude_ai_connectors": true, "future": 1
        }))
        .unwrap();
        assert_eq!(parse_stored(newer.setting_sources.as_deref().unwrap()), vec![SettingSource::Project, SettingSource::User]);
        let unknown: ClaudeCodeOptions = serde_json::from_value(serde_json::json!({"setting_sources": ["local", "project"]})).unwrap();
        assert_eq!(parse_stored(unknown.setting_sources.as_deref().unwrap()), vec![SettingSource::Project]);
        assert_eq!(serde_json::to_value(&newer).unwrap()["future"], 1);
        assert_eq!(SettingSource::parse("user"), Some(SettingSource::User));
        assert_eq!(SettingSource::parse("local"), None);
    }
}
