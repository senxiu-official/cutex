#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct LaunchFixture {
    home: PathBuf,
    child: PathBuf,
}

impl LaunchFixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let home = std::env::temp_dir().join(format!(
            "cutex-native-jsonl-launch-{}-{unique}",
            std::process::id()
        ));
        // A Codex profile: since 8aa47c3 a Claude profile launches through the
        // Claude adapter runtime (`claude_profile::run`) and refuses CLI args,
        // so the generic child wrapper under test is only reached by Codex.
        let profile = home.join(".cutex/profiles/jsonl-profile");
        fs::create_dir_all(&profile).expect("create isolated profile");
        fs::write(profile.join("auth.json"), "{}\n").expect("write fixture auth");
        fs::write(profile.join("config.toml"), "").expect("write fixture config");
        fs::write(
            home.join(".cutex/accounts.json"),
            r#"{
  "version": 3,
  "accounts": [{
    "id": "jsonl-profile",
    "name": "jsonl",
    "email": null,
    "plan_type": null,
    "source": "fixture",
    "runtime": {"kind": "host"},
    "proxy": null,
    "session": {"enabled": true},
    "cli_kind": "codex",
    "default_cli_args": [],
    "agent_name": null,
    "last_used_at": null
  }],
  "active_account_id": "jsonl-profile"
}
"#,
        )
        .expect("write fixture account store");
        fs::write(
            home.join(".cutex/config.json"),
            r#"{"desktop_notify_enabled":false,"agent_bus_enabled":false}"#,
        )
        .expect("write fixture global config");

        let child = home.join("cute-codex-jsonl-fixture");
        fs::write(
            &child,
            r#"#!/bin/sh
case "${CUTEX_JSONL_FIXTURE_CASE:-json_ok}" in
  json_ok)
    printf '%s\n' '{"type":"thread.started","thread_id":"018e0000-0000-7000-8000-0000b0009ae4"}'
    printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1}}'
    printf '%s\n' 'fixture warning on stderr' >&2
    ;;
  nonzero)
    printf '%s\n' '{"type":"thread.started","thread_id":"018e0000-0000-7000-8000-0000b0009ae4"}'
    printf '%s\n' 'fixture nonzero warning' >&2
    exit 7
    ;;
  non_json)
    printf '%s\n' 'Running profile child-owned non-JSON line'
    ;;
  non_utf8)
    printf '\377\n'
    ;;
  human)
    printf '%s\n' 'child human output'
    ;;
esac
"#,
        )
        .expect("write fake child");
        let mut permissions = fs::metadata(&child)
            .expect("fake child metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&child, permissions).expect("make fake child executable");
        Self { home, child }
    }

    fn command(&self, scenario: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cutex"));
        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("CUTEX_CODEX_BIN", &self.child)
            .env("CUTEX_JSONL_FIXTURE_CASE", scenario)
            .env_remove("CODEX_HOME")
            .env_remove("OPENAI_API_KEY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn machine_output(&self, scenario: &str) -> Output {
        self.command(scenario)
            .args([
                "run", "jsonl", "--host", "--agent", "--group", "fixture", "--", "exec", "--json",
                "Hi.",
            ])
            .output()
            .expect("execute actual Cutex run wrapper")
    }
}

impl Drop for LaunchFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn utf8(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("fixture output should be UTF-8")
}

fn assert_every_nonempty_line_is_json(bytes: &[u8]) {
    for line in utf8(bytes).lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).expect("stdout line is JSON");
        assert!(value.is_object(), "stdout JSONL event must be an object");
    }
}

#[test]
fn actual_run_json_wrapper_leaves_stdout_owned_by_child_in_agent_host_mode() {
    let fixture = LaunchFixture::new();
    let output = fixture.machine_output("json_ok");

    assert!(output.status.success());
    assert_every_nonempty_line_is_json(&output.stdout);
    let stdout = utf8(&output.stdout);
    assert!(stdout.starts_with("{\"type\":\"thread.started\""));
    assert!(!stdout.contains("cutex build:"));
    assert!(!stdout.contains("Running profile"));
    assert!(!stdout.contains("Launch: cli="));
    assert!(!output.stdout.contains(&0x1b));

    let stderr = utf8(&output.stderr);
    assert!(stderr.contains("cutex build:"));
    assert!(stderr.contains("Running"));
    assert!(stderr.contains("Launch: cli=codex"));
    assert!(stderr.contains("agent=collab"), "{stderr}");
    assert!(stderr.contains("fixture"), "{stderr}");
    assert!(stderr.contains("fixture warning on stderr"));
    assert!(
        output.stderr.contains(&0x1b),
        "presentation keeps its ANSI styling on stderr"
    );
}

#[test]
fn json_wrapper_preserves_child_nonzero_status_and_exact_structured_stdout() {
    let fixture = LaunchFixture::new();
    let output = fixture.machine_output("nonzero");

    assert_eq!(output.status.code(), Some(7));
    assert_every_nonempty_line_is_json(&output.stdout);
    assert_eq!(
        utf8(&output.stdout),
        "{\"type\":\"thread.started\",\"thread_id\":\"018e0000-0000-7000-8000-0000b0009ae4\"}\n"
    );
    assert!(utf8(&output.stderr).contains("fixture nonzero warning"));
}

#[test]
fn machine_boundary_does_not_filter_or_rewrite_invalid_child_protocol_bytes() {
    let fixture = LaunchFixture::new();

    let non_json = fixture.machine_output("non_json");
    assert!(non_json.status.success());
    assert_eq!(
        utf8(&non_json.stdout),
        "Running profile child-owned non-JSON line\n"
    );
    assert!(utf8(&non_json.stderr).contains("cutex build:"));

    let non_utf8 = fixture.machine_output("non_utf8");
    assert!(non_utf8.status.success());
    assert_eq!(non_utf8.stdout, [0xff, b'\n']);
}

#[test]
fn ordinary_interactive_launch_keeps_compatible_stdout_presentation() {
    let fixture = LaunchFixture::new();
    let output = fixture
        .command("human")
        .args(["run", "jsonl", "--host", "--", "--version"])
        .output()
        .expect("execute ordinary launch");

    assert!(output.status.success());
    let stdout = utf8(&output.stdout);
    assert!(stdout.contains("cutex build:"));
    assert!(stdout.contains("Running"));
    assert!(stdout.contains("Launch: cli=codex"));
    assert!(stdout.ends_with("child human output\n"));
}
