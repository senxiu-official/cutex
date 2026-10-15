use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use serde_json::json;
use serde_json::Value;

const MESSAGE_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_MESSAGE: &str = "Reply with exactly APP_SERVER_SPIKE_OK and no punctuation.";
const ISOLATED_CUTEX_ENV_VARS: &[&str] = &[
    "CODEX_THREAD_ID",
    "CUTEX_AGENT_BUS_TOKEN",
    "CUTEX_AGENT_BUS_URL",
    "CUTEX_AGENT_GROUPS",
    "CUTEX_AGENT_HINT",
    "CUTEX_AGENT_HOST_ID",
    "CUTEX_AGENT_ID",
    "CUTEX_AGENT_NAME",
    "CUTEX_MANAGEMENT_OBSERVER_TOKEN",
    "CUTEX_MANAGEMENT_OBSERVER_URL",
    "CUTEX_RUNTIME_HEARTBEAT_TOKEN",
    "CUTEX_RUNTIME_HEARTBEAT_URL",
    "CUTEX_RUNTIME_LAUNCH_ID",
];

#[derive(Debug)]
struct Options {
    codex_bin: String,
    resume_thread: Option<String>,
    message: String,
}

#[derive(Default)]
struct Observation {
    method_sequence: Vec<String>,
    method_counts: BTreeMap<String, usize>,
    item_types: BTreeSet<String>,
    server_request_methods: BTreeSet<String>,
    expected_client_id: String,
    client_id_echoed: bool,
    agent_delta_bytes: usize,
    completed_agent_message: bool,
    final_reply_matches: bool,
    settings_updated: bool,
    token_usage_updated: bool,
    turn_status: Option<String>,
}

impl Observation {
    fn record(&mut self, message: &Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        self.method_sequence.push(method.to_string());
        *self.method_counts.entry(method.to_string()).or_default() += 1;

        if message.get("id").is_some() {
            self.server_request_methods.insert(method.to_string());
            return;
        }
        if method == "thread/settings/updated" {
            self.settings_updated = true;
        }
        if method == "thread/tokenUsage/updated" {
            self.token_usage_updated = true;
        }
        if method == "item/agentMessage/delta" {
            self.agent_delta_bytes += message
                .pointer("/params/delta")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or_default();
        }
        if matches!(method, "item/started" | "item/completed") {
            if let Some(item_type) = message.pointer("/params/item/type").and_then(Value::as_str) {
                self.item_types.insert(item_type.to_string());
                if item_type == "userMessage"
                    && message
                        .pointer("/params/item/clientId")
                        .and_then(Value::as_str)
                        == Some(self.expected_client_id.as_str())
                {
                    self.client_id_echoed = true;
                }
                if method == "item/completed" && item_type == "agentMessage" {
                    self.completed_agent_message = true;
                    self.final_reply_matches = message
                        .pointer("/params/item/text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.trim() == "APP_SERVER_SPIKE_OK");
                }
            }
        }
        if method == "turn/completed" {
            self.turn_status = message
                .pointer("/params/turn/status")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
}

struct StdioClient {
    child: Child,
    stdin: Option<ChildStdin>,
    inbound: mpsc::Receiver<anyhow::Result<Value>>,
    next_id: u64,
}

impl StdioClient {
    fn spawn(codex_bin: &str) -> anyhow::Result<Self> {
        let mut command = Command::new(codex_bin);
        command
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("RUST_LOG", "error");
        for key in ISOLATED_CUTEX_ENV_VARS {
            command.env_remove(key);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {codex_bin} app-server --stdio"))?;
        let stdin = child
            .stdin
            .take()
            .context("app-server stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("app-server stdout was not piped")?;
        let (sender, inbound) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let result = line
                    .context("failed to read app-server stdout")
                    .and_then(|line| {
                        serde_json::from_str(&line)
                            .with_context(|| format!("invalid app-server JSON: {line}"))
                    });
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            inbound,
            next_id: 1,
        })
    }

    fn notify(&mut self, method: &str, params: Option<Value>) -> anyhow::Result<()> {
        let mut notification = json!({ "method": method });
        if let Some(params) = params {
            notification["params"] = params;
        }
        self.send(&notification)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        observation: &mut Observation,
    ) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "method": method, "id": id, "params": params }))?;
        loop {
            let message = self.recv()?;
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    bail!("app-server {method} failed: {error}");
                }
                return message
                    .get("result")
                    .cloned()
                    .context("app-server response omitted result");
            }
            self.handle_inbound(message, observation)?;
        }
    }

    fn wait_for_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        observation: &mut Observation,
    ) -> anyhow::Result<()> {
        loop {
            let message = self.recv()?;
            let completed = message.get("method").and_then(Value::as_str) == Some("turn/completed")
                && message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
                && message.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn_id);
            self.handle_inbound(message, observation)?;
            if completed {
                return Ok(());
            }
        }
    }

    fn handle_inbound(
        &mut self,
        message: Value,
        observation: &mut Observation,
    ) -> anyhow::Result<()> {
        observation.record(&message);
        if message.get("method").is_some() {
            if let Some(request_id) = message.get("id").cloned() {
                self.send(&json!({
                    "id": request_id,
                    "error": {
                        "code": -32601,
                        "message": "cutex app-server spike does not resolve server requests"
                    }
                }))?;
            }
        }
        Ok(())
    }

    fn send(&mut self, message: &Value) -> anyhow::Result<()> {
        let stdin = self.stdin.as_mut().context("app-server stdin is closed")?;
        serde_json::to_writer(&mut *stdin, message)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn recv(&self) -> anyhow::Result<Value> {
        self.inbound
            .recv_timeout(MESSAGE_TIMEOUT)
            .context("timed out waiting for app-server message")?
    }

    fn stop(mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_options() -> anyhow::Result<Options> {
    let mut codex_bin =
        std::env::var("CUTEX_CODEX_BIN").unwrap_or_else(|_| "cute-codex".to_string());
    let mut resume_thread = None;
    let mut message = DEFAULT_MESSAGE.to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--codex-bin" => {
                codex_bin = args.next().context("--codex-bin requires a path")?;
            }
            "--resume-thread" => {
                resume_thread = Some(args.next().context("--resume-thread requires an id")?);
            }
            "--message" => {
                message = args.next().context("--message requires text")?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cargo run --example app_server_stdio_spike -- [--codex-bin PATH] [--resume-thread ID] [--message TEXT]"
                );
                std::process::exit(0);
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    Ok(Options {
        codex_bin,
        resume_thread,
        message,
    })
}

fn main() -> anyhow::Result<()> {
    let options = parse_options()?;
    let mut client = StdioClient::spawn(&options.codex_bin)?;
    let mut observation = Observation::default();

    let initialize = client.request(
        "initialize",
        json!({
            "clientInfo": {
                "name": "cutex_app_server_spike",
                "title": "cutex app-server spike",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true,
                "optOutNotificationMethods": []
            }
        }),
        &mut observation,
    )?;
    client.notify("initialized", None)?;

    let resumed = if let Some(thread_id) = options.resume_thread.as_deref() {
        client.request(
            "thread/resume",
            json!({ "threadId": thread_id, "excludeTurns": true }),
            &mut observation,
        )?;
        true
    } else {
        false
    };

    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let started = client.request(
        "thread/start",
        json!({
            "cwd": cwd,
            "ephemeral": true,
            "personality": "pragmatic"
        }),
        &mut observation,
    )?;
    let thread_id = started
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .context("thread/start response omitted thread.id")?
        .to_string();
    client.request(
        "thread/settings/update",
        json!({
            "threadId": thread_id,
            "effort": "low",
            "personality": "pragmatic"
        }),
        &mut observation,
    )?;

    let client_message_id = format!("cutex-app-server-spike-{}", std::process::id());
    observation.expected_client_id = client_message_id.clone();
    let turn = client.request(
        "turn/start",
        json!({
            "threadId": thread_id,
            "clientUserMessageId": client_message_id,
            "input": [{
                "type": "text",
                "text": options.message,
                "text_elements": []
            }]
        }),
        &mut observation,
    )?;
    let turn_id = turn
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .context("turn/start response omitted turn.id")?
        .to_string();
    client.wait_for_turn(&thread_id, &turn_id, &mut observation)?;

    let summary = json!({
        "protocol": "codex-app-server-v2",
        "transport": "stdio",
        "user_agent": initialize.get("userAgent"),
        "platform_family": initialize.get("platformFamily"),
        "platform_os": initialize.get("platformOs"),
        "resumed_existing_thread": resumed,
        "thread_id": thread_id,
        "turn_id": turn_id,
        "client_user_message_id": client_message_id,
        "client_id_echoed": observation.client_id_echoed,
        "settings_updated": observation.settings_updated,
        "token_usage_updated": observation.token_usage_updated,
        "turn_status": observation.turn_status,
        "completed_agent_message": observation.completed_agent_message,
        "final_reply_matches": observation.final_reply_matches,
        "agent_delta_bytes": observation.agent_delta_bytes,
        "item_types": observation.item_types,
        "method_counts": observation.method_counts,
        "method_sequence": observation.method_sequence,
        "server_request_methods": observation.server_request_methods,
        "custom_observer_environment_removed": true
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    client.stop();
    Ok(())
}
