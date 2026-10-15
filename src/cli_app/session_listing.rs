use cutex::cli::args::{SessionListArgs, SessionListSort};
use cutex::runtime::alden::{cute_alden_sessions, CuteAldenSession};
use cutex::session::projection::{
    cutex_session_list_row_with_agents, filtered_cutex_session_records_with_agents,
    CutexSessionListFilter,
    CutexSessionListSort,
};
use cutex::session::store::load_cutex_session_store;

use super::session_presenter;
use super::session_reconcile;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";

pub(crate) fn cmd_session_list(list: &SessionListArgs) -> anyhow::Result<()> {
    let alden_sessions = match cute_alden_sessions() {
        Ok(sessions) => Some(sessions),
        Err(err) if list.alden => return Err(err),
        Err(err) => {
            println!("{DIM}cute-alden sessions unavailable: {err:#}{RESET}");
            None
        }
    };

    if !list.alden {
        session_reconcile::mirror_im_registry_into_cutex_session_store(
            &cutex::im::registry::load_im_registry()?,
        )?;
        let store = load_cutex_session_store()?;
        if !store.sessions.values().any(|record| record.is_active()) {
            println!("{DIM}No durable cutex sessions are known.{RESET}");
        } else {
            let empty_alden_sessions: [CuteAldenSession; 0] = [];
            let alden_slice = alden_sessions.as_deref().unwrap_or(&empty_alden_sessions);
            let filter = cutex_session_list_filter_from_args(list);
            // Host and app-server agents are online only by the Agent Bus view;
            // without it every running agent showed as `stale`.
            let live_agents = cutex::agent_bus::client::agent_bus_fetch_agents_if_healthy(
                &cutex::config::store::load_codez_config(),
            );
            let (records, hidden_count) = filtered_cutex_session_records_with_agents(
                &store,
                alden_slice,
                &filter,
                &live_agents,
            );
            let rows = records
                .iter()
                .map(|(_, record)| {
                    cutex_session_list_row_with_agents(record, alden_slice, &live_agents)
                })
                .collect::<Vec<_>>();
            session_presenter::print_cutex_sessions_table(hidden_count, &filter, &rows);
        }
    }

    match alden_sessions {
        Some(sessions) if !sessions.is_empty() => {
            let store = (!list.alden)
                .then(load_cutex_session_store)
                .transpose()?
                .unwrap_or_default();
            session_presenter::print_cute_alden_sessions_table(&sessions, &store);
        }
        Some(_) if list.alden => {
            println!("{DIM}No cute-alden runtime sessions are known.{RESET}");
        }
        Some(_) | None => {}
    }
    Ok(())
}

pub(crate) fn cutex_session_list_filter_from_args(
    list: &SessionListArgs,
) -> CutexSessionListFilter {
    CutexSessionListFilter {
        all: list.all,
        offline: list.offline,
        one_shot: list.one_shot,
        host: list.host,
        attachable: list.attachable,
        projects: list.projects.clone(),
        groups: list.groups.clone(),
        sort: match list.sort {
            SessionListSort::Status => CutexSessionListSort::Status,
            SessionListSort::Recent => CutexSessionListSort::Recent,
            SessionListSort::Name => CutexSessionListSort::Name,
            SessionListSort::Project => CutexSessionListSort::Project,
        },
    }
}
