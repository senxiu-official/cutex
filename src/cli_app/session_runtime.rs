use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use chrono::Utc;
use uuid::Uuid;

use cutex::app_server::runtime::AppServerRuntimeLayout;
use cutex::config::store::load_codez_config;
use cutex::launch::args::codex_args_for_runtime;
use cutex::management::remote::{
    management_api_healthy, management_http_json_with_timeout,
};
use cutex::management::service::{
    management_api_token, management_base_url, management_root_credential,
};
use cutex::platform::host::current_host_name;
use cutex::platform::process::process_is_running;
use cutex::profiles::model::CodezConfig;
use cutex::runtime::alden::cute_alden_sessions;
use cutex::runtime::launch::{
    duplicate_resume_check_response, foreground_resume_host_warning, foreground_resume_plan,
    session_takeover_target,
};
use cutex::runtime::lifecycle::{
    append_codex_cli_args_with_overrides, cutex_session_host_is_local,
    cutex_session_runtime_default_cli_args, session_online_agent_identity_env,
    session_online_terminal_color_env,
};
use cutex::session::model::{
    CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionUserAction,
};
use cutex::session::service::{
    cutex_session_display_name, cutex_session_key_for_user_id, cutex_session_launch_cwd,
    persist_cutex_session_store_and_im_record,
};
use cutex::session::store::load_cutex_session_store;
use cutex::ui::format::{compact_home_path, compact_json_value};

use super::session_attach;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RUNTIME_CLOSE_WAIT_TIMEOUT: Duration = Duration::from_secs(12);
const RUNTIME_CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) fn record_cutex_session_user_action(
    id: &str,
    action: CutexSessionUserAction,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, id)
        .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow!("cutex session disappeared while recording user action: {key}"))?;
    let timestamp = Utc::now().to_rfc3339();
    record.last_user_selected_at = Some(timestamp.clone());
    record.last_user_action = Some(action);
    record.updated_at = timestamp;
    persist_cutex_session_store_and_im_record(&store, &key)
}

pub(crate) fn cmd_session_takeover(id: &str) -> anyhow::Result<()> {
    let store = load_cutex_session_store()?;
    if let Some(key) = cutex_session_key_for_user_id(&store, id) {
        if store.sessions[&key].explicit_launch.is_some() {
            return super::stock_lifecycle::attach(&key);
        }
    }
    let trimmed = id.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Session id cannot be empty");
    }

    if let Some(target) = session_takeover_target(trimmed)? {
        println!(
            "{YELLOW}takeover{RESET} {}  {DIM}pid{RESET} {}",
            target.session_name, target.pid
        );
        return session_attach::cmd_session_attach(&target.session_name, true);
    }

    anyhow::bail!(
        "No live cute-alden runtime found for {trimmed}. Try `cutex session duplicate-check {trimmed}` or `cutex session list`."
    );
}

pub(crate) fn cmd_session_resume_alden(id: &str) -> anyhow::Result<()> {
    cmd_session_resume_alden_with_profile(id, None)
}

pub(crate) fn cmd_session_resume_alden_with_profile(
    id: &str,
    launch_profile: Option<&str>,
) -> anyhow::Result<()> {
    let store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, id)
        .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
    let record = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared while preparing resume: {key}"))?;
    if record.runtime_backend != CutexSessionRuntimeBackend::CuteAlden {
        return cmd_session_resume_foreground_with_profile(&record, None, launch_profile);
    }
    let resume_id = record
        .codex_session_id
        .as_deref()
        .unwrap_or(record.cutex_session_id.as_str())
        .to_string();
    let alden_sessions = cute_alden_sessions().unwrap_or_default();
    if cutex::session::projection::cutex_session_is_attachable(&record, &alden_sessions) {
        if launch_profile.is_some() {
            anyhow::bail!(
                "one-launch profile cannot be applied while taking over an existing cute-alden process"
            );
        }
        // Resume is a foreground handoff. Takeover is harmless without an
        // attached client and is required when another TUI owns the FIFOs.
        record_cutex_session_user_action(&key, CutexSessionUserAction::Takeover)?;
        return cmd_session_takeover(&resume_id);
    }

    cmd_session_online_with_profile(&resume_id, launch_profile, true)?;
    let store = load_cutex_session_store()?;
    let record = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared after online: {key}"))?;
    let alden_sessions = cute_alden_sessions().unwrap_or_default();
    if !cutex::session::projection::cutex_session_is_attachable(&record, &alden_sessions) {
        anyhow::bail!(
            "session online completed but no attachable cute-alden runtime was found for {}",
            cutex_session_display_name(&record)
        );
    }
    record_cutex_session_user_action(&key, CutexSessionUserAction::ResumeAttach)?;
    cmd_session_takeover(&resume_id)
}

pub(crate) fn cmd_session_duplicate_check(id: &str, json: bool) -> anyhow::Result<()> {
    let response = duplicate_resume_check_response(id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    if response.duplicate {
        println!(
            "{YELLOW}warning:{RESET} Codex session {BOLD}{}{RESET} already has a live cute-alden runtime.",
            response.codex_session_id.as_deref().unwrap_or(id)
        );
        if let (Some(display_name), Some(cutex_session_id), Some(alden_pid)) = (
            response.display_name.as_deref(),
            response.cutex_session_id.as_deref(),
            response.alden_pid,
        ) {
            println!(
                "  {DIM}session{RESET} {display_name}  {DIM}cutex{RESET} {cutex_session_id}  {DIM}pid{RESET} {alden_pid}"
            );
        }
        if let Some(cwd) = response.cwd.as_deref() {
            println!("  {DIM}cwd{RESET} {}", compact_home_path(cwd));
        }
        if let Some(command) = response.attach_command.as_ref() {
            println!("  {GREEN}reconnect{RESET} {}", command.join(" "));
        }
        if let Some(command) = response.takeover_command.as_ref() {
            println!("  {YELLOW}takeover{RESET} {}", command.join(" "));
        }
    } else {
        println!("{GREEN}ok{RESET}: no live duplicate runtime found for {BOLD}{id}{RESET}");
    }
    Ok(())
}

pub(crate) fn cmd_session_lifecycle_action(
    id: &str,
    action_type: &str,
    force: bool,
) -> anyhow::Result<()> {
    let payload = if force {
        serde_json::json!({ "force": true })
    } else {
        serde_json::json!({})
    };
    cmd_session_lifecycle_action_with_payload(id, action_type, payload).map(|_| ())
}

pub(crate) fn cmd_session_online_with_profile(
    id: &str,
    launch_profile: Option<&str>,
    open_visible_terminal: bool,
) -> anyhow::Result<serde_json::Value> {
    let mut payload = serde_json::json!({
        "open_visible_terminal": open_visible_terminal,
    });
    if let Some(profile) = launch_profile {
        payload
            .as_object_mut()
            .expect("online lifecycle payload object")
            .insert(
                "launch_profile".to_string(),
                serde_json::Value::String(profile.to_string()),
            );
    }
    cmd_session_lifecycle_action_with_payload(id, "session.online", payload)
}

pub(crate) fn cmd_session_close_and_restart_with_profile(
    id: &str,
    launch_profile: Option<&str>,
    open_visible_terminal: bool,
) -> anyhow::Result<serde_json::Value> {
    let store = load_cutex_session_store()?;
    let key =
        cutex_session_key_for_user_id(&store, id).ok_or_else(|| anyhow!("unknown session"))?;
    if store.sessions[&key].explicit_launch.is_some() {
        validate_managed_launch_overrides(&store.sessions[&key], None, launch_profile)?;
        let receipt = super::stock_lifecycle::online(&key, true)?;
        let response = serde_json::to_value(receipt)?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(response);
    }
    cmd_session_close_and_wait_with_output(id, LifecycleResponseOutput::Print)
        .context("Failed to close runtime before restart; restart was not attempted")?;
    cmd_session_online_with_profile(id, launch_profile, open_visible_terminal)
        .context("Runtime closed, but failed to restart")
}

pub(crate) fn cmd_session_close_and_wait(id: &str) -> anyhow::Result<serde_json::Value> {
    cmd_session_close_and_wait_with_output(id, LifecycleResponseOutput::Print)
}

pub(crate) fn cmd_session_close_and_wait_quiet(id: &str) -> anyhow::Result<serde_json::Value> {
    cmd_session_close_and_wait_with_output(id, LifecycleResponseOutput::Suppress)
}

fn cmd_session_close_and_wait_with_output(
    id: &str,
    output: LifecycleResponseOutput,
) -> anyhow::Result<serde_json::Value> {
    let started = Instant::now();
    loop {
        let close = cmd_session_lifecycle_action_with_payload_and_output(
            id,
            "session.close",
            serde_json::json!({}),
            output,
        )
        .context("Failed to close runtime")?;
        if runtime_close_is_complete(&close)? {
            return Ok(close);
        }
        if started.elapsed() >= RUNTIME_CLOSE_WAIT_TIMEOUT {
            anyhow::bail!(
                "runtime remained closing for {} seconds",
                RUNTIME_CLOSE_WAIT_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(RUNTIME_CLOSE_POLL_INTERVAL);
    }
}

fn runtime_close_is_complete(response: &serde_json::Value) -> anyhow::Result<bool> {
    let status = response
        .pointer("/cutex/result/status")
        .and_then(serde_json::Value::as_str)
        .context("management v2 close response omitted cutex.result.status")?;
    match status {
        "closed" | "offline" => Ok(true),
        "closing" => Ok(false),
        _ => anyhow::bail!("runtime close returned unexpected status {status}"),
    }
}

fn cmd_session_lifecycle_action_with_payload(
    id: &str,
    action_type: &str,
    payload: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    cmd_session_lifecycle_action_with_payload_and_output(
        id,
        action_type,
        payload,
        LifecycleResponseOutput::Print,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleResponseOutput {
    Print,
    Suppress,
}

impl LifecycleResponseOutput {
    fn should_print(self) -> bool {
        self == Self::Print
    }
}

fn cmd_session_lifecycle_action_with_payload_and_output(
    id: &str,
    action_type: &str,
    payload: serde_json::Value,
    output: LifecycleResponseOutput,
) -> anyhow::Result<serde_json::Value> {
    let config = load_codez_config();
    let store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, id)
        .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
    let record = store
        .sessions
        .get(&key)
        .ok_or_else(|| anyhow!("cutex session disappeared while preparing lifecycle request"))?;
    if action_type == "session.online" && record.explicit_launch.is_some() && cutex_session_host_is_local(&record.host_id, &current_host_name()) {
        let profile = launch_profile_from_payload(&payload)?;
        validate_managed_launch_overrides(record, None, profile.as_deref())?;
        let receipt = super::stock_lifecycle::online(&key, false)?;
        let response = serde_json::to_value(receipt)?;
        if output.should_print() {
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        return Ok(response);
    }
    let method = match action_type {
        "session.online" => "cutex/runtime/online",
        "session.offline" => "cutex/runtime/offline",
        "session.close" => "cutex/runtime/close",
        _ => anyhow::bail!("unsupported session lifecycle action: {action_type}"),
    };
    let request_id = Uuid::new_v4().to_string();
    let mut params = serde_json::json!({
        "expectedRuntimeGeneration": record.runtime_generation,
        "reason": "cutex_cli",
    });
    if method == "cutex/runtime/online" {
        let open_visible_terminal = payload
            .get("openVisibleTerminal")
            .or_else(|| payload.get("open_visible_terminal"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        params["openVisibleTerminal"] = serde_json::Value::Bool(open_visible_terminal);
        if let Some(profile) = launch_profile_from_payload(&payload)? {
            params["launchProfile"] = serde_json::Value::String(profile);
        }
    } else {
        let force = payload
            .get("force")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        params["force"] = serde_json::Value::Bool(force);
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "requestId": request_id,
        "method": method,
        "params": params,
    }))?;
    let (base_url, token) = management_endpoint_for_record(&config, record)?;
    let token = if cutex_session_host_is_local(&record.host_id, &current_host_name()) {
        lifecycle_request_credential(&config, method, token)?
    } else { token };
    let encoded_session_id =
        url::form_urlencoded::byte_serialize(record.cutex_session_id.as_bytes())
            .collect::<String>();
    let path = format!("/v2/sessions/{encoded_session_id}/cutex/requests");
    let response = management_http_json_with_timeout(
        &base_url,
        "POST",
        &path,
        token.as_deref(),
        Some(&body),
        Duration::from_secs(30),
    )?;
    if response
        .get("contractVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(2)
        || response
            .get("requestId")
            .and_then(serde_json::Value::as_str)
            != Some(request_id.as_str())
        || response
            .get("cutexSessionId")
            .and_then(serde_json::Value::as_str)
            != Some(record.cutex_session_id.as_str())
        || response
            .pointer("/cutex/method")
            .and_then(serde_json::Value::as_str)
            != Some(method)
    {
        anyhow::bail!("management v2 lifecycle response identity mismatch");
    }
    if output.should_print() {
        print_management_v2_lifecycle_response(&response);
    }
    Ok(response)
}

fn lifecycle_request_credential(
    config: &CodezConfig,
    method: &str,
    ordinary: Option<String>,
) -> anyhow::Result<Option<String>> {
    if matches!(method, "cutex/runtime/offline" | "cutex/runtime/close") {
        Ok(Some(management_root_credential(config, None)?.to_string()))
    } else {
        Ok(ordinary)
    }
}

fn launch_profile_from_payload(payload: &serde_json::Value) -> anyhow::Result<Option<String>> {
    let Some(value) = payload
        .get("launchProfile")
        .or_else(|| payload.get("launch_profile"))
    else {
        return Ok(None);
    };
    let profile = value
        .as_str()
        .ok_or_else(|| anyhow!("launch profile must be a string"))?
        .trim();
    if profile.is_empty() {
        anyhow::bail!("launch profile cannot be empty");
    }
    Ok(Some(profile.to_string()))
}

fn management_endpoint_for_record(
    config: &CodezConfig,
    record: &CutexSessionRecord,
) -> anyhow::Result<(String, Option<String>)> {
    let token = management_api_token(config, None);
    let current_host = current_host_name();
    if cutex_session_host_is_local(&record.host_id, &current_host) {
        if let Ok(base_url) = std::env::var("CUTEX_MANAGEMENT_URL") {
            cutex::config::instance::management_url_override(config)?;
            let url = url::Url::parse(&base_url)?;
            anyhow::ensure!(
                url.scheme() == "http"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "CUTEX_MANAGEMENT_URL must be an http endpoint without embedded credentials"
            );
            return Ok((base_url, token.map(str::to_string)));
        }
        if !management_api_healthy(cutex::management::service::management_port(config), token) {
            cutex::management::launch::require_management_api_running(
                config,
                cutex::management::service::management_port(config),
            )
            .context("A cutex management v2 service is required for lifecycle operations")?;
        }
        return Ok((
            management_base_url(cutex::management::service::management_port(config)),
            token.map(str::to_string),
        ));
    }
    let hosts = cutex::management::connections::Hosts::load()?;
    let (url, token) = hosts.for_host(&record.host_id)?.verified_endpoint()?;
    Ok((url, Some(token)))
}

fn print_management_v2_lifecycle_response(response: &serde_json::Value) {
    let status = response
        .pointer("/cutex/result/status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("accepted");
    println!(
        "{GREEN}{status}{RESET} {DIM}session={} request_id={}{RESET}",
        response
            .get("cutexSessionId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        response
            .get("requestId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
    );
    if let Some(result) = response.pointer("/cutex/result") {
        println!("  {DIM}runtime{RESET} {}", compact_json_value(result));
    }
}

pub(crate) fn cmd_session_foreground(id: &str) -> anyhow::Result<()> {
    cmd_session_foreground_with_profile(id, None)
}

pub(crate) fn cmd_session_foreground_with_profile(
    id: &str,
    launch_profile: Option<&str>,
) -> anyhow::Result<()> {
    let store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, id)
        .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
    let record = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared while starting foreground: {key}"))?;
    let launch_cwd = cutex_session_launch_cwd(&record).to_string();
    record_cutex_session_user_action(&key, CutexSessionUserAction::ResumeManaged)?;
    cmd_session_resume_foreground_with_profile(&record, Some(launch_cwd.as_str()), launch_profile)
}

pub(crate) fn cmd_session_resume_foreground(
    record: &CutexSessionRecord,
    cwd_override: Option<&str>,
) -> anyhow::Result<()> {
    cmd_session_resume_foreground_with_profile(record, cwd_override, None)
}

pub(crate) fn cmd_session_resume_foreground_with_profile(
    record: &CutexSessionRecord,
    cwd_override: Option<&str>,
    launch_profile: Option<&str>,
) -> anyhow::Result<()> {
    cmd_session_resume_foreground_inner(record, cwd_override, launch_profile)
}

fn validate_managed_launch_overrides(
    record: &CutexSessionRecord,
    cwd: Option<&str>,
    profile: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(profile.is_none(), "one-launch profile overrides are unavailable for managed runtimes; choose the saved profile with cutex session profile set {} <profile>", record.cutex_session_id);
    anyhow::ensure!(cwd.is_none_or(|cwd| cwd == cutex_session_launch_cwd(record)), "managed runtime cwd belongs to its saved configuration; update it with cutex human config set {} cwd=<path>", record.cutex_session_id);
    Ok(())
}

fn cmd_session_resume_foreground_inner(
    record: &CutexSessionRecord,
    cwd_override: Option<&str>,
    launch_profile: Option<&str>,
) -> anyhow::Result<()> {
    if record.explicit_launch.is_some() {
        validate_managed_launch_overrides(record, cwd_override, launch_profile)?;
        return super::stock_lifecycle::attach(&record.cutex_session_id);
    }
    if record.is_retired() {
        anyhow::bail!(
            "cannot resume retired cutex session {}; restore it first",
            record.cutex_session_id
        );
    }
    let codex_session_id = record
        .codex_session_id
        .as_deref()
        .ok_or_else(|| anyhow!("cutex session has no Codex session id"))?;
    if record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden {
        return cmd_session_resume_alden_with_profile(codex_session_id, launch_profile);
    }
    if record.runtime_backend == CutexSessionRuntimeBackend::HostForeground {
        return cmd_session_attach_host_foreground_tui(record, cwd_override, launch_profile);
    }
    if launch_profile.is_some() {
        anyhow::bail!(
            "one-launch profile is supported only for manager-owned online, cute-alden restore, or host-foreground TUI launches"
        );
    }
    if record.app_server_runtime.is_some() {
        anyhow::bail!(
            "session {codex_session_id} already uses a manager-owned app-server; a second local Codex core is not allowed"
        );
    }
    let launch_cwd = if let Some(cwd) = cwd_override {
        std::env::set_current_dir(cwd)
            .with_context(|| format!("Failed to enter session launch cwd: {cwd}"))?;
        let current = std::env::current_dir()?;
        let launch_cwd = current.display().to_string();
        println!("{DIM}cwd{RESET} {}", compact_home_path(&launch_cwd));
        launch_cwd
    } else {
        let current = std::env::current_dir()
            .context("Failed to determine current directory")?
            .display()
            .to_string();
        println!("{DIM}cwd{RESET} {}", compact_home_path(&current));
        current
    };
    let current_host = current_host_name();
    if let Some(warning) = foreground_resume_host_warning(record, &current_host) {
        println!(
            "{YELLOW}warning:{RESET} session host is {}, current host is {}. Foreground resume may still work if the Codex home/history is available here.",
            warning.session_host, warning.current_host
        );
    }
    let plan = foreground_resume_plan(
        record,
        || load_codez_config().default_profile,
        &current_host,
    )?;
    let account = super::launch::prepare_account_for_launch(&plan.profile)?;
    let mut codex_args = account.default_cli_args.clone();
    codex_args = append_codex_cli_args_with_overrides(
        codex_args,
        cutex_session_runtime_default_cli_args(record),
    );
    codex_args = append_codex_cli_args_with_overrides(codex_args, record.default_cli_args.clone());
    codex_args = append_codex_cli_args_with_overrides(
        codex_args,
        vec!["--cd".to_string(), launch_cwd.clone()],
    );
    codex_args.push("resume".to_string());
    codex_args.push("-c".to_string());
    codex_args.push("tui.resume_cwd=\"current\"".to_string());
    codex_args.push(plan.codex_session_id);
    let effective_args = codex_args_for_runtime(&account, codex_args);
    println!(
        "CLI binary: {BOLD}{}{RESET}",
        cutex::launch::program::cli_program(&account.cli_kind)
    );
    super::launch_process::ensure_management_api_for_launch(&account)?;
    if plan.agent_mode {
        super::launch_process::ensure_agent_bus_for_launch(&account)?;
    }
    let base_launch = super::launch_command::codex_launch_command_with_agent_mode(
        &account,
        &effective_args,
        plan.agent_mode,
        &plan.groups,
    )?;
    let launch = if plan.agent_mode {
        session_online_agent_identity_env(
            session_online_terminal_color_env(base_launch),
            &account,
            record,
            &plan.groups,
        )
    } else {
        base_launch
    };
    let exit_code = launch
        .to_command()
        .status()
        .with_context(|| "Failed to start foreground session resume command")?
        .code()
        .unwrap_or(1);
    super::session_attach::foreground_status(exit_code)
}

fn cmd_session_attach_host_foreground_tui(
    record: &CutexSessionRecord,
    cwd_override: Option<&str>,
    launch_profile: Option<&str>,
) -> anyhow::Result<()> {
    let current_host = current_host_name();
    if !cutex_session_host_is_local(&record.host_id, &current_host) {
        anyhow::bail!(
            "visible TUI must be opened on the runtime host: session host={} current host={}",
            record.host_id,
            current_host
        );
    }

    let resolved_launch_profile = launch_profile
        .map(super::launch::resolve_launch_profile_override)
        .transpose()?;

    let record =
        if host_foreground_app_server_layout(record)?.is_some() {
            record.clone()
        } else {
            let id = record
                .codex_session_id
                .as_deref()
                .unwrap_or(record.cutex_session_id.as_str());
            cmd_session_online_with_profile(
                id,
                resolved_launch_profile
                    .as_ref()
                    .map(|profile| profile.requested.as_str()),
                false,
            )?;
            let store = load_cutex_session_store()?;
            let key = cutex_session_key_for_user_id(&store, id)
                .ok_or_else(|| anyhow!("cutex session disappeared after session.online: {id}"))?;
            store.sessions.get(&key).cloned().ok_or_else(|| {
                anyhow!("cutex session disappeared after app-server startup: {key}")
            })?
        };
    let layout = host_foreground_app_server_layout(&record)?.ok_or_else(|| {
        anyhow!(
            "session.online did not provide a live manager-owned app-server for {}",
            record.cutex_session_id
        )
    })?;

    let launch_cwd = cwd_override.unwrap_or_else(|| cutex_session_launch_cwd(&record));
    std::env::set_current_dir(launch_cwd)
        .with_context(|| format!("Failed to enter visible TUI cwd: {launch_cwd}"))?;
    println!("{DIM}cwd{RESET} {}", compact_home_path(launch_cwd));
    let default_account = if resolved_launch_profile.is_none() {
        // Current name of the launch profile (bindings keep their
        // launch-time name across a profile rename).
        let profile = record
            .app_server_runtime
            .as_ref()
            .and_then(|binding| binding.current_launched_profile())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "managed runtime occurrence profile is unknown; use an explicit offline/online cutover before opening a new TUI peer"
                )
            })?;
        Some(super::launch::prepare_account_for_launch(profile.trim())?)
    } else {
        None
    };
    let account = resolved_launch_profile
        .as_ref()
        .map(|profile| &profile.account)
        .or(default_account.as_ref())
        .expect("launch profile or durable session profile");
    let launch = super::management_lifecycle::remote_tui_launch_command_with_profile(
        &record,
        account,
        &layout,
        resolved_launch_profile
            .as_ref()
            .map(|profile| &profile.files),
    )?;
    println!(
        "CLI binary: {BOLD}{}{RESET}",
        cutex::launch::program::cli_program(&account.cli_kind)
    );
    let exit_code = launch
        .to_command()
        .status()
        .context("Failed to attach visible TUI to manager-owned app-server")?
        .code()
        .unwrap_or(1);
    super::session_attach::foreground_status(exit_code)
}

fn host_foreground_app_server_layout(
    record: &CutexSessionRecord,
) -> anyhow::Result<Option<AppServerRuntimeLayout>> {
    let Some(binding) = record.app_server_runtime.as_ref() else {
        return Ok(None);
    };
    if !process_is_running(binding.pid) {
        return Ok(None);
    }
    let layout = AppServerRuntimeLayout::from_binding(binding)
        .context("Failed to reconstruct the manager-owned app-server endpoint")?;
    if !layout.endpoint_ready() {
        anyhow::bail!(
            "manager-owned app-server process {} is running but endpoint {} is unavailable",
            binding.pid,
            binding.endpoint
        );
    }
    Ok(Some(layout))
}

#[cfg(test)]
mod tests {
    use super::{launch_profile_from_payload, runtime_close_is_complete, LifecycleResponseOutput};
    use serde_json::json;

    #[test]
    fn managed_foreground_preserves_saved_cwd_and_profile_intent() {
        let record = cutex::session::model::CutexSessionRecord::new(
            "cutex.route-test".into(),
            Some("route-test".into()),
            "local".into(),
            "/saved/work".into(),
            None,
        )
        .unwrap();
        assert!(
            super::validate_managed_launch_overrides(&record, Some("/saved/work"), None).is_ok()
        );
        let error = super::validate_managed_launch_overrides(&record, Some("/other/work"), None)
            .unwrap_err();
        assert!(error.to_string().contains("human config set"));
        let error = super::validate_managed_launch_overrides(&record, None, Some("other-profile"))
            .unwrap_err();
        assert!(error.to_string().contains("session profile set"));
    }

    #[test]
    fn owner_stop_cli_uses_root_credential_without_bus_fallback() {
        let mut config = cutex::profiles::model::CodezConfig {
            management_api_token: Some(cutex::profiles::model::ManagementApiToken::new(
                "private-root",
            )),
            agent_bus_token: Some("ordinary-bus".into()),
            ..Default::default()
        };
        for method in ["cutex/runtime/offline", "cutex/runtime/close"] {
            assert_eq!(
                super::lifecycle_request_credential(&config, method, Some("ordinary-bus".into()))
                    .unwrap(),
                Some("private-root".into())
            );
        }
        assert_eq!(
            super::lifecycle_request_credential(
                &config,
                "cutex/runtime/online",
                Some("ordinary-bus".into())
            )
            .unwrap(),
            Some("ordinary-bus".into())
        );
        config.management_api_token = None;
        assert!(super::lifecycle_request_credential(
            &config,
            "cutex/runtime/offline",
            Some("ordinary-bus".into())
        )
        .is_err());
    }

    #[test]
    fn lifecycle_launch_profile_payload_is_optional_and_trimmed() {
        assert_eq!(
            launch_profile_from_payload(&json!({})).expect("missing profile"),
            None
        );
        assert_eq!(
            launch_profile_from_payload(&json!({ "launch_profile": " beta " })).expect("profile"),
            Some("beta".to_string())
        );
        assert!(launch_profile_from_payload(&json!({ "launchProfile": " " })).is_err());
        assert!(launch_profile_from_payload(&json!({ "launchProfile": 3 })).is_err());
    }

    #[test]
    fn close_wait_recognizes_pending_and_terminal_statuses() {
        assert!(!runtime_close_is_complete(&json!({
            "cutex": { "result": { "status": "closing" } }
        }))
        .expect("closing status"));
        assert!(runtime_close_is_complete(&json!({
            "cutex": { "result": { "status": "closed" } }
        }))
        .expect("closed status"));
        assert!(runtime_close_is_complete(&json!({
            "cutex": { "result": { "status": "offline" } }
        }))
        .expect("offline status"));
        assert!(runtime_close_is_complete(&json!({
            "cutex": { "result": { "status": "error" } }
        }))
        .is_err());
        assert!(runtime_close_is_complete(&json!({})).is_err());
    }

    #[test]
    fn lifecycle_output_is_visible_for_cli_and_suppressed_for_embedded_tui() {
        assert!(LifecycleResponseOutput::Print.should_print());
        assert!(!LifecycleResponseOutput::Suppress.should_print());
    }
}
