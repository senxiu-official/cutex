use super::*;

#[test]
fn start_command_parses_session_filters() {
    let cli = Cli::try_parse_from([
        "cutex",
        "start",
        "--attachable",
        "--group",
        "waveline",
        "--sort",
        "recent",
    ])
    .expect("start command should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Start {
            list:
                SessionListArgs {
                    attachable: true,
                    sort: SessionListSort::Recent,
                    groups,
                    ..
                },
        }) if groups == vec!["waveline".to_string()]
    ));
}

#[test]
fn session_archive_commands_parse_with_json_and_reason() {
    let retired = Cli::try_parse_from(["cutex", "session", "retired", "--json"])
        .expect("retired command should parse");
    assert!(matches!(
        retired.command,
        Some(CommandKind::Session {
            command: SessionCommand::Retired { json: true }
        })
    ));

    let retire = Cli::try_parse_from([
        "cutex",
        "session",
        "retire",
        "cutex.archive",
        "--reason",
        "owner cleanup",
        "--json",
    ])
    .expect("retire command should parse");
    assert!(matches!(
        retire.command,
        Some(CommandKind::Session {
            command: SessionCommand::Retire {
                id,
                reason: Some(reason),
                json: true,
            }
        }) if id == "cutex.archive" && reason == "owner cleanup"
    ));

    let restore = Cli::try_parse_from(["cutex", "session", "restore", "cutex.archive"])
        .expect("restore command should parse");
    assert!(matches!(
        restore.command,
        Some(CommandKind::Session {
            command: SessionCommand::Restore { id, json: false }
        }) if id == "cutex.archive"
    ));
}

#[test]
fn session_repair_history_command_parses_with_json() {
    let repair = Cli::try_parse_from([
        "cutex",
        "session",
        "repair-history",
        "cutex.repair",
        "--json",
    ])
    .expect("repair-history command should parse");
    assert!(matches!(
        repair.command,
        Some(CommandKind::Session {
            command: SessionCommand::RepairHistory { id, json: true }
        }) if id == "cutex.repair"
    ));
}

#[test]
fn session_expose_command_parses_groups() {
    let cli = Cli::try_parse_from([
        "cutex",
        "session",
        "expose",
        "019e-session",
        "--name",
        "aria-data",
        "--group",
        "aria",
        "scgpt",
    ])
    .expect("session expose should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Session {
            command: SessionCommand::Expose {
                id,
                name: Some(name),
                groups,
            }
        }) if id == "019e-session"
            && name == "aria-data"
            && groups == vec!["aria".to_string(), "scgpt".to_string()]
    ));
}

#[test]
fn session_adopt_and_unmanage_commands_parse() {
    let adopt = Cli::try_parse_from([
        "cutex",
        "session",
        "adopt",
        "019e-session",
        "--name",
        "test",
        "--current-cwd",
        "--group",
        "waveline",
        "aria",
        "--im",
        "--pin",
    ])
    .expect("session adopt should parse");
    assert!(matches!(
        adopt.command,
        Some(CommandKind::Session {
            command:
                SessionCommand::Adopt {
                    id,
                    name: Some(name),
                    current_cwd: true,
                    groups,
                    expose_to_im: true,
                    pin: true,
                    ..
                },
        }) if id == "019e-session"
            && name == "test"
            && groups == vec!["waveline".to_string(), "aria".to_string()]
    ));

    let unmanage = Cli::try_parse_from(["cutex", "session", "unmanage", "019e-session"])
        .expect("session unmanage should parse");
    assert!(matches!(
        unmanage.command,
        Some(CommandKind::Session {
            command: SessionCommand::Unmanage { id }
        }) if id == "019e-session"
    ));
}

#[test]
fn session_groups_set_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex",
        "session",
        "groups",
        "set",
        "019e-session",
        "--group",
        "waveline",
    ])
    .expect("session groups set should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Session {
            command: SessionCommand::Groups {
            command: SessionGroupsCommand::Set { id, groups }
            }
        }) if id == "019e-session" && groups == vec!["waveline".to_string()]
    ));
}

#[test]
fn session_quick_commands_parse() {
    let pin = Cli::try_parse_from(["cutex", "session", "quick", "pin", "019e-session"])
        .expect("session quick pin should parse");
    assert!(matches!(
        pin.command,
        Some(CommandKind::Session {
            command: SessionCommand::Quick {
                command: SessionQuickCommand::Pin { id }
            }
        }) if id == "019e-session"
    ));

    let hide = Cli::try_parse_from(["cutex", "session", "quick", "hide", "019e-session"])
        .expect("session quick hide should parse");
    assert!(matches!(
        hide.command,
        Some(CommandKind::Session {
            command: SessionCommand::Quick {
                command: SessionQuickCommand::Hide { id }
            }
        }) if id == "019e-session"
    ));

    let auto = Cli::try_parse_from(["cutex", "session", "quick", "auto", "019e-session"])
        .expect("session quick auto should parse");
    assert!(matches!(
        auto.command,
        Some(CommandKind::Session {
            command: SessionCommand::Quick {
                command: SessionQuickCommand::Auto { id }
            }
        }) if id == "019e-session"
    ));
}

#[test]
fn session_cwd_commands_parse() {
    let set = Cli::try_parse_from([
        "cutex",
        "session",
        "cwd",
        "set",
        "019e-session",
        "~/Projects/scgpt",
    ])
    .expect("session cwd set should parse");
    assert!(matches!(
        set.command,
        Some(CommandKind::Session {
            command: SessionCommand::Cwd {
                command: SessionCwdCommand::Set { id, path }
            }
        }) if id == "019e-session" && path == "~/Projects/scgpt"
    ));

    let current = Cli::try_parse_from(["cutex", "session", "cwd", "current", "019e-session"])
        .expect("session cwd current should parse");
    assert!(matches!(
        current.command,
        Some(CommandKind::Session {
            command: SessionCommand::Cwd {
                command: SessionCwdCommand::Current { id }
            }
        }) if id == "019e-session"
    ));
}

#[test]
fn session_defaults_set_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex",
        "session",
        "defaults",
        "set",
        "019e-session",
        "--runtime-backend",
        "cute-alden",
        "--permission",
        "full access",
        "--model",
        "gpt-5.5",
        "--reasoning",
        "xhigh",
        "--cli-arg",
        "--no-alt-screen",
    ])
    .expect("session defaults set should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Session {
            command: SessionCommand::Defaults {
                command: SessionDefaultsCommand::Set {
                    id,
                    runtime_backend: Some(runtime_backend),
                    permission_defaults: Some(permission_defaults),
                    model: Some(model),
                    reasoning: Some(reasoning),
                    cli_args,
                    ..
                }
            }
        }) if id == "019e-session"
            && runtime_backend == "cute-alden"
            && permission_defaults == "full access"
            && model == "gpt-5.5"
            && reasoning == "xhigh"
            && cli_args == vec!["--no-alt-screen".to_string()]
    ));
}

#[test]
fn session_wizard_and_lifecycle_commands_parse() {
    let wizard =
        Cli::try_parse_from(["cutex", "session", "wizard"]).expect("session wizard parses");
    assert!(matches!(
        wizard.command,
        Some(CommandKind::Session {
            command: SessionCommand::Wizard { .. }
        })
    ));

    let edit =
        Cli::try_parse_from(["cutex", "session", "edit"]).expect("session edit alias parses");
    assert!(matches!(
        edit.command,
        Some(CommandKind::Session {
            command: SessionCommand::Wizard { .. }
        })
    ));

    let online = Cli::try_parse_from([
        "cutex",
        "session",
        "online",
        "019e-session",
        "--profile",
        "beta",
    ])
    .expect("session online --profile parses");
    assert!(matches!(
        online.command,
        Some(CommandKind::Session {
            command: SessionCommand::Online {
                id,
                profile: Some(profile),
            }
        }) if id == "019e-session" && profile == "beta"
    ));

    let foreground = Cli::try_parse_from([
        "cutex",
        "session",
        "foreground",
        "019e-session",
        "--profile",
        "beta",
    ])
    .expect("session foreground --profile parses");
    assert!(matches!(
        foreground.command,
        Some(CommandKind::Session {
            command: SessionCommand::Foreground {
                id,
                profile: Some(profile),
            }
        }) if id == "019e-session" && profile == "beta"
    ));

    let offline = Cli::try_parse_from(["cutex", "session", "offline", "019e-session", "--force"])
        .expect("session offline --force parses");
    assert!(matches!(
        offline.command,
        Some(CommandKind::Session {
            command: SessionCommand::Offline { id, force: true }
        }) if id == "019e-session"
    ));

    let close = Cli::try_parse_from(["cutex", "session", "close", "019e-session"])
        .expect("session close parses");
    assert!(matches!(
        close.command,
        Some(CommandKind::Session {
            command: SessionCommand::Close { id, force: false }
        }) if id == "019e-session"
    ));
}

#[test]
fn session_list_filter_commands_parse() {
    let cli = Cli::try_parse_from([
        "cutex",
        "session",
        "list",
        "--all",
        "--group",
        "aria",
        "--project",
        "scgpt",
        "--sort",
        "recent",
    ])
    .expect("session list filters should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Session {
            command:
                SessionCommand::List {
                    list:
                        SessionListArgs {
                            all: true,
                            sort: SessionListSort::Recent,
                            groups,
                            projects,
                            ..
                        },
                }
        }) if groups == vec!["aria".to_string()]
            && projects == vec!["scgpt".to_string()]
    ));

    let wizard = Cli::try_parse_from([
        "cutex",
        "session",
        "wizard",
        "--attachable",
        "--sort",
        "name",
    ])
    .expect("session wizard filters should parse");
    assert!(matches!(
        wizard.command,
        Some(CommandKind::Session {
            command: SessionCommand::Wizard {
                list: SessionListArgs {
                    attachable: true,
                    sort: SessionListSort::Name,
                    ..
                },
            }
        })
    ));
}

#[test]
fn session_duplicate_check_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex",
        "session",
        "duplicate-check",
        "019e-session",
        "--json",
    ])
    .expect("session duplicate-check should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Session {
            command: SessionCommand::DuplicateCheck { id, json: true }
        }) if id == "019e-session"
    ));
}

#[test]
fn session_attach_takeover_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex",
        "session",
        "attach",
        "--name",
        "cutex.orchid.host.cutex.019e-session",
        "--takeover",
    ])
    .expect("session attach --takeover should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Session {
            command: SessionCommand::Attach { name, takeover: true }
        }) if name == "cutex.orchid.host.cutex.019e-session"
    ));
}

#[test]
fn session_takeover_command_parses() {
    let cli = Cli::try_parse_from(["cutex", "session", "takeover", "019e-session"])
        .expect("session takeover should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Session {
            command: SessionCommand::Takeover { id }
        }) if id == "019e-session"
    ));
}
