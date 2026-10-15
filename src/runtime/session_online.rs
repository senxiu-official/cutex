//! `session.online` runtime identity, terminal, naming, and log helpers.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use uuid::Uuid;

use crate::agent_bus::identity::{
    cutex_agent_hint, fnv1a_hex, normalize_launch_agent_groups, sanitize_session_component,
};
use crate::agent_bus::service::{agent_bus_base_url, agent_bus_port};
use crate::config::env::{
    CUTEX_AGENT_BUS_TOKEN_ENV_VAR, CUTEX_AGENT_BUS_URL_ENV_VAR, CUTEX_AGENT_GROUPS_ENV_VAR,
    CUTEX_AGENT_HINT_ENV_VAR, CUTEX_AGENT_HOST_ID_ENV_VAR, CUTEX_AGENT_ID_ENV_VAR,
    CUTEX_AGENT_NAME_ENV_VAR,
};
use crate::config::paths::runtime_dir;
use crate::config::store::load_codez_config;
use crate::launch::command::LaunchCommand;
use crate::platform::host::current_host_name;
use crate::profiles::model::StoredAccount;
use crate::session::model::CutexSessionRecord;
use crate::session::service::{cutex_session_display_name, cutex_session_launch_cwd};

pub fn session_online_agent_identity_env(
    launch: LaunchCommand,
    account: &StoredAccount,
    record: &CutexSessionRecord,
    groups: &[String],
) -> LaunchCommand {
    let runtime_agent_id = session_online_agent_id(account, record);
    session_online_agent_identity_env_with_id(launch, record, groups, &runtime_agent_id)
}

pub fn session_online_agent_identity_env_with_id(
    launch: LaunchCommand,
    record: &CutexSessionRecord,
    groups: &[String],
    runtime_agent_id: &str,
) -> LaunchCommand {
    let codex_session_id = record.codex_session_id.as_deref().unwrap_or_default();
    let agent_name = cutex_session_display_name(record);
    let global_config = load_codez_config();
    let mut launch = launch
        .env_remove("CODEX_THREAD_ID")
        .env_remove(CUTEX_AGENT_BUS_URL_ENV_VAR)
        .env_remove(CUTEX_AGENT_BUS_TOKEN_ENV_VAR)
        .env_remove(CUTEX_AGENT_ID_ENV_VAR)
        .env_remove(CUTEX_AGENT_NAME_ENV_VAR)
        .env_remove(CUTEX_AGENT_GROUPS_ENV_VAR)
        .env_remove(CUTEX_AGENT_HOST_ID_ENV_VAR)
        .env_remove(CUTEX_AGENT_HINT_ENV_VAR)
        .env("CODEX_THREAD_ID", codex_session_id)
        .env(
            CUTEX_AGENT_BUS_URL_ENV_VAR,
            agent_bus_base_url(agent_bus_port(&global_config)),
        )
        .env(CUTEX_AGENT_ID_ENV_VAR, runtime_agent_id)
        .env(CUTEX_AGENT_NAME_ENV_VAR, agent_name)
        .env(
            CUTEX_AGENT_GROUPS_ENV_VAR,
            normalize_launch_agent_groups(groups).join(","),
        )
        .env(CUTEX_AGENT_HOST_ID_ENV_VAR, current_host_name())
        .env(CUTEX_AGENT_HINT_ENV_VAR, cutex_agent_hint());
    // A per-agent credential bound to this runtime id and durable session,
    // never the shared root token (docs/agent-tool-security.md).
    if let Some(token) = crate::agent_bus::agent_auth::launch_credential(
        &global_config,
        runtime_agent_id,
        &record.cutex_session_id,
    ) {
        launch = launch.env(CUTEX_AGENT_BUS_TOKEN_ENV_VAR, token);
    }
    launch
}

pub fn session_online_agent_id(account: &StoredAccount, record: &CutexSessionRecord) -> String {
    let launch_cwd = cutex_session_launch_cwd(record);
    let project = sanitize_session_component(
        Path::new(launch_cwd)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project"),
        24,
        "project",
    );
    let name = sanitize_session_component(&cutex_session_display_name(record), 24, "agent");
    let codex_session_id = record.codex_session_id.as_deref().unwrap_or_default();
    let launch_nonce = Uuid::new_v4();
    let hash = fnv1a_hex(format!(
        "{}\0{}\0{}\0{}\0{}",
        account.id, record.cutex_session_id, codex_session_id, launch_cwd, launch_nonce
    ));
    format!("cutex.{name}.{project}.{}", &hash[..10])
}

pub fn session_online_terminal_color_env(launch: LaunchCommand) -> LaunchCommand {
    let launch = launch.env_remove("NO_COLOR");
    let launch = launch.env("COLORTERM", "truecolor").env("CLICOLOR", "1");
    if std::env::var_os("TERM")
        .and_then(|value| value.into_string().ok())
        .map(|value| value.trim().is_empty() || value.trim().eq_ignore_ascii_case("dumb"))
        .unwrap_or(true)
    {
        launch.env("TERM", "xterm-256color")
    } else {
        launch
    }
}

pub fn default_cutex_alden_session_name(record: &CutexSessionRecord) -> String {
    let launch_cwd = cutex_session_launch_cwd(record);
    let profile = sanitize_session_component(
        record.profile.as_deref().unwrap_or("profile"),
        24,
        "profile",
    );
    let project = sanitize_session_component(
        Path::new(launch_cwd)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project"),
        24,
        "project",
    );
    let key = sanitize_session_component(&record.cutex_session_id, 32, "session");
    format!("cutex.{profile}.host.{project}.{key}")
}

pub fn session_online_log_path(record: &CutexSessionRecord) -> anyhow::Result<PathBuf> {
    let log_dir = runtime_dir()?.join("sessions");
    fs::create_dir_all(&log_dir).with_context(|| {
        format!(
            "Failed to create session runtime dir: {}",
            log_dir.display()
        )
    })?;
    let name = sanitize_session_component(&record.cutex_session_id, 64, "session");
    Ok(log_dir.join(format!("{name}.log")))
}

pub fn session_online_log_tail(log_path: &Path) -> String {
    let Ok(data) = fs::read_to_string(log_path) else {
        return "<log unavailable>".to_string();
    };
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return "<empty log>".to_string();
    }
    let mut chars = trimmed.chars().rev().take(2000).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}
