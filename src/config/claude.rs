//! Claude runtime host choices persisted in `config.json`.

use serde::{Deserialize, Serialize};

/// Which Claude Code CLI new Claude sessions use when no explicit
/// `CUTEX_CLAUDE_CLI` override is present.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeCliChoice {
    #[default]
    Bundled,
    System,
}

impl ClaudeCliChoice {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::System => "system",
        }
    }
}
