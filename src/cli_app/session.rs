use cutex::cli::args::{
    SessionCommand, SessionCwdCommand, SessionDefaultsCommand, SessionGroupsCommand,
    SessionListArgs, SessionProfileCommand, SessionQuickCommand,
};
use cutex::session::model::{
    CutexSessionQuickActionMode, CutexSessionRecord, CutexSessionUserAction,
};
use cutex::session::projection::CutexSessionListFilter;

pub(crate) fn run_command(command: SessionCommand) -> anyhow::Result<()> {
    match command {
        SessionCommand::Stock {
            request,
            management_url,
        } => super::stock_lifecycle::request(&request, &management_url),
        SessionCommand::CloseNative { id, socket } => {
            let result = cutex::catalog::native_archive::change(&id, false, socket.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        SessionCommand::RestoreNative { id, socket } => {
            let result = cutex::catalog::native_archive::change(&id, true, socket.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        SessionCommand::StockAttach { id } => super::stock_lifecycle::attach(&id),
        SessionCommand::Wizard { list } => cmd_session_wizard(&list),
        SessionCommand::List { list } => cmd_session_list(&list),
        SessionCommand::Show { id } => cmd_session_show(&id),
        SessionCommand::Retired { json } => super::session_archive::cmd_session_retired(json),
        SessionCommand::Retire { id, reason, json } => {
            super::session_archive::cmd_session_retire(&id, reason.as_deref(), json)
        }
        SessionCommand::Restore { id, json } => {
            super::session_archive::cmd_session_restore(&id, json)
        }
        SessionCommand::RepairHistory { id, json } => cmd_session_repair_history(&id, json),
        SessionCommand::Adopt {
            id,
            name,
            cwd,
            current_cwd,
            groups,
            expose_to_im,
            pin,
        } => cmd_session_adopt(&id, name, cwd, current_cwd, groups, expose_to_im, pin),
        SessionCommand::Expose { id, name, groups } => {
            cmd_session_expose(&id, name.as_deref(), groups)
        }
        SessionCommand::Hide { id } => cmd_session_hide(&id),
        SessionCommand::Unmanage { id } => cmd_session_unmanage(&id),
        SessionCommand::Quick { command } => cmd_session_quick(command),
        SessionCommand::Groups { command } => cmd_session_groups(command),
        SessionCommand::Profile { command } => cmd_session_profile(command),
        SessionCommand::Defaults { command } => cmd_session_defaults(command),
        SessionCommand::Cwd { command } => cmd_session_cwd(command),
        SessionCommand::Online { id, profile } => {
            cmd_session_online_with_profile(&id, profile.as_deref(), true).map(|_| ())
        }
        SessionCommand::Foreground { id, profile } => {
            cmd_session_foreground_with_profile(&id, profile.as_deref())
        }
        SessionCommand::Offline { id, force } => {
            cmd_session_lifecycle_action(&id, "session.offline", force)
        }
        SessionCommand::Close { id, force } => {
            cmd_session_lifecycle_action(&id, "session.close", force)
        }
        SessionCommand::Attach { name, takeover } => {
            super::session_attach::cmd_session_attach(&name, takeover)
        }
        SessionCommand::Takeover { id } => cmd_session_takeover(&id),
        SessionCommand::DuplicateCheck { id, json } => cmd_session_duplicate_check(&id, json),
    }
}

pub(crate) fn start_wizard(list: &SessionListArgs) -> anyhow::Result<()> {
    super::session_wizard::cmd_start_wizard(list)
}

pub(crate) fn cmd_session_wizard(list: &SessionListArgs) -> anyhow::Result<()> {
    super::session_wizard::cmd_session_wizard(list)
}

pub(crate) fn record_cutex_session_user_action(
    id: &str,
    action: CutexSessionUserAction,
) -> anyhow::Result<()> {
    super::session_runtime::record_cutex_session_user_action(id, action)
}

pub(crate) fn cmd_session_takeover(id: &str) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_takeover(id)
}

pub(crate) fn cmd_session_resume_alden(id: &str) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_resume_alden(id)
}

pub(crate) fn cmd_session_resume_alden_with_profile(
    id: &str,
    launch_profile: Option<&str>,
) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_resume_alden_with_profile(id, launch_profile)
}

pub(crate) fn cmd_session_duplicate_check(id: &str, json: bool) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_duplicate_check(id, json)
}

pub(crate) fn repair_interrupted_history(
    id: &str,
) -> anyhow::Result<cutex::runtime::codex_home::InterruptedHistoryRepair> {
    let store = cutex::session::store::load_cutex_session_store()?;
    let key = cutex::session::service::cutex_session_key_for_user_id(&store, id)
        .ok_or_else(|| anyhow::anyhow!("cutex session is not known: {id}"))?;
    let record = store
        .sessions
        .get(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared: {key}"))?;
    let session_id = record
        .codex_session_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cutex session has no native thread id: {key}"))?;
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &cutex::platform::host::current_host_name(),
    ) {
        anyhow::bail!(
            "history repair requires the runtime's local host: {}",
            record.host_id
        );
    }
    let live_pid = [
        record.runtime_pid,
        record.alden_pid,
        record
            .app_server_runtime
            .as_ref()
            .map(|binding| binding.pid),
    ]
    .into_iter()
    .flatten()
    .find(|pid| cutex::platform::process::process_is_running(*pid));
    if let Some(pid) = live_pid {
        anyhow::bail!(
            "history repair requires the Agent to be offline; runtime pid {pid} is still running"
        );
    }
    let config = cutex::config::store::load_codez_config();
    if cutex::agent_bus::client::agent_bus_fetch_agents_if_healthy(&config)
        .iter()
        .any(|agent| agent.session_id.as_deref() == Some(session_id))
    {
        anyhow::bail!("history repair requires the Agent to be offline; Agent Bus is still live");
    }

    cutex::runtime::codex_home::repair_interrupted_rollout_history(session_id)
}

fn cmd_session_repair_history(id: &str, json: bool) -> anyhow::Result<()> {
    let repair = repair_interrupted_history(id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "rolloutPath": repair.rollout_path,
                "backupPath": repair.backup_path,
                "repairedTurnIds": repair.repaired_turn_ids,
                "normalizedOrdinals": repair.normalized_ordinals,
            }))?
        );
    } else if repair.repaired_turn_ids.is_empty() && !repair.normalized_ordinals {
        println!(
            "No interrupted history defects found in {}",
            repair.rollout_path.display()
        );
    } else if repair.repaired_turn_ids.is_empty() {
        println!(
            "Normalized the interrupted history ordinal suffix in {}",
            repair.rollout_path.display()
        );
        if let Some(backup_path) = repair.backup_path {
            println!("Backup: {}", backup_path.display());
        }
    } else {
        println!(
            "Repaired {} orphaned turn(s){} in {}",
            repair.repaired_turn_ids.len(),
            if repair.normalized_ordinals {
                " and normalized the broken ordinal suffix"
            } else {
                ""
            },
            repair.rollout_path.display()
        );
        if let Some(backup_path) = repair.backup_path {
            println!("Backup: {}", backup_path.display());
        }
    }
    Ok(())
}

pub(crate) fn cmd_session_lifecycle_action(
    id: &str,
    action_type: &str,
    force: bool,
) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_lifecycle_action(id, action_type, force)
}

pub(crate) fn cmd_session_online_with_profile(
    id: &str,
    launch_profile: Option<&str>,
    open_visible_terminal: bool,
) -> anyhow::Result<serde_json::Value> {
    super::session_runtime::cmd_session_online_with_profile(
        id,
        launch_profile,
        open_visible_terminal,
    )
}

pub(crate) fn cmd_session_close_and_restart_with_profile(
    id: &str,
    launch_profile: Option<&str>,
    open_visible_terminal: bool,
) -> anyhow::Result<serde_json::Value> {
    super::session_runtime::cmd_session_close_and_restart_with_profile(
        id,
        launch_profile,
        open_visible_terminal,
    )
}

pub(crate) fn cmd_session_close_and_wait(id: &str) -> anyhow::Result<serde_json::Value> {
    super::session_runtime::cmd_session_close_and_wait(id)
}

pub(crate) fn cmd_session_close_and_wait_quiet(id: &str) -> anyhow::Result<serde_json::Value> {
    super::session_runtime::cmd_session_close_and_wait_quiet(id)
}

pub(crate) fn cmd_session_foreground(id: &str) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_foreground(id)
}

pub(crate) fn cmd_session_foreground_with_profile(
    id: &str,
    launch_profile: Option<&str>,
) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_foreground_with_profile(id, launch_profile)
}

pub(crate) fn cmd_session_resume_foreground(
    record: &CutexSessionRecord,
    cwd_override: Option<&str>,
) -> anyhow::Result<()> {
    super::session_runtime::cmd_session_resume_foreground(record, cwd_override)
}

pub(crate) fn cmd_session_list(list: &SessionListArgs) -> anyhow::Result<()> {
    super::session_listing::cmd_session_list(list)
}

pub(crate) fn cutex_session_list_filter_from_args(
    list: &SessionListArgs,
) -> CutexSessionListFilter {
    super::session_listing::cutex_session_list_filter_from_args(list)
}

pub(crate) fn cmd_session_show(id: &str) -> anyhow::Result<()> {
    super::session_management::cmd_session_show(id)
}

pub(crate) fn cmd_session_adopt(
    id: &str,
    name: Option<String>,
    cwd: Option<String>,
    current_cwd: bool,
    groups: Vec<String>,
    expose_to_im: bool,
    pin: bool,
) -> anyhow::Result<()> {
    super::session_management::cmd_session_adopt(
        id,
        name,
        cwd,
        current_cwd,
        groups,
        expose_to_im,
        pin,
    )
}

pub(crate) fn cmd_session_expose(
    id: &str,
    name: Option<&str>,
    groups: Vec<String>,
) -> anyhow::Result<()> {
    super::session_management::cmd_session_expose(id, name, groups)
}

pub(crate) fn cmd_session_hide(id: &str) -> anyhow::Result<()> {
    super::session_management::cmd_session_hide(id)
}

pub(crate) fn cmd_session_unmanage(id: &str) -> anyhow::Result<()> {
    super::session_management::cmd_session_unmanage(id)
}

pub(crate) fn cmd_session_quick(command: SessionQuickCommand) -> anyhow::Result<()> {
    super::session_management::cmd_session_quick(command)
}

pub(crate) fn cmd_session_quick_set(
    id: &str,
    mode: CutexSessionQuickActionMode,
) -> anyhow::Result<()> {
    super::session_management::cmd_session_quick_set(id, mode)
}

pub(crate) fn cmd_session_groups(command: SessionGroupsCommand) -> anyhow::Result<()> {
    super::session_management::cmd_session_groups(command)
}

pub(crate) fn cmd_session_profile(command: SessionProfileCommand) -> anyhow::Result<()> {
    super::session_management::cmd_session_profile(command)
}

pub(crate) fn cmd_session_defaults(command: SessionDefaultsCommand) -> anyhow::Result<()> {
    super::session_settings::cmd_session_defaults(command)
}

pub(crate) fn cmd_session_defaults_edit(id: &str) -> anyhow::Result<()> {
    super::session_settings::cmd_session_defaults_edit(id)
}

pub(crate) fn cmd_session_cwd(command: SessionCwdCommand) -> anyhow::Result<()> {
    super::session_settings::cmd_session_cwd(command)
}
