//! Managed cute-alden wrapping and default session-name planning.

use std::path::Path;

use anyhow::Context;

use crate::agent_bus::identity::{fnv1a_hex, sanitize_session_component};
use crate::config::store::load_codez_config;
use crate::launch::command::LaunchCommand;
use crate::profiles::model::{runtime_label, StoredAccount};
use crate::runtime::alden::{
    already_inside_cute_alden_session, cute_alden_program, wrap_launch_with_cute_alden,
};
use crate::session::config::effective_session_config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedLaunchSessionPlan {
    pub launch: LaunchCommand,
    pub session_name: Option<String>,
}

pub fn should_wrap_launch_with_session(account: &StoredAccount, codex_args: &[String]) -> bool {
    if already_inside_cute_alden_session() {
        return false;
    }

    if !codex_args.is_empty() {
        return false;
    }

    let global_config = load_codez_config();
    effective_session_config(account, &global_config).enabled
}

pub fn maybe_wrap_launch_with_session(
    account: &StoredAccount,
    codex_args: &[String],
    launch: LaunchCommand,
) -> anyhow::Result<ManagedLaunchSessionPlan> {
    if !should_wrap_launch_with_session(account, codex_args) {
        return Ok(ManagedLaunchSessionPlan {
            launch,
            session_name: None,
        });
    }

    let session_name = default_managed_session_name(account)?;
    let alden_program = cute_alden_program()?;
    Ok(ManagedLaunchSessionPlan {
        launch: wrap_launch_with_cute_alden(launch, &alden_program, &session_name),
        session_name: Some(session_name),
    })
}

pub fn default_managed_session_name(account: &StoredAccount) -> anyhow::Result<String> {
    let cwd = std::env::current_dir().context("Failed to determine current directory")?;
    Ok(default_managed_session_name_for_cwd(account, &cwd))
}

pub fn default_managed_session_name_for_cwd(account: &StoredAccount, cwd: &Path) -> String {
    let profile = sanitize_session_component(&account.name, 24, "profile");
    let runtime = sanitize_session_component(runtime_label(&account.runtime), 12, "runtime");
    let project = sanitize_session_component(
        cwd.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("root"),
        24,
        "project",
    );
    let hash = fnv1a_hex(format!(
        "{}\0{}\0{}",
        account.id,
        runtime_label(&account.runtime),
        cwd.display()
    ));
    format!("cutex.{profile}.{runtime}.{project}.{}", &hash[..10])
}
