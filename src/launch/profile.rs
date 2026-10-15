//! Profile-aware launch command construction.

use crate::launch::command::LaunchCommand;
use crate::launch::env::{
    apply_profile_launch_envs, ApplyProfileLaunchEnvOptions, LaunchEnvContext,
};
use crate::launch::native_features::instant_interrupt_off_args;
use crate::launch::program::cli_program;
use crate::launch::selected_profile::{api_key_shell_env_blank_args, API_KEY_ENV};
use crate::profiles::model::{CliKind, MaterializedAccountFiles, RuntimeConfig, StoredAccount};

pub fn profile_launch_command(
    account: &StoredAccount,
    codex_args: &[String],
    files: &MaterializedAccountFiles,
    install_dir: Option<String>,
    agent_mode: bool,
    agent_groups: &[String],
    context: &LaunchEnvContext<'_>,
) -> anyhow::Result<LaunchCommand> {
    match &account.runtime {
        RuntimeConfig::Host => Ok(host_profile_launch_command(
            account,
            codex_args,
            files,
            install_dir,
            agent_mode,
            agent_groups,
            context,
        )),
        RuntimeConfig::Docker { .. } => anyhow::bail!("Cutex Docker integration was retired; select a Host profile"),
    }
}

pub fn host_profile_launch_command(
    account: &StoredAccount,
    codex_args: &[String],
    files: &MaterializedAccountFiles,
    install_dir: Option<String>,
    agent_mode: bool,
    agent_groups: &[String],
    context: &LaunchEnvContext<'_>,
) -> LaunchCommand {
    let auth_path = files.auth_path.to_string_lossy();
    let config_path = files.config_path.to_string_lossy();
    let custom_status_items_path = files.custom_status_items_path.to_string_lossy();

    // D2: native instant interrupts stay off for managed agent runtimes (the
    // ones cutex steers, queues and delivers ExternalInput to). A root `-c`
    // placed first, so an explicit per-profile / per-session
    // `-c features.instant_interrupt=...` later on the command line still
    // wins (evaluation only). Foreground owner CLI runs are passed through.
    let mut base = LaunchCommand::new(cli_program(&account.cli_kind));
    if agent_mode && account.cli_kind == CliKind::Codex {
        base = base.args(instant_interrupt_off_args());
    }
    let launch = apply_profile_launch_envs(
        base.args(codex_args.iter().cloned()),
        ApplyProfileLaunchEnvOptions {
            account,
            auth_path: auth_path.as_ref(),
            config_path: config_path.as_ref(),
            custom_status_items_path: custom_status_items_path.as_ref(),
            install_dir,
            api_key_auth_path: Some(&files.auth_path),
            agent_mode,
            agent_groups,
            context,
        },
    );
    with_api_key_kept_from_tools(launch)
}

/// A launch that carries an API-key profile's provider key in its
/// environment blanks it for native tool subprocesses and shell snapshots
/// (`selected_profile::API_KEY_SHELL_ENV_BLANK_OVERRIDE`). The override goes
/// first: a root `-c` applies to every native subcommand and cannot be taken
/// for a trailing prompt or session id.
fn with_api_key_kept_from_tools(mut launch: LaunchCommand) -> LaunchCommand {
    if launch.envs.iter().any(|(key, _)| key == API_KEY_ENV) {
        launch.args.splice(0..0, api_key_shell_env_blank_args());
    }
    launch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_key_in_launch_env_is_blanked_for_native_tools_before_subcommand() {
        let launch = LaunchCommand::new("cute-codex")
            .args(["resume", "019e-session"])
            .env(API_KEY_ENV, "sk-fixture-not-a-key");
        let launch = with_api_key_kept_from_tools(launch);
        assert_eq!(
            launch.args,
            ["-c", "shell_environment_policy.set.OPENAI_API_KEY=\"\"", "resume", "019e-session"]
        );
        assert!(!launch.args.iter().any(|arg| arg.contains("sk-fixture")));

        let plain = LaunchCommand::new("cute-codex").arg("resume");
        assert_eq!(with_api_key_kept_from_tools(plain.clone()), plain);
    }
}
