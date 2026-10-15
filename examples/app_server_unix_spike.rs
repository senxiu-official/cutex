#[cfg(unix)]
mod unix_spike {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Child;
    use std::process::Command;
    use std::process::Stdio;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use anyhow::bail;
    use anyhow::Context;
    use serde_json::json;
    use serde_json::Value;
    use tungstenite::client::client;
    use tungstenite::client::IntoClientRequest;
    use tungstenite::Message;
    use tungstenite::WebSocket;

    const MESSAGE_TIMEOUT: Duration = Duration::from_secs(180);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const DEFAULT_MESSAGE: &str =
        "Reply with exactly APP_SERVER_MULTI_CLIENT_OK and no punctuation.";
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
        message: String,
    }

    #[derive(Default)]
    struct Observation {
        method_counts: BTreeMap<String, usize>,
        method_sequence: Vec<String>,
        item_ids: BTreeSet<String>,
        item_types: BTreeSet<String>,
        server_request_methods: BTreeSet<String>,
        expected_client_id: String,
        expected_agent_reply: String,
        client_id_echoed: bool,
        agent_delta_bytes: usize,
        completed_agent_message: bool,
        final_reply_matches: bool,
        turn_started: bool,
        turn_completed: bool,
        turn_status: Option<String>,
    }

    impl Observation {
        fn record(&mut self, message: &Value, thread_id: Option<&str>, turn_id: Option<&str>) {
            let Some(method) = message.get("method").and_then(Value::as_str) else {
                return;
            };
            self.method_sequence.push(method.to_string());
            *self.method_counts.entry(method.to_string()).or_default() += 1;

            if message.get("id").is_some() {
                self.server_request_methods.insert(method.to_string());
                return;
            }
            if method == "item/agentMessage/delta" {
                self.agent_delta_bytes += message
                    .pointer("/params/delta")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or_default();
            }
            if matches!(method, "item/started" | "item/completed") {
                let message_thread_id = message.pointer("/params/threadId").and_then(Value::as_str);
                let message_turn_id = message.pointer("/params/turnId").and_then(Value::as_str);
                if thread_id.is_some_and(|expected| message_thread_id != Some(expected))
                    || turn_id.is_some_and(|expected| message_turn_id != Some(expected))
                {
                    return;
                }
                if let Some(item_id) = message.pointer("/params/item/id").and_then(Value::as_str) {
                    self.item_ids.insert(item_id.to_string());
                }
                if let Some(item_type) =
                    message.pointer("/params/item/type").and_then(Value::as_str)
                {
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
                            .is_some_and(|text| text.trim() == self.expected_agent_reply);
                    }
                }
            }
            if method == "turn/started" && thread_and_turn_match(message, thread_id, turn_id) {
                self.turn_started = true;
            }
            if method == "turn/completed" && thread_and_turn_match(message, thread_id, turn_id) {
                self.turn_completed = true;
                self.turn_status = message
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }

        fn passed(&self) -> bool {
            self.client_id_echoed
                && self.completed_agent_message
                && self.final_reply_matches
                && self.turn_started
                && self.turn_completed
                && self.turn_status.as_deref() == Some("completed")
        }

        fn summary(&self) -> Value {
            json!({
                "method_counts": self.method_counts,
                "method_sequence": self.method_sequence,
                "item_ids": self.item_ids,
                "item_types": self.item_types,
                "server_request_methods": self.server_request_methods,
                "client_id_echoed": self.client_id_echoed,
                "agent_delta_bytes": self.agent_delta_bytes,
                "completed_agent_message": self.completed_agent_message,
                "final_reply_matches": self.final_reply_matches,
                "turn_started": self.turn_started,
                "turn_completed": self.turn_completed,
                "turn_status": self.turn_status,
                "passed": self.passed(),
            })
        }
    }

    fn thread_and_turn_match(
        message: &Value,
        thread_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> bool {
        let message_thread_id = message.pointer("/params/threadId").and_then(Value::as_str);
        let message_turn_id = message.pointer("/params/turn/id").and_then(Value::as_str);
        thread_id.is_none_or(|expected| message_thread_id == Some(expected))
            && turn_id.is_none_or(|expected| message_turn_id == Some(expected))
    }

    struct SocketClient {
        name: &'static str,
        socket: WebSocket<UnixStream>,
        next_id: u64,
    }

    impl SocketClient {
        fn connect(name: &'static str, socket_path: &Path) -> anyhow::Result<Self> {
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            let stream = loop {
                match UnixStream::connect(socket_path) {
                    Ok(stream) => break stream,
                    Err(err) if Instant::now() < deadline => {
                        let _ = err;
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(err) => {
                        return Err(err).with_context(|| {
                            format!("{name} failed to connect to {}", socket_path.display())
                        });
                    }
                }
            };
            stream.set_read_timeout(Some(MESSAGE_TIMEOUT))?;
            stream.set_write_timeout(Some(MESSAGE_TIMEOUT))?;
            let request = "ws://localhost/".into_client_request()?;
            let (socket, response) = client(request, stream)?;
            if response.status() != 101 {
                bail!("{name} websocket upgrade returned {}", response.status());
            }
            Ok(Self {
                name,
                socket,
                next_id: 1,
            })
        }

        fn initialize(&mut self, observation: &mut Observation) -> anyhow::Result<()> {
            self.request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": format!("cutex_{}_spike", self.name),
                        "title": format!("cutex {} spike", self.name),
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": true,
                        "optOutNotificationMethods": []
                    }
                }),
                observation,
                None,
                None,
            )?;
            self.notify("initialized", None)
        }

        fn request(
            &mut self,
            method: &str,
            params: Value,
            observation: &mut Observation,
            thread_id: Option<&str>,
            turn_id: Option<&str>,
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
                        bail!("{} app-server {method} failed: {error}", self.name);
                    }
                    return message
                        .get("result")
                        .cloned()
                        .context("app-server response omitted result");
                }
                self.handle_inbound(message, observation, thread_id, turn_id)?;
            }
        }

        fn wait_for_turn(
            &mut self,
            thread_id: &str,
            turn_id: &str,
            observation: &mut Observation,
        ) -> anyhow::Result<()> {
            while !observation.turn_completed {
                let message = self.recv()?;
                self.handle_inbound(message, observation, Some(thread_id), Some(turn_id))?;
            }
            Ok(())
        }

        fn handle_inbound(
            &mut self,
            message: Value,
            observation: &mut Observation,
            thread_id: Option<&str>,
            turn_id: Option<&str>,
        ) -> anyhow::Result<()> {
            observation.record(&message, thread_id, turn_id);
            if message.get("method").is_some() {
                if let Some(request_id) = message.get("id").cloned() {
                    if message.get("method").and_then(Value::as_str)
                        == Some("item/commandExecution/requestApproval")
                    {
                        self.send(&json!({
                            "id": request_id,
                            "result": { "decision": "accept" }
                        }))?;
                    } else {
                        self.send(&json!({
                            "id": request_id,
                            "error": {
                                "code": -32601,
                                "message": format!("{} spike does not resolve server requests", self.name)
                            }
                        }))?;
                    }
                }
            }
            Ok(())
        }

        fn notify(&mut self, method: &str, params: Option<Value>) -> anyhow::Result<()> {
            let mut message = json!({ "method": method });
            if let Some(params) = params {
                message["params"] = params;
            }
            self.send(&message)
        }

        fn send(&mut self, message: &Value) -> anyhow::Result<()> {
            self.socket
                .send(Message::Text(serde_json::to_string(message)?.into()))
                .with_context(|| format!("{} failed to send websocket message", self.name))
        }

        fn recv(&mut self) -> anyhow::Result<Value> {
            loop {
                match self.socket.read()? {
                    Message::Text(text) => {
                        return serde_json::from_str(text.as_ref()).with_context(|| {
                            format!("{} received invalid app-server JSON", self.name)
                        });
                    }
                    Message::Ping(payload) => self.socket.send(Message::Pong(payload))?,
                    Message::Close(frame) => {
                        bail!("{} app-server websocket closed: {frame:?}", self.name)
                    }
                    Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        }
    }

    struct AppServerProcess {
        child: Child,
        socket_dir: PathBuf,
        socket_path: PathBuf,
    }

    impl AppServerProcess {
        fn spawn(codex_bin: &str) -> anyhow::Result<Self> {
            let socket_dir = std::env::temp_dir().join(format!(
                "cutex-app-server-multi-client-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&socket_dir)?;
            fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700))?;
            let socket_path = socket_dir.join("app-server.sock");
            let mut command = Command::new(codex_bin);
            command
                .args([
                    "app-server",
                    "--listen",
                    &format!("unix://{}", socket_path.display()),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .env("RUST_LOG", "error");
            for key in ISOLATED_CUTEX_ENV_VARS {
                command.env_remove(key);
            }
            let child = command
                .spawn()
                .with_context(|| format!("failed to start {codex_bin} Unix app-server"))?;
            Ok(Self {
                child,
                socket_dir,
                socket_path,
            })
        }
    }

    impl Drop for AppServerProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_file(&self.socket_path);
            let _ = fs::remove_dir(&self.socket_dir);
        }
    }

    fn parse_options() -> anyhow::Result<Options> {
        let mut codex_bin =
            std::env::var("CUTEX_CODEX_BIN").unwrap_or_else(|_| "cute-codex".to_string());
        let mut message = DEFAULT_MESSAGE.to_string();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--codex-bin" => {
                    codex_bin = args.next().context("--codex-bin requires a path")?;
                }
                "--message" => {
                    message = args.next().context("--message requires text")?;
                }
                "--help" | "-h" => {
                    println!(
                        "Usage: cargo run --example app_server_unix_spike -- [--codex-bin PATH] [--message TEXT]"
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }
        Ok(Options { codex_bin, message })
    }

    pub fn run() -> anyhow::Result<()> {
        let options = parse_options()?;
        let server = AppServerProcess::spawn(&options.codex_bin)?;
        let mut controller = SocketClient::connect("controller", &server.socket_path)?;
        let mut observer = SocketClient::connect("observer", &server.socket_path)?;
        let mut controller_observation = Observation::default();
        let mut observer_observation = Observation::default();
        controller.initialize(&mut controller_observation)?;
        observer.initialize(&mut observer_observation)?;

        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        let started = controller.request(
            "thread/start",
            json!({
                "cwd": cwd,
                "ephemeral": false,
                "personality": "pragmatic"
            }),
            &mut controller_observation,
            None,
            None,
        )?;
        let thread_id = started
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("thread/start response omitted thread.id")?
            .to_string();

        let bootstrap_client_id = format!("cutex-app-server-bootstrap-{}", std::process::id());
        controller_observation.expected_client_id = bootstrap_client_id.clone();
        let bootstrap_turn = controller.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "clientUserMessageId": bootstrap_client_id,
                "input": [{
                    "type": "text",
                    "text": "Reply with exactly APP_SERVER_BOOTSTRAP_OK and no punctuation.",
                    "text_elements": []
                }]
            }),
            &mut controller_observation,
            Some(&thread_id),
            None,
        )?;
        let bootstrap_turn_id = bootstrap_turn
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("bootstrap turn/start response omitted turn.id")?
            .to_string();
        controller.wait_for_turn(&thread_id, &bootstrap_turn_id, &mut controller_observation)?;
        if controller_observation.turn_status.as_deref() != Some("completed") {
            bail!("bootstrap turn did not complete");
        }

        controller_observation = Observation::default();
        observer_observation = Observation::default();
        observer.request(
            "thread/resume",
            json!({ "threadId": thread_id, "excludeTurns": true }),
            &mut observer_observation,
            Some(&thread_id),
            None,
        )?;

        let client_message_id = format!("cutex-app-server-multi-client-{}", std::process::id());
        controller_observation.expected_client_id = client_message_id.clone();
        controller_observation.expected_agent_reply = "APP_SERVER_MULTI_CLIENT_OK".to_string();
        observer_observation.expected_client_id = client_message_id.clone();
        observer_observation.expected_agent_reply = "APP_SERVER_MULTI_CLIENT_OK".to_string();
        let turn = controller.request(
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
            &mut controller_observation,
            Some(&thread_id),
            None,
        )?;
        let turn_id = turn
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("turn/start response omitted turn.id")?
            .to_string();
        controller.wait_for_turn(&thread_id, &turn_id, &mut controller_observation)?;
        observer.wait_for_turn(&thread_id, &turn_id, &mut observer_observation)?;

        let shared_same_item_ids = controller_observation.item_ids == observer_observation.item_ids;
        let shared_passed = controller_observation.passed()
            && observer_observation.passed()
            && shared_same_item_ids
            && controller_observation.server_request_methods.is_empty()
            && observer_observation.server_request_methods.is_empty();
        let shared_controller_summary = controller_observation.summary();
        let shared_observer_summary = observer_observation.summary();

        controller_observation = Observation::default();
        observer_observation = Observation::default();
        let approval_client_id = format!("cutex-app-server-approval-{}", std::process::id());
        controller_observation.expected_client_id = approval_client_id.clone();
        controller_observation.expected_agent_reply = "APP_SERVER_APPROVAL_OK".to_string();
        observer_observation.expected_client_id = approval_client_id.clone();
        observer_observation.expected_agent_reply = "APP_SERVER_APPROVAL_OK".to_string();
        let approval_file = std::env::temp_dir().join(format!(
            "cutex-app-server-approval-{}",
            uuid::Uuid::new_v4()
        ));
        let approval_prompt = format!(
            "Use the shell tool to run exactly `touch {}`. After it succeeds, reply with exactly APP_SERVER_APPROVAL_OK and no punctuation.",
            approval_file.display()
        );
        let approval_turn = controller.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "clientUserMessageId": approval_client_id,
                "approvalPolicy": "untrusted",
                "input": [{
                    "type": "text",
                    "text": approval_prompt,
                    "text_elements": []
                }]
            }),
            &mut controller_observation,
            Some(&thread_id),
            None,
        )?;
        let approval_turn_id = approval_turn
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("approval turn/start response omitted turn.id")?
            .to_string();

        let observer_thread_id = thread_id.clone();
        let observer_turn_id = approval_turn_id.clone();
        let observer_handle = thread::spawn(move || {
            observer.wait_for_turn(
                &observer_thread_id,
                &observer_turn_id,
                &mut observer_observation,
            )?;
            Ok::<_, anyhow::Error>((observer, observer_observation))
        });
        controller.wait_for_turn(&thread_id, &approval_turn_id, &mut controller_observation)?;
        let (_observer, observer_observation) = observer_handle
            .join()
            .map_err(|_| anyhow::anyhow!("observer reader thread panicked"))??;

        let approval_same_item_ids =
            controller_observation.item_ids == observer_observation.item_ids;
        let controller_requested_approval = controller_observation
            .server_request_methods
            .contains("item/commandExecution/requestApproval");
        let observer_requested_approval = observer_observation
            .server_request_methods
            .contains("item/commandExecution/requestApproval");
        let approval_request_owner =
            match (controller_requested_approval, observer_requested_approval) {
                (true, false) => "controller",
                (false, true) => "observer",
                (true, true) => "both",
                (false, false) => "none",
            };
        let approval_file_created = approval_file.exists();
        let approval_passed = controller_observation.passed()
            && observer_observation.passed()
            && approval_same_item_ids
            && approval_file_created
            && controller_requested_approval
            && observer_requested_approval;
        let _ = fs::remove_file(&approval_file);

        let passed = shared_passed && approval_passed;
        let summary = json!({
            "protocol": "codex-app-server-v2",
            "transport": "unix-websocket",
            "thread_id": thread_id,
            "shared_turn": {
                "turn_id": turn_id,
                "client_user_message_id": client_message_id,
                "same_item_ids": shared_same_item_ids,
                "controller": shared_controller_summary,
                "observer": shared_observer_summary,
                "passed": shared_passed,
            },
            "approval_turn": {
                "turn_id": approval_turn_id,
                "client_user_message_id": approval_client_id,
                "same_item_ids": approval_same_item_ids,
                "request_owner": approval_request_owner,
                "approval_file_created": approval_file_created,
                "controller": controller_observation.summary(),
                "observer": observer_observation.summary(),
                "passed": approval_passed,
            },
            "custom_observer_environment_removed": true,
            "passed": passed,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
        controller.request(
            "thread/delete",
            json!({ "threadId": thread_id }),
            &mut controller_observation,
            Some(&thread_id),
            None,
        )?;
        drop(server);
        if !passed {
            bail!("multi-client app-server or approval acceptance checks failed");
        }
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix_spike::run()
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("app_server_unix_spike requires a Unix host")
}
