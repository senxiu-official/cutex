//! Long-lived app-server client ownership for the host management process.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use serde_json::Value;

use super::bus_bridge::AppServerAgentBusBridge;
use super::bus_bridge::AppServerAgentBusBridgeOptions;
use super::bus_bridge::AppServerAgentBusBridgeStatus;
use super::bus_bridge::InterAgentMessageSubmitter;
use super::bus_bridge::RuntimeAgentBus;
use super::client::AppServerClient;
use super::client::AppServerClientOptions;
use super::client::AppServerEndpoint;
use super::client::AppServerEvent;
use super::client::AppServerHandle;
use super::commands::AppServerCommands;
use super::commands::ThreadResumeParams;
use super::commands::ThreadStartParams;
use super::commands::TurnInterruptParams;
use super::journal::AppServerSchemaIdentity;
use super::journal::DiagnosticJournal;
use super::journal::DiagnosticJournalOptions;
use super::protocol::InboundMessage;
use super::runtime::endpoint_from_runtime_binding;
use crate::session::model::CutexAppServerRuntimeBinding;

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct AppServerControlSubmitter {
    endpoint: AppServerEndpoint,
    connection: Mutex<Option<AppServerControlConnection>>,
}

impl AppServerControlSubmitter {
    fn new(endpoint: AppServerEndpoint) -> Self {
        Self {
            endpoint,
            connection: Mutex::new(None),
        }
    }
}

impl InterAgentMessageSubmitter for AppServerControlSubmitter {
    fn thread_busy(&self, thread_id: &str) -> anyhow::Result<bool> {
        let mut connection = self.connection.lock()
            .map_err(|_| anyhow::anyhow!("app-server control connection lock was poisoned"))?;
        if connection.is_none() {
            *connection = Some(AppServerControlConnection::connect(self.endpoint.clone())?);
        }
        let result = connection.as_ref().expect("control connection was initialized")
            .commands.thread_busy(thread_id);
        if result.is_err() { connection.take(); }
        result
    }

    fn submit_inter_agent_message(
        &self,
        params: &super::commands::ThreadInterAgentMessageParams,
    ) -> anyhow::Result<String> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server control connection lock was poisoned"))?;
        if connection.is_none() {
            *connection = Some(AppServerControlConnection::connect(self.endpoint.clone())?);
        }
        let result = connection
            .as_ref()
            .expect("control connection was initialized")
            .commands
            .submit_inter_agent_message(params);
        if result.is_err() {
            connection.take();
        }
        result
    }

    fn inter_agent_message_status(
        &self,
        params: &super::commands::ThreadInterAgentMessageStatusParams,
    ) -> anyhow::Result<super::commands::ThreadInterAgentMessageStatusResponse> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server control connection lock was poisoned"))?;
        if connection.is_none() {
            *connection = Some(AppServerControlConnection::connect(self.endpoint.clone())?);
        }
        let result = connection
            .as_ref()
            .expect("control connection was initialized")
            .commands
            .thread_inter_agent_message_status(params)
            .map_err(Into::into);
        if result.is_err() {
            connection.take();
        }
        result
    }
}

struct AppServerControlConnection {
    commands: AppServerCommands,
    handle: AppServerHandle,
    stop_tx: mpsc::SyncSender<()>,
    worker: Option<JoinHandle<()>>,
}

impl AppServerControlConnection {
    fn connect(endpoint: AppServerEndpoint) -> anyhow::Result<Self> {
        let client = AppServerClient::connect(AppServerClientOptions::new(endpoint))
            .context("failed to connect app-server mailbox control channel")?;
        let handle = client.handle();
        let commands = AppServerCommands::new(handle.clone());
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || drain_control_events(client, stop_rx));
        Ok(Self {
            commands,
            handle,
            stop_tx,
            worker: Some(worker),
        })
    }
}

impl Drop for AppServerControlConnection {
    fn drop(&mut self) {
        let _ = self.handle.shutdown();
        let _ = self.stop_tx.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn drain_control_events(client: AppServerClient, stop_rx: mpsc::Receiver<()>) {
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        match client.recv_event_timeout(EVENT_POLL_INTERVAL) {
            Ok(Some(AppServerEvent::Disconnected { .. })) | Err(_) => break,
            Ok(Some(_)) | Ok(None) => {}
        }
    }
}

pub trait AppServerRuntimeEventSink: Send + Sync + 'static {
    /// Ephemeral owner conversations must not enter raw diagnostic files.
    /// Defaults to the existing behavior for all other consumers. This hook
    /// only controls diagnostics; handle_event still receives every event.
    fn should_journal_event(
        &self,
        _context: &AppServerRuntimeEventContext,
        _event: &AppServerEvent,
        _origin: &AppServerHandle,
    ) -> bool {
        true
    }
    /// Hold diagnostics while an ephemeral fork's native id is unresolved.
    /// This never delays event delivery; classification is retried in memory.
    fn should_defer_journal_event(
        &self,
        _context: &AppServerRuntimeEventContext,
        _event: &AppServerEvent,
        _origin: &AppServerHandle,
    ) -> bool {
        false
    }
    /// The worker's connection identity remains stable across same-generation
    /// reconnects, including while an old worker drains its final events.
    fn handle_event_from_connection(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &AppServerEvent,
        _origin: &AppServerHandle,
    ) -> anyhow::Result<()> {
        self.handle_event(context, event)
    }
    fn handle_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &AppServerEvent,
    ) -> anyhow::Result<()>;
    /// Single journal writer (`single_writer`): the ledger that decides
    /// which native events this process journals when several processes are
    /// connected to one runtime. `None`: every event (the default).
    fn event_ledger(&self) -> Option<Arc<super::single_writer::Ledger>> {
        None
    }
    /// An event another process journals: only connection-local claims
    /// (this process's own ephemeral threads) may run; no durable effects.
    fn handle_unowned_event(
        &self,
        _context: &AppServerRuntimeEventContext,
        _event: &AppServerEvent,
        _origin: &AppServerHandle,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

impl<F> AppServerRuntimeEventSink for F
where
    F: Fn(&AppServerRuntimeEventContext, &AppServerEvent) -> anyhow::Result<()>
        + Send
        + Sync
        + 'static,
{
    fn handle_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &AppServerEvent,
    ) -> anyhow::Result<()> {
        self(context, event)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerRuntimeEventContext {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub runtime_generation: u64,
    pub runtime_backend: String,
    pub launched_profile: Option<String>,
    /// Stable id of the launched profile; readers resolve the current name
    /// from it (`current_profile`), so a rename applies to a live runtime.
    pub launched_profile_id: Option<String>,
    /// Actual bootstrap selection and this occurrence's immutable model bindings.
    pub usage: crate::management::v2::usage::UsageRuntimeContext,
    pub schema: AppServerSchemaIdentity,
}

impl AppServerRuntimeEventContext {
    /// The launched profile under its current name (resolved by id).
    pub fn current_profile(&self) -> crate::profiles::identity::LaunchedProfile {
        crate::profiles::identity::LaunchedProfile::resolve(
            self.launched_profile_id.as_deref(),
            self.launched_profile.as_deref(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerManagedRuntimeStatus {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub runtime_generation: u64,
    pub connected: bool,
    pub active_turn_id: Option<String>,
    pub active_turn_observed_at: Option<String>,
    pub thread_status: Option<Value>,
    pub thread_status_observed_at: Option<String>,
    pub thread_settings: Option<Value>,
    pub thread_settings_source: Option<String>,
    pub thread_settings_complete: bool,
    pub thread_settings_observed_at: Option<String>,
    pub runtime_workspace_roots: Option<Value>,
    pub instruction_sources: Option<Value>,
    pub resume_snapshot_observed_at: Option<String>,
    pub event_method_counts: HashMap<String, u64>,
    pub initialized_user_agent: Option<String>,
    pub last_event_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppServerExactRuntimeHandleError {
    StaleGeneration { expected: u64, actual: Option<u64> },
    Unavailable(String),
}

impl std::fmt::Display for AppServerExactRuntimeHandleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration { expected, actual } => {
                write!(
                    formatter,
                    "app-server runtime generation changed: expected {expected}, actual {}",
                    actual
                        .map(|generation| generation.to_string())
                        .unwrap_or_else(|| "none".to_string())
                )
            }
            Self::Unavailable(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AppServerExactRuntimeHandleError {}

#[derive(Debug, Clone, PartialEq)]
pub struct AppServerRuntimeConnectResult {
    pub initialize_response: Value,
    pub resume_response: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppServerRuntimeStartResult {
    pub initialize_response: Value,
    pub start_response: Value,
    pub thread_id: String,
}

enum ThreadBootstrap {
    Resume(ThreadResumeParams),
    Start(ThreadStartParams),
}

/// Effective native config for `cwd` as the app-server itself resolves it
/// (its CODEX_HOME config.toml, `-c` launch overrides, project layers,
/// requirements). `None` when the read fails.
fn read_effective_native_config(
    handle: &AppServerHandle,
    cwd: Option<&std::path::Path>,
) -> Option<Value> {
    let params = match cwd {
        Some(cwd) => serde_json::json!({ "cwd": cwd }),
        None => serde_json::json!({}),
    };
    match handle.request("config/read", params) {
        Ok(response) => match response.get("config") {
            Some(config) if config.is_object() => Some(config.clone()),
            _ => {
                eprintln!("config/read returned no config object; skipping launch augmentation");
                None
            }
        },
        Err(error) => {
            eprintln!("config/read failed; skipping launch augmentation: {error}");
            None
        }
    }
}

/// Resolve cutex launch additions (file-citation / project memory sections and
/// the memory writable root) against the effective native config so they
/// append to, rather than shadow, configured developer instructions and roots.
fn resolve_bootstrap_launch_augmentation(
    handle: &AppServerHandle,
    bootstrap: ThreadBootstrap,
) -> ThreadBootstrap {
    match bootstrap {
        ThreadBootstrap::Resume(mut params) => {
            if let Some(augmentation) = params.launch_augmentation.take() {
                let config = if params.developer_instructions.is_none()
                    || augmentation.memory.is_some()
                {
                    read_effective_native_config(handle, params.cwd.as_deref())
                } else {
                    None
                };
                let resolved = crate::memory::resolve_launch_augmentation(
                    &augmentation,
                    params.developer_instructions.take(),
                    params.cwd.as_deref(),
                    params.permissions.as_deref(),
                    config.as_ref(),
                );
                params.developer_instructions = resolved.developer_instructions;
                if resolved.runtime_workspace_roots.is_some() {
                    params.runtime_workspace_roots = resolved.runtime_workspace_roots;
                }
            }
            ThreadBootstrap::Resume(params)
        }
        ThreadBootstrap::Start(mut params) => {
            if let Some(augmentation) = params.launch_augmentation.take() {
                let config = if params.developer_instructions.is_none()
                    || augmentation.memory.is_some()
                {
                    read_effective_native_config(handle, params.cwd.as_deref())
                } else {
                    None
                };
                let resolved = crate::memory::resolve_launch_augmentation(
                    &augmentation,
                    params.developer_instructions.take(),
                    params.cwd.as_deref(),
                    params.permissions.as_deref(),
                    config.as_ref(),
                );
                params.developer_instructions = resolved.developer_instructions;
                if resolved.runtime_workspace_roots.is_some() {
                    params.runtime_workspace_roots = resolved.runtime_workspace_roots;
                }
            }
            ThreadBootstrap::Start(params)
        }
    }
}

struct AppServerRuntimeBootstrapResult {
    initialize_response: Value,
    thread_response: Value,
    thread_id: String,
}

#[derive(Clone)]
pub struct AppServerRuntimeManager {
    runtimes: Arc<Mutex<HashMap<String, ManagedRuntime>>>,
    event_sink: Arc<dyn AppServerRuntimeEventSink>,
}

impl AppServerRuntimeManager {
    pub fn supports_cutex_ingress(&self, cutex_session_id: &str) -> anyhow::Result<bool> {
        let runtimes = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime lock unavailable"))?;
        Ok(runtimes
            .get(cutex_session_id)
            .is_some_and(|runtime| runtime.supports_cutex_ingress))
    }
    pub fn new(event_sink: Arc<dyn AppServerRuntimeEventSink>) -> Self {
        Self {
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            event_sink,
        }
    }

    pub fn without_event_sink() -> Self {
        Self::new(Arc::new(
            |_: &AppServerRuntimeEventContext, _: &AppServerEvent| Ok(()),
        ))
    }

    pub fn connect_binding(
        &self,
        cutex_session_id: &str,
        binding: &CutexAppServerRuntimeBinding,
        resume: ThreadResumeParams,
        runtime_generation: u64,
        runtime_backend: &str,
    ) -> anyhow::Result<AppServerRuntimeConnectResult> {
        let endpoint = endpoint_from_runtime_binding(binding)?;
        self.connect_endpoint_with_profile(
            cutex_session_id,
            endpoint,
            binding.diagnostic_journal_path.clone(),
            AppServerSchemaIdentity {
                version: binding.schema_version.clone(),
                sha256: binding.schema_sha256.clone(),
            },
            ThreadBootstrap::Resume(resume),
            runtime_generation,
            runtime_backend,
            binding.launched_profile.clone(),
            // r217 binding (name only): the profile that held the name at
            // launch (rename history), not whoever holds it now.
            binding.current_launched_profile_ref().id,
        )
        .map(|result| AppServerRuntimeConnectResult {
            initialize_response: result.initialize_response,
            resume_response: result.thread_response,
        })
    }

    /// A non-Codex adapter supplies its own authenticated endpoint and schema.
    /// It still uses the common bounded reader, diagnostics and state projection.
    pub fn connect_adapter(&self, observation: crate::runtime::claude::ClaudeObservation) -> anyhow::Result<AppServerRuntimeConnectResult> {
        self.connect_endpoint(
            &observation.record.cutex_session_id,
            observation.endpoint,
            observation.diagnostic_path.to_string_lossy().into_owned(),
            observation.schema,
            ThreadBootstrap::Resume(observation.resume),
            observation.record.runtime_generation,
            "claude_subscription",
            // Usage rows carry the session's profile (Claude API-account quota).
            observation.record.profile.clone(),
        ).map(|result| AppServerRuntimeConnectResult {
            initialize_response: result.initialize_response,
            resume_response: result.thread_response,
        })
    }

    /// Connect one newly launched app-server occurrence by invoking native
    /// `thread/start`. This is intentionally separate from normal durable
    /// runtime recovery, which must always resume its recorded thread.
    pub fn connect_new_thread_binding(
        &self,
        cutex_session_id: &str,
        binding: &CutexAppServerRuntimeBinding,
        start: ThreadStartParams,
        runtime_generation: u64,
        runtime_backend: &str,
    ) -> anyhow::Result<AppServerRuntimeStartResult> {
        let endpoint = endpoint_from_runtime_binding(binding)?;
        self.connect_endpoint_with_profile(
            cutex_session_id,
            endpoint,
            binding.diagnostic_journal_path.clone(),
            AppServerSchemaIdentity {
                version: binding.schema_version.clone(),
                sha256: binding.schema_sha256.clone(),
            },
            ThreadBootstrap::Start(start),
            runtime_generation,
            runtime_backend,
            binding.launched_profile.clone(),
            // r217 binding (name only): the profile that held the name at
            // launch (rename history), not whoever holds it now.
            binding.current_launched_profile_ref().id,
        )
        .map(|result| AppServerRuntimeStartResult {
            initialize_response: result.initialize_response,
            start_response: result.thread_response,
            thread_id: result.thread_id,
        })
    }

    pub fn commands(&self, cutex_session_id: &str) -> anyhow::Result<AppServerCommands> {
        Ok(AppServerCommands::new(self.handle(cutex_session_id)?))
    }

    /// Interrupt the currently running turn, if this runtime has one.
    ///
    /// The app-server does not acknowledge `turn/interrupt` until it has emitted
    /// the terminal `turn/completed` notification. Callers should then stop the
    /// app-server gracefully so its normal shutdown path flushes the rollout.
    pub fn interrupt_active_turn(&self, cutex_session_id: &str) -> anyhow::Result<Option<String>> {
        let Some(status) = self.status(cutex_session_id)? else {
            return Ok(None);
        };
        if !status.connected {
            return Ok(None);
        }
        let Some(turn_id) = status.active_turn_id else {
            return Ok(None);
        };
        self.commands(cutex_session_id)?
            .turn_interrupt(&TurnInterruptParams {
                thread_id: status.thread_id,
                turn_id: turn_id.clone(),
            })
            .with_context(|| {
                format!("failed to interrupt active turn {turn_id} for runtime {cutex_session_id}")
            })?;
        Ok(Some(turn_id))
    }

    pub fn handle(&self, cutex_session_id: &str) -> anyhow::Result<AppServerHandle> {
        let runtimes = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
        let runtime = runtimes
            .get(cutex_session_id)
            .with_context(|| format!("app-server runtime is not connected: {cutex_session_id}"))?;
        let connected = runtime
            .status
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime status lock was poisoned"))?
            .connected
            && runtime.runtime_alive.load(Ordering::Acquire);
        if !connected {
            anyhow::bail!("app-server runtime is disconnected: {cutex_session_id}");
        }
        Ok(runtime.handle.clone())
    }

    pub fn handle_for_generation(
        &self,
        cutex_session_id: &str,
        expected_generation: u64,
    ) -> Result<AppServerHandle, AppServerExactRuntimeHandleError> {
        let runtimes = self.runtimes.lock().map_err(|_| {
            AppServerExactRuntimeHandleError::Unavailable(
                "app-server runtime manager lock was poisoned".to_string(),
            )
        })?;
        let Some(runtime) = runtimes.get(cutex_session_id) else {
            return Err(AppServerExactRuntimeHandleError::StaleGeneration {
                expected: expected_generation,
                actual: None,
            });
        };
        let status = runtime.status.lock().map_err(|_| {
            AppServerExactRuntimeHandleError::Unavailable(
                "app-server runtime status lock was poisoned".to_string(),
            )
        })?;
        if status.runtime_generation != expected_generation {
            return Err(AppServerExactRuntimeHandleError::StaleGeneration {
                expected: expected_generation,
                actual: Some(status.runtime_generation),
            });
        }
        if !status.connected || !runtime.runtime_alive.load(Ordering::Acquire) {
            return Err(AppServerExactRuntimeHandleError::Unavailable(format!(
                "app-server runtime is disconnected: {cutex_session_id}"
            )));
        }
        Ok(runtime.handle.clone())
    }

    pub fn status(
        &self,
        cutex_session_id: &str,
    ) -> anyhow::Result<Option<AppServerManagedRuntimeStatus>> {
        let status = {
            let runtimes = self
                .runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
            runtimes
                .get(cutex_session_id)
                .map(|runtime| (runtime.status.clone(), runtime.runtime_alive.clone()))
        };
        status
            .map(|(status, runtime_alive)| {
                status
                    .lock()
                    .map(|status| {
                        let mut status = status.clone();
                        if !runtime_alive.load(Ordering::Acquire) {
                            status.connected = false;
                        }
                        status
                    })
                    .map_err(|_| anyhow::anyhow!("app-server runtime status lock was poisoned"))
            })
            .transpose()
    }

    /// The exact launch-time developer override, including Cutex augmentation.
    /// Runtime memory only; never part of public status or a durable record.
    pub fn developer_instructions_for_generation(
        &self,
        session: &str,
        generation: u64,
    ) -> anyhow::Result<Option<String>> {
        let runtimes = self.runtimes.lock()
            .map_err(|_| anyhow::anyhow!("runtime manager lock poisoned"))?;
        let runtime = runtimes.get(session).context("runtime not connected")?;
        let status = runtime.status.lock()
            .map_err(|_| anyhow::anyhow!("runtime status lock poisoned"))?;
        anyhow::ensure!(
            status.runtime_generation == generation
                && status.connected
                && runtime.runtime_alive.load(Ordering::Acquire),
            "runtime generation changed"
        );
        Ok(runtime.developer_instructions.clone())
    }

    pub fn note_active_turn(
        &self,
        cutex_session_id: &str,
        active_turn_id: Option<String>,
    ) -> anyhow::Result<()> {
        let status = {
            let runtimes = self
                .runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
            runtimes
                .get(cutex_session_id)
                .map(|runtime| runtime.status.clone())
                .with_context(|| {
                    format!("app-server runtime is not connected: {cutex_session_id}")
                })?
        };
        let mut status = status
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime status lock was poisoned"))?;
        status.active_turn_id = active_turn_id;
        status.active_turn_observed_at = Some(Utc::now().to_rfc3339());
        Ok(())
    }

    pub fn start_agent_bus_bridge(
        &self,
        cutex_session_id: &str,
        bus: Arc<dyn RuntimeAgentBus>,
        options: AppServerAgentBusBridgeOptions,
    ) -> anyhow::Result<AppServerAgentBusBridgeStatus> {
        let runtime_status = self
            .status(cutex_session_id)?
            .with_context(|| format!("app-server runtime is not connected: {cutex_session_id}"))?;
        if !runtime_status.connected {
            anyhow::bail!("app-server runtime is disconnected: {cutex_session_id}");
        }
        if options.thread_id != runtime_status.thread_id {
            anyhow::bail!(
                "agent-bus bridge threadId {} does not match managed thread {}",
                options.thread_id,
                runtime_status.thread_id
            );
        }
        {
            let mut runtimes = self
                .runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
            let runtime = runtimes.get_mut(cutex_session_id).with_context(|| {
                format!("app-server runtime is not connected: {cutex_session_id}")
            })?;
            if runtime.agent_bus_bridge.is_some() || runtime.agent_bus_bridge_starting {
                anyhow::bail!("agent-bus bridge is already running: {cutex_session_id}");
            }
            runtime.agent_bus_bridge_starting = true;
        }
        let bridge = match (|| {
            let (runtime_alive, endpoint) = {
                let runtimes = self
                    .runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
                runtimes
                    .get(cutex_session_id)
                    .map(|runtime| (runtime.runtime_alive.clone(), runtime.endpoint.clone()))
                    .with_context(|| {
                        format!("app-server runtime is not connected: {cutex_session_id}")
                    })?
            };
            AppServerAgentBusBridge::spawn_with_liveness(
                bus,
                Arc::new(AppServerControlSubmitter::new(endpoint)),
                options,
                runtime_alive,
            )
        })() {
            Ok(bridge) => bridge,
            Err(error) => {
                self.clear_agent_bus_bridge_reservation(cutex_session_id);
                return Err(error);
            }
        };
        let bridge_status = match bridge.status() {
            Ok(status) => status,
            Err(error) => {
                self.clear_agent_bus_bridge_reservation(cutex_session_id);
                return Err(error);
            }
        };
        let mut bridge = Some(bridge);
        let insertion = {
            let mut runtimes = self
                .runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
            match runtimes.get_mut(cutex_session_id) {
                Some(runtime) => {
                    runtime.agent_bus_bridge_starting = false;
                    if runtime.agent_bus_bridge.is_some() {
                        Err(anyhow::anyhow!(
                            "agent-bus bridge is already running: {cutex_session_id}"
                        ))
                    } else {
                        let connected = runtime
                            .status
                            .lock()
                            .map_err(|_| {
                                anyhow::anyhow!("app-server runtime status lock was poisoned")
                            })?
                            .connected
                            && runtime.runtime_alive.load(Ordering::Acquire);
                        if !connected {
                            Err(anyhow::anyhow!(
                                "app-server runtime disconnected while starting agent-bus bridge: {cutex_session_id}"
                            ))
                        } else {
                            runtime.agent_bus_bridge = bridge.take();
                            Ok(())
                        }
                    }
                }
                None => Err(anyhow::anyhow!(
                    "app-server runtime disconnected while starting agent-bus bridge: {cutex_session_id}"
                )),
            }
        };
        if let Err(error) = insertion {
            if let Some(bridge) = bridge {
                let _ = bridge.shutdown();
            }
            return Err(error);
        }
        Ok(bridge_status)
    }

    pub fn refresh_agent_bus_registration(
        &self,
        cutex_session_id: &str,
    ) -> anyhow::Result<Option<AppServerAgentBusBridgeStatus>> {
        let runtimes = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
        let runtime = runtimes
            .get(cutex_session_id)
            .with_context(|| format!("app-server runtime is not connected: {cutex_session_id}"))?;
        runtime
            .agent_bus_bridge
            .as_ref()
            .map(AppServerAgentBusBridge::refresh_registration)
            .transpose()
    }

    pub fn agent_bus_bridge_status(
        &self,
        cutex_session_id: &str,
    ) -> anyhow::Result<Option<AppServerAgentBusBridgeStatus>> {
        let runtimes = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
        let runtime = runtimes
            .get(cutex_session_id)
            .with_context(|| format!("app-server runtime is not connected: {cutex_session_id}"))?;
        runtime
            .agent_bus_bridge
            .as_ref()
            .map(AppServerAgentBusBridge::status)
            .transpose()
    }

    fn clear_agent_bus_bridge_reservation(&self, cutex_session_id: &str) {
        if let Ok(mut runtimes) = self.runtimes.lock() {
            if let Some(runtime) = runtimes.get_mut(cutex_session_id) {
                runtime.agent_bus_bridge_starting = false;
            }
        }
    }

    pub fn agent_bus_status(
        &self,
        cutex_session_id: &str,
    ) -> anyhow::Result<Option<AppServerAgentBusBridgeStatus>> {
        let runtimes = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
        runtimes
            .get(cutex_session_id)
            .and_then(|runtime| runtime.agent_bus_bridge.as_ref())
            .map(AppServerAgentBusBridge::status)
            .transpose()
    }

    pub fn stop_agent_bus_bridge(&self, cutex_session_id: &str) -> anyhow::Result<bool> {
        let bridge = {
            let mut runtimes = self
                .runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
            let Some(runtime) = runtimes.get_mut(cutex_session_id) else {
                return Ok(false);
            };
            if runtime.agent_bus_bridge_starting {
                anyhow::bail!("agent-bus bridge is still starting: {cutex_session_id}");
            }
            runtime.agent_bus_bridge.take()
        };
        let Some(bridge) = bridge else {
            return Ok(false);
        };
        bridge.shutdown()?;
        Ok(true)
    }

    pub fn disconnect(&self, cutex_session_id: &str) -> anyhow::Result<bool> {
        let runtime = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?
            .remove(cutex_session_id);
        let Some(runtime) = runtime else {
            return Ok(false);
        };
        stop_managed_runtime(runtime)?;
        Ok(true)
    }

    pub fn disconnect_all(&self) -> anyhow::Result<()> {
        let runtimes = {
            let mut entries = self
                .runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
            entries
                .drain()
                .map(|(_, runtime)| runtime)
                .collect::<Vec<_>>()
        };
        for runtime in runtimes {
            stop_managed_runtime(runtime)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn connect_endpoint(
        &self,
        cutex_session_id: &str,
        endpoint: AppServerEndpoint,
        diagnostic_journal_path: String,
        schema: AppServerSchemaIdentity,
        bootstrap: ThreadBootstrap,
        runtime_generation: u64,
        runtime_backend: &str,
        launched_profile: Option<String>,
    ) -> anyhow::Result<AppServerRuntimeBootstrapResult> {
        self.connect_endpoint_with_profile(
            cutex_session_id,
            endpoint,
            diagnostic_journal_path,
            schema,
            bootstrap,
            runtime_generation,
            runtime_backend,
            launched_profile,
            None,
        )
    }

    /// `launched_profile_id` is the identity; a binding written before r218
    /// has only the name, which is mapped to the current profile id once here.
    #[allow(clippy::too_many_arguments)]
    fn connect_endpoint_with_profile(
        &self,
        cutex_session_id: &str,
        endpoint: AppServerEndpoint,
        diagnostic_journal_path: String,
        schema: AppServerSchemaIdentity,
        bootstrap: ThreadBootstrap,
        runtime_generation: u64,
        runtime_backend: &str,
        launched_profile: Option<String>,
        launched_profile_id: Option<String>,
    ) -> anyhow::Result<AppServerRuntimeBootstrapResult> {
        let launched_profile_id = launched_profile_id
            .filter(|id| !id.trim().is_empty())
            .or_else(|| launched_profile.as_deref().and_then(crate::profiles::identity::id_for_name));
        if cutex_session_id.trim().is_empty() {
            anyhow::bail!("cutex_session_id must not be empty");
        }
        if matches!(&bootstrap, ThreadBootstrap::Resume(resume) if resume.thread_id.trim().is_empty())
        {
            anyhow::bail!("threadId must not be empty");
        }
        let stale_runtime = {
            let mut runtimes = self
                .runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
            let connected = runtimes
                .get(cutex_session_id)
                .map(|runtime| {
                    runtime
                        .status
                        .lock()
                        .map(|status| {
                            status.connected && runtime.runtime_alive.load(Ordering::Acquire)
                        })
                        .map_err(|_| anyhow::anyhow!("app-server runtime status lock was poisoned"))
                })
                .transpose()?;
            match connected {
                Some(true) => {
                    anyhow::bail!("app-server runtime is already connected: {cutex_session_id}")
                }
                Some(false) => runtimes.remove(cutex_session_id),
                None => None,
            }
        };
        if let Some(runtime) = stale_runtime {
            stop_managed_runtime(runtime)?;
        }

        let mut options = AppServerClientOptions::new(endpoint.clone());
        if runtime_backend == "claude_subscription" {
            // Management relays Claude approvals and AskUserQuestion to its
            // clients (server-request-responses), like Codex server requests;
            // without this the adapter only asks an attached TUI.
            options
                .extra_capabilities
                .insert("claudeInteractiveApprovals".to_string(), Value::Bool(true));
        }
        // Registered before connecting: an owner adopting meanwhile hands
        // over rather than taking every event (`single_writer`).
        let mut ownership = self
            .event_sink
            .event_ledger()
            .map(|ledger| ledger.open(cutex_session_id, runtime_generation));
        let client = AppServerClient::connect(options)?;
        let initialize_response = client.initialize_response().clone();
        crate::launch::stock::validate_ingress_capability(
            crate::launch::stock::schema_has_external_input(&schema.version, &schema.sha256),
            &initialize_response,
        )?;
        let handle = client.handle();
        let commands = AppServerCommands::new(handle.clone());
        let bootstrap = resolve_bootstrap_launch_augmentation(&handle, bootstrap);
        let developer_instructions = match &bootstrap {
            ThreadBootstrap::Resume(params) => params.developer_instructions.clone(),
            ThreadBootstrap::Start(params) => params.developer_instructions.clone(),
        };
        let (thread_response, expected_thread_id, settings_source) = match bootstrap {
            ThreadBootstrap::Resume(resume) => {
                let expected = resume.thread_id.clone();
                (
                    commands.thread_resume(&resume)?,
                    Some(expected),
                    "thread/resume",
                )
            }
            ThreadBootstrap::Start(start) => (commands.thread_start(&start)?, None, "thread/start"),
        };
        let response_thread_id = thread_response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("thread bootstrap response omitted thread.id")?
            .to_string();
        if let Some(expected_thread_id) = expected_thread_id {
            if response_thread_id != expected_thread_id {
                anyhow::bail!(
                    "thread/resume returned {response_thread_id}, expected {expected_thread_id}"
                );
            }
        }

        if let Some(turn) = active_turn_id_from_thread_response(&thread_response) {
            crate::management::v2::admission::turn_started(cutex_session_id);
            if let Ok(store) = crate::asks::store::AskStore::open_default() {
                let _ = store.inspect(|state| crate::management::v2::admission::restore_lease_turn(cutex_session_id, &turn, state));
            }
            if crate::management::v2::agent_bus_state::agent_bus_message_repository()
                .and_then(|repository| repository.admission_started_turn(cutex_session_id, &turn))
                .unwrap_or(false)
            {
                crate::management::v2::admission::note_turn(cutex_session_id, "delivery", None);
            }
        }
        let bootstrap_observed_at = Utc::now().to_rfc3339();
        let status = Arc::new(Mutex::new(AppServerManagedRuntimeStatus {
            cutex_session_id: cutex_session_id.to_string(),
            thread_id: response_thread_id.clone(),
            runtime_generation,
            connected: true,
            active_turn_id: active_turn_id_from_thread_response(&thread_response),
            active_turn_observed_at: Some(bootstrap_observed_at.clone()),
            thread_status: thread_response.pointer("/thread/status").cloned(),
            thread_status_observed_at: Some(bootstrap_observed_at.clone()),
            thread_settings: Some(thread_settings_from_resume_response(&thread_response)),
            thread_settings_source: Some(settings_source.to_string()),
            thread_settings_complete: false,
            thread_settings_observed_at: Some(bootstrap_observed_at.clone()),
            runtime_workspace_roots: thread_response.get("runtimeWorkspaceRoots").cloned(),
            instruction_sources: thread_response.get("instructionSources").cloned(),
            resume_snapshot_observed_at: Some(bootstrap_observed_at),
            event_method_counts: HashMap::new(),
            initialized_user_agent: initialize_response
                .get("userAgent")
                .and_then(Value::as_str)
                .map(str::to_string),
            last_event_at: None,
            last_error: None,
        }));
        let event_context = AppServerRuntimeEventContext {
            cutex_session_id: cutex_session_id.to_string(),
            thread_id: response_thread_id.clone(),
            runtime_generation,
            runtime_backend: runtime_backend.to_string(),
            usage: crate::management::v2::usage::UsageRuntimeContext::capture(
                cutex_session_id, &response_thread_id, runtime_generation,
                launched_profile_id.as_deref(), &thread_settings_from_resume_response(&thread_response),
            ),
            launched_profile,
            launched_profile_id,
            schema: schema.clone(),
        };
        let mut journal_options = DiagnosticJournalOptions::new(diagnostic_journal_path.into());
        // Neither private stock nor U+S6 speaks the legacy K business ingress.
        let supports_cutex_ingress = !crate::launch::stock::is_private_native_schema(&schema.sha256)
            && !crate::launch::stock::is_stock_schema_version(&schema.version);
        journal_options.schema = schema;
        let journal = DiagnosticJournal::new(journal_options)?;
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let runtime_alive = Arc::new(AtomicBool::new(true));
        let worker_runtime_alive = runtime_alive.clone();
        let worker_status = status.clone();
        let event_sink = self.event_sink.clone();
        // Subscribed: the thread bootstrap answered and no event of this
        // connection has been handled yet.
        if let Some(ownership) = ownership.as_mut() {
            ownership.activate();
        }
        let worker = thread::spawn(move || {
            run_event_worker(
                client,
                stop_rx,
                worker_status,
                journal,
                event_sink,
                event_context,
                worker_runtime_alive,
                ownership,
            )
        });
        let runtime = ManagedRuntime {
            developer_instructions,
            supports_cutex_ingress,
            endpoint,
            handle,
            status,
            stop_tx,
            worker: Some(worker),
            agent_bus_bridge: None,
            agent_bus_bridge_starting: false,
            runtime_alive,
        };

        let mut runtimes = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("app-server runtime manager lock was poisoned"))?;
        if runtimes.contains_key(cutex_session_id) {
            drop(runtimes);
            stop_managed_runtime(runtime)?;
            anyhow::bail!("app-server runtime connected concurrently: {cutex_session_id}");
        }
        runtimes.insert(cutex_session_id.to_string(), runtime);
        Ok(AppServerRuntimeBootstrapResult {
            initialize_response,
            thread_response,
            thread_id: response_thread_id,
        })
    }
}

struct ManagedRuntime {
    developer_instructions: Option<String>,
    supports_cutex_ingress: bool,
    endpoint: AppServerEndpoint,
    handle: AppServerHandle,
    status: Arc<Mutex<AppServerManagedRuntimeStatus>>,
    stop_tx: mpsc::SyncSender<()>,
    worker: Option<JoinHandle<()>>,
    agent_bus_bridge: Option<AppServerAgentBusBridge>,
    agent_bus_bridge_starting: bool,
    runtime_alive: Arc<AtomicBool>,
}

fn stop_managed_runtime(mut runtime: ManagedRuntime) -> anyhow::Result<()> {
    #[cfg(feature = "stock-launch-test-hook")]
    super::private_restart_phase("manager.bridge_shutdown.begin");
    runtime.runtime_alive.store(false, Ordering::Release);
    let bridge_result = runtime
        .agent_bus_bridge
        .take()
        .map(AppServerAgentBusBridge::shutdown)
        .transpose();
    #[cfg(feature = "stock-launch-test-hook")]
    super::private_restart_phase("manager.bridge_shutdown.end");
    let _ = runtime.stop_tx.try_send(());
    #[cfg(feature = "stock-launch-test-hook")]
    super::private_restart_phase("manager.handle_shutdown.begin");
    let _ = runtime.handle.shutdown();
    #[cfg(feature = "stock-launch-test-hook")]
    super::private_restart_phase("manager.event_join.begin");
    let worker_result = if let Some(worker) = runtime.worker.take() {
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("app-server event worker panicked"))
    } else {
        Ok(())
    };
    #[cfg(feature = "stock-launch-test-hook")]
    super::private_restart_phase("manager.event_join.end");
    bridge_result?;
    worker_result
}

#[allow(clippy::too_many_arguments)]
fn run_event_worker(
    client: AppServerClient,
    stop_rx: mpsc::Receiver<()>,
    status: Arc<Mutex<AppServerManagedRuntimeStatus>>,
    journal: DiagnosticJournal,
    event_sink: Arc<dyn AppServerRuntimeEventSink>,
    event_context: AppServerRuntimeEventContext,
    runtime_alive: Arc<AtomicBool>,
    // Dropped when the worker ends: releases this connection's lease or
    // writer registration.
    mut ownership: Option<super::single_writer::ConnectionOwnership>,
) {
    let origin = client.handle();
    // An event pulled while merging a delta backlog, handled next.
    let mut carried: Option<AppServerEvent> = None;
    let mut deferred_diagnostics: Vec<(AppServerEvent, usize)> = Vec::new();
    let mut deferred_bytes: usize = 0;
    loop {
        if stop_rx.try_recv().is_ok() {
            mark_disconnected(&status, "runtime manager disconnected the client");
            break;
        }
        deferred_diagnostics.retain(|(event, bytes)| {
            if event_sink.should_defer_journal_event(&event_context, event, &origin) {
                return true;
            }
            deferred_bytes = deferred_bytes.saturating_sub(*bytes);
            if event_sink.should_journal_event(&event_context, event, &origin) {
                if let Some(inbound) = event_as_inbound(event) {
                    if let Err(error) = journal.append(&inbound) {
                        mark_error(&status, format!(
                            "failed to append deferred app-server diagnostic event: {error:#}"
                        ));
                    }
                }
            }
            false
        });
        let received = match carried.take() {
            Some(event) => Ok(Some(event)),
            None => client.recv_event_timeout(EVENT_POLL_INTERVAL),
        };
        match received {
            Ok(Some(mut event)) => {
                let split_after = ownership
                    .as_ref()
                    .and_then(super::single_writer::ConnectionOwnership::split_after);
                carried = super::delta_coalesce::coalesce_ready_until(&mut event, split_after, || {
                    client.recv_event_timeout(Duration::ZERO).ok().flatten()
                });
                update_runtime_status_from_event(&status, &event);
                let owned = ownership.as_mut().is_none_or(|ownership| ownership.handles(&event));
                replay_taken_over_events(
                    ownership.as_mut(),
                    &journal,
                    &event_sink,
                    &event_context,
                    &origin,
                    &status,
                );
                if !owned {
                    // Another process journals this event: in-memory status
                    // only (above), no diagnostics or durable effects.
                    mark_event_received(&status);
                    let disconnected = match &event {
                        AppServerEvent::Disconnected { reason } => {
                            runtime_alive.store(false, Ordering::Release);
                            mark_disconnected(&status, reason);
                            true
                        }
                        _ => false,
                    };
                    if let Err(error) =
                        event_sink.handle_unowned_event(&event_context, &event, &origin)
                    {
                        mark_error(&status, format!("failed to handle app-server event: {error:#}"));
                    }
                    if disconnected {
                        break;
                    }
                    continue;
                }
                if event_sink.should_defer_journal_event(&event_context, &event, &origin) {
                    let bytes = event_as_inbound(&event)
                        .map(|message| message.raw().to_string().len())
                        .unwrap_or(0);
                    // Diagnostics were already best effort and bounded on disk.
                    // Never spill undecidable raw data to disk on overflow.
                    if deferred_diagnostics.len() < 512
                        && deferred_bytes.saturating_add(bytes) <= 8 * 1024 * 1024
                    {
                        deferred_bytes += bytes;
                        deferred_diagnostics.push((event.clone(), bytes));
                    } else {
                        mark_error(&status,
                            "ephemeral diagnostic deferral capacity exceeded; omitted diagnostic record".to_owned());
                    }
                } else if let Some(inbound) = event_as_inbound(&event)
                    .filter(|_| event_sink.should_journal_event(&event_context, &event, &origin))
                {
                    if let Err(error) = journal.append(&inbound) {
                        mark_error(
                            &status,
                            format!("failed to append app-server diagnostic event: {error:#}"),
                        );
                    }
                }
                mark_event_received(&status);
                let disconnected_reason = match &event {
                    AppServerEvent::Disconnected { reason } => {
                        // Publish the transport loss before invoking any
                        // durable sink side effects. An online request racing
                        // this event must enter persisted-binding recovery.
                        runtime_alive.store(false, Ordering::Release);
                        mark_disconnected(&status, reason);
                        Some(reason)
                    }
                    _ => None,
                };
                #[cfg(feature = "stock-launch-test-hook")]
                super::private_restart_phase("event.sink.begin");
                if let Err(error) = event_sink
                    .handle_event_from_connection(&event_context, &event, &origin)
                {
                    let reason = format!("failed to durably handle app-server event: {error:#}");
                    mark_disconnected(&status, &reason);
                    break;
                }
                #[cfg(feature = "stock-launch-test-hook")]
                super::private_restart_phase("event.sink.end");
                if disconnected_reason.is_some() {
                    break;
                }
            }
            Ok(None) => {
                if let Some(ownership) = ownership.as_mut() {
                    ownership.poll();
                }
                replay_taken_over_events(
                    ownership.as_mut(),
                    &journal,
                    &event_sink,
                    &event_context,
                    &origin,
                    &status,
                );
            }
            Err(error) => {
                let reason = error.to_string();
                runtime_alive.store(false, Ordering::Release);
                mark_disconnected(&status, &reason);
                let event = AppServerEvent::Disconnected { reason };
                let handled = if ownership.as_mut().is_none_or(|ownership| ownership.handles(&event)) {
                    event_sink.handle_event_from_connection(&event_context, &event, &origin)
                } else {
                    event_sink.handle_unowned_event(&event_context, &event, &origin)
                };
                if let Err(error) = handled {
                    mark_error(
                        &status,
                        format!("failed to durably handle app-server disconnect: {error:#}"),
                    );
                }
                break;
            }
        }
    }
    // Preserve definitely unrelated deferred diagnostics on shutdown too. A
    // failed fork whose id never arrived remains unclassifiable: emit bounded
    // count-only evidence rather than silently drop it or persist private text.
    let mut omitted_count = 0usize;
    let mut omitted_bytes = 0usize;
    for (event, bytes) in deferred_diagnostics {
        if event_sink.should_defer_journal_event(&event_context, &event, &origin) {
            omitted_count += 1;
            omitted_bytes += bytes;
        } else if event_sink.should_journal_event(&event_context, &event, &origin) {
            if let Some(inbound) = event_as_inbound(&event) {
                let _ = journal.append(&inbound);
            }
        }
    }
    if omitted_count > 0 {
        let params = serde_json::json!({
            "count": omitted_count,
            "bytes": omitted_bytes,
            "reason": "runtime closed before ephemeral fork identity resolved"
        });
        let method = "cutex/diagnostics/ephemeralOmitted".to_owned();
        let raw = serde_json::json!({"method":method,"params":params});
        let _ = journal.append(&InboundMessage::Notification(super::protocol::RpcNotification {
            method, params: Some(params), raw,
        }));
    }
    runtime_alive.store(false, Ordering::Release);
}

/// Journals the events of a range this connection took over from another
/// writer that the journal does not hold yet (`single_writer`), in stream
/// order and before the event at hand.
fn replay_taken_over_events(
    ownership: Option<&mut super::single_writer::ConnectionOwnership>,
    journal: &DiagnosticJournal,
    event_sink: &Arc<dyn AppServerRuntimeEventSink>,
    event_context: &AppServerRuntimeEventContext,
    origin: &AppServerHandle,
    status: &Arc<Mutex<AppServerManagedRuntimeStatus>>,
) {
    let Some(ownership) = ownership else {
        return;
    };
    for event in ownership.take_replay() {
        if let Some(inbound) = event_as_inbound(&event)
            .filter(|_| event_sink.should_journal_event(event_context, &event, origin))
        {
            let _ = journal.append(&inbound);
        }
        if let Err(error) = event_sink.handle_event_from_connection(event_context, &event, origin) {
            mark_error(status, format!("failed to handle a taken-over app-server event: {error:#}"));
        }
    }
}

fn active_turn_id_from_thread_response(response: &Value) -> Option<String> {
    if matches!(
        response
            .pointer("/thread/status/type")
            .and_then(Value::as_str),
        Some("idle" | "notLoaded" | "systemError")
    ) {
        return None;
    }
    response
        .pointer("/initialTurnsPage/data")
        .or_else(|| response.pointer("/thread/turns"))
        .and_then(Value::as_array)
        .and_then(|turns| {
            turns
                .iter()
                .rev()
                .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        })
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn thread_settings_from_resume_response(response: &Value) -> Value {
    let mut settings = serde_json::Map::new();
    for (source, target) in [
        ("model", "model"),
        ("modelProvider", "modelProvider"),
        ("serviceTier", "serviceTier"),
        ("cwd", "cwd"),
        ("approvalPolicy", "approvalPolicy"),
        ("approvalsReviewer", "approvalsReviewer"),
        ("sandbox", "sandboxPolicy"),
        ("activePermissionProfile", "activePermissionProfile"),
        ("reasoningEffort", "effort"),
        // The native restores a persisted Plan / Default mode on resume; without it the
        // snapshot reads as "no mode" and clients fall back to Default while the thread plans.
        ("collaborationMode", "collaborationMode"),
    ] {
        if let Some(value) = response.get(source) {
            settings.insert(target.to_string(), value.clone());
        }
    }
    Value::Object(settings)
}

fn update_runtime_status_from_event(
    status: &Arc<Mutex<AppServerManagedRuntimeStatus>>,
    event: &AppServerEvent,
) {
    let AppServerEvent::Notification(notification) = event else {
        return;
    };
    let turn_id = notification
        .params
        .as_ref()
        .and_then(|params| params.pointer("/turn/id"))
        .and_then(Value::as_str);
    let observed_at = Utc::now().to_rfc3339();
    if let Ok(mut status) = status.lock() {
        *status
            .event_method_counts
            .entry(notification.method.clone())
            .or_default() += 1;
        // Sub-agent threads share this app-server connection; their turn and
        // status changes must not overwrite the bound thread's state.
        let notification_thread_id = notification
            .params
            .as_ref()
            .and_then(|params| params.get("threadId"))
            .and_then(Value::as_str);
        if notification_thread_id.is_some_and(|thread_id| thread_id != status.thread_id) {
            return;
        }
        match notification.method.as_str() {
            "turn/started" => {
                crate::management::v2::admission::turn_started(&status.cutex_session_id);
                status.active_turn_id = turn_id.map(str::to_string);
                status.active_turn_observed_at = Some(observed_at);
            }
            "turn/completed" => {
                if turn_id.is_none() || status.active_turn_id.as_deref() == turn_id {
                    crate::management::v2::admission::turn_completed(&status.cutex_session_id);
                    status.active_turn_id = None;
                    status.active_turn_observed_at = Some(observed_at);
                }
            }
            "thread/status/changed" => {
                status.thread_status = notification
                    .params
                    .as_ref()
                    .and_then(|params| params.get("status"))
                    .cloned();
                status.thread_status_observed_at = Some(observed_at.clone());
                let thread_status = notification
                    .params
                    .as_ref()
                    .and_then(|params| params.pointer("/status/type"))
                    .and_then(Value::as_str);
                if matches!(thread_status, Some("idle" | "notLoaded" | "systemError")) {
                    crate::management::v2::admission::turn_completed(&status.cutex_session_id);
                    status.active_turn_id = None;
                    status.active_turn_observed_at = Some(observed_at);
                }
            }
            // Native thread/revert interrupts any running turn and reloads the
            // thread in place; no turn survives the revert.
            "thread/reverted" => {
                crate::management::v2::admission::turn_completed(&status.cutex_session_id);
                status.active_turn_id = None;
                status.active_turn_observed_at = Some(observed_at);
            }
            "thread/settings/updated" => {
                if let Some(thread_settings) = notification
                    .params
                    .as_ref()
                    .and_then(|params| params.get("threadSettings"))
                    .cloned()
                {
                    status.thread_settings = Some(thread_settings);
                    status.thread_settings_source = Some("thread/settings/updated".to_string());
                    status.thread_settings_complete = true;
                    status.thread_settings_observed_at = Some(observed_at);
                }
            }
            _ => {}
        }
    }
}

fn event_as_inbound(event: &AppServerEvent) -> Option<InboundMessage> {
    match event {
        AppServerEvent::Notification(notification) => {
            Some(InboundMessage::Notification(notification.clone()))
        }
        AppServerEvent::ServerRequest(request) => {
            Some(InboundMessage::ServerRequest(request.clone()))
        }
        AppServerEvent::ProtocolViolation { .. } | AppServerEvent::Disconnected { .. } => None,
    }
}

fn mark_event_received(status: &Arc<Mutex<AppServerManagedRuntimeStatus>>) {
    if let Ok(mut status) = status.lock() {
        status.last_event_at = Some(Utc::now().to_rfc3339());
    }
}

fn mark_error(status: &Arc<Mutex<AppServerManagedRuntimeStatus>>, error: String) {
    if let Ok(mut status) = status.lock() {
        status.last_error = Some(error);
    }
}

fn mark_disconnected(status: &Arc<Mutex<AppServerManagedRuntimeStatus>>, reason: &str) {
    if let Ok(mut status) = status.lock() {
        status.connected = false;
        status.last_error = Some(reason.to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use serde_json::json;
    #[cfg(unix)]
    use tungstenite::Message;
    #[cfg(unix)]
    use tungstenite::WebSocket;

    use super::*;
    use crate::agent_bus::delivery::AgentDeliveryMode;

    #[test]
    fn resume_snapshot_keeps_collaboration_mode() {
        let response = json!({
            "model": "m",
            "reasoningEffort": "high",
            "collaborationMode": { "mode": "plan", "settings": { "model": "m", "reasoning_effort": "high", "developer_instructions": null } },
        });
        let settings = thread_settings_from_resume_response(&response);
        assert_eq!(settings["collaborationMode"]["mode"], "plan");
        assert_eq!(settings["effort"], "high");
        assert!(thread_settings_from_resume_response(&json!({ "model": "m" })).get("collaborationMode").is_none());
    }
    use crate::agent_bus::model::AgentBusMessage;
    use crate::agent_bus::model::AgentBusRegisterRequest;
    use crate::agent_bus::model::AgentRegistrationClass;
    use crate::app_server::bus_bridge::AppServerAgentBusBridgeOptions;
    use crate::app_server::client::saturated_event_client_for_test;
    use crate::app_server::commands::ThreadInterAgentMessageParams;
    use crate::app_server::commands::ThreadReadParams;

    #[cfg(unix)]
    fn short_socket_test_directory() -> std::path::PathBuf {
        let directory =
            std::path::PathBuf::from("/tmp").join(format!("cx-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).expect("create test directory");
        directory
    }

    #[test]
    fn manager_disconnect_completes_when_event_queue_is_saturated() {
        let (client, send_attempted_rx) = saturated_event_client_for_test();
        send_attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor attempted saturated event send");
        let handle = client.handle();
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            stop_rx.recv().expect("manager stop signal");
            drop(client);
        });
        let runtime_alive = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(AppServerManagedRuntimeStatus {
            cutex_session_id: "saturated-close".to_string(),
            thread_id: "thread-1".to_string(),
            runtime_generation: 1,
            connected: true,
            active_turn_id: None,
            active_turn_observed_at: None,
            thread_status: None,
            thread_status_observed_at: None,
            thread_settings: None,
            thread_settings_source: None,
            thread_settings_complete: false,
            thread_settings_observed_at: None,
            runtime_workspace_roots: None,
            instruction_sources: None,
            resume_snapshot_observed_at: None,
            event_method_counts: HashMap::new(),
            initialized_user_agent: None,
            last_event_at: None,
            last_error: None,
        }));
        let manager = AppServerRuntimeManager::without_event_sink();
        manager
            .runtimes
            .lock()
            .expect("runtime manager lock")
            .insert(
                "saturated-close".to_string(),
                ManagedRuntime {
                    developer_instructions: None,
                    supports_cutex_ingress: true,
                    endpoint: AppServerEndpoint::LoopbackWebSocket {
                        url: "ws://127.0.0.1:1".to_string(),
                        bearer_token: None,
                    },
                    handle,
                    status,
                    stop_tx,
                    worker: Some(worker),
                    agent_bus_bridge: None,
                    agent_bus_bridge_starting: false,
                    runtime_alive,
                },
            );

        let manager_for_disconnect = manager.clone();
        let (disconnect_done_tx, disconnect_done_rx) = mpsc::sync_channel(1);
        let disconnect_thread = thread::spawn(move || {
            let result = manager_for_disconnect.disconnect("saturated-close");
            disconnect_done_tx
                .send(result)
                .expect("signal manager disconnect completion");
        });
        let disconnected = disconnect_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("manager disconnect must not hang on a saturated event queue")
            .expect("manager disconnect should succeed");
        assert!(disconnected);
        disconnect_thread.join().expect("manager disconnect thread");
        assert!(manager
            .status("saturated-close")
            .expect("runtime status")
            .is_none());
    }

    #[test]
    fn bounded_resume_preserves_live_turn_without_historical_payloads() {
        let mut response = json!({"thread":{"id":"thread-1","status":{"type":"active"},"turns":[]},
            "initialTurnsPage":{"data":[{"id":"live-turn","status":"inProgress","items":[]}],"nextCursor":"older"}});
        assert_eq!(active_turn_id_from_thread_response(&response).as_deref(), Some("live-turn"));
        response["thread"]["status"]["type"] = json!("idle");
        assert_eq!(active_turn_id_from_thread_response(&response), None);
        response["thread"]["status"]["type"] = json!("active");
        response["initialTurnsPage"]["data"][0]["status"] = json!("completed");
        assert_eq!(active_turn_id_from_thread_response(&response), None);
    }

    #[test]
    fn idle_resume_ignores_historical_in_progress_turn() {
        let response = json!({
            "thread": {
                "id": "thread-1",
                "status": { "type": "idle" },
                "turns": [
                    { "id": "orphaned-turn", "status": "inProgress", "items": [] }
                ]
            }
        });

        assert!(active_turn_id_from_thread_response(&response).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn manager_interrupts_active_turn_before_runtime_stop() {
        for bounded in [false, true] {
            let directory = short_socket_test_directory();
            let socket_path = directory.join("app-server.sock");
            let listener = UnixListener::bind(&socket_path).expect("bind test socket");
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept client");
                let mut socket = tungstenite::accept(stream).expect("accept websocket");
                initialize_test_connection(&mut socket);
                let resume = read_json(&mut socket);
                assert_eq!(resume["method"], "thread/resume");
                write_json(
                    &mut socket,
                    json!({
                        "id": resume["id"].clone(),
                        "result": {
                            "thread": {
                                "id": "thread-1",
                                "status": { "type": "active", "activeFlags": [] },
                                "turns": if bounded { json!([]) } else { json!([
                                    { "id": "turn-1", "status": "inProgress", "items": [] }
                                ]) }
                            },
                            "initialTurnsPage": if bounded { json!({"data":[
                                {"id":"turn-1","status":"inProgress","items":[]}
                            ],"nextCursor":"older"}) } else { serde_json::Value::Null }
                        }
                    }),
                );
                let interrupt = read_json(&mut socket);
                assert_eq!(interrupt["method"], "turn/interrupt");
                assert_eq!(interrupt["params"]["threadId"], "thread-1");
                assert_eq!(interrupt["params"]["turnId"], "turn-1");
                write_json(
                    &mut socket,
                    json!({
                        "method": "turn/completed",
                        "params": {
                            "threadId": "thread-1",
                            "turn": { "id": "turn-1", "status": "interrupted", "items": [] }
                        }
                    }),
                );
                write_json(
                    &mut socket,
                    json!({ "id": interrupt["id"].clone(), "result": {} }),
                );
                let _ = socket.read();
            });

            let manager = AppServerRuntimeManager::without_event_sink();
            manager
                .connect_endpoint(
                    "cutex-1",
                    AppServerEndpoint::UnixSocket {
                        socket_path: socket_path.clone(),
                    },
                    directory.join("journal.jsonl").display().to_string(),
                    AppServerSchemaIdentity {
                        version: "test-schema".to_string(),
                        sha256: "test-schema-sha256".to_string(),
                    },
                    ThreadBootstrap::Resume(ThreadResumeParams {
                        thread_id: "thread-1".to_string(),
                        ..Default::default()
                    }),
                    1,
                    "host",
                    None,
                )
                .expect("connect managed runtime");

            assert_eq!(
                manager
                    .interrupt_active_turn("cutex-1")
                    .expect("interrupt active turn")
                    .as_deref(),
                Some("turn-1")
            );
            for _ in 0..20 {
                if manager
                    .status("cutex-1")
                    .expect("runtime status")
                    .expect("connected runtime")
                    .active_turn_id
                    .is_none()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                manager
                    .status("cutex-1")
                    .expect("runtime status")
                    .expect("connected runtime")
                    .active_turn_id
                    .is_none()
            );
            assert!(manager.disconnect("cutex-1").expect("disconnect runtime"));
            server.join().expect("server thread");
            fs::remove_dir_all(directory).expect("remove test directory");
        }
    }

    #[cfg(unix)]
    #[test]
    fn mailbox_control_connection_progresses_while_primary_events_are_backpressured() {
        let directory = short_socket_test_directory();
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (primary_stream, _) = listener.accept().expect("accept primary client");
            let mut primary =
                tungstenite::accept(primary_stream).expect("accept primary websocket");
            initialize_test_connection(&mut primary);
            write_json(
                &mut primary,
                json!({ "method": "item/agentMessage/delta", "params": { "delta": "one" } }),
            );
            write_json(
                &mut primary,
                json!({ "method": "item/agentMessage/delta", "params": { "delta": "two" } }),
            );

            let (control_stream, _) = listener.accept().expect("accept control client");
            let mut control =
                tungstenite::accept(control_stream).expect("accept control websocket");
            initialize_test_connection(&mut control);
            let request = read_json(&mut control);
            assert_eq!(request["method"], "thread/inter_agent_message");
            assert_eq!(request["params"]["messageId"], "message-1");
            write_json(
                &mut control,
                json!({
                    "id": request["id"].clone(),
                    "result": { "submissionId": "submission-1" }
                }),
            );
        });

        let endpoint = AppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        };
        let mut primary_options = AppServerClientOptions::new(endpoint.clone());
        primary_options.event_capacity = 1;
        let primary = AppServerClient::connect(primary_options).expect("connect primary client");
        thread::sleep(Duration::from_millis(50));

        let submitter = AppServerControlSubmitter::new(endpoint);
        let submission_id = submitter
            .submit_inter_agent_message(&ThreadInterAgentMessageParams {
                thread_id: "thread-1".to_string(),
                message_id: "message-1".to_string(),
                author: "/root/sender".to_string(),
                author_metadata: None,
                recipient: "/root/receiver".to_string(),
                recipient_metadata: None,
                other_recipients: Vec::new(),
                content: "hello".to_string(),
                delivery_mode: AgentDeliveryMode::Soon,
            })
            .expect("submit through isolated control connection");
        assert_eq!(submission_id, "submission-1");

        primary
            .recv_event_timeout(Duration::from_secs(1))
            .expect("receive first primary event")
            .expect("first primary event");
        primary
            .recv_event_timeout(Duration::from_secs(1))
            .expect("receive second primary event")
            .expect("second primary event");
        drop(submitter);
        drop(primary);
        server.join().expect("server thread");
        let _ = fs::remove_file(socket_path);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn manager_resumes_thread_forwards_events_and_exposes_commands() {
        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-manager-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);
            let resume = read_json(&mut socket);
            assert_eq!(resume["method"], "thread/resume");
            write_json(
                &mut socket,
                json!({
                    "id": resume["id"].clone(),
                    "result": {
                        "thread": {
                            "id": "thread-1",
                            "status": { "type": "idle" }
                        },
                        "model": "gpt-test",
                        "activePermissionProfile": { "id": ":workspace" },
                        "runtimeWorkspaceRoots": ["/workspace", "/shared"],
                        "instructionSources": ["/workspace/AGENTS.md"]
                    }
                }),
            );
            write_json(
                &mut socket,
                json!({ "method": "future/native/event", "params": { "threadId": "thread-1" } }),
            );
            let read = read_json(&mut socket);
            assert_eq!(read["method"], "thread/read");
            write_json(
                &mut socket,
                json!({ "id": read["id"].clone(), "result": { "thread": { "id": "thread-1" } } }),
            );
            let _ = socket.read();
        });

        let (event_tx, event_rx) = mpsc::channel();
        let manager = AppServerRuntimeManager::new(Arc::new(
            move |context: &AppServerRuntimeEventContext, event: &AppServerEvent| {
                let _ = event_tx.send((
                    context.cutex_session_id.clone(),
                    context.launched_profile.clone(),
                    event.clone(),
                ));
                Ok(())
            },
        ));
        let journal_path = directory.join("native-events.jsonl");
        let result = manager
            .connect_endpoint(
                "cutex-1",
                AppServerEndpoint::UnixSocket {
                    socket_path: socket_path.clone(),
                },
                journal_path.display().to_string(),
                AppServerSchemaIdentity {
                    version: "test-schema".to_string(),
                    sha256: "test-schema-sha256".to_string(),
                },
                ThreadBootstrap::Resume(ThreadResumeParams {
                    thread_id: "thread-1".to_string(),
                    ..Default::default()
                }),
                1,
                "host",
                Some("profile-a".to_string()),
            )
            .expect("connect managed runtime");
        assert_eq!(result.thread_response["thread"]["id"], "thread-1");
        let runtime_status = manager
            .status("cutex-1")
            .expect("runtime status")
            .expect("managed runtime status");
        assert_eq!(
            runtime_status.thread_status,
            Some(json!({ "type": "idle" }))
        );
        assert_eq!(
            runtime_status
                .thread_settings
                .as_ref()
                .and_then(|settings| settings.pointer("/activePermissionProfile/id"))
                .and_then(Value::as_str),
            Some(":workspace")
        );
        assert_eq!(
            runtime_status.thread_settings_source.as_deref(),
            Some("thread/resume")
        );
        assert!(!runtime_status.thread_settings_complete);
        assert!(runtime_status.active_turn_observed_at.is_some());
        assert!(runtime_status.thread_status_observed_at.is_some());
        assert!(runtime_status.thread_settings_observed_at.is_some());
        assert!(runtime_status.resume_snapshot_observed_at.is_some());
        assert_eq!(
            runtime_status.runtime_workspace_roots,
            Some(json!(["/workspace", "/shared"]))
        );
        assert_eq!(
            runtime_status.instruction_sources,
            Some(json!(["/workspace/AGENTS.md"]))
        );
        manager
            .note_active_turn("cutex-1", Some("turn-submitted".to_string()))
            .expect("record submitted turn");
        let submitted_status = manager
            .status("cutex-1")
            .expect("submitted runtime status")
            .expect("managed submitted runtime status");
        assert_eq!(
            submitted_status.active_turn_id.as_deref(),
            Some("turn-submitted")
        );
        assert!(submitted_status.active_turn_observed_at.is_some());
        let (session_id, launched_profile, event) = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("receive forwarded event");
        assert_eq!(session_id, "cutex-1");
        assert_eq!(launched_profile.as_deref(), Some("profile-a"));
        assert!(matches!(
            event,
            AppServerEvent::Notification(notification)
                if notification.method == "future/native/event"
        ));
        let read = manager
            .commands("cutex-1")
            .expect("managed commands")
            .thread_read(&ThreadReadParams {
                thread_id: "thread-1".to_string(),
                include_turns: false,
            })
            .expect("thread/read");
        assert_eq!(read["thread"]["id"], "thread-1");
        assert!(
            manager
                .status("cutex-1")
                .expect("runtime status")
                .expect("connected runtime")
                .connected
        );
        let bus = Arc::new(EmptyRuntimeAgentBus::default());
        let bridge_options = AppServerAgentBusBridgeOptions::new(
            AgentBusRegisterRequest {
                id: "runtime-1".to_string(),
                name: "agent".to_string(),
                base_name: Some("agent".to_string()),
                thread_name: None,
                path_key: None,
                session_id: Some("thread-1".to_string()),
                profile: "profile".to_string(),
                cwd: "/tmp".to_string(),
                pid: 42,
                host_id: Some("host".to_string()),
                groups: Vec::new(),
                registration_class: AgentRegistrationClass::Persistent,
            },
            "thread-1",
        );
        let bridge_status = manager
            .start_agent_bus_bridge("cutex-1", bus.clone(), bridge_options.clone())
            .expect("start agent-bus bridge");
        assert!(bridge_status.registered);
        assert_eq!(bus.registrations.load(Ordering::SeqCst), 1);
        assert!(
            manager
                .agent_bus_status("cutex-1")
                .expect("agent-bus status")
                .expect("running bridge")
                .running
        );
        assert!(manager
            .start_agent_bus_bridge("cutex-1", bus.clone(), bridge_options)
            .is_err());
        assert_eq!(bus.registrations.load(Ordering::SeqCst), 1);
        assert!(manager.disconnect("cutex-1").expect("disconnect runtime"));
        assert!(manager
            .agent_bus_status("cutex-1")
            .expect("agent-bus status")
            .is_none());

        server.join().expect("server thread");
        let journal = fs::read_to_string(&journal_path).expect("read diagnostic journal");
        assert!(journal.contains("future/native/event"));
        let _ = fs::remove_file(socket_path);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn manager_appends_launch_augmentation_to_effective_native_config() {
        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-augment-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);
            let read = read_json(&mut socket);
            assert_eq!(read["method"], "config/read");
            assert_eq!(read["params"]["cwd"], "/work");
            write_json(
                &mut socket,
                json!({
                    "id": read["id"].clone(),
                    "result": {
                        "config": {
                            "developer_instructions": "Profile says: be terse.",
                            "sandbox_workspace_write": { "writable_roots": ["/extra"] }
                        },
                        "origins": {}
                    }
                }),
            );
            let start = read_json(&mut socket);
            assert_eq!(start["method"], "thread/start");
            let instructions = start["params"]["developerInstructions"]
                .as_str()
                .expect("developer instructions");
            assert!(instructions.starts_with("Profile says: be terse.\n\n# File references"));
            assert!(instructions.contains("# Memory"));
            assert_eq!(
                start["params"]["runtimeWorkspaceRoots"],
                json!(["/work", "/extra", "/mem"])
            );
            assert!(start["params"].get("launchAugmentation").is_none());
            write_json(
                &mut socket,
                json!({
                    "id": start["id"].clone(),
                    "result": {
                        "thread": { "id": "thread-aug", "status": { "type": "idle" } },
                        "model": "gpt-test"
                    }
                }),
            );
            let _ = socket.read();
        });
        let scope = crate::memory::ProjectMemoryScope {
            project_id: crate::agent_management::ProjectId::new("proj").unwrap(),
            dir: std::path::PathBuf::from("/mem"),
        };
        let manager = AppServerRuntimeManager::without_event_sink();
        manager
            .connect_endpoint(
                "cutex-aug",
                AppServerEndpoint::UnixSocket {
                    socket_path: socket_path.clone(),
                },
                directory.join("native-events.jsonl").display().to_string(),
                AppServerSchemaIdentity {
                    version: "test-schema".to_string(),
                    sha256: "test-schema-sha256".to_string(),
                },
                ThreadBootstrap::Start(ThreadStartParams {
                    cwd: Some(std::path::PathBuf::from("/work")),
                    launch_augmentation: Some(crate::memory::LaunchAugmentation {
                        harness: None,
                        memory: Some(crate::memory::LaunchMemoryContext {
                            instructions: crate::memory::render_memory_instructions(&scope, None),
                            scope,
                            project_workspace_root: None,
                        }),
                    }),
                    ..Default::default()
                }),
                1,
                "host",
                None,
            )
            .expect("start augmented thread");
        assert!(manager.disconnect("cutex-aug").expect("disconnect"));
        server.join().expect("server thread");
        let _ = fs::remove_file(socket_path);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn manager_starts_one_new_thread_without_resume() {
        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-start-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);
            let start = read_json(&mut socket);
            assert_eq!(start["method"], "thread/start");
            assert!(start["params"].get("threadId").is_none());
            assert_eq!(start["params"]["sessionStartSource"], "startup");
            write_json(
                &mut socket,
                json!({
                    "id": start["id"].clone(),
                    "result": {
                        "thread": { "id": "thread-new", "status": { "type": "idle" } },
                        "model": "gpt-test"
                    }
                }),
            );
            let _ = socket.read();
        });
        let manager = AppServerRuntimeManager::without_event_sink();
        let result = manager
            .connect_endpoint(
                "cutex-new",
                AppServerEndpoint::UnixSocket {
                    socket_path: socket_path.clone(),
                },
                directory.join("native-events.jsonl").display().to_string(),
                AppServerSchemaIdentity {
                    version: "test-schema".to_string(),
                    sha256: "test-schema-sha256".to_string(),
                },
                ThreadBootstrap::Start(ThreadStartParams {
                    session_start_source: Some("startup".to_string()),
                    ..Default::default()
                }),
                1,
                "host",
                Some("release".to_string()),
            )
            .expect("start new managed thread");
        assert_eq!(result.thread_id, "thread-new");
        let status = manager
            .status("cutex-new")
            .expect("status")
            .expect("connected runtime");
        assert_eq!(status.thread_id, "thread-new");
        assert_eq!(
            status.thread_settings_source.as_deref(),
            Some("thread/start")
        );
        assert!(manager.disconnect("cutex-new").expect("disconnect"));
        server.join().expect("server thread");
        let _ = fs::remove_file(socket_path);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn durable_event_sink_failure_disconnects_runtime_and_stops_consumption() {
        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-sink-failure-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);
            let resume = read_json(&mut socket);
            write_json(
                &mut socket,
                json!({ "id": resume["id"].clone(), "result": { "thread": { "id": "thread-1" } } }),
            );
            write_json(
                &mut socket,
                json!({ "method": "item/started", "params": { "threadId": "thread-1", "item": { "id": "item-1", "type": "reasoning" } } }),
            );
            let _ = socket.read();
        });

        let sink_calls = Arc::new(AtomicUsize::new(0));
        let sink_calls_for_handler = sink_calls.clone();
        let manager = AppServerRuntimeManager::new(Arc::new(
            move |_: &AppServerRuntimeEventContext, _: &AppServerEvent| {
                sink_calls_for_handler.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("canonical event repository unavailable")
            },
        ));
        manager
            .connect_endpoint(
                "cutex-1",
                AppServerEndpoint::UnixSocket {
                    socket_path: socket_path.clone(),
                },
                directory.join("native-events.jsonl").display().to_string(),
                AppServerSchemaIdentity {
                    version: "test-schema".to_string(),
                    sha256: "test-schema-sha256".to_string(),
                },
                ThreadBootstrap::Resume(ThreadResumeParams {
                    thread_id: "thread-1".to_string(),
                    ..Default::default()
                }),
                1,
                "host",
                None,
            )
            .expect("connect managed runtime");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let status = loop {
            let status = manager
                .status("cutex-1")
                .expect("runtime status")
                .expect("managed runtime status");
            if !status.connected {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime did not fail closed after sink failure"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(sink_calls.load(Ordering::SeqCst), 1);
        assert!(status
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("canonical event repository unavailable")));
        assert!(manager.commands("cutex-1").is_err());
        assert!(manager.disconnect("cutex-1").expect("disconnect runtime"));

        server.join().expect("server thread");
        let _ = fs::remove_file(socket_path);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn event_worker_keeps_transport_disconnects_fatal_when_the_sink_errors() {
        let directory = short_socket_test_directory();
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);
            let resume = read_json(&mut socket);
            write_json(
                &mut socket,
                json!({ "id": resume["id"].clone(), "result": { "thread": { "id": "thread-1" } } }),
            );
        });

        let sink_events = Arc::new(Mutex::new(Vec::new()));
        let sink_events_for_handler = sink_events.clone();
        let manager = AppServerRuntimeManager::new(Arc::new(
            move |_: &AppServerRuntimeEventContext, event: &AppServerEvent| {
                sink_events_for_handler
                    .lock()
                    .expect("sink event lock")
                    .push(event.clone());
                anyhow::bail!("reject disconnected sink event")
            },
        ));
        manager
            .connect_endpoint(
                "cutex-1",
                AppServerEndpoint::UnixSocket {
                    socket_path: socket_path.clone(),
                },
                directory.join("native-events.jsonl").display().to_string(),
                AppServerSchemaIdentity {
                    version: "test-schema".to_string(),
                    sha256: "test-schema-sha256".to_string(),
                },
                ThreadBootstrap::Resume(ThreadResumeParams {
                    thread_id: "thread-1".to_string(),
                    ..Default::default()
                }),
                1,
                "host",
                None,
            )
            .expect("connect managed runtime");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let status = loop {
            let status = manager
                .status("cutex-1")
                .expect("runtime status")
                .expect("managed runtime status");
            if !status.connected
                && status
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("reject disconnected sink event"))
            {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime did not fail closed after transport disconnect"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("reject disconnected sink event")));
        assert!(matches!(
            sink_events.lock().expect("sink event lock").as_slice(),
            [AppServerEvent::Disconnected { .. }]
        ));
        assert!(manager.commands("cutex-1").is_err());
        assert!(manager.disconnect("cutex-1").expect("disconnect runtime"));

        server.join().expect("server thread");
        let _ = fs::remove_file(socket_path);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn manager_replaces_a_disconnected_runtime() {
        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-reconnect-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let (close_first_tx, close_first_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for connection_index in 0..2 {
                let (stream, _) = listener.accept().expect("accept client");
                let mut socket = tungstenite::accept(stream).expect("accept websocket");
                initialize_test_connection(&mut socket);
                let resume = read_json(&mut socket);
                write_json(
                    &mut socket,
                    json!({
                        "id": resume["id"].clone(),
                        "result": { "thread": { "id": "thread-1" } }
                    }),
                );
                if connection_index == 0 {
                    close_first_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("wait for first bridge registration");
                    socket.close(None).expect("close first connection");
                } else {
                    let _ = socket.read();
                }
            }
        });

        let manager = AppServerRuntimeManager::without_event_sink();
        let endpoint = AppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        };
        let journal_path = directory.join("native-events.jsonl");
        let schema = AppServerSchemaIdentity {
            version: "test-schema".to_string(),
            sha256: "test-schema-sha256".to_string(),
        };
        let resume = ThreadResumeParams {
            thread_id: "thread-1".to_string(),
            ..Default::default()
        };
        manager
            .connect_endpoint(
                "cutex-1",
                endpoint.clone(),
                journal_path.display().to_string(),
                schema.clone(),
                ThreadBootstrap::Resume(resume.clone()),
                1,
                "host",
                None,
            )
            .expect("connect first runtime");

        let bus = Arc::new(EmptyRuntimeAgentBus::default());
        let bridge_options = AppServerAgentBusBridgeOptions::new(
            AgentBusRegisterRequest {
                id: "runtime-1".to_string(),
                name: "agent".to_string(),
                base_name: Some("agent".to_string()),
                thread_name: None,
                path_key: None,
                session_id: Some("thread-1".to_string()),
                profile: "profile-a".to_string(),
                cwd: "/tmp".to_string(),
                pid: 42,
                host_id: Some("host".to_string()),
                groups: Vec::new(),
                registration_class: AgentRegistrationClass::Persistent,
            },
            "thread-1",
        );
        manager
            .start_agent_bus_bridge("cutex-1", bus.clone(), bridge_options.clone())
            .expect("start first agent-bus bridge");
        assert_eq!(bus.registrations.load(Ordering::SeqCst), 1);
        close_first_tx
            .send(())
            .expect("signal first connection shutdown");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let disconnected = manager
                .status("cutex-1")
                .expect("runtime status")
                .is_some_and(|status| !status.connected);
            if disconnected {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime did not observe the closed connection"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(manager.commands("cutex-1").is_err());

        manager
            .connect_endpoint(
                "cutex-1",
                endpoint,
                journal_path.display().to_string(),
                schema,
                ThreadBootstrap::Resume(resume),
                1,
                "host",
                None,
            )
            .expect("reconnect same-generation runtime");
        assert!(
            manager
                .status("cutex-1")
                .expect("runtime status")
                .expect("connected runtime")
                .connected
        );
        assert!(manager.handle_for_generation("cutex-1", 1).is_ok());
        manager
            .start_agent_bus_bridge("cutex-1", bus.clone(), bridge_options)
            .expect("restore same-generation agent-bus bridge");
        assert_eq!(bus.registrations.load(Ordering::SeqCst), 2);
        assert!(manager.disconnect("cutex-1").expect("disconnect runtime"));

        server.join().expect("server thread");
        let _ = fs::remove_file(socket_path);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn native_turn_events_update_active_turn_status() {
        let session = format!("admission-native-events-{}", uuid::Uuid::new_v4());
        let status = Arc::new(Mutex::new(AppServerManagedRuntimeStatus {
            cutex_session_id: session.clone(),
            thread_id: "thread-1".to_string(),
            runtime_generation: 1,
            connected: true,
            active_turn_id: None,
            active_turn_observed_at: None,
            thread_status: Some(json!({ "type": "idle" })),
            thread_status_observed_at: None,
            thread_settings: None,
            thread_settings_source: None,
            thread_settings_complete: false,
            thread_settings_observed_at: None,
            runtime_workspace_roots: None,
            instruction_sources: None,
            resume_snapshot_observed_at: None,
            event_method_counts: HashMap::new(),
            initialized_user_agent: None,
            last_event_at: None,
            last_error: None,
        }));
        let started = AppServerEvent::Notification(crate::app_server::protocol::RpcNotification {
            method: "turn/started".to_string(),
            params: Some(json!({ "threadId": "thread-1", "turn": { "id": "turn-1" } })),
            raw: Value::Null,
        });
        update_runtime_status_from_event(&status, &started);
        assert_eq!(
            status
                .lock()
                .expect("status lock")
                .active_turn_id
                .as_deref(),
            Some("turn-1")
        );

        crate::management::v2::admission::note_turn(&session, "delivery", None);
        for (thread, turn) in [("child", "child-turn"), ("thread-1", "old-turn")] {
            update_runtime_status_from_event(&status, &AppServerEvent::Notification(crate::app_server::protocol::RpcNotification {
                method: "turn/completed".into(), params: Some(json!({"threadId":thread,"turn":{"id":turn}})), raw: Value::Null,
            }));
            assert_eq!(crate::management::v2::admission::current_turn(&session, &crate::asks::store::AskState::default()).unwrap().kind, "delivery");
            assert!(!crate::management::v2::admission::owner_priority(&session));
        }

        let completed =
            AppServerEvent::Notification(crate::app_server::protocol::RpcNotification {
                method: "turn/completed".to_string(),
                params: Some(json!({ "threadId": "thread-1", "turn": { "id": "turn-1" } })),
                raw: Value::Null,
            });
        update_runtime_status_from_event(&status, &completed);
        assert!(status.lock().expect("status lock").active_turn_id.is_none());
        assert!(crate::management::v2::admission::current_turn(&session, &crate::asks::store::AskState::default()).is_none());
        assert!(crate::management::v2::admission::owner_priority(&session));
        crate::management::v2::admission::owner_started(&session);

        // Sub-agent threads on the same connection never move the bound
        // thread's active turn or status.
        for (method, params) in [
            ("turn/started", json!({ "threadId": "child", "turn": { "id": "child-turn" } })),
            ("thread/status/changed", json!({ "threadId": "child", "status": { "type": "active", "activeFlags": [] } })),
        ] {
            update_runtime_status_from_event(
                &status,
                &AppServerEvent::Notification(crate::app_server::protocol::RpcNotification {
                    method: method.to_string(),
                    params: Some(params),
                    raw: Value::Null,
                }),
            );
        }
        {
            let status = status.lock().expect("status lock");
            assert!(status.active_turn_id.is_none());
            assert_eq!(status.thread_status, Some(json!({ "type": "idle" })));
        }

        // A live thread/revert stops the running turn; clear it even if no
        // turn/completed for it was observed.
        update_runtime_status_from_event(&status, &started);
        let reverted = AppServerEvent::Notification(crate::app_server::protocol::RpcNotification {
            method: "thread/reverted".to_string(),
            params: Some(json!({ "threadId": "thread-1" })),
            raw: Value::Null,
        });
        update_runtime_status_from_event(&status, &reverted);
        assert!(status.lock().expect("status lock").active_turn_id.is_none());

        let status_changed =
            AppServerEvent::Notification(crate::app_server::protocol::RpcNotification {
                method: "thread/status/changed".to_string(),
                params: Some(json!({
                    "threadId": "thread-1",
                    "status": {
                        "type": "active",
                        "activeFlags": ["waitingOnUserInput"]
                    }
                })),
                raw: Value::Null,
            });
        update_runtime_status_from_event(&status, &status_changed);
        assert_eq!(
            status.lock().expect("status lock").thread_status,
            Some(json!({
                "type": "active",
                "activeFlags": ["waitingOnUserInput"]
            }))
        );

        let settings = AppServerEvent::Notification(crate::app_server::protocol::RpcNotification {
            method: "thread/settings/updated".to_string(),
            params: Some(json!({
                "threadId": "thread-1",
                "threadSettings": {
                    "model": "gpt-test",
                    "activePermissionProfile": {"id": ":workspace"}
                }
            })),
            raw: Value::Null,
        });
        update_runtime_status_from_event(&status, &settings);
        assert_eq!(
            status
                .lock()
                .expect("status lock")
                .thread_settings
                .as_ref()
                .and_then(|settings| settings.get("model"))
                .and_then(Value::as_str),
            Some("gpt-test")
        );
        let status = status.lock().expect("status lock");
        assert_eq!(
            status.thread_settings_source.as_deref(),
            Some("thread/settings/updated")
        );
        assert!(status.thread_settings_complete);
        assert!(status.active_turn_observed_at.is_some());
        assert!(status.thread_status_observed_at.is_some());
        assert!(status.thread_settings_observed_at.is_some());
        // Counts include ignored sub-agent and stale-turn notifications.
        assert_eq!(status.event_method_counts["turn/started"], 3);
        assert_eq!(status.event_method_counts["thread/reverted"], 1);
        assert_eq!(status.event_method_counts["turn/completed"], 3);
        assert_eq!(status.event_method_counts["thread/status/changed"], 2);
        assert_eq!(status.event_method_counts["thread/settings/updated"], 1);
    }

    /// Records what one simulated cutex process journals (its owned events).
    struct RecordingSink {
        ledger: Arc<super::super::single_writer::Ledger>,
        journaled: Arc<Mutex<Vec<Value>>>,
        unowned: Arc<AtomicUsize>,
    }

    impl AppServerRuntimeEventSink for RecordingSink {
        fn handle_event(
            &self,
            _context: &AppServerRuntimeEventContext,
            event: &AppServerEvent,
        ) -> anyhow::Result<()> {
            if let AppServerEvent::Notification(notification) = event {
                self.journaled.lock().unwrap().push(notification.raw.clone());
            }
            Ok(())
        }
        fn event_ledger(&self) -> Option<Arc<super::super::single_writer::Ledger>> {
            Some(self.ledger.clone())
        }
        fn handle_unowned_event(
            &self,
            _context: &AppServerRuntimeEventContext,
            _event: &AppServerEvent,
            _origin: &AppServerHandle,
        ) -> anyhow::Result<()> {
            self.unowned.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// The Agent Bus process launched and connected a runtime; Management
    /// adopts it while a turn streams. The app-server sends every event to
    /// both connections; across both workers (delta coalescing included)
    /// each event is journaled exactly once, nothing is lost at the cut, and
    /// the shared diagnostic journal also holds each event once.
    #[cfg(unix)]
    #[test]
    fn two_processes_on_one_runtime_journal_each_native_event_once() {
        use super::super::single_writer::{Ledger, ProcessIdentity, Role};
        let directory = short_socket_test_directory();
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let subscribers: Arc<Mutex<Vec<WebSocket<std::os::unix::net::UnixStream>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let accepting = subscribers.clone();
        let acceptor = thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().expect("accept client");
                let mut socket = tungstenite::accept(stream).expect("accept websocket");
                initialize_test_connection(&mut socket);
                let resume = read_json(&mut socket);
                assert_eq!(resume["method"], "thread/resume");
                // Subscribe before answering, as the app-server does; the
                // emitter holds the lock while it fans one event out.
                let mut subscribers = accepting.lock().unwrap();
                write_json(
                    &mut socket,
                    json!({ "id": resume["id"].clone(), "result": { "thread": { "id": "thread-1", "status": { "type": "active", "activeFlags": [] } } } }),
                );
                subscribers.push(socket);
            }
        });
        let expected = Arc::new(Mutex::new(Vec::<(String, i64, String)>::new()));
        let emitted = expected.clone();
        let emitting = subscribers.clone();
        let started = Arc::new(AtomicBool::new(false));
        let start_emitting = started.clone();
        let emitter = thread::spawn(move || {
            while !start_emitting.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            for index in 0..150 {
                let emitted_at = super::super::single_writer::now_ms();
                let (method, params, token) = if index % 5 == 0 {
                    (
                        "item/started",
                        json!({ "threadId": "thread-1", "turnId": "turn-1", "item": { "id": format!("item-{index}"), "type": "reasoning" } }),
                        format!("item-{index}"),
                    )
                } else {
                    let token = format!("d{index},");
                    (
                        "item/agentMessage/delta",
                        json!({ "threadId": "thread-1", "turnId": "turn-1", "itemId": "msg", "delta": token }),
                        token,
                    )
                };
                let message = json!({ "method": method, "params": params, "emittedAtMs": emitted_at });
                {
                    let mut subscribers = emitting.lock().unwrap();
                    for socket in subscribers.iter_mut() {
                        write_json(socket, message.clone());
                    }
                }
                emitted.lock().unwrap().push((method.to_string(), emitted_at, token));
                // Bursts let the workers coalesce deltas across the run.
                if index % 4 == 3 {
                    thread::sleep(Duration::from_millis(60));
                }
            }
        });

        let live = |_: &ProcessIdentity| true;
        let identity = |pid| ProcessIdentity { pid, started_at: None };
        let bus_journal = Arc::new(Mutex::new(Vec::new()));
        let management_journal = Arc::new(Mutex::new(Vec::new()));
        let bus_unowned = Arc::new(AtomicUsize::new(0));
        let management_unowned = Arc::new(AtomicUsize::new(0));
        let bus = AppServerRuntimeManager::new(Arc::new(RecordingSink {
            ledger: Arc::new(Ledger::new(&directory, identity(1), Role::Launcher, live)),
            journaled: bus_journal.clone(),
            unowned: bus_unowned.clone(),
        }));
        let management = AppServerRuntimeManager::new(Arc::new(RecordingSink {
            ledger: Arc::new(Ledger::new(&directory, identity(2), Role::Owner, live)),
            journaled: management_journal.clone(),
            unowned: management_unowned.clone(),
        }));
        let diagnostics = directory.join("native-events.jsonl");
        let connect = |manager: &AppServerRuntimeManager| {
            manager
                .connect_endpoint(
                    "cutex-1",
                    AppServerEndpoint::UnixSocket { socket_path: socket_path.clone() },
                    diagnostics.display().to_string(),
                    AppServerSchemaIdentity {
                        version: "test-schema".to_string(),
                        sha256: "test-schema-sha256".to_string(),
                    },
                    ThreadBootstrap::Resume(ThreadResumeParams {
                        thread_id: "thread-1".to_string(),
                        ..Default::default()
                    }),
                    1,
                    "host",
                    None,
                )
                .expect("connect runtime")
        };
        connect(&bus);
        started.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(400));
        connect(&management);
        acceptor.join().expect("acceptor");
        emitter.join().expect("emitter");
        // Let both workers drain.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let expected = expected.lock().unwrap().clone();
        let tokens_of = |journal: &Arc<Mutex<Vec<Value>>>| {
            journal
                .lock()
                .unwrap()
                .iter()
                .flat_map(|raw| match raw["method"].as_str() {
                    Some("item/started") => vec![raw["params"]["item"]["id"].as_str().unwrap().to_string()],
                    _ => raw["params"]["delta"]
                        .as_str()
                        .unwrap()
                        .split_inclusive(',')
                        .map(str::to_string)
                        .collect(),
                })
                .collect::<Vec<_>>()
        };
        loop {
            let total = tokens_of(&bus_journal).len() + tokens_of(&management_journal).len();
            if total >= expected.len() || std::time::Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(100));
        let bus_tokens = tokens_of(&bus_journal);
        let management_tokens = tokens_of(&management_journal);
        let mut journaled = bus_tokens.iter().chain(&management_tokens).cloned().collect::<Vec<_>>();
        journaled.sort();
        let mut wanted = expected.iter().map(|(_, _, token)| token.clone()).collect::<Vec<_>>();
        wanted.sort();
        assert_eq!(journaled, wanted, "each native event exactly once");
        // Both sides wrote: the bus up to the cut, Management after it, and
        // Management saw (and skipped) the bus's part of its stream.
        assert!(!bus_tokens.is_empty() && !management_tokens.is_empty());
        assert!(management_unowned.load(Ordering::SeqCst) > 0);
        let cut = {
            let first_management = management_journal.lock().unwrap()[0]["emittedAtMs"].as_i64().unwrap();
            let last_bus = bus_journal.lock().unwrap().last().unwrap()["emittedAtMs"].as_i64().unwrap();
            assert!(last_bus < first_management, "the bus stops where Management starts");
            first_management
        };
        assert!(expected.iter().any(|(_, at, _)| *at >= cut));
        let started_records = fs::read_to_string(&diagnostics)
            .unwrap()
            .lines()
            .filter(|line| line.contains("item/started"))
            .count();
        assert_eq!(started_records, expected.iter().filter(|(method, _, _)| method == "item/started").count());

        assert!(management.disconnect("cutex-1").unwrap());
        assert!(bus.disconnect("cutex-1").unwrap());
        assert!(!directory.join("event-owners.json").exists(), "lease and registration released");
        subscribers.lock().unwrap().clear();
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    fn initialize_test_connection(socket: &mut WebSocket<std::os::unix::net::UnixStream>) {
        let initialize = read_json(socket);
        assert_eq!(initialize["method"], "initialize");
        write_json(
            socket,
            json!({ "id": initialize["id"].clone(), "result": { "userAgent": "test" } }),
        );
        assert_eq!(read_json(socket)["method"], "initialized");
    }

    #[cfg(unix)]
    fn read_json(socket: &mut WebSocket<std::os::unix::net::UnixStream>) -> Value {
        loop {
            match socket.read().expect("read websocket") {
                Message::Text(text) => {
                    return serde_json::from_str(text.as_ref()).expect("parse websocket JSON");
                }
                Message::Ping(payload) => socket
                    .send(Message::Pong(payload))
                    .expect("send websocket pong"),
                message => panic!("unexpected websocket message: {message:?}"),
            }
        }
    }

    #[cfg(unix)]
    fn write_json(socket: &mut WebSocket<std::os::unix::net::UnixStream>, value: Value) {
        socket
            .send(Message::Text(
                serde_json::to_string(&value)
                    .expect("serialize websocket JSON")
                    .into(),
            ))
            .expect("write websocket");
    }

    #[derive(Default)]
    struct EmptyRuntimeAgentBus {
        registrations: AtomicUsize,
    }

    impl RuntimeAgentBus for EmptyRuntimeAgentBus {
        fn register(&self, _request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
            self.registrations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn unregister(&self, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn poll(&self, _agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            Ok(Vec::new())
        }

        fn ack(&self, _agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
            Ok(message_ids.len())
        }
    }
}
