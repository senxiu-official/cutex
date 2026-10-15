use cutex::compat::codex::codex_compat_install_dir_for_host_launch;
use cutex::config::store::load_codez_config;
use cutex::launch::command::LaunchCommand;
use cutex::launch::env::default_launch_env_context;
use cutex::launch::profile::profile_launch_command;
use cutex::launch::program::codex_program;
use cutex::platform::host::current_host_name;
use cutex::profiles::materialize::ensure_materialized_account_files;
use cutex::profiles::model::{MaterializedAccountFiles, StoredAccount};

const RESET: &str = "\x1b[0m";
const YELLOW: &str = "\x1b[33m";

#[cfg(test)]
pub(crate) fn codex_launch_command(
    account: &StoredAccount,
    codex_args: &[String],
) -> anyhow::Result<LaunchCommand> {
    codex_launch_command_with_agent_mode(account, codex_args, false, &[])
}

pub(crate) fn codex_launch_command_with_agent_mode(
    account: &StoredAccount,
    codex_args: &[String],
    agent_mode: bool,
    agent_groups: &[String],
) -> anyhow::Result<LaunchCommand> {
    let files = ensure_materialized_account_files(account)?;
    codex_launch_command_with_prevalidated_profile(
        account,
        codex_args,
        agent_mode,
        agent_groups,
        &files,
    )
}

pub(crate) fn codex_launch_command_with_prevalidated_profile(
    account: &StoredAccount,
    codex_args: &[String],
    agent_mode: bool,
    agent_groups: &[String],
    files: &MaterializedAccountFiles,
) -> anyhow::Result<LaunchCommand> {
    let global_config = load_codez_config();
    let launch_context = default_launch_env_context(&global_config, current_host_name());
    profile_launch_command(
        account,
        codex_args,
        files,
        codex_install_dir_for_host_launch(account),
        agent_mode,
        agent_groups,
        &launch_context,
    )
}

fn codex_install_dir_for_host_launch(account: &StoredAccount) -> Option<String> {
    let program = codex_program();
    match codex_compat_install_dir_for_host_launch(&account.runtime, &program) {
        Ok(Some(path)) => Some(path.to_string_lossy().to_string()),
        Ok(None) => None,
        Err(err) => {
            eprintln!(
                "{YELLOW}warning:{RESET} failed to prepare CODEX_INSTALL_DIR for app-server compatibility: {err:#}"
            );
            None
        }
    }
}
