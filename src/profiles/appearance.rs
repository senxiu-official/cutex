//! Profile display/default settings and optional quota presentation configuration.
use super::model::{CliKind, StoredAccount};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Chatgpt,
    Deepseek,
    Glm,
    Claude,
    Other,
}
impl Provider {
    pub const ALL: [Self; 5] = [
        Self::Chatgpt,
        Self::Deepseek,
        Self::Glm,
        Self::Claude,
        Self::Other,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Chatgpt => "ChatGPT",
            Self::Deepseek => "DeepSeek",
            Self::Glm => "GLM",
            Self::Claude => "Claude Code",
            Self::Other => "Other",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::Deepseek => "deepseek",
            Self::Glm => "glm",
            Self::Claude => "claude",
            Self::Other => "other",
        }
    }
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.key() == s)
            .ok_or_else(|| anyhow::anyhow!("Unknown provider display category"))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextStyle {
    pub fg: Option<String>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underlined: bool,
    /// Fields a newer cutex wrote (kept on save; persistence is not
    /// `deny_unknown_fields`, RFC backend-auto-update §6.2).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
impl Default for TextStyle {
    fn default() -> Self {
        Self {
            fg: None,
            bold: true,
            dim: false,
            italic: false,
            underlined: false,
            extra: Default::default(),
        }
    }
}
impl TextStyle {
    pub fn color(hex: &str) -> Self {
        Self {
            fg: Some(hex.into()),
            ..Self::default()
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(fg) = &self.fg {
            anyhow::ensure!(
                fg.len() == 7
                    && fg.starts_with('#')
                    && fg[1..].bytes().all(|b| b.is_ascii_hexdigit()),
                "Color must be #RRGGBB or inherit"
            );
        }
        Ok(())
    }
    pub fn status_style(&self) -> super::model::CustomStatusItemStyle {
        super::model::CustomStatusItemStyle {
            fg: self.fg.clone(),
            bg: None,
            bold: self.bold,
            dim: self.dim,
            italic: self.italic,
            underlined: self.underlined,
        }
    }
}
/// Official SDK effort values. None delegates to the selected Claude runtime.
/// What a Claude agent's AskUserQuestion does in a turn that was not started
/// by the owner (Agent Bus message, service or other external input).
/// Owner-started turns always wait for an answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTurnQuestions {
    /// Do not block: the model is told no human is available and decides
    /// itself (or asks the requesting agent in its reply).
    #[default]
    Decide,
    /// Block until someone answers in a client, as for owner turns.
    Wait,
}
impl AgentTurnQuestions {
    pub const ALL: [Self; 2] = [Self::Decide, Self::Wait];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decide => "decide",
            Self::Wait => "wait",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == value)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeEffort { Low, Medium, High, Xhigh, Max }
impl ClaudeEffort {
    pub fn as_str(self) -> &'static str {
        match self { Self::Low => "low", Self::Medium => "medium", Self::High => "high", Self::Xhigh => "xhigh", Self::Max => "max" }
    }
    pub fn parse(value: &str) -> anyhow::Result<Option<Self>> {
        Ok(match value { "" | "default" => None, "low" => Some(Self::Low), "medium" => Some(Self::Medium), "high" => Some(Self::High), "xhigh" => Some(Self::Xhigh), "max" => Some(Self::Max), _ => anyhow::bail!("Claude effort must be default/low/medium/high/xhigh/max") })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeDefaults {
    pub model: String,
    pub effort: Option<ClaudeEffort>,
    /// File-editing tools (Write/Edit/...) of new sessions; None = the
    /// default [`CLAUDE_TOOLS_DEFAULT`] (on). Read it with [`Self::write`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_write: Option<bool>,
    /// Bash of new sessions; None = the default (on). See [`Self::exec`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_exec: Option<bool>,
    /// Fields a newer cutex wrote (kept on save; persistence is not
    /// `deny_unknown_fields`, RFC backend-auto-update §6.2).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
/// Default of a Claude profile's write / exec rules (owner decision
/// 2026-10-11: on). The one-time migration `claude-tool-defaults` cleared
/// the values stored before (`cli_app::claude_tool_defaults`).
pub const CLAUDE_TOOLS_DEFAULT: bool = true;
impl ClaudeDefaults {
    /// Effective write rule of new sessions.
    pub fn write(&self) -> bool {
        self.allow_write.unwrap_or(CLAUDE_TOOLS_DEFAULT)
    }
    /// Effective exec rule of new sessions.
    pub fn exec(&self) -> bool {
        self.allow_exec.unwrap_or(CLAUDE_TOOLS_DEFAULT)
    }
}
impl Default for ClaudeDefaults {
    fn default() -> Self {
        Self {
            model: "sonnet".into(),
            effort: None,
            allow_write: None,
            allow_exec: None,
            extra: Default::default(),
        }
    }
}
/// `default` | `true` | `false` (the TUI profile settings values).
fn tool_rule(value: &str) -> anyhow::Result<Option<bool>> {
    Ok(match value {
        "" | "default" => None,
        value => Some(value.parse()?),
    })
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileOptions {
    pub quota: super::quota::Config,
    /// Extra quota gauges shown after `quota` (e.g. a weekly window).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quotas: Vec<super::quota::Config>,
    pub provider: Option<Provider>,
    pub style: Option<TextStyle>,
    pub claude: ClaudeDefaults,
    /// Explicit opt-in/out for mid-conversation profile switching; absent
    /// derives from the profile's auth kind (see `allow_session_switch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_session_switch: Option<bool>,
    /// Native cute-codex subagents (`features.multi_agent`) for runtimes on
    /// this profile; absent keeps the native default (enabled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_subagents: Option<bool>,
    /// Agents on this profile get the `delegation_wait` tool; absent = on
    /// for Codex, off for Claude (naming doc §Waiting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegation_wait: Option<bool>,
    /// Agents may launch agents on this profile only with the owner's
    /// approval (Agent Management create / replace).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub agent_use_requires_approval: bool,
    /// Claude profiles: AskUserQuestion in turns not started by the owner;
    /// absent = [`AgentTurnQuestions::default`] (decide).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_turn_questions: Option<AgentTurnQuestions>,
    /// Claude file snapshots retained by the official runtime; absent = off.
    /// Read at the next runtime launch, independently of file editing tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_file_checkpointing: Option<bool>,
    /// Fields a newer cutex wrote (kept on save; persistence is not
    /// `deny_unknown_fields`, RFC backend-auto-update §6.2).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub providers: BTreeMap<Provider, TextStyle>,
    pub host_suffix: TextStyle,
    pub model_display_names: BTreeMap<String, String>,
    /// Fields a newer cutex wrote (kept on save; persistence is not
    /// `deny_unknown_fields`, RFC backend-auto-update §6.2).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
impl Default for Appearance {
    fn default() -> Self {
        Self {
            providers: BTreeMap::new(),
            model_display_names: [("gpt-6-astra".into(), "astra 6".into()), ("gpt-6-sol".into(), "sol 6".into()), ("gpt-5.6-sol".into(), "sol 5.6".into()), ("gpt-5.6-terra".into(), "terra 5.6".into()), ("gpt-6-luna".into(), "lune 6".into()), ("gpt-5.6-luna".into(), "lune 5.6".into()), ("sonnet".into(), "Sonnet".into()), ("opus".into(), "Opus".into()), ("haiku".into(), "Haiku".into()), ("opus[1m]".into(), "Opus[1m]".into()), ("claude-fable-5-1[1m]".into(), "fable5.1[1m]".into()), ("claude-fable-5-1".into(), "fable5.1".into())].into(),
            host_suffix: TextStyle {
                bold: false,
                ..TextStyle::color("#74BAC3")
            },
            extra: Default::default(),
        }
    }
}
impl Appearance {
    pub fn provider_style(&self, p: Provider) -> TextStyle {
        self.providers.get(&p).cloned().unwrap_or_else(|| match p {
            Provider::Chatgpt => TextStyle::color("#F7B3CD"),
            Provider::Deepseek => TextStyle::color("#7180C9"),
            Provider::Glm => TextStyle::color("#97E0F0"),
            Provider::Claude => TextStyle::color("#EE7B32"),
            Provider::Other => TextStyle::default(),
        })
    }
    pub fn profile_style(&self, options: &ProfileOptions, p: Provider) -> TextStyle {
        options
            .style
            .clone()
            .unwrap_or_else(|| self.provider_style(p))
    }
}
/// Backend facts outrank display guesses; custom API classifications are explicit.
pub fn provider(account: &StoredAccount) -> Provider {
    if account.cli_kind == CliKind::Claude {
        return Provider::Claude;
    }
    if let Some(p) = account.options.provider {
        return p;
    }
    if account.source.as_deref() == Some("official") {
        return Provider::Chatgpt;
    }
    let config = super::materialize::materialized_account_files(account)
        .ok()
        .and_then(|files| {
            super::profile_config::read_profile_specific_config_table(&files.config_path).ok()
        });
    match config
        .as_ref()
        .and_then(|t| t.get("model_provider"))
        .and_then(toml::Value::as_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("glm" | "zai" | "z.ai") => Provider::Glm,
        Some("deepseek") => Provider::Deepseek,
        _ => Provider::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityField {
    Provider,
    Color,
    Bold,
    Dim,
    Italic,
    Underline,
    ClaudeModel,
    ClaudeEffort,
    ClaudeWrite,
    ClaudeExec,
}
impl IdentityField {
    pub const STYLES: [Self; 6] = [
        Self::Color,
        Self::Bold,
        Self::Dim,
        Self::Italic,
        Self::Underline,
        Self::Provider,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Provider => "Provider category",
            Self::Color => "Color",
            Self::Bold => "Bold",
            Self::Dim => "Dim",
            Self::Italic => "Italic",
            Self::Underline => "Underline",
            Self::ClaudeModel => "Default model",
            Self::ClaudeEffort => "Reasoning effort",
            Self::ClaudeWrite => "Allow file writes",
            Self::ClaudeExec => "Allow commands",
        }
    }
    pub fn choices(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Self::Provider => std::iter::once(("Automatic", "inherit"))
                .chain(Provider::ALL.into_iter().map(|p| (p.label(), p.key())))
                .collect(),
            Self::Color | Self::ClaudeModel => vec![],
            Self::ClaudeEffort => vec![("Default", "default"),("Low","low"),("Medium","medium"),("High","high"),("XHigh","xhigh"),("Max","max")],
            Self::ClaudeWrite | Self::ClaudeExec => vec![("Default (on)", "default"), ("Off", "false"), ("On", "true")],
            _ => vec![("Off", "false"), ("On", "true")],
        }
    }
    pub fn value(self, o: &ProfileOptions) -> String {
        match self {
            Self::Provider => o
                .provider
                .map(|p| p.key().into())
                .unwrap_or("inherit".into()),
            Self::Color => o
                .style
                .as_ref()
                .and_then(|s| s.fg.clone())
                .unwrap_or("inherit".into()),
            Self::ClaudeModel => o.claude.model.clone(),
            Self::ClaudeEffort => o.claude.effort.map(|v|v.as_str()).unwrap_or("default").into(),
            Self::ClaudeWrite => o.claude.allow_write.map_or("default".into(), |v| v.to_string()),
            Self::ClaudeExec => o.claude.allow_exec.map_or("default".into(), |v| v.to_string()),
            _ => o
                .style
                .as_ref()
                .map(|s| {
                    match self {
                        Self::Bold => s.bold,
                        Self::Dim => s.dim,
                        Self::Italic => s.italic,
                        Self::Underline => s.underlined,
                        _ => false,
                    }
                    .to_string()
                })
                .unwrap_or("inherit".into()),
        }
    }
    pub fn apply(
        self,
        o: &mut ProfileOptions,
        value: &str,
        base: &TextStyle,
    ) -> anyhow::Result<()> {
        match self {
            Self::Provider => {
                o.provider = if value == "inherit" || value.is_empty() {
                    None
                } else {
                    Some(Provider::parse(value)?)
                }
            }
            Self::ClaudeModel => {
                anyhow::ensure!(
                    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
                    "Valid model required"
                );
                o.claude.model = value.into();
            }
            Self::ClaudeEffort => o.claude.effort = ClaudeEffort::parse(value)?,
            Self::ClaudeWrite => o.claude.allow_write = tool_rule(value)?,
            Self::ClaudeExec => o.claude.allow_exec = tool_rule(value)?,
            _ => {
                if value == "inherit" || value.is_empty() {
                    o.style = None;
                    return Ok(());
                }
                let s = o.style.get_or_insert_with(|| base.clone());
                match self {
                    Self::Color => s.fg = Some(value.into()),
                    Self::Bold => s.bold = value.parse()?,
                    Self::Dim => s.dim = value.parse()?,
                    Self::Italic => s.italic = value.parse()?,
                    Self::Underline => s.underlined = value.parse()?,
                    _ => unreachable!(),
                }
                s.validate()?;
            }
        }
        Ok(())
    }
}

pub fn catalog() -> BTreeMap<String, (Provider, Option<TextStyle>)> {
    let store = crate::config::paths::accounts_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice::<super::model::AccountsStore>(&bytes).ok());
    store
        .map(|s| {
            s.accounts
                .into_iter()
                .map(|a| {
                    let p = provider(&a);
                    (a.name, (p, a.options.style))
                })
                .collect()
        })
        .unwrap_or_default()
}
pub fn selected_profile_style(name: &str) -> anyhow::Result<Option<TextStyle>> {
    let catalog = catalog();
    let Some((provider, style)) = catalog.get(name) else {
        return Ok(None);
    };
    let config = crate::config::store::load_codez_config_checked()?;
    Ok(Some(style.clone().unwrap_or_else(|| {
        config.appearance.provider_style(*provider)
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_override_and_reset_preserve_provider_and_claude_defaults() {
        let mut options = ProfileOptions {
            provider: Some(Provider::Glm),
            ..Default::default()
        };
        let mut appearance = Appearance::default();
        appearance
            .providers
            .insert(Provider::Glm, TextStyle::color("#123456"));
        let base = appearance.provider_style(Provider::Glm);
        IdentityField::Color
            .apply(&mut options, "#ABCDEF", &base)
            .unwrap();
        IdentityField::Bold
            .apply(&mut options, "false", &base)
            .unwrap();
        assert_eq!(
            appearance
                .profile_style(&options, Provider::Glm)
                .fg
                .as_deref(),
            Some("#ABCDEF")
        );
        assert!(!appearance.profile_style(&options, Provider::Glm).bold);
        IdentityField::Color
            .apply(&mut options, "inherit", &base)
            .unwrap();
        assert_eq!(appearance.profile_style(&options, Provider::Glm), base);
        assert_eq!(options.provider, Some(Provider::Glm));
        assert_eq!(options.claude, ClaudeDefaults::default());
        let reloaded: ProfileOptions =
            serde_json::from_slice(&serde_json::to_vec(&options).unwrap()).unwrap();
        assert_eq!(reloaded, options);
    }
    #[test]
    fn appearance_rejects_terminal_control_and_non_rgb_color() {
        for value in ["red", "#000", "#GG0000", "#FFFFFF\u{1b}[0m", "#💖💖"] {
            assert!(TextStyle::color(value).validate().is_err());
        }
        assert!(TextStyle::color("#e08eb2").validate().is_ok());
    }
}
