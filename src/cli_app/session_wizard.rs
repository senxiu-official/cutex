use std::io;
use std::io::Write;

use anyhow::anyhow;
use anyhow::Context;

use cutex::agent_bus::client::agent_bus_fetch_agents_if_healthy;
use cutex::agent_bus::model::AgentBusAgent;
use cutex::cli::args::SessionCwdCommand;
use cutex::cli::args::SessionGroupsCommand;
use cutex::cli::args::SessionListArgs;
use cutex::cli::args::SessionListSort;
use cutex::config::store::load_codez_config;
use cutex::im::registry::load_im_registry;
use cutex::runtime::alden::cute_alden_sessions;
use cutex::session::model::parse_cutex_session_quick_action_mode;
use cutex::session::model::CutexSessionUserAction;
use cutex::session::projection::cutex_session_choice_rows_with_agents;
use cutex::session::projection::cutex_session_has_live_native_agent;
use cutex::session::projection::cutex_session_is_attachable;
use cutex::session::projection::cutex_session_status_label_with_agents;
use cutex::session::projection::filtered_cutex_session_records_with_agents;
use cutex::session::projection::recommended_start_quick_actions_with_agents;
use cutex::session::projection::StartQuickAction;
use cutex::session::projection::StartQuickActionKind;
use cutex::session::service::cutex_session_is_managed;
use cutex::session::service::cutex_session_launch_cwd;
use cutex::session::store::load_cutex_session_store;

use super::prompt::parse_optional_csv;
use super::prompt::prompt_line;
use super::prompt::read_wizard_choice;
use super::session;
use super::session_attach;
use super::session_presenter;
use super::session_reconcile;
use super::session_start_menu::start_session_menu_choices;
use super::session_start_menu::StartSessionMenuAction;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

fn live_agents_for_session_wizard() -> Vec<AgentBusAgent> {
    let config = load_codez_config();
    agent_bus_fetch_agents_if_healthy(&config)
}

pub(crate) fn cmd_start_wizard(list: &SessionListArgs) -> anyhow::Result<()> {
    loop {
        session_reconcile::mirror_im_registry_into_cutex_session_store(&load_im_registry()?)?;
        let store = load_cutex_session_store()?;
        let alden_sessions = cute_alden_sessions().unwrap_or_default();
        let live_agents = live_agents_for_session_wizard();
        let current_cwd = std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| ".".to_string());
        let quick_actions = recommended_start_quick_actions_with_agents(
            &store,
            &alden_sessions,
            &live_agents,
            &current_cwd,
        );

        let base = session_presenter::print_start_wizard_menu(&quick_actions);

        let Some(choice) = read_wizard_choice(base + 5)? else {
            println!("Done.");
            return Ok(());
        };
        if choice <= base {
            let action = quick_actions[choice - 1].clone();
            if action.kind.opens_detail_first() {
                session_start_wizard_loop(&action.key, list)?;
                continue;
            }
            return execute_start_quick_action(action);
        }
        match choice - base {
            1 => {
                if let Some(key) = choose_session_for_wizard(list)? {
                    session_start_wizard_loop(&key, list)?;
                }
            }
            2 => cmd_session_adopt_wizard(list)?,
            3 => {
                return super::launch::quick_run(
                    Vec::new(),
                    super::launch_output::LaunchOutput::Human,
                    true,
                    false,
                    false,
                    Vec::new(),
                )
            }
            4 => {
                return super::launch::quick_run(
                    Vec::new(),
                    super::launch_output::LaunchOutput::Human,
                    false,
                    false,
                    false,
                    Vec::new(),
                )
            }
            5 => cmd_session_wizard(list)?,
            _ => unreachable!(),
        }
    }
}

fn execute_start_quick_action(action: StartQuickAction) -> anyhow::Result<()> {
    let store = load_cutex_session_store()?;
    let record = store
        .sessions
        .get(&action.key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared while starting: {}", action.key))?;
    if record.explicit_launch.is_some() {
        anyhow::ensure!(
            action.kind == StartQuickActionKind::OpenDetails,
            "explicit stock action changed; reopen the session details"
        );
        return session_start_wizard_loop(&action.key, &SessionListArgs::default());
    }
    session::record_cutex_session_user_action(&action.key, action.kind.user_action())?;
    let id = record
        .codex_session_id
        .as_deref()
        .unwrap_or(record.cutex_session_id.as_str())
        .to_string();

    match action.kind {
        StartQuickActionKind::OpenDetails => {
            session_start_wizard_loop(&action.key, &SessionListArgs::default())
        }
        StartQuickActionKind::Attach => {
            let name = record
                .alden_session_name
                .as_deref()
                .ok_or_else(|| anyhow!("cutex session has no cute-alden session name"))?;
            session_attach::cmd_session_attach(name, false)
        }
        StartQuickActionKind::Takeover => session::cmd_session_takeover(&id),
        StartQuickActionKind::ResumeAttach => session::cmd_session_resume_alden(&id),
        StartQuickActionKind::VisibleTui => {
            let cwd = cutex_session_launch_cwd(&record).to_string();
            session::cmd_session_resume_foreground(&record, Some(cwd.as_str()))
        }
        StartQuickActionKind::Online => {
            session::cmd_session_lifecycle_action(&id, "session.online", false)
        }
        StartQuickActionKind::ResumeHere => session::cmd_session_resume_foreground(&record, None),
        StartQuickActionKind::ResumeManaged => {
            let cwd = cutex_session_launch_cwd(&record).to_string();
            session::cmd_session_resume_foreground(&record, Some(cwd.as_str()))
        }
    }
}

fn session_start_wizard_loop(initial_key: &str, list: &SessionListArgs) -> anyhow::Result<()> {
    let mut key = initial_key.to_string();
    loop {
        let store = load_cutex_session_store()?;
        let record = store
            .sessions
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("cutex session disappeared while starting: {key}"))?;
        let id = record
            .codex_session_id
            .as_deref()
            .unwrap_or(record.cutex_session_id.as_str())
            .to_string();
        let alden_sessions = cute_alden_sessions().unwrap_or_default();
        let live_agents = live_agents_for_session_wizard();
        let status = cutex_session_status_label_with_agents(&record, &alden_sessions, &live_agents);
        let attachable = cutex_session_is_attachable(&record, &alden_sessions);
        let live_native = cutex_session_has_live_native_agent(&record, &live_agents);
        let choices = start_session_menu_choices(&record, attachable, live_native);
        let rows = choices
            .iter()
            .map(|choice| session_presenter::StartSessionMenuRow {
                enabled_marker: choice.row.enabled_marker,
                label: choice.row.label.clone(),
            })
            .collect::<Vec<_>>();
        session_presenter::print_start_session_menu(&record, &id, status, &rows)?;

        let Some(choice) = read_wizard_choice(choices.len())? else {
            println!("Done.");
            return Ok(());
        };
        match choices[choice - 1].action {
            StartSessionMenuAction::ResumeAttach => {
                return session::cmd_session_resume_alden(&id);
            }
            StartSessionMenuAction::Attach => {
                let Some(name) = record.alden_session_name.as_deref().filter(|_| attachable) else {
                    println!("{YELLOW}No attachable cute-alden runtime for this session.{RESET}");
                    continue;
                };
                session::record_cutex_session_user_action(&key, CutexSessionUserAction::Attach)?;
                return session_attach::cmd_session_attach(name, false);
            }
            StartSessionMenuAction::StockAttach => {
                super::stock_lifecycle::attach(&record.cutex_session_id)?;
                session::record_cutex_session_user_action(&key, CutexSessionUserAction::Attach)?;
                return Ok(());
            }
            StartSessionMenuAction::Takeover => {
                if !attachable {
                    println!("{YELLOW}No attachable cute-alden runtime for this session.{RESET}");
                    continue;
                }
                session::record_cutex_session_user_action(&key, CutexSessionUserAction::Takeover)?;
                return session::cmd_session_takeover(&id);
            }
            StartSessionMenuAction::Foreground => {
                let cwd = cutex_session_launch_cwd(&record).to_string();
                session::record_cutex_session_user_action(
                    &key,
                    CutexSessionUserAction::ResumeManaged,
                )?;
                return session::cmd_session_resume_foreground(&record, Some(cwd.as_str()));
            }
            StartSessionMenuAction::Online => {
                session::record_cutex_session_user_action(&key, CutexSessionUserAction::Online)?;
                return session::cmd_session_lifecycle_action(&id, "session.online", false);
            }
            StartSessionMenuAction::ResumeHere => {
                session::record_cutex_session_user_action(
                    &key,
                    CutexSessionUserAction::ResumeHere,
                )?;
                return session::cmd_session_resume_foreground(&record, None);
            }
            StartSessionMenuAction::ResumeManaged => {
                let cwd = cutex_session_launch_cwd(&record).to_string();
                session::record_cutex_session_user_action(
                    &key,
                    CutexSessionUserAction::ResumeManaged,
                )?;
                return session::cmd_session_resume_foreground(&record, Some(cwd.as_str()));
            }
            StartSessionMenuAction::Edit => session_wizard_loop(&key, list)?,
            StartSessionMenuAction::ChooseAnother => {
                if let Some(next_key) = choose_session_for_wizard(list)? {
                    key = next_key;
                } else {
                    println!("Done.");
                    return Ok(());
                }
            }
        }
    }
}

pub(crate) fn cmd_session_wizard(list: &SessionListArgs) -> anyhow::Result<()> {
    loop {
        let Some(key) = choose_session_for_wizard(list)? else {
            println!("Done.");
            return Ok(());
        };
        session_wizard_loop(&key, list)?;
    }
}

fn cmd_session_adopt_wizard(list: &SessionListArgs) -> anyhow::Result<()> {
    let mut adopt_list = list.clone();
    adopt_list.all = true;
    adopt_list.sort = SessionListSort::Recent;
    loop {
        let Some(key) = choose_session_for_wizard(&adopt_list)? else {
            println!("Done.");
            return Ok(());
        };
        session_adopt_wizard_loop(&key, &adopt_list)?;
    }
}

fn session_adopt_wizard_loop(initial_key: &str, list: &SessionListArgs) -> anyhow::Result<()> {
    let mut key = initial_key.to_string();
    loop {
        let store = load_cutex_session_store()?;
        let record = store
            .sessions
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("cutex session disappeared while adopting: {key}"))?;
        let id = record
            .codex_session_id
            .as_deref()
            .unwrap_or(record.cutex_session_id.as_str())
            .to_string();
        let already_managed = cutex_session_is_managed(&record);
        session_presenter::print_adopt_session_menu(&record, &id, already_managed)?;

        let Some(choice) = read_wizard_choice(6)? else {
            println!("Done.");
            return Ok(());
        };
        match choice {
            1 => {
                return session::cmd_session_adopt(&id, None, None, false, Vec::new(), false, false)
            }
            2 => {
                return session::cmd_session_adopt(&id, None, None, true, Vec::new(), false, false)
            }
            3 => {
                return session::cmd_session_adopt(&id, None, None, false, Vec::new(), true, false)
            }
            4 => return session::cmd_session_adopt(&id, None, None, true, Vec::new(), true, false),
            5 => {
                session::cmd_session_adopt(&id, None, None, false, Vec::new(), false, false)?;
                session_wizard_loop(&key, list)?;
            }
            6 => {
                if let Some(next_key) = choose_session_for_wizard(list)? {
                    key = next_key;
                } else {
                    println!("Done.");
                    return Ok(());
                }
            }
            _ => unreachable!(),
        }
    }
}

fn choose_session_for_wizard(list: &SessionListArgs) -> anyhow::Result<Option<String>> {
    session_reconcile::mirror_im_registry_into_cutex_session_store(&load_im_registry()?)?;
    let store = load_cutex_session_store()?;
    if !store.sessions.values().any(|record| record.is_active()) {
        println!("{DIM}No durable cutex sessions are known yet.{RESET}");
        return Ok(None);
    }
    let alden_sessions = cute_alden_sessions().unwrap_or_default();
    let live_agents = live_agents_for_session_wizard();
    let filter = session::cutex_session_list_filter_from_args(list);
    let (records, hidden_count) =
        filtered_cutex_session_records_with_agents(&store, &alden_sessions, &filter, &live_agents);

    let rows = cutex_session_choice_rows_with_agents(&records, &alden_sessions, &live_agents);
    session_presenter::print_choose_session_menu(hidden_count, &filter, &rows);
    if rows.is_empty() {
        println!("{DIM}No sessions match these filters.{RESET}");
        return Ok(None);
    }

    loop {
        print!("{CYAN}Select session number{RESET}, or q to quit: ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let input = line.trim();
        if input.is_empty() || input.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        let choice = input
            .parse::<usize>()
            .with_context(|| format!("Invalid session selection: {input}"))?;
        if choice == 0 || choice > rows.len() {
            eprintln!("{YELLOW}warning:{RESET} session selection out of range: {choice}");
            continue;
        }
        return Ok(Some(rows[choice - 1].key.clone()));
    }
}

fn session_wizard_loop(initial_key: &str, list: &SessionListArgs) -> anyhow::Result<()> {
    let mut key = initial_key.to_string();
    loop {
        let store = load_cutex_session_store()?;
        let record = store
            .sessions
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("cutex session disappeared while editing: {key}"))?;
        let id = record
            .codex_session_id
            .as_deref()
            .unwrap_or(record.cutex_session_id.as_str())
            .to_string();
        let alden_sessions = cute_alden_sessions().unwrap_or_default();
        let live_agents = live_agents_for_session_wizard();
        let status = cutex_session_status_label_with_agents(&record, &alden_sessions, &live_agents);
        session_presenter::print_session_edit_menu(&record, &id, status);

        let Some(choice) = read_wizard_choice(15)? else {
            println!("Done.");
            return Ok(());
        };
        match choice {
            1 => session::cmd_session_show(&id)?,
            2 => session::cmd_session_cwd(SessionCwdCommand::Show { id: id.clone() })?,
            3 => session::cmd_session_cwd(SessionCwdCommand::Current { id: id.clone() })?,
            4 => {
                let current = record
                    .managed_cwd
                    .as_deref()
                    .unwrap_or_else(|| cutex_session_launch_cwd(&record));
                let path = prompt_line("Managed cwd", current)?;
                session::cmd_session_cwd(SessionCwdCommand::Set {
                    id: id.clone(),
                    path,
                })?;
            }
            5 => session::cmd_session_cwd(SessionCwdCommand::Clear { id: id.clone() })?,
            6 => session::cmd_session_defaults_edit(&id)?,
            7 => {
                let current = if record.agent_groups.is_empty() {
                    "-".to_string()
                } else {
                    record.agent_groups.join(",")
                };
                let groups = prompt_line("Groups CSV", &current)?;
                let groups = parse_optional_csv(&groups).unwrap_or_default();
                if groups.is_empty() {
                    println!("{YELLOW}No groups entered; unchanged.{RESET}");
                } else {
                    session::cmd_session_groups(SessionGroupsCommand::Set {
                        id: id.clone(),
                        groups,
                    })?;
                }
            }
            8 => {
                if cutex_session_is_managed(&record) {
                    session::cmd_session_unmanage(&id)?;
                } else {
                    session::cmd_session_adopt(&id, None, None, false, Vec::new(), false, false)?;
                }
            }
            9 => {
                if record.exposed_to_backend {
                    session::cmd_session_hide(&id)?;
                } else {
                    session::cmd_session_expose(&id, None, Vec::new())?;
                }
            }
            10 => {
                let value = prompt_line(
                    "Quick action mode: auto, pinned, hidden",
                    record.quick_action.label(),
                )?;
                let mode = parse_cutex_session_quick_action_mode(&value)?;
                session::cmd_session_quick_set(&id, mode)?;
            }
            11 => session::cmd_session_lifecycle_action(&id, "session.online", false)?,
            12 => session::cmd_session_lifecycle_action(&id, "session.offline", false)?,
            13 => session::cmd_session_lifecycle_action(&id, "session.close", false)?,
            14 => session::cmd_session_takeover(&id)?,
            15 => {
                if let Some(next_key) = choose_session_for_wizard(list)? {
                    key = next_key;
                } else {
                    println!("Done.");
                    return Ok(());
                }
            }
            _ => unreachable!(),
        }
    }
}
