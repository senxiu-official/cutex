use anyhow::Context;

use cutex::launch::args::{
    codex_args_for_runtime, combined_profile_cli_args, should_add_docker_sandbox_bypass,
};
use cutex::launch::program::cli_program;
use cutex::profiles::model::{CliKind, RuntimeConfig, StoredAccount};

use super::agent_bus_config;
use super::agent_bus_runtime;
use super::launch_command;
use super::launch_output::LaunchOutput;
use super::launch_presenter;
use super::launch_session;
use super::prompt::cli_args_label;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

pub(crate) fn run_codex_process(
    account: &StoredAccount,
    codex_args: Vec<String>,
    output: LaunchOutput,
    agent_mode: bool,
    agent_groups: Vec<String>,
) -> anyhow::Result<()> {
    if account.cli_kind == CliKind::Claude {return super::claude_profile::run(account,&codex_args,agent_mode);}
    let program = cli_program(&account.cli_kind);
    let base_codex_args = combined_profile_cli_args(account, codex_args);
    let add_docker_sandbox_bypass = should_add_docker_sandbox_bypass(account, &base_codex_args);
    let effective_codex_args = codex_args_for_runtime(account, base_codex_args);
    let output = output.including_child_args(&effective_codex_args);
    output.line(format_args!("CLI binary: {BOLD}{}{RESET}", program));
    launch_presenter::print_launch_summary(account, agent_mode, &agent_groups, output);
    if !account.default_cli_args.is_empty() {
        output.line(format_args!(
            "Default CLI args: {}",
            cli_args_label(&account.default_cli_args)
        ));
    }
    if !effective_codex_args.is_empty() {
        output.line(format_args!(
            "CLI args: {}",
            cli_args_label(&effective_codex_args)
        ));
    }
    if add_docker_sandbox_bypass {
        output.line(format_args!(
            "docker detected: adding {} --sandbox danger-full-access to avoid bubblewrap/userns failures",
            program
        ));
    }
    if !agent_mode && effective_codex_args.first().is_some_and(|arg| arg == "resume") {
        if let Some(native) = effective_codex_args.get(1).filter(|arg| !arg.starts_with('-')) {
            let store = cutex::session::store::load_cutex_session_store()?;
            if let Some(record) = store.sessions.values().find(|r| r.is_owned_session()
                && r.codex_session_id.as_deref() == Some(native)) {
                anyhow::ensure!(effective_codex_args.len() == 2,
                    "This Cutex session has a dedicated runtime; resume without extra native overrides or use Recent Sessions");
                super::stock_lifecycle::online(&record.cutex_session_id, false)?;
                return super::stock_lifecycle::attach(&record.cutex_session_id);
            }
        }
    }
    if !agent_mode && effective_codex_args.is_empty()
        && account.cli_kind == CliKind::Codex
        && matches!(account.runtime, cutex::profiles::model::RuntimeConfig::Host)
        && super::light_standalone::selected_entry_available()?
    {
        let id=super::light_new::create_owned_session(&account.name,&std::env::current_dir()?)?;
        super::stock_lifecycle::online(&id,false)?;
        return super::stock_lifecycle::attach(&id);
    }
    if !agent_mode {
        if let Some(mut command) = super::light_standalone::command(account, &effective_codex_args)? {
            let status = command.status().context("Failed to start ordinary light session")?;
            std::process::exit(exit_code_from_status(status));
        }
    }
    launch_session::warn_if_resume_target_is_already_running(&effective_codex_args, output)?;
    ensure_management_api_for_launch(account)?;
    if agent_mode {
        ensure_agent_bus_for_launch(account)?;
    }
    let launch = launch_session::maybe_wrap_launch_with_session(
        account,
        &effective_codex_args,
        output,
        launch_command::codex_launch_command_with_agent_mode(
            account,
            &effective_codex_args,
            agent_mode,
            &agent_groups,
        )?,
    )?;
    let exit_code = exit_code_from_status(
        launch
            .to_command()
            .status()
            .with_context(|| format!("Failed to start launch command for {program}"))?,
    );

    std::process::exit(exit_code);
}

pub(crate) fn ensure_management_api_for_launch(account: &StoredAccount) -> anyhow::Result<()> {
    if account.cli_kind != CliKind::Codex || !matches!(account.runtime, RuntimeConfig::Host) {
        return Ok(());
    }
    // Keep the owner's bus switch: forcing it on here made the agent-mode
    // check in `ensure_agent_bus_for_launch` always pass, so a home with
    // `agent_bus_enabled: false` still started / probed a bus.
    let enabled = cutex::config::store::load_codez_config().agent_bus_enabled;
    let config = agent_bus_config::ensure_agent_bus_config(enabled, None)?;
    if let Err(err) =
        cutex::management::launch::require_management_api_running(&config, cutex::management::service::management_port(&config))
    {
        cutex::management::launch::warn_management_api_unavailable(&err);
    }
    Ok(())
}

pub(crate) fn ensure_agent_bus_for_launch(account: &StoredAccount) -> anyhow::Result<()> {
    if account.cli_kind != CliKind::Codex || !matches!(account.runtime, RuntimeConfig::Host) {
        return Ok(());
    }
    let config = cutex::config::store::load_codez_config();
    if !config.agent_bus_enabled {
        return Ok(());
    }
    let config = agent_bus_config::ensure_agent_bus_config(true, None)?;
    agent_bus_runtime::ensure_agent_bus_running(&config, true)
}

fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
