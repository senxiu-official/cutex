use clap::Parser;
use cutex::agent_bus::identity::{merge_agent_groups, normalize_agent_groups};
use cutex::cli::args::{Cli, CommandKind, SessionListArgs, SessionListSort};

use super::{
    agent, auth, launch, launch_output::LaunchOutput, management, notify, profile, root_wizard,
    session, session_tui, settings, usage,
};

pub(crate) fn run() -> anyhow::Result<()> {
    // The workflow script sandbox child of `management serve` (JSON lines on
    // stdio; no config, no home, no profile): handled before any setup.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv == ["workflow", "exec"] {
        return cutex::workflows::exec::run(std::io::stdin().lock(), std::io::stdout().lock());
    }
    // `--version` / `--help` print and exit inside `parse`, before any
    // startup migration below.
    let cli = Cli::parse();
    instance_preflight(&cli)?;
    let migrate = !skips_startup_migrations(&cli);
    // 2026-10-15: decide once per home whether it predates the auto-mode
    // agent default (before any command writes into the home). Fatal only
    // when a FRESH home's decision cannot be recorded (else a later start
    // would pin it to full access).
    let archive_json = session_archive_requests_json(&cli);
    if migrate {
        // A JSON archive request owns stderr (one JSON object): no notice.
        cutex::config::fresh_agent_permissions::run_on_startup("cli", archive_json)?;
    }
    if archive_json {
        super::app_server_runtime::suppress_background_diagnostics();
    }
    if should_open_session_tui(&cli) {
        return session_tui::run();
    }
    root_wizard::set_codez_codex_home(migrate)?;

    match cli.command {
        Some(CommandKind::List) => profile::list()?,
        Some(CommandKind::Current) => profile::current()?,
        Some(CommandKind::Use { target }) => profile::use_profile(&target)?,
        Some(CommandKind::New { profile }) => launch::new_session(profile.as_deref())?,
        Some(CommandKind::Run {
            profile,
            host,
            agent,
            groups,
            docker_image,
            docker_user_name,
            codex_args,
        }) => {
            let output = LaunchOutput::for_child_args(&codex_args);
            launch::print_build(output);
            let agent_groups = merge_agent_groups(cli.groups, groups);
            launch::run_profile(
                &profile,
                codex_args,
                output,
                cli.host || host,
                cli.agent || agent || !agent_groups.is_empty(),
                agent_groups,
                docker_image,
                docker_user_name,
            )?
        }
        Some(CommandKind::Start { list }) => {
            launch::print_build(LaunchOutput::Human);
            session::start_wizard(&list)?
        }
        Some(CommandKind::Tui) => unreachable!("TUI command is handled before runtime setup"),
        Some(CommandKind::Add {
            from_auth,
            from_config,
            docker_image,
            docker_user_name,
            name,
            cli,
        }) => auth::add(
            &from_auth,
            from_config.as_deref(),
            docker_image,
            docker_user_name,
            &name,
            &cli,
        )?,
        Some(CommandKind::Login {
            name,
            cli,
            api_key,
            base_url,
            provider,
        }) => auth::login(
            name.as_deref(),
            cli.as_deref(),
            api_key.as_deref(),
            base_url.as_deref(),
            provider.as_deref(),
        )?,
        Some(CommandKind::Rename { target, name }) => profile::rename(&target, &name)?,
        Some(CommandKind::Remove { target }) => profile::remove(&target)?,
        Some(CommandKind::Annotate {
            target,
            source,
            clear_source,
            plan,
            clear_plan,
            email,
            clear_email,
        }) => profile::annotate(
            &target,
            source,
            clear_source,
            plan,
            clear_plan,
            email,
            clear_email,
        )?,
        Some(CommandKind::Runtime {
            target,
            host,
            docker_image,
            docker_user_name,
        }) => profile::runtime(&target, host, docker_image, docker_user_name)?,
        Some(CommandKind::Profile { command }) => profile::run_command(command)?,
        Some(CommandKind::Global { command }) => settings::global(command)?,
        Some(CommandKind::Usage {
            period,
            group_by,
            since,
            until,
            last,
            reset_window,
            json,
        }) => usage::run(
            period,
            group_by,
            since.as_deref(),
            until.as_deref(),
            last.as_deref(),
            reset_window,
            json,
        )?,
        Some(CommandKind::Proxy { command }) => settings::proxy(command)?,
        Some(CommandKind::Session { command }) => session::run_command(command)?,
        Some(CommandKind::Quota { profile, config, disable, refresh }) => println!("{}", serde_json::to_string_pretty(&cutex::profiles::quota::command(&profile, config.as_deref(), disable, refresh)?)?),
        Some(CommandKind::Alarm { command }) => {
            use cutex::{agent_bus::alarms,cli::args::AlarmCommand};
            let result=match command {
                AlarmCommand::Set {to,message,at,after,id} => alarms::set("human",alarms::SetArgs {
                    alarm_id:id.unwrap_or_else(||uuid::Uuid::new_v4().to_string()),to:alarms::resolve_local(&to)?,message,at,delay_seconds:after})?,
                AlarmCommand::List => alarms::list(None)?,
                AlarmCommand::Cancel {id} => alarms::cancel(None,&id)?,
            };
            println!("{}",serde_json::to_string_pretty(&result)?);
        },
        Some(CommandKind::Notify { command }) => notify::run_command(command)?,
        Some(CommandKind::Im { command }) => agent::im(command)?,
        Some(CommandKind::Management { command }) => management::run_command(command)?,
        Some(CommandKind::Instance { command }) => super::instance_cli::run(command)?,
        Some(CommandKind::Upgrade { command }) => super::upgrade_cli::run_upgrade(command)?,
        Some(CommandKind::Service { command }) => super::upgrade_cli::run_service(command)?,
        Some(CommandKind::Human { command }) => super::human::run_command(command)?,
        Some(CommandKind::Agent { command }) => agent::run_command(command)?,
        Some(CommandKind::Wizard) => launch::wizard()?,
        None => {
            let output = LaunchOutput::for_child_args(&cli.codex_args);
            launch::print_build(output);
            let agent_groups = normalize_agent_groups(cli.groups);
            launch::quick_run(
                cli.codex_args,
                output,
                cli.quick,
                cli.host,
                cli.agent || !agent_groups.is_empty(),
                agent_groups,
            )?
        }
    }

    Ok(())
}

fn session_archive_requests_json(cli: &Cli) -> bool {
    matches!(
        cli.command.as_ref(),
        Some(CommandKind::Session {
            command: cutex::cli::args::SessionCommand::Retire { json: true, .. }
                | cutex::cli::args::SessionCommand::Restore { json: true, .. }
        })
    )
}

fn should_open_session_tui(cli: &Cli) -> bool {
    match cli.command.as_ref() {
        Some(CommandKind::Tui) => true,
        Some(CommandKind::Start { list }) => session_list_args_are_default(list),
        None => {
            !cli.quick
                && !cli.host
                && !cli.agent
                && normalize_agent_groups(cli.groups.clone()).is_empty()
                && cli.codex_args.is_empty()
        }
        Some(_) => false,
    }
}

fn session_list_args_are_default(list: &SessionListArgs) -> bool {
    let SessionListArgs {
        all,
        offline,
        one_shot,
        host,
        alden,
        attachable,
        projects,
        groups,
        sort,
    } = list;
    !*all
        && !*offline
        && !*one_shot
        && !*host
        && !*alden
        && !*attachable
        && projects.is_empty()
        && groups.is_empty()
        && *sort == SessionListSort::Status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("CLI should parse")
    }

    #[test]
    fn read_only_probes_and_the_upgrade_runner_skip_startup_migrations() {
        for probe in [
            &["cutex", "upgrade", "selftest", "--json"][..],
            &["cutex", "upgrade", "run", "upg-0123456789ab"],
            &["cutex", "upgrade", "status", "--json"],
            &["cutex", "upgrade", "history"],
            &["cutex", "service", "status"],
        ] {
            assert!(skips_startup_migrations(&parsed(probe)), "{probe:?}");
        }
        for command in [
            &["cutex", "management", "serve"][..],
            &["cutex", "upgrade", "apply"],
            &["cutex", "list"],
            &["cutex"],
        ] {
            assert!(!skips_startup_migrations(&parsed(command)), "{command:?}");
        }
        // `--version` never reaches them: clap prints and returns first.
        let version = Cli::try_parse_from(["cutex", "--version"]).unwrap_err();
        assert_eq!(version.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    fn assert_non_default(update: impl FnOnce(&mut SessionListArgs)) {
        let mut list = SessionListArgs::default();
        update(&mut list);
        assert!(!session_list_args_are_default(&list));
    }

    #[test]
    fn default_root_tui_and_bare_start_open_the_session_tui() {
        assert!(should_open_session_tui(&parsed(&["cutex"])));
        assert!(should_open_session_tui(&parsed(&["cutex", "tui"])));
        assert!(should_open_session_tui(&parsed(&["cutex", "start"])));
        assert!(should_open_session_tui(&parsed(&[
            "cutex", "start", "--sort", "status",
        ])));
    }

    #[test]
    fn every_non_default_start_list_field_keeps_the_compatibility_picker() {
        assert!(session_list_args_are_default(&SessionListArgs::default()));
        assert_non_default(|list| list.all = true);
        assert_non_default(|list| list.offline = true);
        assert_non_default(|list| list.one_shot = true);
        assert_non_default(|list| list.host = true);
        assert_non_default(|list| list.alden = true);
        assert_non_default(|list| list.attachable = true);
        assert_non_default(|list| list.projects.push("waveline".to_string()));
        assert_non_default(|list| list.groups.push("waveline".to_string()));
        assert_non_default(|list| list.sort = SessionListSort::Recent);
    }

    #[test]
    fn plain_launch_intent_never_opens_the_tui() {
        for invocation in [
            &["cutex", "--quick"][..],
            &["cutex", "--host"][..],
            &["cutex", "--agent"][..],
            &["cutex", "--group", "waveline"][..],
            &["cutex", "--", "--version"][..],
        ] {
            assert!(!should_open_session_tui(&parsed(invocation)));
        }
    }

    #[test]
    fn json_archive_commands_suppress_unrelated_background_diagnostics() {
        assert!(session_archive_requests_json(&parsed(&[
            "cutex",
            "session",
            "retire",
            "session-1",
            "--json"
        ])));
        assert!(session_archive_requests_json(&parsed(&[
            "cutex",
            "session",
            "restore",
            "session-1",
            "--json"
        ])));
        assert!(!session_archive_requests_json(&parsed(&[
            "cutex",
            "session",
            "retire",
            "session-1"
        ])));
    }

    #[test]
    fn filtered_start_and_explicit_wizards_remain_fallback_routes() {
        assert!(!should_open_session_tui(&parsed(&[
            "cutex",
            "start",
            "--attachable",
        ])));
        assert!(!should_open_session_tui(&parsed(&[
            "cutex", "start", "--sort", "recent",
        ])));
        assert!(!should_open_session_tui(&parsed(&["cutex", "wizard",])));
        assert!(!should_open_session_tui(&parsed(&["cutex", "config",])));
        assert!(!should_open_session_tui(&parsed(&[
            "cutex", "session", "wizard",
        ])));
    }
}

/// Independent instance guard (docs/INSTANCES.md §3): every command except
/// `cutex instance init|show` runs only in a valid instance; server commands
/// drop per-agent identity inherited from the shell that started them.
/// Commands that must not run the one-time startup migrations: the
/// read-only probes a STAGED binary answers against the live home during
/// preflight (`upgrade selftest`), the upgrade runner (`upgrade run`, often
/// a staged binary: the new daemon migrates when it starts as
/// `management serve`) and the status / history readers. Startup
/// migrations (`fresh_agent_permissions::run_on_startup`, the legacy
/// `~/.codez-cli` → `~/.cutex` move in `set_codez_codex_home`) would
/// otherwise mutate the live home from a binary that may never be
/// activated. (`--version` exits inside `Cli::parse`, before both.)
pub(crate) fn skips_startup_migrations(cli: &Cli) -> bool {
    use cutex::cli::args::{ServiceCommand, UpgradeCommand};
    matches!(
        &cli.command,
        Some(CommandKind::Upgrade {
            command: UpgradeCommand::Selftest { .. }
                | UpgradeCommand::Run { .. }
                | UpgradeCommand::Status { .. }
                | UpgradeCommand::History { .. }
        }) | Some(CommandKind::Service { command: ServiceCommand::Status })
    )
}

fn instance_preflight(cli: &Cli) -> anyhow::Result<()> {
    use cutex::cli::args::{AgentCommand, InstanceCommand, ManagementCommand};
    let exempt = matches!(
        &cli.command,
        Some(CommandKind::Instance {
            command: InstanceCommand::Init { .. } | InstanceCommand::Show { .. }
        })
    );
    if !exempt {
        cutex::config::instance::preflight()?;
    }
    let server = matches!(
        &cli.command,
        Some(CommandKind::Management { command: ManagementCommand::Serve { .. } })
            | Some(CommandKind::Agent { command: AgentCommand::Serve { .. } })
    );
    if server {
        cutex::config::instance::scrub_inherited_agent_env();
    }
    Ok(())
}
