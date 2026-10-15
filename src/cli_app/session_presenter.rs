use cutex::runtime::alden::CuteAldenSession;
use cutex::session::model::CutexSessionRecord;
use cutex::session::model::CutexSessionStore;
use cutex::session::projection::cutex_session_cwd_summary;
use cutex::session::projection::cutex_session_filter_note;
use cutex::session::projection::runtime_backend_short_label;
use cutex::session::projection::CutexSessionChoiceRow;
use cutex::session::projection::CutexSessionCwdSummary;
use cutex::session::projection::CutexSessionFilterNote;
use cutex::session::projection::CutexSessionListFilter;
use cutex::session::projection::CutexSessionListRow;
use cutex::session::projection::StartQuickAction;
use cutex::session::service::cutex_session_display_name;
use cutex::session::service::cutex_session_is_managed;
use cutex::session::service::cutex_session_launch_cwd;
use cutex::ui::format::bool_label;
use cutex::ui::format::compact_home_path;
use cutex::ui::format::truncate_end;
use cutex::ui::format::truncate_middle;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

pub(super) struct StartSessionMenuRow {
    pub enabled_marker: Option<bool>,
    pub label: String,
}

pub(super) fn print_start_wizard_menu(quick_actions: &[StartQuickAction]) -> usize {
    println!();
    println!("{BOLD}{CYAN}cutex Start{RESET}");
    for (idx, action) in quick_actions.iter().enumerate() {
        println!(
            "  {BOLD}{:>2}{RESET}. {} {CYAN}{}{RESET}  {DIM}{}{RESET}",
            idx + 1,
            action.kind.start_menu_label(),
            truncate_end(&action.display_name, 28),
            action.reason
        );
    }
    let base = quick_actions.len();
    println!(
        "  {BOLD}{:>2}{RESET}. Choose managed/recent session",
        base + 1
    );
    println!(
        "  {BOLD}{:>2}{RESET}. Adopt recent/local session into management",
        base + 2
    );
    println!(
        "  {BOLD}{:>2}{RESET}. Start new throwaway session",
        base + 3
    );
    println!(
        "  {BOLD}{:>2}{RESET}. Choose profile and start throwaway session",
        base + 4
    );
    println!("  {BOLD}{:>2}{RESET}. Manage sessions", base + 5);
    base
}

pub(super) fn print_start_session_menu(
    record: &CutexSessionRecord,
    id: &str,
    status: &str,
    rows: &[StartSessionMenuRow],
) -> anyhow::Result<()> {
    println!();
    println!(
        "{BOLD}{CYAN}Start Session{RESET} {BOLD}{}{RESET}",
        cutex_session_display_name(record)
    );
    println!(
        "{DIM}id={} status={} profile={} backend={} host={} groups={}{RESET}",
        id,
        status,
        record.profile.as_deref().unwrap_or("-"),
        runtime_backend_short_label(record.runtime_backend),
        record.host_id,
        groups_label(record)
    );
    print_session_cwd_summary(record)?;
    for (idx, row) in rows.iter().enumerate() {
        let marker = row
            .enabled_marker
            .map(checkbox)
            .unwrap_or_else(|| "   ".to_string());
        println!("  {}. {} {}", idx + 1, marker, row.label);
    }
    Ok(())
}

pub(super) fn print_adopt_session_menu(
    record: &CutexSessionRecord,
    id: &str,
    already_managed: bool,
) -> anyhow::Result<()> {
    println!();
    println!(
        "{BOLD}{CYAN}Adopt Session{RESET} {BOLD}{}{RESET}",
        cutex_session_display_name(record)
    );
    println!(
        "{DIM}id={} managed={} backend={} im={} groups={}{RESET}",
        id,
        bool_label(already_managed),
        runtime_backend_short_label(record.runtime_backend),
        bool_label(record.exposed_to_backend),
        groups_label(record)
    );
    print_session_cwd_summary(record)?;
    println!("  1.     adopt as managed using session cwd");
    println!("  2.     adopt as managed using current cwd");
    println!("  3.     adopt + expose to IM using session cwd");
    println!("  4.     adopt + expose to IM using current cwd");
    println!("  5.     edit/manage after adoption");
    println!("  6.     choose another session");
    Ok(())
}

pub(super) fn print_choose_session_menu(
    hidden_count: usize,
    filter: &CutexSessionListFilter,
    rows: &[CutexSessionChoiceRow],
) {
    println!();
    println!("{BOLD}{CYAN}Choose Session{RESET}");
    print_session_filter_note(hidden_count, filter);
    for (idx, row) in rows.iter().enumerate() {
        let managed = if row.has_managed_cwd {
            " managed-cwd"
        } else {
            ""
        };
        println!(
            "  {BOLD}{:>2}{RESET}. {CYAN}{}{RESET}  {DIM}{} {} {}{} cwd={}{RESET}",
            idx + 1,
            truncate_end(&row.display_name, 28),
            row.status,
            row.backend,
            row.scope,
            managed,
            compact_home_path(&row.launch_cwd),
        );
    }
}

pub(super) fn print_session_edit_menu(record: &CutexSessionRecord, id: &str, status: &str) {
    println!();
    println!(
        "{BOLD}{CYAN}Session Wizard{RESET} {BOLD}{}{RESET}",
        cutex_session_display_name(record)
    );
    println!(
        "{DIM}id={} status={} profile={} backend={} visible={} quick={} groups={}{RESET}",
        id,
        status,
        record.profile.as_deref().unwrap_or("-"),
        runtime_backend_short_label(record.runtime_backend),
        bool_label(record.exposed_to_backend),
        record.quick_action.label(),
        groups_label(record)
    );
    println!(
        "  1.     show raw session record               {}",
        wizard_value(&record.cutex_session_id)
    );
    println!(
        "  2.     show cwd                              {}",
        wizard_value(compact_home_path(cutex_session_launch_cwd(record)))
    );
    println!("  3.     set managed cwd to current directory");
    println!("  4.     set managed cwd manually");
    println!("  5.     clear managed cwd");
    println!(
        "  6.     edit runtime defaults                 {}",
        wizard_value(runtime_backend_short_label(record.runtime_backend))
    );
    println!(
        "  7.     set groups                            {}",
        wizard_value(groups_label(record))
    );
    println!(
        "  8. {} cutex managed session                 {}",
        checkbox(cutex_session_is_managed(record)),
        if cutex_session_is_managed(record) {
            "managed"
        } else {
            "local/recent"
        }
    );
    println!(
        "  9. {} expose to IM/backend                   {}",
        checkbox(record.exposed_to_backend),
        bool_label(record.exposed_to_backend)
    );
    println!(
        " 10.     set start quick action mode           {}",
        wizard_value(record.quick_action.label())
    );
    println!(" 11.     online managed runtime");
    println!(" 12.     offline managed runtime");
    println!(" 13.     close managed runtime");
    println!(" 14.     attach/takeover TUI when available");
    println!(" 15.     choose another session");
}

pub(super) fn print_cutex_sessions_table(
    hidden_count: usize,
    filter: &CutexSessionListFilter,
    rows: &[CutexSessionListRow],
) {
    println!("{BOLD}{CYAN}cutex sessions{RESET}");
    print_session_filter_note(hidden_count, filter);
    println!(
        "  {DIM}{:<11} {:<22} {:<9} {:<10} {:<10} {:<24} {:<28} {RESET}",
        "status", "name", "scope", "profile", "backend", "codex", "cwd"
    );
    for row in rows {
        print_cutex_session_list_record(row);
    }
    if rows.is_empty() {
        println!("  {DIM}<none>{RESET}");
    }
}

pub(super) fn print_cute_alden_sessions_table(
    sessions: &[CuteAldenSession],
    store: &CutexSessionStore,
) {
    println!("{BOLD}{CYAN}cute-alden runtime sessions{RESET}");
    println!(
        "  {DIM}{:<8} {:<46} {:<22} {RESET}",
        "pid", "name", "linked"
    );
    for session in sessions {
        print_cute_alden_list_record(session, store);
    }
}

pub(super) fn print_session_cwd_summary(record: &CutexSessionRecord) -> anyhow::Result<()> {
    let summary = cutex_session_cwd_summary(record)?;
    print_session_cwd_summary_fields(&summary);
    Ok(())
}

pub(super) fn print_session_cwd_summary_fields(summary: &CutexSessionCwdSummary) {
    println!(
        "  {DIM}session cwd{RESET}   {}",
        compact_home_path(&summary.session_cwd)
    );
    println!(
        "  {DIM}current cwd{RESET}   {}",
        compact_home_path(&summary.current_cwd)
    );
    println!(
        "  {DIM}managed cwd{RESET}   {}",
        summary
            .managed_cwd
            .as_deref()
            .map(compact_home_path)
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "  {DIM}launch cwd{RESET}    {}",
        compact_home_path(&summary.effective_launch_cwd)
    );
}

fn print_session_filter_note(hidden_count: usize, filter: &CutexSessionListFilter) {
    match cutex_session_filter_note(hidden_count, filter) {
        Some(CutexSessionFilterNote::DefaultHidden { hidden_count }) => {
            println!(
                "{DIM}Showing managed/IM-visible sessions and attachable runtimes. {hidden_count} historical/local sessions hidden; use --all to include them or adopt one into management.{RESET}"
            );
        }
        None => {}
    }
}

fn print_cutex_session_list_record(row: &CutexSessionListRow) {
    let cwd = compact_home_path(&row.cwd);
    println!(
        "  {:<11} {:<22} {:<9} {:<10} {:<10} {:<24} {:<28}",
        row.status,
        truncate_end(&row.display_name, 22),
        row.scope,
        truncate_end(&row.profile, 10),
        row.backend,
        truncate_middle(&row.codex_session_id, 24),
        truncate_middle(&cwd, 28),
    );
    if let Some(name) = row.attach_session_name.as_deref() {
        println!("    {GREEN}attach{RESET} cutex session attach --name {name}");
        println!("    {YELLOW}takeover{RESET} cutex session attach --name {name} --takeover");
    }
    if let Some(managed_cwd) = row.managed_cwd.as_deref() {
        println!(
            "    {DIM}managed cwd{RESET} {}",
            compact_home_path(managed_cwd)
        );
    }
}

fn print_cute_alden_list_record(session: &CuteAldenSession, store: &CutexSessionStore) {
    let name = session.name.as_deref().unwrap_or("-");
    let linked = session.name.as_deref().and_then(|name| {
        store
            .sessions
            .values()
            .find(|record| record.alden_session_name.as_deref() == Some(name))
    });
    let linked_label = linked
        .map(cutex_session_display_name)
        .unwrap_or_else(|| "-".to_string());
    println!(
        "  {:<8} {:<46} {:<22}",
        session.pid,
        truncate_middle(name, 46),
        truncate_end(&linked_label, 22)
    );
    if session.name.is_some() {
        println!("    {GREEN}attach{RESET} cutex session attach --name {name}");
        println!("    {YELLOW}takeover{RESET} cutex session attach --name {name} --takeover");
    }
}

fn groups_label(record: &CutexSessionRecord) -> String {
    if record.agent_groups.is_empty() {
        "-".to_string()
    } else {
        record.agent_groups.join(",")
    }
}

fn checkbox(value: bool) -> String {
    if value {
        format!("{GREEN}[x]{RESET}")
    } else {
        format!("{DIM}[ ]{RESET}")
    }
}

fn wizard_value(value: impl AsRef<str>) -> String {
    let value = value.as_ref();
    if value.is_empty() || value == "-" {
        format!("{DIM}-{RESET}")
    } else {
        format!("{BOLD}{value}{RESET}")
    }
}
