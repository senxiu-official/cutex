use cutex::launch::command::LaunchCommand;
use cutex::profiles::model::StoredAccount;
use cutex::runtime::launch::{
    duplicate_resume_warning_plan, maybe_wrap_launch_with_session as runtime_maybe_wrap_launch,
};
use cutex::ui::format::compact_home_path;

use super::launch_output::LaunchOutput;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

pub(crate) fn maybe_wrap_launch_with_session(
    account: &StoredAccount,
    codex_args: &[String],
    output: LaunchOutput,
    launch: LaunchCommand,
) -> anyhow::Result<LaunchCommand> {
    let plan = runtime_maybe_wrap_launch(account, codex_args, launch)?;
    if let Some(session_name) = plan.session_name.as_deref() {
        output.line(format_args!(
            "Session: managed via {BOLD}{session_name}{RESET}"
        ));
    }
    Ok(plan.launch)
}

pub(crate) fn warn_if_resume_target_is_already_running(
    codex_args: &[String],
    output: LaunchOutput,
) -> anyhow::Result<()> {
    let Some(runtime) = duplicate_resume_warning_plan(codex_args)? else {
        return Ok(());
    };

    output.line(format_args!(
        "{YELLOW}warning:{RESET} Codex session {BOLD}{}{RESET} already has a live cute-alden runtime.",
        runtime.codex_session_id
    ));
    output.line(format_args!(
        "  {DIM}session{RESET} {}  {DIM}cutex{RESET} {}  {DIM}pid{RESET} {}",
        runtime.display_name, runtime.cutex_session_id, runtime.alden_pid
    ));
    output.line(format_args!(
        "  {DIM}cwd{RESET} {}",
        compact_home_path(&runtime.cwd)
    ));
    output.line(format_args!(
        "  {GREEN}reconnect{RESET} cutex session attach --name {}",
        runtime.alden_session_name
    ));
    output.line(format_args!(
        "  {YELLOW}takeover{RESET} cutex session attach --name {} --takeover",
        runtime.alden_session_name
    ));
    output.line(format_args!(
        "  {DIM}continuing will start a second cute-codex process on the same history; attach is usually the right action.{RESET}"
    ));
    Ok(())
}
