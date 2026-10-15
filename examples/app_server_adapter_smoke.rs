#[cfg(unix)]
mod unix_smoke {
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Child;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use std::time::Instant;

    use anyhow::bail;
    use anyhow::Context;
    use cutex::agent_bus::delivery::AgentDeliveryMode;
    use cutex::agent_bus::model::AgentBusEnvelopeKind;
    use cutex::agent_bus::model::AgentBusMessage;
    use cutex::agent_bus::model::AgentBusRegisterRequest;
    use cutex::agent_bus::model::AgentMessageKind;
    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::app_server::bus_bridge::AppServerAgentBusBridge;
    use cutex::app_server::bus_bridge::AppServerAgentBusBridgeOptions;
    use cutex::app_server::bus_bridge::RuntimeAgentBus;
    use cutex::app_server::client::AppServerClient;
    use cutex::app_server::client::AppServerClientOptions;
    use cutex::app_server::client::AppServerEndpoint;
    use cutex::app_server::client::AppServerEvent;
    use cutex::app_server::commands::AppServerCommands;
    use cutex::app_server::commands::ThreadInterAgentMessageParams;
    use cutex::app_server::commands::ThreadReadParams;
    use cutex::app_server::commands::ThreadSettingsUpdateParams;
    use cutex::app_server::commands::ThreadStartParams;
    use serde_json::json;
    use serde_json::Value;

    const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
    const ISOLATED_CUTEX_ENV_VARS: &[&str] = &[
        "CODEX_THREAD_ID",
        "CUTEX_AGENT_BUS_TOKEN",
        "CUTEX_AGENT_BUS_URL",
        "CUTEX_AGENT_GROUPS",
        "CUTEX_AGENT_HINT",
        "CUTEX_AGENT_HOST_ID",
        "CUTEX_AGENT_ID",
        "CUTEX_AGENT_NAME",
        "CUTEX_OBSERVER_TOKEN",
        "CUTEX_OBSERVER_URL",
        "CUTEX_MANAGEMENT_OBSERVER_TOKEN",
        "CUTEX_MANAGEMENT_OBSERVER_URL",
        "CUTEX_RUNTIME_HEARTBEAT_TOKEN",
        "CUTEX_RUNTIME_HEARTBEAT_URL",
        "CUTEX_RUNTIME_LAUNCH_ID",
    ];

    struct SmokeOptions {
        codex_bin: String,
        durable: bool,
        inter_agent: bool,
    }

    struct AppServerProcess {
        child: Child,
        socket_dir: PathBuf,
        socket_path: PathBuf,
    }

    impl AppServerProcess {
        fn spawn(codex_bin: &str) -> anyhow::Result<Self> {
            let socket_dir = std::env::temp_dir().join(format!(
                "cutex-app-server-adapter-smoke-{}",
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

    struct OneShotAgentBus {
        message: Mutex<Option<AgentBusMessage>>,
        acknowledged: Mutex<Vec<String>>,
        registered: Mutex<bool>,
        unregistered: Mutex<bool>,
    }

    impl OneShotAgentBus {
        fn new(message: AgentBusMessage) -> Self {
            Self {
                message: Mutex::new(Some(message)),
                acknowledged: Mutex::new(Vec::new()),
                registered: Mutex::new(false),
                unregistered: Mutex::new(false),
            }
        }

        fn acknowledged(&self) -> anyhow::Result<Vec<String>> {
            self.acknowledged
                .lock()
                .map(|message_ids| message_ids.clone())
                .map_err(|_| anyhow::anyhow!("one-shot bus acknowledgement lock was poisoned"))
        }

        fn was_registered(&self) -> anyhow::Result<bool> {
            self.registered
                .lock()
                .map(|registered| *registered)
                .map_err(|_| anyhow::anyhow!("one-shot bus registration lock was poisoned"))
        }

        fn was_unregistered(&self) -> anyhow::Result<bool> {
            self.unregistered
                .lock()
                .map(|unregistered| *unregistered)
                .map_err(|_| anyhow::anyhow!("one-shot bus unregister lock was poisoned"))
        }
    }

    impl RuntimeAgentBus for OneShotAgentBus {
        fn register(&self, _request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
            *self
                .registered
                .lock()
                .map_err(|_| anyhow::anyhow!("one-shot bus registration lock was poisoned"))? =
                true;
            Ok(())
        }

        fn unregister(&self, _agent_id: &str) -> anyhow::Result<bool> {
            *self
                .unregistered
                .lock()
                .map_err(|_| anyhow::anyhow!("one-shot bus unregister lock was poisoned"))? = true;
            Ok(true)
        }

        fn poll(&self, _agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            let message = self
                .message
                .lock()
                .map_err(|_| anyhow::anyhow!("one-shot bus message lock was poisoned"))?
                .take();
            Ok(message.into_iter().collect())
        }

        fn ack(&self, _agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
            self.acknowledged
                .lock()
                .map_err(|_| anyhow::anyhow!("one-shot bus acknowledgement lock was poisoned"))?
                .extend_from_slice(message_ids);
            Ok(message_ids.len())
        }
    }

    pub fn run() -> anyhow::Result<()> {
        let total_started = Instant::now();
        let smoke_options = parse_options()?;

        let phase_started = Instant::now();
        let server = AppServerProcess::spawn(&smoke_options.codex_bin)?;
        let process_spawn_ms = elapsed_ms(phase_started);

        let mut client_options = AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
            socket_path: server.socket_path.clone(),
        });
        client_options.client_name = "cutex_adapter_smoke".to_string();
        client_options.client_title = "cutex adapter smoke".to_string();
        let phase_started = Instant::now();
        let client = AppServerClient::connect(client_options)?;
        let connect_initialize_ms = elapsed_ms(phase_started);
        let commands = AppServerCommands::new(client.handle());

        let phase_started = Instant::now();
        let started = commands.thread_start(&ThreadStartParams {
            cwd: Some(std::env::current_dir()?),
            personality: Some("pragmatic".to_string()),
            ephemeral: Some(!smoke_options.durable),
            ..Default::default()
        })?;
        let thread_start_ms = elapsed_ms(phase_started);
        let thread_id = response_thread_id(&started, "thread/start")?;
        let durable_bootstrap_ms = if smoke_options.durable {
            let phase_started = Instant::now();
            client.handle().request(
                "thread/inject_items",
                json!({
                    "threadId": thread_id,
                    "items": [{
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "cutex app-server durable smoke bootstrap"
                        }]
                    }]
                }),
            )?;
            Some(elapsed_ms(phase_started))
        } else {
            None
        };

        let phase_started = Instant::now();
        commands.thread_settings_update(&ThreadSettingsUpdateParams {
            thread_id: thread_id.clone(),
            effort: Some("low".to_string()),
            personality: Some("pragmatic".to_string()),
            ..Default::default()
        })?;
        let settings_update_ms = elapsed_ms(phase_started);

        let phase_started = Instant::now();
        let read = commands.thread_read(&ThreadReadParams {
            thread_id: thread_id.clone(),
            include_turns: false,
        })?;
        let thread_read_ms = elapsed_ms(phase_started);
        if response_thread_id(&read, "thread/read")? != thread_id {
            bail!("thread/read returned a different thread");
        }
        let (inter_agent, inter_agent_bridge_ms) = if smoke_options.inter_agent {
            let phase_started = Instant::now();
            let result = run_inter_agent_bridge_smoke(server.child.id(), &thread_id, &commands)?;
            (Some(result), Some(elapsed_ms(phase_started)))
        } else {
            (None, None)
        };

        let mut required = BTreeSet::from([
            "thread/started".to_string(),
            "thread/settings/updated".to_string(),
        ]);
        let expected_inter_agent_message_id = inter_agent.as_ref().and_then(|result| {
            result
                .get("messageId")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if expected_inter_agent_message_id.is_some() {
            required.insert("thread/interAgentMessage/received".to_string());
        }
        let mut observed = BTreeSet::new();
        let mut received_inter_agent_notifications = 0_u64;
        let phase_started = Instant::now();
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !required.is_subset(&observed) && Instant::now() < deadline {
            match client.recv_event_timeout(Duration::from_millis(100))? {
                Some(AppServerEvent::Notification(notification)) => {
                    if notification.method == "thread/interAgentMessage/received" {
                        let message_id = notification
                            .params
                            .as_ref()
                            .and_then(|params| params.get("messageId"))
                            .and_then(Value::as_str)
                            .context("inter-agent receive notification omitted messageId")?;
                        if Some(message_id) != expected_inter_agent_message_id.as_deref() {
                            bail!("inter-agent receive notification used an unexpected messageId");
                        }
                        received_inter_agent_notifications += 1;
                    }
                    observed.insert(notification.method);
                }
                Some(AppServerEvent::ServerRequest(request)) => {
                    client.handle().respond_error(
                        request.id,
                        -32601,
                        "adapter smoke does not resolve server requests",
                        None,
                    )?;
                }
                Some(AppServerEvent::ProtocolViolation { message }) => {
                    bail!("app-server protocol violation: {message}")
                }
                Some(AppServerEvent::Disconnected { reason }) => {
                    bail!("app-server disconnected during smoke: {reason}")
                }
                None => {}
            }
        }
        if !required.is_subset(&observed) {
            let missing = required.difference(&observed).collect::<Vec<_>>();
            bail!("missing native notifications: {missing:?}");
        }
        let duplicate_deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < duplicate_deadline {
            match client.recv_event_timeout(Duration::from_millis(25))? {
                Some(AppServerEvent::Notification(notification)) => {
                    if notification.method == "thread/interAgentMessage/received" {
                        received_inter_agent_notifications += 1;
                    }
                    observed.insert(notification.method);
                }
                Some(AppServerEvent::ServerRequest(request)) => {
                    client.handle().respond_error(
                        request.id,
                        -32601,
                        "adapter smoke does not resolve server requests",
                        None,
                    )?;
                }
                Some(AppServerEvent::ProtocolViolation { message }) => {
                    bail!("app-server protocol violation: {message}")
                }
                Some(AppServerEvent::Disconnected { reason }) => {
                    bail!("app-server disconnected during smoke: {reason}")
                }
                None => {}
            }
        }
        if expected_inter_agent_message_id.is_some() && received_inter_agent_notifications != 1 {
            bail!(
                "expected one inter-agent receive notification, observed {received_inter_agent_notifications}"
            );
        }
        let notification_drain_ms = elapsed_ms(phase_started);
        let total_ms = elapsed_ms(total_started);

        println!(
            "{}",
            serde_json::to_string(&json!({
                "protocol": "codex-app-server-v2",
                "adapter": "cutex::app_server",
                "threadId": thread_id,
                "initializeUserAgent": client.initialize_response().get("userAgent"),
                "observedMethods": observed,
                "durable": smoke_options.durable,
                "interAgent": inter_agent,
                "interAgentReceiveNotifications": received_inter_agent_notifications,
                "timingsMs": {
                    "processSpawn": process_spawn_ms,
                    "connectInitialize": connect_initialize_ms,
                    "threadStart": thread_start_ms,
                    "durableBootstrap": durable_bootstrap_ms,
                    "settingsUpdate": settings_update_ms,
                    "threadRead": thread_read_ms,
                    "interAgentBridge": inter_agent_bridge_ms,
                    "notificationDrain": notification_drain_ms,
                    "total": total_ms,
                },
                "passed": true,
            }))?
        );
        Ok(())
    }

    fn run_inter_agent_bridge_smoke(
        app_server_pid: u32,
        thread_id: &str,
        commands: &AppServerCommands,
    ) -> anyhow::Result<Value> {
        let runtime_agent_id = "cutex.app-server-adapter-smoke";
        let message_id = format!("stage6-inter-agent-{}", uuid::Uuid::new_v4());
        let bus = Arc::new(OneShotAgentBus::new(AgentBusMessage {
            id: message_id.clone(),
            kind: AgentBusEnvelopeKind::Message,
            from: "stage6 sender".to_string(),
            to: runtime_agent_id.to_string(),
            from_cutex_session_id: None,
            to_cutex_session_id: None,
            content: "exact refined inter-agent bridge smoke".to_string(),
            delivery_mode: AgentDeliveryMode::Passive,
            trigger_turn: false,
            created_at_epoch_secs: 1,
            sender_kind: AgentMessageKind::Agent,
            display_source: None,
            submit_mode: None,
            control_type: None,
            control_payload: None,
            external_action_id: None,
            external_message_id: None,
        }));
        let registration = AgentBusRegisterRequest {
            id: runtime_agent_id.to_string(),
            name: "app-server-adapter-smoke".to_string(),
            base_name: Some("app-server-adapter-smoke".to_string()),
            thread_name: None,
            path_key: None,
            session_id: Some(thread_id.to_string()),
            profile: "smoke".to_string(),
            cwd: std::env::current_dir()?.display().to_string(),
            pid: app_server_pid,
            host_id: Some("local-smoke".to_string()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Ephemeral,
        };
        let mut bridge_options =
            AppServerAgentBusBridgeOptions::new(registration, thread_id.to_string());
        bridge_options.poll_interval = Duration::from_millis(10);
        bridge_options.retry_interval = Duration::from_millis(10);
        bridge_options.registration_refresh_interval = Duration::from_secs(60);
        let bridge = AppServerAgentBusBridge::spawn(
            bus.clone(),
            Arc::new(commands.clone()),
            bridge_options,
        )?;

        let deadline = Instant::now() + EVENT_TIMEOUT;
        let status = loop {
            let status = bridge.status()?;
            if status.acknowledged_count == 1 {
                break status;
            }
            if let Some(error) = status.last_error.as_deref() {
                bail!("inter-agent bridge failed: {error}");
            }
            if Instant::now() >= deadline {
                bail!("inter-agent bridge did not acknowledge the message before timeout");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let submission_id = status
            .last_submission_id
            .as_deref()
            .filter(|submission_id| !submission_id.trim().is_empty())
            .context("inter-agent bridge omitted the native submission id")?
            .to_string();
        let retry_submission_id = commands
            .thread_inter_agent_message(&ThreadInterAgentMessageParams {
                thread_id: thread_id.to_string(),
                message_id: message_id.clone(),
                author: "/root/stage6_sender".to_string(),
                recipient: "/root".to_string(),
                other_recipients: Vec::new(),
                content: "duplicate delivery must be ignored".to_string(),
                delivery_mode: AgentDeliveryMode::Passive,
                author_metadata: None,
                recipient_metadata: None,
            })?
            .submission_id;
        if status.last_message_id.as_deref() != Some(message_id.as_str()) {
            bail!("inter-agent bridge status lost the bus message id");
        }
        if !bus.was_registered()? {
            bail!("inter-agent bridge did not register before polling");
        }
        if bus.acknowledged()? != [message_id.clone()] {
            bail!("one-shot bus did not acknowledge the exact submitted message");
        }
        bridge.shutdown()?;
        if !bus.was_unregistered()? {
            bail!("inter-agent bridge did not unregister during shutdown");
        }

        Ok(json!({
            "messageId": message_id,
            "deliveryMode": "passive",
            "submissionId": submission_id,
            "retrySubmissionId": retry_submission_id,
            "submittedCount": status.submitted_count,
            "acknowledgedCount": status.acknowledged_count,
            "unregistered": true,
        }))
    }

    fn response_thread_id(response: &Value, method: &str) -> anyhow::Result<String> {
        response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("{method} response omitted thread.id"))
    }

    fn elapsed_ms(started: Instant) -> f64 {
        started.elapsed().as_secs_f64() * 1_000.0
    }

    fn parse_options() -> anyhow::Result<SmokeOptions> {
        let mut codex_bin =
            std::env::var("CUTEX_CODEX_BIN").unwrap_or_else(|_| "cute-codex".to_string());
        let mut durable = false;
        let mut inter_agent = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--codex-bin" => {
                    codex_bin = args.next().context("--codex-bin requires a path")?;
                }
                "--durable" => durable = true,
                "--inter-agent" => inter_agent = true,
                "--help" | "-h" => {
                    println!(
                        "Usage: cargo run --example app_server_adapter_smoke -- [--codex-bin PATH] [--durable] [--inter-agent]"
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }
        Ok(SmokeOptions {
            codex_bin,
            durable,
            inter_agent,
        })
    }
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix_smoke::run()
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("app_server_adapter_smoke currently requires a Unix socket host")
}
