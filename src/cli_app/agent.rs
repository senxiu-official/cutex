use cutex::cli::args::{AgentCommand, ImCommand};

pub(crate) fn run_command(command: AgentCommand) -> anyhow::Result<()> {
    super::agent_cli::run_command(command)
}

pub(crate) fn im(command: ImCommand) -> anyhow::Result<()> {
    super::im_cli::run_command(command)
}
