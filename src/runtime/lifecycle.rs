//! Runtime lifecycle planning for durable `cutex_session` records.

use std::fs::OpenOptions;
#[cfg(unix)]
use std::io;
use std::path::Path;
use std::process::{Child, Stdio};

use anyhow::Context;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::agent_bus::identity::default_agent_group_for;
use crate::config::env::CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR;
use crate::launch::args::codex_args_for_runtime;
use crate::launch::command::LaunchCommand;
use crate::profiles::model::{CliKind, RuntimeConfig, StoredAccount};
use crate::runtime::alden::{cute_alden_program, wrap_launch_with_cute_alden_server_only};
pub use crate::runtime::args::{
    append_codex_cli_args_with_overrides, cutex_session_runtime_default_cli_args,
    effective_runtime_permission_defaults,
};
pub use crate::runtime::codex_home::codex_session_exists_in_home;
use crate::runtime::process_scope::{managed_agent_launch_plan, ManagedAgentProcessIsolation};
pub use crate::runtime::session_online::{
    default_cutex_alden_session_name, session_online_agent_id, session_online_agent_identity_env,
    session_online_agent_identity_env_with_id, session_online_log_path, session_online_log_tail,
    session_online_terminal_color_env,
};
pub use crate::runtime::stop::{session_runtime_stop_target, SessionRuntimeStopTarget};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};
use crate::session::service::cutex_session_launch_cwd;

#[derive(Debug, Clone)]
pub struct SessionOnlineResumePlan {
    pub effective_args: Vec<String>,
    pub groups: Vec<String>,
    pub codex_session_id: String,
    pub launch_cwd: String,
}

#[derive(Debug, Clone)]
pub struct LiveRemoteTuiAttachPlan {
    pub effective_args: Vec<String>,
    pub codex_session_id: String,
    pub launch_cwd: String,
}

#[derive(Debug, Clone)]
pub struct SessionOnlineLaunch {
    pub launch: LaunchCommand,
    pub backend: CutexSessionRuntimeBackend,
    pub alden_session_name: Option<String>,
    pub cwd: String,
}

pub fn cutex_session_host_is_local(session_host_id: &str, current_host: &str) -> bool {
    let host = session_host_id.trim();
    host.is_empty()
        || host.eq_ignore_ascii_case(current_host)
        || (host.eq_ignore_ascii_case("localhost") && current_host != "unknown")
}

pub fn session_online_resume_plan(
    record: &CutexSessionRecord,
    account: &StoredAccount,
) -> anyhow::Result<SessionOnlineResumePlan> {
    let mut codex_args = session_online_base_codex_args(record, account)?;
    let codex_session_id = record
        .codex_session_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cutex session has no Codex session id"))?;
    codex_args.push("resume".to_string());
    codex_args.push("-c".to_string());
    codex_args.push("tui.resume_cwd=\"current\"".to_string());
    codex_args.push(codex_session_id.to_string());
    let effective_args = codex_args_for_runtime(account, codex_args);
    let groups = session_online_agent_groups(record);

    Ok(SessionOnlineResumePlan {
        effective_args,
        groups,
        codex_session_id: codex_session_id.to_string(),
        launch_cwd: cutex_session_launch_cwd(record).to_string(),
    })
}

pub fn session_online_base_codex_args(
    record: &CutexSessionRecord,
    account: &StoredAccount,
) -> anyhow::Result<Vec<String>> {
    let codex_args = session_runtime_base_codex_args(record, account)?;
    let codex_session_id = required_codex_session_id(record)?;
    if !codex_session_exists_in_home(codex_session_id)? {
        anyhow::bail!("codex_session_not_found: {codex_session_id}");
    }
    Ok(codex_args)
}

pub fn live_remote_tui_attach_plan(
    record: &CutexSessionRecord,
    account: &StoredAccount,
) -> anyhow::Result<LiveRemoteTuiAttachPlan> {
    let mut codex_args = session_runtime_base_codex_args(record, account)?;
    codex_args = remote_attach_args(codex_args);
    let codex_session_id = required_codex_session_id(record)?;
    codex_args.push("resume".to_string());
    codex_args.push("-c".to_string());
    codex_args.push("tui.resume_cwd=\"current\"".to_string());
    codex_args.push(codex_session_id.to_string());
    Ok(LiveRemoteTuiAttachPlan {
        effective_args: codex_args_for_runtime(account, codex_args),
        codex_session_id: codex_session_id.to_string(),
        launch_cwd: cutex_session_launch_cwd(record).to_string(),
    })
}

/// A remote frontend observes receiver permissions, rather than overriding them.
pub fn remote_attach_args(args: Vec<String>) -> Vec<String> {
    let mut result = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let flag = arg.split('=').next().unwrap_or(&arg);
        if matches!(flag, "--sandbox" | "-s" | "--ask-for-approval" | "-a" | "--permission-profile" | "--add-dir") {
            if !arg.contains('=') { args.next(); }
        } else if matches!(flag, "--full-auto" | "--yolo" | "--dangerously-bypass-approvals-and-sandbox") {
            continue;
        } else if arg == "-c" || arg == "--config" {
            if let Some(value) = args.next() {
                if !remote_permission_config(&value) { result.extend([arg, value]); }
            }
        } else if let Some(value) = arg.strip_prefix("--config=") {
            if !remote_permission_config(value) { result.push(arg); }
        } else {
            result.push(arg);
        }
    }
    result
}

fn remote_permission_config(value: &str) -> bool {
    let key = value.split('=').next().unwrap_or(value).trim();
    matches!(key.split('.').next().unwrap_or(key),
        "approval_policy" | "approvals_reviewer" | "sandbox_mode" |
        "default_permissions" | "permissions" | "network" | "sandbox_workspace_write")
}

fn required_codex_session_id(record: &CutexSessionRecord) -> anyhow::Result<&str> {
    record
        .codex_session_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cutex session has no Codex session id"))
}

/// Codex native memories stay off for managed Agents (cutex manages project
/// memory instead; DESIGN 3.2). Placed first so profile / session defaults
/// that set `features.memories` later on the command line still win.
pub fn native_memory_default_args() -> Vec<String> {
    vec!["-c".to_string(), "features.memories=false".to_string()]
}

fn session_runtime_base_codex_args(
    record: &CutexSessionRecord,
    account: &StoredAccount,
) -> anyhow::Result<Vec<String>> {
    if account.cli_kind != CliKind::Codex {
        anyhow::bail!("session.online currently supports Codex profiles only");
    }
    if !matches!(account.runtime, RuntimeConfig::Host) {
        anyhow::bail!("session.online currently supports host runtime only");
    }
    let launch_cwd = cutex_session_launch_cwd(record).to_string();
    let mut codex_args = native_memory_default_args();
    codex_args.extend(account.default_cli_args.iter().cloned());
    codex_args = append_codex_cli_args_with_overrides(
        codex_args,
        cutex_session_runtime_default_cli_args(record),
    );
    codex_args = append_codex_cli_args_with_overrides(codex_args, record.default_cli_args.clone());
    codex_args = append_codex_cli_args_with_overrides(
        codex_args,
        vec!["--cd".to_string(), launch_cwd.clone()],
    );
    Ok(codex_args)
}

pub fn session_online_agent_groups(record: &CutexSessionRecord) -> Vec<String> {
    if record.agent_groups.is_empty() {
        vec![default_agent_group_for(
            None,
            cutex_session_launch_cwd(record),
        )]
    } else {
        record.agent_groups.clone()
    }
}

pub fn finalize_session_online_launch(
    record: &CutexSessionRecord,
    account: &StoredAccount,
    base_launch: LaunchCommand,
    resume_plan: &SessionOnlineResumePlan,
) -> anyhow::Result<SessionOnlineLaunch> {
    let launch = session_online_agent_identity_env(
        session_online_terminal_color_env(base_launch),
        account,
        record,
        &resume_plan.groups,
    );
    match record.runtime_backend {
        CutexSessionRuntimeBackend::Host => Ok(SessionOnlineLaunch {
            launch: launch.env(CUTEX_HEADLESS_AGENT_RUNTIME_ENV_VAR, "1"),
            backend: CutexSessionRuntimeBackend::Host,
            alden_session_name: None,
            cwd: resume_plan.launch_cwd.clone(),
        }),
        CutexSessionRuntimeBackend::CuteAlden => {
            let alden_program = cute_alden_program()?;
            let alden_session_name = record
                .alden_session_name
                .clone()
                .unwrap_or_else(|| default_cutex_alden_session_name(record));
            Ok(SessionOnlineLaunch {
                launch: wrap_launch_with_cute_alden_server_only(
                    launch,
                    &alden_program,
                    &alden_session_name,
                    &resume_plan.launch_cwd,
                ),
                backend: CutexSessionRuntimeBackend::CuteAlden,
                alden_session_name: Some(alden_session_name),
                cwd: resume_plan.launch_cwd.clone(),
            })
        }
        CutexSessionRuntimeBackend::HostForeground => {
            anyhow::bail!(
                "session.online requires a visible terminal for runtime_backend=host_foreground; run `cutex session foreground {}` from the local terminal",
                resume_plan.codex_session_id
            )
        }
        CutexSessionRuntimeBackend::ClaudeSubscription => {
            anyhow::bail!("Claude sessions use the Claude runtime controller, not Codex resume")
        }
        CutexSessionRuntimeBackend::Docker | CutexSessionRuntimeBackend::Future => {
            anyhow::bail!(
                "session.online does not support runtime_backend={:?}",
                record.runtime_backend
            )
        }
    }
}

pub fn spawn_detached_session_launch(
    launch: &LaunchCommand,
    cwd: &str,
    log_path: &Path,
) -> anyhow::Result<Child> {
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("Failed to open session runtime log: {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .context("Failed to clone session runtime log")?;
    let mut command = launch.to_command();
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .with_context(|| format!("Failed to start session runtime: {}", launch.program))
}

pub fn spawn_managed_agent_runtime_launch(
    cutex_session_id: &str,
    launch: &LaunchCommand,
    cwd: &str,
    log_path: &Path,
) -> anyhow::Result<Child> {
    let plan = managed_agent_launch_plan(cutex_session_id, launch.clone());
    match &plan.isolation {
        ManagedAgentProcessIsolation::Direct { reason } => eprintln!(
            "managed Agent process isolation fallback: cutex_session_id={cutex_session_id} mode=direct reason={reason}"
        ),
        ManagedAgentProcessIsolation::WindowsJobObject { job_name } => {
            #[cfg(windows)]
            return spawn_in_windows_job(job_name, &plan.launch, cwd, log_path);
            #[cfg(not(windows))]
            anyhow::bail!("Windows job object isolation requested off Windows: {job_name}");
        }
        ManagedAgentProcessIsolation::SystemdScope { .. } => {}
    }
    spawn_detached_session_launch(&plan.launch, cwd, log_path)
}

/// Create the Agent's job, start the runtime suspended, make it a job member
/// holding the job's last long-lived handle, then resume it. The daemon's own
/// handle closes on return, so a management restart leaves the runtime (and
/// its job) running for adoption. Any failure kills the suspended child.
#[cfg(windows)]
fn spawn_in_windows_job(
    job_name: &str,
    launch: &LaunchCommand,
    cwd: &str,
    log_path: &Path,
) -> anyhow::Result<Child> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    use crate::platform::windows_job;

    let job = windows_job::create_agent_job(job_name)?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("Failed to open session runtime log: {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .context("Failed to clone session runtime log")?;
    let mut command = launch.to_command();
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .creation_flags(CREATE_SUSPENDED);
    let mut child = command
        .spawn()
        .with_context(|| format!("Failed to start session runtime: {}", launch.program))?;
    if let Err(error) = windows_job::adopt_suspended_child(&job, &child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.context(format!("managed Agent job {job_name}")));
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn host_codex_account() -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            claude_code: Default::default(),
            id: "account-live-remote-tui".to_string(),
            name: "live-remote-tui".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: None,
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: vec!["--model".to_string(), "profile-model".to_string()],
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        }
    }

    #[test]
    fn fresh_live_remote_attach_skips_rollout_but_offline_resume_still_requires_it() {
        let thread_id = format!("fresh-live-no-rollout-{}", Uuid::new_v4());
        assert!(!codex_session_exists_in_home(&thread_id).expect("rollout lookup"));
        let mut record = CutexSessionRecord::new_at(
            "cutex.fresh-live".to_string(),
            Some(thread_id.clone()),
            "host".to_string(),
            "/tmp/fresh-live".to_string(),
            Some("live-remote-tui".to_string()),
            "2026-08-27T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.default_cli_args = vec!["--no-alt-screen".to_string(), "--sandbox".to_string(), "danger-full-access".to_string()];
        let account = host_codex_account();

        let plan = live_remote_tui_attach_plan(&record, &account)
            .expect("live remote attach does not need a rollout");
        assert!(!plan.effective_args.iter().any(|arg| arg == "--sandbox" || arg == "danger-full-access"));
        assert_eq!(remote_attach_args(vec![
            "--ask-for-approval".into(), "never".into(), "--yolo".into(),
            "-c".into(), "approval_policy=\"never\"".into(),
            "--config=permissions.default=\"full\"".into(),
            "-c".into(), "model=\"test\"".into(),
        ]), vec!["-c", "model=\"test\""]);
        assert_eq!(plan.codex_session_id, thread_id);
        assert_eq!(plan.launch_cwd, "/tmp/fresh-live");
        assert!(plan
            .effective_args
            .windows(2)
            .any(|args| args == ["--model", "profile-model"]));
        assert!(plan
            .effective_args
            .windows(2)
            .any(|args| args == ["--cd", "/tmp/fresh-live"]));
        assert!(plan
            .effective_args
            .iter()
            .any(|arg| arg == "--no-alt-screen"));
        assert!(plan.effective_args.windows(4).any(|args| args
            == [
                "resume",
                "-c",
                "tui.resume_cwd=\"current\"",
                plan.codex_session_id.as_str()
            ]));

        let error = session_online_resume_plan(&record, &account)
            .expect_err("ordinary offline resume must still require a rollout");
        assert_eq!(
            error.to_string(),
            format!("codex_session_not_found: {}", plan.codex_session_id)
        );
    }

    #[cfg(unix)]
    #[test]
    fn detached_session_launch_uses_cwd_and_appends_output_to_log() {
        let temp_dir =
            std::env::temp_dir().join(format!("cutex-detached-launch-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let log_path = temp_dir.join("runtime.log");
        let launch = LaunchCommand::new("sh").args([
            "-c",
            "printf '%s\\n' \"$PWD\"; printf '%s\\n' runtime-stderr >&2",
        ]);

        let mut child = spawn_detached_session_launch(
            &launch,
            temp_dir.to_str().expect("temp path should be utf-8"),
            &log_path,
        )
        .expect("launch should spawn");
        let status = child.wait().expect("child should be waitable");

        assert!(status.success(), "child exited with {status}");
        let log = fs::read_to_string(&log_path).expect("runtime log should exist");
        assert!(log.contains(temp_dir.to_string_lossy().as_ref()));
        assert!(log.contains("runtime-stderr"));
        fs::remove_dir_all(&temp_dir).expect("temp dir should be removed");
    }
}
