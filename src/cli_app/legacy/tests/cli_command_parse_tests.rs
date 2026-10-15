use super::*;

#[test]
fn config_alias_opens_wizard() {
    let cli = Cli::try_parse_from(["cutex", "config"]).expect("config alias should parse");
    assert!(matches!(cli.command, Some(CommandKind::Wizard)));
}

#[test]
fn agent_send_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex",
        "agent",
        "send",
        "worker",
        "please inspect the latest logs",
        "--queue-only",
        "--from",
        "lead",
        "--external-message-id",
        "  upstream-42  ",
    ])
    .expect("agent send should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Agent {
            command: AgentCommand::Send {
                target,
                message,
                external_message_id: Some(external_message_id),
                all_groups: false,
                queue_only: true,
                soon: false,
                interrupt: false,
                from: Some(from),
            }
        }) if target == "worker"
            && message == "please inspect the latest logs"
            && from == "lead"
            && external_message_id == "upstream-42"
    ));
}

#[test]
fn agent_send_command_omits_external_message_id_by_default() {
    let cli = Cli::try_parse_from(["cutex", "agent", "send", "worker", "hello"])
        .expect("agent send should parse without an external message ID");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Agent {
            command: AgentCommand::Send {
                external_message_id: None,
                ..
            }
        })
    ));
}

#[test]
fn agent_send_command_rejects_blank_external_message_id() {
    for blank in ["", "   ", "\t\n"] {
        let error = Cli::try_parse_from([
            "cutex",
            "agent",
            "send",
            "worker",
            "hello",
            "--external-message-id",
            blank,
        ])
        .expect_err("blank external message ID should fail parsing");
        assert!(error.to_string().contains("cannot be empty"));
    }
}

#[test]
fn run_group_command_parses_multiple_values_before_passthrough_args() {
    let cli = Cli::try_parse_from([
        "cutex",
        "run",
        "orchid",
        "--agent",
        "--group",
        "aria",
        "scgpt",
        "--",
        "--sandbox",
        "danger-full-access",
    ])
    .expect("run group should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Run {
            profile,
            agent: true,
            groups,
            codex_args,
            ..
        }) if profile == "orchid"
            && groups == vec!["aria".to_string(), "scgpt".to_string()]
            && codex_args == vec!["--sandbox".to_string(), "danger-full-access".to_string()]
    ));
}

#[test]
fn prompt_input_normalization_strips_bom() {
    assert_eq!(normalize_prompt_input("\u{feff}1\r\n"), "1");
    assert_eq!(normalize_prompt_input(" q \n"), "q");
}

#[test]
fn im_register_command_parses_session_groups() {
    let cli = Cli::try_parse_from([
        "cutex",
        "im",
        "register",
        "019e-session",
        "--name",
        "aria-data",
        "--group",
        "aria",
        "scgpt",
    ])
    .expect("im register should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Im {
            command: ImCommand::Register {
                session_id,
                name: Some(name),
                groups,
                ..
            }
        }) if session_id == "019e-session"
            && name == "aria-data"
            && groups == vec!["aria".to_string(), "scgpt".to_string()]
    ));
}

#[test]
fn agent_log_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex", "agent", "log", "--agent", "aria-it", "--limit", "20", "--json",
    ])
    .expect("agent log should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Agent {
            command: AgentCommand::Log {
                agent: Some(agent),
                limit: 20,
                json: true,
            }
        }) if agent == "aria-it"
    ));
}

#[test]
fn agent_remote_up_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex",
        "agent",
        "remote-up",
        "devbox",
        "--local-port",
        "24261",
        "--remote-port",
        "24260",
        "--token",
        "secret",
        "--show-ssh-fallback",
    ])
    .expect("agent remote-up should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Agent {
            command: AgentCommand::RemoteUp {
                host,
                service_id: None,
                local_port: Some(24261),
                remote_port: Some(24260),
                token: Some(token),
                show_ssh_fallback: true,
                no_config: false,
            }
        }) if host == "devbox" && token == "secret"
    ));
}

#[test]
fn global_agent_bus_set_command_parses() {
    let cli = Cli::try_parse_from([
        "cutex",
        "global",
        "set",
        "--agent-bus-enable",
        "false",
        "--agent-bus-port",
        "24261",
        "--agent-bus-token",
        "-",
    ])
    .expect("global agent bus settings should parse");
    assert!(matches!(
        cli.command,
        Some(CommandKind::Global {
            command: GlobalCommand::Set {
                agent_bus_enable: Some(false),
                agent_bus_port: Some(24261),
                agent_bus_token: Some(token),
                ..
            }
        }) if token == "-"
    ));
}

#[test]
fn global_agent_message_template_options_are_gone() {
    // Agent messages carry a structured `Sender:` header (messaging v2);
    // the prefix / suffix templates were removed.
    for flag in ["--agent-message-prefix", "--agent-message-suffix"] {
        assert!(Cli::try_parse_from(["cutex", "global", "set", flag, "x"]).is_err(), "{flag}");
    }
}

#[test]
fn ordinary_new_session_accepts_optional_profile_without_agent_flags() {
    let picker = Cli::try_parse_from(["cutex", "new"]).unwrap();
    assert!(matches!(picker.command, Some(CommandKind::New { profile: None })));
    let selected = Cli::try_parse_from(["cutex", "new", "orchid"]).unwrap();
    assert!(matches!(selected.command, Some(CommandKind::New { profile: Some(name) }) if name == "orchid"));
    assert!(Cli::try_parse_from(["cutex", "new", "--agent"]).is_err());
}
