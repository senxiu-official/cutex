#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
        Self::with_config(r#"{"desktop_notify_enabled":false,"agent_bus_enabled":false}"#)
    }

    fn with_config(config_json: &str) -> Self {
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
            config_json,
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

impl LaunchFixture {
    fn stored_config(&self) -> serde_json::Value {
        let text = fs::read_to_string(self.home.join(".cutex/config.json")).expect("read config");
        serde_json::from_str(&text).expect("config is JSON")
    }
}

/// Default-instance ports (management, bus, their tunnels, desktop notify).
const DEFAULT_PORTS: [u64; 5] = [24270, 24260, 24670, 24660, 24250];

/// A stand-in bus on a free loopback port: answers `200 OK` to every request
/// and counts the connections it accepted.
fn fake_bus() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake bus");
    let port = listener.local_addr().expect("fake bus address").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buffer = [0u8; 8192];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            );
        }
    });
    (port, hits)
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

#[test]
fn agent_launch_keeps_a_disabled_bus_off_and_never_takes_a_default_port() {
    // Regression (2026-10-18): the launch forced the bus on and normalized
    // the fixture to 24260, then probed / handed off the machine's live bus
    // or spawned a detached `agent serve --port 24260`.
    let fixture = LaunchFixture::new();
    let output = fixture.machine_output("json_ok");
    assert!(output.status.success(), "{}", utf8(&output.stderr));

    let config = fixture.stored_config();
    assert_eq!(config["agent_bus_enabled"], false, "{config}");
    let port = config["agent_bus_port"].as_u64().expect("a bus port of its own");
    assert!(!DEFAULT_PORTS.contains(&port), "{config}");
    assert!(config.get("management_port").is_none_or(|value| !DEFAULT_PORTS.contains(&value.as_u64().unwrap_or(0))));
}

#[test]
fn agent_launch_with_an_enabled_bus_targets_only_the_homes_own_port() {
    let (port, hits) = fake_bus();
    let fixture = LaunchFixture::with_config(&format!(
        r#"{{"desktop_notify_enabled":false,"agent_bus_enabled":true,"agent_bus_port":{port},"agent_bus_token":"fixture-bus-token-0123456789"}}"#
    ));
    let output = fixture.machine_output("json_ok");
    assert!(output.status.success(), "{}", utf8(&output.stderr));

    // The health probe reached this home's bus (so nothing was spawned), and
    // the stored port was kept rather than normalized to the default bus.
    assert!(hits.load(Ordering::SeqCst) >= 1, "{}", utf8(&output.stderr));
    let config = fixture.stored_config();
    assert_eq!(config["agent_bus_port"].as_u64(), Some(u64::from(port)), "{config}");
}

#[test]
fn agent_launch_replaces_a_stored_default_bus_port_and_never_probes_it() {
    // An old fixture that stores the default port: the guard swaps in a free
    // port of its own (nothing listens there, the launch stays offline).
    let fixture = LaunchFixture::with_config(
        r#"{"desktop_notify_enabled":false,"agent_bus_enabled":false,"agent_bus_port":24260,"management_port":24270}"#,
    );
    let output = fixture.machine_output("json_ok");
    assert!(output.status.success(), "{}", utf8(&output.stderr));
    let config = fixture.stored_config();
    let port = config["agent_bus_port"].as_u64().expect("bus port");
    assert!(!DEFAULT_PORTS.contains(&port), "{config}");
    // The management probe refuses the stored default port with a warning.
    assert!(utf8(&output.stderr).contains("management"), "{}", utf8(&output.stderr));
}
