//! CLI binary selection for supported agent CLIs.

use std::path::Path;

use crate::config::env::{env_var_first, CUTEX_CLAUDE_BIN_ENV_VAR, CUTEX_CODEX_BIN_ENV_VAR};
use crate::platform::command::command_exists_in_path;
use crate::profiles::model::CliKind;

pub fn codex_program() -> String {
    env_var_first(&[CUTEX_CODEX_BIN_ENV_VAR])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            if command_exists_in_path("cute-codex") {
                "cute-codex".to_string()
            } else if command_exists_in_path("cutex-codex") {
                "cutex-codex".to_string()
            } else {
                "codex".to_string()
            }
        })
}

pub fn claude_program() -> String {
    env_var_first(&[CUTEX_CLAUDE_BIN_ENV_VAR])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "claude".to_string())
}

pub fn cli_program(kind: &CliKind) -> String {
    match kind {
        CliKind::Codex => codex_program(),
        CliKind::Claude => claude_program(),
    }
}

pub fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program)
}
