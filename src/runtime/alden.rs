//! cute-alden runtime adapter helpers.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context};

use crate::config::env::CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR;
use crate::config::env::{env_bool_override, env_var_first, CUTEX_ALDEN_BIN_ENV_VAR};
use crate::launch::command::LaunchCommand;
use crate::platform::command::command_exists_in_path;
use crate::platform::process::process_is_running;

pub const DEFAULT_ALDEN_HISTORY_BYTES: &str = "262144";
const CUTE_ALDEN_PARENT_ENV_VARS: &[&str] = &[
    "ALDEN",
    "ALDEN_SESSION_ACTIVE",
    "ALDEN_SESSION_LOG",
    "ALDEN_SESSION_NAME",
    "ALDEN_SESSION_PID",
    "CUTE_ALDEN",
    "CUTE_ALDEN_SESSION_ACTIVE",
    "CUTE_ALDEN_SESSION_LOG",
    "CUTE_ALDEN_SESSION_NAME",
    "CUTE_ALDEN_SESSION_PID",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuteAldenSession {
    pub pid: u32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuteAldenAttachPlan {
    pub program: String,
    pub session_name: String,
    pub takeover: bool,
}

impl CuteAldenAttachPlan {
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.arg("--attach").arg(&self.session_name);
        if self.takeover {
            command.arg("--takeover");
        }
        command
    }
}

pub fn cute_alden_program() -> anyhow::Result<String> {
    if let Some(program) = env_var_first(&[CUTEX_ALDEN_BIN_ENV_VAR])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(program);
    }

    if command_exists_in_path("cute-alden") {
        return Ok("cute-alden".to_string());
    }

    let repo_candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|path| {
            path.join("cute-alden")
                .join("cute-alden-0.2")
                .join("cute-alden")
        })
        .filter(|path| path.is_file());
    if let Some(repo_candidate) = repo_candidate {
        return Ok(repo_candidate.to_string_lossy().to_string());
    }

    anyhow::bail!(
        "cute-alden binary not found. Set {CUTEX_ALDEN_BIN_ENV_VAR} or put `cute-alden` on PATH."
    );
}

pub fn cute_alden_attach_plan(
    session_name: &str,
    takeover: bool,
) -> anyhow::Result<CuteAldenAttachPlan> {
    let session_name = session_name.trim();
    if session_name.is_empty() {
        anyhow::bail!("Session name cannot be empty");
    }
    Ok(CuteAldenAttachPlan {
        program: cute_alden_program()?,
        session_name: session_name.to_string(),
        takeover,
    })
}

pub fn wrap_launch_with_cute_alden(
    launch: LaunchCommand,
    alden_program: &str,
    session_name: &str,
) -> LaunchCommand {
    let LaunchCommand {
        program,
        args,
        envs,
        env_removes,
    } = launch;

    let mut wrapped = LaunchCommand::new(alden_program);
    for key in env_removes {
        wrapped = wrapped.env_remove(key);
    }
    for (key, value) in envs {
        wrapped = wrapped.env(key, value);
    }

    wrapped
        .arg("--name")
        .arg(session_name)
        .arg("--")
        .arg(program)
        .args(args)
}

pub fn wrap_launch_with_cute_alden_server_only(
    launch: LaunchCommand,
    alden_program: &str,
    session_name: &str,
    cwd: &str,
) -> LaunchCommand {
    let LaunchCommand {
        program,
        args,
        envs,
        env_removes,
    } = launch;

    let mut wrapped = LaunchCommand::new(alden_program);
    for key in env_removes {
        wrapped = wrapped.env_remove(key);
    }
    for (key, value) in envs {
        wrapped = wrapped.env(key, value);
    }

    let wrapped = wrapped
        .env(CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR, "1")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .arg("--allow-nesting")
        .arg("--server-only")
        .arg("--history-bytes")
        .arg(DEFAULT_ALDEN_HISTORY_BYTES)
        .arg("--name")
        .arg(session_name);
    let wrapped = CUTE_ALDEN_PARENT_ENV_VARS
        .iter()
        .fold(wrapped, |command, key| command.env_remove(*key));
    #[cfg(windows)]
    let wrapped = wrapped.arg("--cwd").arg(cwd);
    #[cfg(not(windows))]
    {
        let _ = cwd;
    }
    wrapped.arg("--").arg(program).args(args)
}

pub fn cute_alden_sessions() -> anyhow::Result<Vec<CuteAldenSession>> {
    let program = cute_alden_program()?;
    let output = Command::new(&program)
        .arg("--list")
        .output()
        .with_context(|| format!("Failed to start {program} --list"))?;
    if !output.status.success() {
        anyhow::bail!("{program} --list exited with status {}", output.status);
    }

    let stdout =
        String::from_utf8(output.stdout).context("cute-alden --list returned invalid UTF-8")?;
    parse_cute_alden_list(&stdout)
}

pub fn parse_cute_alden_list(stdout: &str) -> anyhow::Result<Vec<CuteAldenSession>> {
    let mut sessions = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let (pid_text, name_text) = line
            .split_once('\t')
            .ok_or_else(|| anyhow!("Unexpected cute-alden --list output line: {line}"))?;
        let pid = pid_text
            .trim()
            .parse::<u32>()
            .with_context(|| format!("Invalid cute-alden session pid: {pid_text}"))?;
        let name = match name_text.trim() {
            "" | "-" => None,
            value => Some(value.to_string()),
        };
        sessions.push(CuteAldenSession { pid, name });
    }

    Ok(sessions)
}

pub fn find_cute_alden_session_by_name(name: &str) -> Option<CuteAldenSession> {
    cute_alden_sessions()
        .ok()?
        .into_iter()
        .find(|session| session.name.as_deref() == Some(name))
}

pub fn find_live_cute_alden_session_by_name(name: &str) -> Option<CuteAldenSession> {
    find_cute_alden_session_by_name(name).filter(|session| process_is_running(session.pid))
}

pub fn already_inside_cute_alden_session() -> bool {
    env_bool_override("CUTE_ALDEN_SESSION_ACTIVE").unwrap_or(false)
        || env_bool_override("ALDEN_SESSION_ACTIVE").unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cute_alden_list_accepts_named_and_anonymous_sessions() {
        let sessions =
            parse_cute_alden_list("123\talpha\n456\t-\n789\t\n").expect("list should parse");

        assert_eq!(
            sessions,
            vec![
                CuteAldenSession {
                    pid: 123,
                    name: Some("alpha".to_string()),
                },
                CuteAldenSession {
                    pid: 456,
                    name: None,
                },
                CuteAldenSession {
                    pid: 789,
                    name: None,
                },
            ]
        );
    }

    #[test]
    fn attach_plan_builds_expected_command_shape() {
        let plan = CuteAldenAttachPlan {
            program: "cute-alden".to_string(),
            session_name: "alpha".to_string(),
            takeover: true,
        };
        let command = plan.command();
        assert_eq!(command.get_program(), "cute-alden");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(args, vec!["--attach", "alpha", "--takeover"]);
    }

    #[test]
    fn server_only_launch_removes_parent_alden_environment() {
        let launch = LaunchCommand::new("cute-codex")
            .arg("resume")
            .arg("019e-session")
            .env("CUTEX_AGENT_ID", "cutex.test.agent");

        let wrapped =
            wrap_launch_with_cute_alden_server_only(launch, "cute-alden", "cutex.test", "/tmp");
        let shell_command = wrapped.to_shell_command();

        assert_eq!(wrapped.program, "cute-alden");
        assert!(wrapped.args.iter().any(|arg| arg == "--allow-nesting"));
        assert!(wrapped.args.iter().any(|arg| arg == "--server-only"));
        for key in CUTE_ALDEN_PARENT_ENV_VARS {
            assert!(
                wrapped.env_removes.iter().any(|removed| removed == key),
                "{key} should be removed from server-only launch"
            );
            assert!(
                shell_command.contains(&format!("-u '{key}'")),
                "{key} should be removed in shell form"
            );
        }
        assert!(wrapped
            .envs
            .iter()
            .any(|(key, value)| { key == CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR && value == "1" }));
        assert!(wrapped
            .envs
            .iter()
            .any(|(key, value)| { key == "TERM" && value == "xterm-256color" }));
        assert!(wrapped
            .envs
            .iter()
            .any(|(key, value)| { key == "COLORTERM" && value == "truecolor" }));
        assert!(wrapped
            .envs
            .iter()
            .any(|(key, value)| { key == "CUTEX_AGENT_ID" && value == "cutex.test.agent" }));
    }
}
