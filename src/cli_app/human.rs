//! Human commands use existing local Management credentials, with no login ceremony.
use super::management_control_plane::ManagementControlClient;
use anyhow::{ensure, Context};
use cutex::agent_management::AgentActionId;
use cutex::app_server::external_recovery::{RecoveryRequest, RecoveryReview, REPEAT_WARNING};
use cutex::cli::args::{HumanCommand, HumanConfigCommand};
use cutex::role_revision::CutexSessionId;

pub(super) fn resolve_id(selector: &str) -> anyhow::Result<String> {
    let store = cutex::session::store::load_cutex_session_store()?;
    if store.sessions.contains_key(selector) {
        return Ok(selector.into());
    }
    let native: Vec<_> = store
        .sessions
        .values()
        .filter(|r| r.codex_session_id.as_deref() == Some(selector))
        .collect();
    if native.len() == 1 {
        return Ok(native[0].cutex_session_id.clone());
    }
    let index = cutex::short_ids::Index::new(store.sessions.values().map(|record| {
        cutex::short_ids::Entry {
            id: record.cutex_session_id.clone(),
            name: record
                .formal_agent_name
                .iter()
                .chain(record.display_name_hint.iter())
                .find(|name| name.eq_ignore_ascii_case(selector.trim()))
                .cloned()
                .or_else(|| record.formal_agent_name.clone()),
        }
    }));
    index
        .resolve(selector, &index, cutex::short_ids::settings().minimum())
        .map_err(|error| anyhow::anyhow!("{}", error.to_json()))
}

pub(super) fn recover(
    id: &str,
    action_id: Option<String>,
) -> anyhow::Result<cutex::agent_management::HumanRuntimeRecovery> {
    let id =
        CutexSessionId::new(resolve_id(id)?).map_err(|_| anyhow::anyhow!("invalid agent ID"))?;
    let action = AgentActionId::new(
        action_id.unwrap_or_else(|| format!("human-recovery-{}", uuid::Uuid::new_v4())),
    )?;
    ManagementControlClient::connect()?.recover_runtime(id, action)
}

pub(super) fn run_command(command: HumanCommand) -> anyhow::Result<()> {
    match command {
        HumanCommand::Peer { command } => {
            use cutex::cli::args::PeerCommand;
            use serde_json::json;
            let (method, path, body) = match command {
                PeerCommand::ExportAndStrip { out } => {
                    let report = cutex::peers::strip::export_and_strip(out.as_deref())?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    return Ok(());
                }
                PeerCommand::Invite { label } => (
                    "POST",
                    "/v2/peers/invites".to_string(),
                    Some(json!({"label":label})),
                ),
                PeerCommand::Add { url, code, label } => (
                    "POST",
                    "/v2/peers".to_string(),
                    Some(json!({"url":url,"code":code,"label":label})),
                ),
                PeerCommand::List => ("GET", "/v2/peers".to_string(), None),
                PeerCommand::Remove { peer_id } => ("DELETE", format!("/v2/peers/{peer_id}"), None),
            };
            let bytes = body.map(|b| serde_json::to_vec(&b)).transpose()?;
            let value: serde_json::Value = ManagementControlClient::connect()?
                .request_with_timeout(
                    method,
                    &path,
                    bytes.as_deref(),
                    std::time::Duration::from_secs(30),
                )?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        HumanCommand::Claude { command } => {
            use cutex::cli::args::ClaudeCommand;
            use cutex::runtime::claude;
            let result = match command {
                ClaudeCommand::Install { file } => {
                    let deployment: claude::ClaudeDeployment =
                        serde_json::from_slice(&std::fs::read(file)?)?;
                    deployment.install()?;
                    serde_json::json!({"installed":true})
                }
                ClaudeCommand::New {
                    name,
                    cwd,
                    model,
                    allow_write,
                    allow_exec,
                    profile,
                } => {
                    let cwd = cwd.unwrap_or(std::env::current_dir()?);
                    serde_json::to_value(match profile {
                        None => claude::create(&name, &cwd, &model, allow_write, allow_exec)?,
                        Some(profile) => {
                            claude::create_profile(&name, &cwd, &model, allow_write.then_some(true), allow_exec.then_some(true), Some(profile), true, None)?
                        }
                    })?
                }
                ClaudeCommand::Account { command } => super::claude_account_cli::run(command)?,
                ClaudeCommand::Start { id } => claude::start(&resolve_id(&id)?)?,
                ClaudeCommand::Status { id } => claude::status(&resolve_id(&id)?)?,
                ClaudeCommand::Close { id } => claude::close(&resolve_id(&id)?)?,
                ClaudeCommand::Attach { id } => {
                    claude::attach(&resolve_id(&id)?)?;
                    return Ok(());
                }
                ClaudeCommand::Repin {
                    id,
                    all,
                    deployment,
                    dry_run,
                } => super::claude_repin::run(
                    id.as_deref().map(resolve_id).transpose()?.as_deref(),
                    all,
                    deployment.as_deref(),
                    dry_run,
                )?,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        HumanCommand::StatusAnimation { file, install } => {
            let bytes = std::fs::read(file)?;
            ensure!(
                bytes.len() <= 1_048_576,
                "animation configuration exceeds 1 MiB"
            );
            let spec: serde_json::Value = serde_json::from_slice(&bytes)?;
            let animation = cutex::launch::status_animation::compile(&spec)?;
            if install {
                let mut config = cutex::config::store::load_codez_config_checked()?;
                config.custom_status_items =
                    cutex::profiles::materialize::normalize_custom_status_items(
                        &config.custom_status_items,
                    );
                let item = config
                    .custom_status_items
                    .iter_mut()
                    .find(|i| i.id == "cutex_welcome")
                    .context("Welcome item missing")?;
                item.animation = Some(spec);
                cutex::config::store::save_codez_config(&config)?;
                eprintln!("Saved Cutex Welcome animation; reopen the foreground TUI to apply.");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"version":1,"items":[{"id":"cutex_welcome","text":"旅途愉快","style":{},"animation":animation}]})
                )?
            );
        }
        HumanCommand::Hosts { command } => {
            use cutex::cli::args::HostsCommand;
            use cutex::management::connections::Hosts;
            match command {
                HostsCommand::Sessions { id, query, cursor } => println!(
                    "{}",
                    serde_json::to_string_pretty(&super::remote_sessions::list(
                        &super::remote_sessions::connection(&id)?,
                        &query,
                        cursor.as_deref()
                    )?)?
                ),
                HostsCommand::Online { id, session } => println!(
                    "{}",
                    serde_json::to_string_pretty(&super::remote_sessions::lifecycle(
                        &super::remote_sessions::connection(&id)?,
                        &session,
                        false
                    )?)?
                ),
                HostsCommand::Close { id, session } => println!(
                    "{}",
                    serde_json::to_string_pretty(&super::remote_sessions::lifecycle(
                        &super::remote_sessions::connection(&id)?,
                        &session,
                        true
                    )?)?
                ),
                HostsCommand::Foreground { id, session } => {
                    super::remote_sessions::foreground(
                        &super::remote_sessions::connection(&id)?,
                        &session,
                    )?;
                }
                HostsCommand::List => {
                    println!("{}", serde_json::to_string_pretty(&Hosts::load()?)?)
                }
                HostsCommand::Configure { file } => {
                    let hosts: Hosts = serde_json::from_slice(&std::fs::read(file)?)?;
                    hosts.save()?;
                    println!("Saved host connections");
                }
                HostsCommand::Test { id } => {
                    let hosts = Hosts::load()?;
                    let c = hosts
                        .connections
                        .iter()
                        .find(|c| c.id == id)
                        .context("Connection ID not found")?;
                    c.verified_endpoint()?;
                    println!(
                        "{}: connected; host identity {} verified",
                        c.name, c.host_id
                    );
                }
            }
        }
        HumanCommand::Doctor {
            id,
            check,
            fix,
            yes,
        } => super::human_diagnostics::run(id.as_deref(), check, fix, yes)?,
        HumanCommand::New { name, cwd } => {
            let cwd = cwd.unwrap_or(std::env::current_dir()?);
            let result = super::light_new::create(&name, &cwd.to_string_lossy())?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        HumanCommand::InstallRuntime {
            bundle_manifest,
            source_home,
            job_descriptor,
        } => {
            use cutex::launch::local_deployment::LocalDeployment;
            let prior = LocalDeployment::selected()?;
            let deployment = LocalDeployment {
                bundle_manifest: bundle_manifest.canonicalize()?,
                native_home: source_home
                    .unwrap_or(cutex::config::paths::host_codex_home_dir()?)
                    .canonicalize()?,
                job_mcp: match job_descriptor {
                    Some(path) => Some(serde_json::from_slice(&std::fs::read(path)?)?),
                    None => prior.and_then(|p| p.job_mcp),
                },
            };
            deployment.install()?;
            println!("{}", serde_json::to_string_pretty(&deployment)?);
        }
        HumanCommand::Config { command } => run_config(command)?,
        HumanCommand::Action { action_id, resume } => {
            let client = ManagementControlClient::connect()?;
            let action_id = AgentActionId::new(action_id)?;
            let result = client.runtime_action_status(action_id.clone())?;
            if resume {
                let receipt = result
                    .receipt
                    .context("no runtime start receipt to resume")?;
                print_runtime_result(client.run_stock_runtime(action_id, receipt.review)?)?;
            } else {
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
        }
        HumanCommand::Recover { id, action_id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&recover(&id, action_id)?)?
            );
        }
        HumanCommand::ExternalInput { command } => run_external_input(command)?,
        HumanCommand::BusMessages { command } => run_bus_messages(command)?,
        HumanCommand::Project { command } => super::human_project::run(command)?,
        HumanCommand::Stop { id, force } => {
            stop(&resolve_id(&id)?, force)?;
        }
        HumanCommand::Restart { id, force } => {
            let id = resolve_id(&id)?;
            if force || is_claude(&id)? {
                stop(&id, force)?;
                run_command(HumanCommand::Start { id })?;
            } else {
                print_runtime_result(super::stock_lifecycle::online(&id, true)?)?;
            }
        }
        HumanCommand::Attach { id } => {
            let id = resolve_id(&id)?;
            if is_claude(&id)? {
                cutex::runtime::claude::attach(&id)?;
            } else {
                super::stock_lifecycle::attach(&id)?;
            }
        }
        HumanCommand::Start { id } => {
            let id = resolve_id(&id)?;
            let store = cutex::session::store::load_cutex_session_store()?;
            let record = store.sessions.get(&id).context("agent disappeared")?;
            if record.claude_session.is_some() {
                let result = super::claude_runtime::lifecycle(&id, true)?;
                cutex::asks::wake::note_owner_start(&id);
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            ensure!(
                record.explicit_launch.is_some(),
                "this agent uses the legacy launcher; use session online until upgraded"
            );
            print_runtime_result(super::stock_lifecycle::online(&id, false)?)?;
        }
        HumanCommand::Session { command } => super::session::run_command(command)?,
        HumanCommand::Management { command } => super::management::run_command(command)?,
    }
    Ok(())
}

fn run_external_input(command: cutex::cli::args::HumanExternalInputCommand) -> anyhow::Result<()> {
    match command {
        cutex::cli::args::HumanExternalInputCommand::List { id, all } => {
            let result = cutex::app_server::external_recovery::list(&resolve_id(&id)?, all)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        cutex::cli::args::HumanExternalInputCommand::Analyze { id, older_than } => {
            let owner = resolve_backlog_owner(&id)?;
            let stale_after_secs = parse_backlog_age(&older_than)?;
            let candidates =
                cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
                    .analyze_backlog(&owner, stale_after_secs, cutex::platform::now_epoch_secs())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "cutexSessionId": owner,
                    "staleAfterSecs": stale_after_secs,
                    "shown": candidates.len(),
                    "candidates": candidates,
                }))?
            );
        }
        cutex::cli::args::HumanExternalInputCommand::Release {
            id,
            message_id,
            semantic_sha256,
            confirm_release,
        } => {
            ensure!(
                confirm_release,
                "pass --confirm-release only after analyzing this exact message"
            );
            let owner = resolve_backlog_owner(&id)?;
            let released = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
                .release_stale_held(&owner, &message_id, &semantic_sha256, chrono::Utc::now())?;
            let config = cutex::config::store::load_codez_config();
            let redriven = cutex::agent_bus::client::AgentBusHttpClient::from_config(&config)
                .redrive_durable_messages()
                .context("message was released durably but live redrive failed; the next Agent Bus restart will retry it")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "released": released,
                    "messageId": message_id,
                    "redriven": redriven,
                }))?
            );
        }
        cutex::cli::args::HumanExternalInputCommand::Discard {
            id,
            message_id,
            semantic_sha256,
            reason,
            confirm_discard,
        } => {
            ensure!(
                confirm_discard,
                "pass --confirm-discard only after analyzing this exact message"
            );
            let owner = resolve_backlog_owner(&id)?;
            let discarded = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
                .discard_stale_held(
                    &owner,
                    &message_id,
                    &semantic_sha256,
                    &reason,
                    chrono::Utc::now(),
                )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "discarded": discarded,
                    "messageId": message_id,
                    "reason": reason,
                }))?
            );
        }
        command => {
            let client = ManagementControlClient::connect()?;
            match command {
                cutex::cli::args::HumanExternalInputCommand::Review { id, message_id } => {
                    let result = client.native_recovery(&RecoveryRequest::Review {
                        cutex_session_id: resolve_id(&id)?,
                        message_id,
                    })?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                cutex::cli::args::HumanExternalInputCommand::Retry {
                    id,
                    message_id,
                    action_id,
                    retry_id,
                    confirm_repeat,
                } => {
                    ensure!(
                        confirm_repeat,
                        "{} Pass --confirm-repeat only after reviewing the exact message.",
                        REPEAT_WARNING
                    );
                    let fresh = client.native_recovery(&RecoveryRequest::Review {
                        cutex_session_id: resolve_id(&id)?,
                        message_id: message_id.clone(),
                    })?;
                    let review: RecoveryReview = serde_json::from_value(fresh)
                        .context("native recovery review returned an invalid receipt")?;
                    let action_id = action_id.unwrap_or_else(|| {
                        format!("human-external-input-retry-{}", uuid::Uuid::new_v4())
                    });
                    let retry_id = retry_id.unwrap_or_else(|| format!("{}-retry", action_id));
                    eprintln!("Action: {}", action_id);
                    eprintln!("Retry ID: {}", retry_id);
                    eprintln!("Warning: {}", REPEAT_WARNING);
                    let result = client.native_recovery(&RecoveryRequest::Confirm {
                        action_id,
                        retry_id,
                        review,
                        confirm_repeat: true,
                    })?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                cutex::cli::args::HumanExternalInputCommand::Status { action_id } => {
                    let result = client.native_recovery(&RecoveryRequest::Status { action_id })?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                cutex::cli::args::HumanExternalInputCommand::List { .. } => {
                    unreachable!("the read-only local list branch returned before connecting")
                }
                cutex::cli::args::HumanExternalInputCommand::Analyze { .. }
                | cutex::cli::args::HumanExternalInputCommand::Release { .. }
                | cutex::cli::args::HumanExternalInputCommand::Discard { .. } => {
                    unreachable!("the backlog branch returned before connecting")
                }
            }
        }
    }
    Ok(())
}

/// `cutex human bus-messages`: the Management route
/// `/v2/agent-bus/stuck-messages` (the record change and its event must
/// happen in the management process).
fn run_bus_messages(command: cutex::cli::args::HumanBusMessagesCommand) -> anyhow::Result<()> {
    use cutex::cli::args::HumanBusMessagesCommand;
    let client = ManagementControlClient::connect()?;
    match command {
        HumanBusMessagesCommand::List {
            min_age,
            agent,
            limit,
            json,
        } => {
            let min_age_secs = if min_age.trim() == "0" {
                0
            } else {
                parse_backlog_age(&min_age)?
            };
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("minAgeSecs", &min_age_secs.to_string());
            query.append_pair("limit", &limit.to_string());
            if let Some(agent) = agent {
                query.append_pair("agentId", &resolve_backlog_owner(&agent)?);
            }
            let view = client.stuck_bus_messages(&query.finish())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print_stuck_bus_messages(&view);
            }
        }
        HumanBusMessagesCommand::Discard {
            message_id,
            reason,
            expected_sha256,
            allow_protected,
        } => {
            let mut body = serde_json::json!({
                "messageId": message_id,
                "reason": reason,
                "allowProtected": allow_protected,
            });
            if let Some(sha) = expected_sha256 {
                body["expectedSemanticSha256"] = serde_json::json!(sha);
            }
            let result = client.discard_bus_message(&body)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }
    Ok(())
}

fn print_stuck_bus_messages(view: &serde_json::Value) {
    use serde_json::Value;
    let messages = view["messages"].as_array().cloned().unwrap_or_default();
    if view["liveQueue"]["available"] != true {
        println!(
            "live bus queue unavailable: {}",
            view["liveQueue"]["error"]
                .as_str()
                .unwrap_or("unknown error")
        );
    }
    if messages.is_empty() {
        println!(
            "no stuck agent-bus messages (minimum age {} s)",
            view["minAgeSecs"].as_u64().unwrap_or(0)
        );
        return;
    }
    let name = |party: &Value| {
        party["name"]
            .as_str()
            .or_else(|| party["id"].as_str())
            .unwrap_or("-")
            .to_string()
    };
    for message in &messages {
        let age = message["ageSecs"]
            .as_u64()
            .map(|secs| match secs {
                s if s >= 86_400 => format!("{}d", s / 86_400),
                s if s >= 3_600 => format!("{}h", s / 3_600),
                s => format!("{}m", s / 60),
            })
            .unwrap_or_else(|| "?".into());
        let live = message["liveQueues"]
            .as_array()
            .map(|queues| queues.len())
            .unwrap_or(0);
        println!(
            "{}  {}  age {}  {} -> {}  kind {}{}{}",
            message["messageId"].as_str().unwrap_or("-"),
            message["state"].as_str().unwrap_or("-"),
            age,
            name(&message["from"]),
            name(&message["to"]),
            message["senderKind"].as_str().unwrap_or("-"),
            message["controlType"]
                .as_str()
                .map(|control| format!(" ({control})"))
                .unwrap_or_default(),
            if live > 0 { "  [live queue]" } else { "" },
        );
        for reason in message["reasons"].as_array().into_iter().flatten() {
            println!("    - {}", reason.as_str().unwrap_or_default());
        }
        if let Some(preview) = message["contentPreview"].as_str() {
            let line = preview.split_whitespace().collect::<Vec<_>>().join(" ");
            println!(
                "    \"{}\"",
                cutex::agent_bus::audit::content_preview(&line, 120)
            );
        }
    }
    let total = view["total"].as_u64().unwrap_or(messages.len() as u64);
    if total > messages.len() as u64 {
        println!("({} of {total} shown; raise --limit)", messages.len());
    }
}

fn resolve_backlog_owner(selector: &str) -> anyhow::Result<String> {
    match resolve_id(selector) {
        Ok(owner) => Ok(owner),
        Err(_)
            if selector.starts_with("cutex.")
                && selector.len() <= 256
                && selector.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
                }) =>
        {
            Ok(selector.to_string())
        }
        Err(error) => Err(error),
    }
}

fn parse_backlog_age(value: &str) -> anyhow::Result<u64> {
    let value = value.trim();
    ensure!(!value.is_empty(), "stale age must not be empty");
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, unit) = value.split_at(split);
    let amount = amount.parse::<u64>().context("invalid stale age")?;
    ensure!(amount > 0, "stale age must be positive");
    let multiplier = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => anyhow::bail!("stale age unit must be s, m, h, d, or w"),
    };
    amount
        .checked_mul(multiplier)
        .context("stale age is too large")
}

fn is_claude(id: &str) -> anyhow::Result<bool> {
    Ok(cutex::session::store::load_cutex_session_store()?
        .sessions
        .get(id)
        .context("agent disappeared")?
        .runtime_backend
        == cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription)
}

fn stop(id: &str, force: bool) -> anyhow::Result<()> {
    if is_claude(id)? {
        super::claude_runtime::lifecycle(id, false)?;
        return Ok(());
    }
    if force {
        super::session_runtime::cmd_session_lifecycle_action(id, "session.close", true)?;
    }
    super::session_runtime::cmd_session_close_and_wait_quiet(id)?;
    println!(
        "{}",
        serde_json::json!({"cutex_session_id":id,"state":"stopped"})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use cutex::cli::args::{Cli, CommandKind};
    #[test]
    fn human_stop_and_restart_parse_without_agent_identity_or_task_flags() {
        assert!(matches!(
            Cli::try_parse_from(["cutex", "human", "action", "existing-action", "--resume"])
                .unwrap()
                .command,
            Some(CommandKind::Human {
                command: HumanCommand::Action { resume: true, .. }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cutex", "human", "restart", "worker", "--force"])
                .unwrap()
                .command,
            Some(CommandKind::Human {
                command: HumanCommand::Restart { force: true, .. }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cutex", "human", "stop", "worker"])
                .unwrap()
                .command,
            Some(CommandKind::Human {
                command: HumanCommand::Stop { force: false, .. }
            })
        ));
        // r219: the Task Service is gone, and with it task cancellation on
        // stop / restart and `human tasks`.
        for args in [
            &["cutex", "human", "stop", "worker", "--cancel-tasks"][..],
            &[
                "cutex",
                "human",
                "restart",
                "worker",
                "--cancel-tasks",
                "--force",
            ][..],
            &["cutex", "human", "tasks", "list"][..],
            &[
                "cutex",
                "human",
                "tasks",
                "reassign",
                "old-assignment",
                "new-worker",
                "--action-id",
                "reassign-1",
            ][..],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn human_external_input_recovery_commands_parse_explicit_boundary() {
        assert!(matches!(
            Cli::try_parse_from([
                "cutex",
                "human",
                "external-input",
                "list",
                "worker",
                "--all"
            ])
            .unwrap()
            .command,
            Some(CommandKind::Human {
                command: HumanCommand::ExternalInput {
                    command: cutex::cli::args::HumanExternalInputCommand::List { all: true, .. }
                }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "cutex",
                "human",
                "external-input",
                "retry",
                "worker",
                "message",
                "--confirm-repeat"
            ])
            .unwrap()
            .command,
            Some(CommandKind::Human {
                command: HumanCommand::ExternalInput {
                    command: cutex::cli::args::HumanExternalInputCommand::Retry {
                        confirm_repeat: true,
                        ..
                    }
                }
            })
        ));
        assert!(Cli::try_parse_from([
            "cutex",
            "human",
            "external-input",
            "retry",
            "worker",
            "message"
        ])
        .is_err());
    }
}

fn run_config(command: HumanConfigCommand) -> anyhow::Result<()> {
    use cutex::agent_management::HumanConfigRequest;
    let id = |value: &str| {
        CutexSessionId::new(resolve_id(value)?).map_err(|_| anyhow::anyhow!("invalid agent ID"))
    };
    let action = |value: Option<String>| {
        AgentActionId::new(
            value.unwrap_or_else(|| format!("human-config-{}", uuid::Uuid::new_v4())),
        )
    };
    let request = match command {
        HumanConfigCommand::Show { id: selector } => HumanConfigRequest::Show {
            cutex_session_id: id(&selector)?,
        },
        HumanConfigCommand::Set {
            id: selector,
            fields,
            action_id,
        } => {
            let mut patch = std::collections::BTreeMap::new();
            for field in fields {
                let (key, value) = field
                    .split_once('=')
                    .context("configuration must be KEY=VALUE or KEY=null")?;
                let value = if value == "null" {
                    None
                } else if matches!(key, "cwd" | "bundle_manifest") {
                    Some(
                        std::path::Path::new(value)
                            .canonicalize()?
                            .to_string_lossy()
                            .into_owned(),
                    )
                } else {
                    Some(value.to_string())
                };
                ensure!(
                    patch.insert(key.to_string(), value).is_none(),
                    "duplicate configuration field: {key}"
                );
            }
            HumanConfigRequest::Set {
                cutex_session_id: id(&selector)?,
                action_id: action(action_id)?,
                patch,
            }
        }
        HumanConfigCommand::Undo {
            id: selector,
            original_action_id,
            action_id,
        } => HumanConfigRequest::Undo {
            cutex_session_id: id(&selector)?,
            action_id: action(action_id)?,
            original_action_id: AgentActionId::new(original_action_id)?,
        },
    };
    match &request {
        HumanConfigRequest::Set { action_id, .. } | HumanConfigRequest::Undo { action_id, .. } => {
            eprintln!("Action: {}", action_id.as_str())
        }
        _ => {}
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&ManagementControlClient::connect()?.human_config(&request)?)?
    );
    Ok(())
}

fn print_runtime_result(
    receipt: cutex::agent_management::StockRuntimeReceipt,
) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    ensure!(receipt.stage == cutex::agent_management::StockRuntimeStage::Ready && receipt.error.is_none(),
        "{}; inspect with cutex human action {}",
        receipt.error.as_deref().unwrap_or("runtime readiness incomplete"),receipt.action_id.as_str());
    Ok(())
}
