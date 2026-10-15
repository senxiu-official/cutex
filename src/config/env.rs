//! Environment variable names used by cutex and launched CLIs.

pub const CODEX_CONFIG_FILE_ENV_VAR: &str = "CODEX_CONFIG_FILE";
pub const CODEX_AUTH_FILE_ENV_VAR: &str = "CODEX_AUTH_FILE";
pub const CUTEX_CODEX_BIN_ENV_VAR: &str = "CUTEX_CODEX_BIN";
pub const CUTEX_CLAUDE_BIN_ENV_VAR: &str = "CUTEX_CLAUDE_BIN";
pub const CLAUDE_CONFIG_DIR_ENV_VAR: &str = "CLAUDE_CONFIG_DIR";
pub const CUTEX_ALDEN_BIN_ENV_VAR: &str = "CUTEX_ALDEN_BIN";
pub const CUTEX_DOCKER_USE_SUDO_ENV_VAR: &str = "CUTEX_DOCKER_USE_SUDO";
pub const CODEZ_DOCKER_USE_SUDO_ENV_VAR: &str = "CODEZ_DOCKER_USE_SUDO";
pub const CUTEX_CLAUDE_USAGE_REFRESH_SECONDS_ENV_VAR: &str =
    "CUTEX_CLAUDE_USAGE_REFRESH_SECONDS";
pub const CODEX_CUSTOM_STATUS_ITEMS_FILE_ENV_VAR: &str = "CODEX_CUSTOM_STATUS_ITEMS_FILE";
pub const CODEX_INSTALL_DIR_ENV_VAR: &str = "CODEX_INSTALL_DIR";
pub const CUTE_CODEX_FORCE_HTTP_TRANSPORT_ENV_VAR: &str = "CUTE_CODEX_FORCE_HTTP_TRANSPORT";
pub const CODEX_NOTIFY_IDLE_TIMEOUT_ENV_VAR: &str = "CODEX_NOTIFY_IDLE_TIMEOUT";
pub const CODEX_NOTIFY_COMPOSER_IDLE_TIMEOUT_ENV_VAR: &str = "CODEX_NOTIFY_COMPOSER_IDLE_TIMEOUT";
pub const CODEX_NOTIFY_APPROVAL_TIMEOUT_ENV_VAR: &str = "CODEX_NOTIFY_APPROVAL_TIMEOUT";
pub const CODEX_NOTIFY_STARTUP_IDLE_TIMEOUT_ENV_VAR: &str = "CODEX_NOTIFY_STARTUP_IDLE_TIMEOUT";
pub const CODEX_NOTIFY_EVENTS_ENV_VAR: &str = "CODEX_NOTIFY_EVENTS";
pub const CODEX_NOTIFY_USER_MESSAGE_CONTENT_ENV_VAR: &str = "CODEX_NOTIFY_USER_MESSAGE_CONTENT";
pub const CODEX_NOTIFY_USER_MESSAGE_PREVIEW_CHARS_ENV_VAR: &str =
    "CODEX_NOTIFY_USER_MESSAGE_PREVIEW_CHARS";
pub const CODEX_RATE_LIMIT_THRESHOLD_WARNING_MODE_ENV_VAR: &str =
    "CODEX_RATE_LIMIT_THRESHOLD_WARNING_MODE";
pub const CODEX_RATE_LIMIT_MODEL_NUDGE_MODE_ENV_VAR: &str = "CODEX_RATE_LIMIT_MODEL_NUDGE_MODE";
pub const CUTEX_AGENT_BUS_URL_ENV_VAR: &str = "CUTEX_AGENT_BUS_URL";
pub const CUTEX_AGENT_BUS_TOKEN_ENV_VAR: &str = "CUTEX_AGENT_BUS_TOKEN";
pub const CUTEX_AGENT_ID_ENV_VAR: &str = "CUTEX_AGENT_ID";
pub const CUTEX_AGENT_NAME_ENV_VAR: &str = "CUTEX_AGENT_NAME";
pub const CUTEX_AGENT_GROUPS_ENV_VAR: &str = "CUTEX_AGENT_GROUPS";
pub const CUTEX_AGENT_HOST_ID_ENV_VAR: &str = "CUTEX_AGENT_HOST_ID";
pub const CUTEX_AGENT_HINT_ENV_VAR: &str = "CUTEX_AGENT_HINT";
pub const CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR: &str = "CUTEX_HEADLESS_AGENT_RUNTIME";
pub const CUTEX_WINDOWS_DESKTOP_LAUNCHER_ENV_VAR: &str = "CUTEX_WINDOWS_DESKTOP_LAUNCHER";
pub const CUTEX_RUNTIME_HEARTBEAT_URL_ENV_VAR: &str = "CUTEX_RUNTIME_HEARTBEAT_URL";
pub const CUTEX_RUNTIME_HEARTBEAT_TOKEN_ENV_VAR: &str = "CUTEX_RUNTIME_HEARTBEAT_TOKEN";
pub const CUTEX_RUNTIME_LAUNCH_ID_ENV_VAR: &str = "CUTEX_RUNTIME_LAUNCH_ID";

pub const CODEX_LAUNCH_PROFILE_ENV_VAR: &str = "CODEX_LAUNCH_PROFILE";
pub const CODEX_LAUNCH_RUNTIME_ENV_VAR: &str = "CODEX_LAUNCH_RUNTIME";
pub const CODEX_LAUNCH_PROFILE_SOURCE_ENV_VAR: &str = "CODEX_LAUNCH_PROFILE_SOURCE";
pub const CODEX_LAUNCH_PROFILE_TYPE_ENV_VAR: &str = "CODEX_LAUNCH_PROFILE_TYPE";
pub const CODEX_LAUNCH_PROFILE_EMAIL_ENV_VAR: &str = "CODEX_LAUNCH_PROFILE_EMAIL";

pub fn env_var_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| std::env::var(name).ok())
}

pub fn env_bool_override(name: &str) -> Option<bool> {
    parse_bool_env(std::env::var(name).ok().as_deref())
}

pub fn env_bool_override_any(names: &[&str]) -> Option<bool> {
    names.iter().find_map(|name| env_bool_override(name))
}

pub fn parse_bool_env(value: Option<&str>) -> Option<bool> {
    match value {
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES") | Some("on")
        | Some("ON") => Some(true),
        Some("0") | Some("false") | Some("FALSE") | Some("no") | Some("NO") | Some("off")
        | Some("OFF") => Some(false),
        _ => None,
    }
}
