use anyhow::anyhow;
use chrono::Utc;

use cutex::agent_bus::groups::{apply_group_update, normalize_registered_agent_groups};
use cutex::agent_bus::identity::{default_agent_group_for, normalize_agent_groups};
use cutex::agent_bus::model::{AgentGroupUpdateMode, AgentRegistrationClass};
use cutex::agent_bus::routing::agent_sender_label;
use cutex::cli::args::{ImCommand, ImGroupsCommand};
use cutex::im::registry::{load_im_registry, save_im_registry, CodingSessionRegistration};
use cutex::management::service::management_base_url;
use cutex::management::v2::repository::management_v2_repository;
use cutex::management::v2::session::session_resource_including_hidden;
use cutex::platform::host::current_host_name;
use cutex::session::service::cutex_session_key_for_user_id;
use cutex::session::store::load_cutex_session_store;
use cutex::ui::format::bool_label;

use super::agent_bus_runtime;
use super::agent_context;
use super::profile::active_profile_name;

pub(crate) fn run_command(command: ImCommand) -> anyhow::Result<()> {
    match command {
        ImCommand::Register {
            session_id,
            name,
            host,
            cwd,
            profile,
            groups,
            temporary,
        } => cmd_im_register(&session_id, name, host, cwd, profile, groups, temporary),
        ImCommand::RegisterCurrent {
            name,
            groups,
            temporary,
        } => cmd_im_register_current(name, groups, temporary),
        ImCommand::Unregister { session_id } => cmd_im_unregister(&session_id),
        ImCommand::UnregisterCurrent => cmd_im_unregister_current(),
        ImCommand::List => cmd_im_list(),
        ImCommand::Show { session_id } => cmd_im_show(&session_id),
        ImCommand::StatusCurrent => cmd_im_status_current(),
        ImCommand::Groups { command } => cmd_im_groups(command),
    }
}

fn cmd_im_register(
    session_id: &str,
    name: Option<String>,
    host: Option<String>,
    cwd: Option<String>,
    profile: Option<String>,
    groups: Vec<String>,
    temporary: bool,
) -> anyhow::Result<()> {
    let session_id = agent_context::normalize_session_id(session_id)?;
    let mut registry = load_im_registry()?;
    let now = Utc::now().to_rfc3339();
    let existing = registry.sessions.get(&session_id).cloned();
    let cwd = cwd
        .filter(|value| !value.trim().is_empty())
        .or_else(|| existing.as_ref().map(|entry| entry.cwd.clone()))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".to_string())
        });
    let host_id = host
        .filter(|value| !value.trim().is_empty())
        .or_else(|| existing.as_ref().map(|entry| entry.host_id.clone()))
        .unwrap_or_else(current_host_name);
    let profile = profile
        .filter(|value| !value.trim().is_empty())
        .or_else(|| existing.as_ref().and_then(|entry| entry.profile.clone()))
        .or_else(active_profile_name);
    let display_name = name
        .filter(|value| !value.trim().is_empty())
        .or_else(|| existing.as_ref().map(|entry| entry.display_name.clone()))
        .unwrap_or_else(|| session_id.clone());
    let groups = if groups.is_empty() {
        existing
            .as_ref()
            .map(|entry| entry.groups.clone())
            .filter(|groups| !groups.is_empty())
            .unwrap_or_else(|| vec![default_agent_group_for(None, &cwd)])
    } else {
        normalize_registered_agent_groups(groups, None, &cwd)
    };
    let registration_class = if temporary {
        AgentRegistrationClass::Ephemeral
    } else {
        AgentRegistrationClass::Persistent
    };
    let created_at = existing
        .as_ref()
        .map(|entry| entry.created_at.clone())
        .unwrap_or_else(|| now.clone());
    let last_runtime_agent_id = agent_bus_runtime::maybe_patch_live_agent_groups(
        &session_id,
        &groups,
        AgentGroupUpdateMode::Set,
    )
    .ok()
    .flatten()
    .or_else(|| {
        existing
            .as_ref()
            .and_then(|entry| entry.last_runtime_agent_id.clone())
    });
    let entry = CodingSessionRegistration {
        session_id: session_id.clone(),
        display_name,
        host_id,
        cwd,
        profile,
        groups,
        registration_class,
        visible: true,
        created_at,
        updated_at: now,
        last_runtime_agent_id,
    };
    registry.sessions.insert(session_id.clone(), entry.clone());
    save_im_registry(&registry)?;
    if let Err(err) = cutex::session::service::reconcile_cutex_session_from_im_registration(&entry)
    {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to reconcile cutex session registry: {err:#}");
    }
    println!(
        "\x1b[32mRegistered\x1b[0m session \x1b[1m{}\x1b[0m name={} class={} groups={}",
        entry.session_id,
        entry.display_name,
        entry.registration_class.label(),
        entry.groups.join(",")
    );
    Ok(())
}

fn cmd_im_register_current(
    name: Option<String>,
    groups: Vec<String>,
    temporary: bool,
) -> anyhow::Result<()> {
    let agent = agent_context::current_live_agent()?;
    let session_id = agent
        .session_id
        .as_deref()
        .ok_or_else(|| anyhow!("Current live agent has no CODEX_THREAD_ID/session_id"))?
        .to_string();
    let display_name = name
        .filter(|value| !value.trim().is_empty())
        .or_else(|| agent.thread_name.clone())
        .unwrap_or_else(|| agent_sender_label(&agent));
    let groups = if groups.is_empty() {
        agent.groups.clone()
    } else {
        groups
    };
    cmd_im_register(
        &session_id,
        Some(display_name),
        Some(current_host_name()),
        Some(agent.cwd),
        Some(agent.profile),
        groups,
        temporary,
    )
}

fn cmd_im_unregister(session_id: &str) -> anyhow::Result<()> {
    let session_id = agent_context::normalize_session_id(session_id)?;
    let mut registry = load_im_registry()?;
    let entry = registry
        .sessions
        .get_mut(&session_id)
        .ok_or_else(|| anyhow!("IM session is not registered: {session_id}"))?;
    entry.visible = false;
    entry.updated_at = Utc::now().to_rfc3339();
    let entry_snapshot = entry.clone();
    save_im_registry(&registry)?;
    if let Err(err) =
        cutex::session::service::reconcile_cutex_session_from_im_registration(&entry_snapshot)
    {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to reconcile cutex session registry: {err:#}");
    }
    println!("\x1b[33mUnregistered\x1b[0m session \x1b[1m{session_id}\x1b[0m from IM/workbench");
    Ok(())
}

fn cmd_im_unregister_current() -> anyhow::Result<()> {
    let agent = agent_context::current_live_agent()?;
    let session_id = agent
        .session_id
        .as_deref()
        .ok_or_else(|| anyhow!("Current live agent has no CODEX_THREAD_ID/session_id"))?
        .to_string();
    cmd_im_unregister(&session_id)
}

fn cmd_im_list() -> anyhow::Result<()> {
    let registry = load_im_registry()?;
    if registry.sessions.is_empty() {
        println!("\x1b[2mNo IM coding sessions are registered.\x1b[0m");
        return Ok(());
    }
    let mut sessions = registry.sessions.values().collect::<Vec<_>>();
    sessions.sort_by(|a, b| {
        b.visible
            .cmp(&a.visible)
            .then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    println!("\x1b[1m\x1b[36mcutex IM coding sessions\x1b[0m");
    for session in sessions {
        println!(
            "\x1b[1m{}\x1b[0m  \x1b[2msession={} visible={} class={} groups={} host={} cwd={} updated={}\x1b[0m",
            session.display_name,
            session.session_id,
            bool_label(session.visible),
            session.registration_class.label(),
            if session.groups.is_empty() {
                "-".to_string()
            } else {
                session.groups.join(",")
            },
            session.host_id,
            session.cwd,
            session.updated_at
        );
    }
    Ok(())
}

fn cmd_im_show(session_id: &str) -> anyhow::Result<()> {
    let session_id = agent_context::normalize_session_id(session_id)?;
    let registry = load_im_registry()?;
    let entry = registry
        .sessions
        .get(&session_id)
        .ok_or_else(|| anyhow!("IM session is not registered: {session_id}"))?;
    println!("{}", serde_json::to_string_pretty(entry)?);
    Ok(())
}

fn cmd_im_status_current() -> anyhow::Result<()> {
    let (_config, agent, _agents) = agent_context::current_live_agent_context()?;
    let session_id = agent.session_id.clone();
    let registry = load_im_registry()?;
    let registration = session_id
        .as_deref()
        .and_then(|session_id| registry.sessions.get(session_id))
        .cloned();
    let management_v2_session = match session_id.as_deref() {
        Some(session_id) => {
            let store = load_cutex_session_store()?;
            let cutex_session_id = cutex_session_key_for_user_id(&store, session_id)
                .and_then(|key| store.sessions.get(&key))
                .map(|record| record.cutex_session_id.clone());
            match cutex_session_id {
                Some(cutex_session_id) => session_resource_including_hidden(
                    &cutex_session_id,
                    &registry,
                    super::management_context::load_app_server_runtime_status,
                    management_v2_repository()?,
                )?,
                None => None,
            }
        }
        None => None,
    };
    let payload = serde_json::json!({
        "current_agent_id": agent.id,
        "current_agent_name": agent.name,
        "current_agent_base_name": agent.base_name,
        "current_thread_name": agent.thread_name,
        "current_agent_path_key": agent.path_key,
        "session_id": session_id,
        "profile": agent.profile,
        "cwd": agent.cwd,
        "groups": agent.groups,
        "registration_class": agent.registration_class,
        "im_registered": registration.is_some(),
        "im_registration": registration,
        "management_v2_session": management_v2_session,
        "management_url": management_base_url(cutex::management::service::management_port(&cutex::config::store::load_codez_config())),
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn cmd_im_groups(command: ImGroupsCommand) -> anyhow::Result<()> {
    match command {
        ImGroupsCommand::Set { session_id, groups } => {
            cmd_im_groups_update(&session_id, groups, AgentGroupUpdateMode::Set)
        }
        ImGroupsCommand::Add { session_id, groups } => {
            cmd_im_groups_update(&session_id, groups, AgentGroupUpdateMode::Add)
        }
        ImGroupsCommand::Remove { session_id, groups } => {
            cmd_im_groups_update(&session_id, groups, AgentGroupUpdateMode::Remove)
        }
    }
}

fn cmd_im_groups_update(
    session_id: &str,
    groups: Vec<String>,
    mode: AgentGroupUpdateMode,
) -> anyhow::Result<()> {
    let session_id = agent_context::normalize_session_id(session_id)?;
    let groups = normalize_agent_groups(groups);
    if groups.is_empty() {
        anyhow::bail!("At least one non-empty group is required");
    }
    let mut registry = load_im_registry()?;
    let entry = registry
        .sessions
        .get_mut(&session_id)
        .ok_or_else(|| anyhow!("IM session is not registered: {session_id}"))?;
    entry.groups = apply_group_update(&entry.groups, &groups, mode);
    entry.updated_at = Utc::now().to_rfc3339();
    entry.last_runtime_agent_id = agent_bus_runtime::maybe_patch_live_agent_groups(
        &session_id,
        &entry.groups,
        AgentGroupUpdateMode::Set,
    )
    .ok()
    .flatten()
    .or_else(|| entry.last_runtime_agent_id.clone());
    let entry_snapshot = entry.clone();
    let groups_label = entry.groups.join(",");
    save_im_registry(&registry)?;
    if let Err(err) =
        cutex::session::service::reconcile_cutex_session_from_im_registration(&entry_snapshot)
    {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to reconcile cutex session registry: {err:#}");
    }
    println!("\x1b[32mUpdated\x1b[0m IM groups for \x1b[1m{session_id}\x1b[0m: {groups_label}");
    Ok(())
}
