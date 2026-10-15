//! Agent-bus polling and native inter-agent submission for one app-server runtime.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use chrono::Utc;
use sha2::Digest;
use sha2::Sha256;

use crate::agent_bus::client::AgentBusHttpClient;
use crate::agent_bus::model::canonical_recipient_label;
use crate::agent_bus::model::AgentBusEnvelopeKind;
use crate::agent_bus::model::AgentBusMessage;
use crate::agent_bus::model::AgentBusRegisterRequest;
use crate::agent_management::{
    AgentManagementMessageMetadata, AgentManagementSchema, AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE,
    AGENT_MANAGEMENT_START_CONTROL_TYPE, AGENT_MANAGEMENT_SYSTEM_SENDER,
};
use crate::management::v2::agent_bus_state::agent_bus_message_repository;
use crate::session::identity::default_cutex_session_id_for_codex_session;

use super::commands::AppServerCommands;
#[cfg(test)]
use super::commands::InterAgentContextPersistedReceipt;
use super::commands::ParticipantPresentationMetadata;
use super::commands::ThreadInterAgentMessageDeliveryState;
use super::commands::ThreadInterAgentMessageParams;
use super::commands::ThreadInterAgentMessageStatusParams;
use super::commands::ThreadInterAgentMessageStatusQuery;
use super::commands::ThreadInterAgentMessageStatusResponse;
use super::participants::{ParticipantMetadataResolver, RegistryParticipantMetadataResolver};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const MODEL_VISIBLE_MESSAGE_ID_PREFIX: &str = "amsg_";
const MODEL_VISIBLE_MESSAGE_ID_MAX_CHARS: usize = 64;
const MODEL_VISIBLE_MESSAGE_ID_HASH_DOMAIN: &str = "cutex:model-visible-inter-agent-message:v1\0";
const INTER_AGENT_SEMANTIC_HASH_DOMAIN: &[u8] = b"cutex:inter-agent-message-semantic:v1\0";
const INTER_AGENT_STATUS_SCHEMA: &str = "cutex/inter-agent-delivery-status/v1";

#[path = "bus_bridge_external.rs"]
mod external;

#[cfg(test)]
pub(crate) fn projected_external_envelope(
    message: &AgentBusMessage,
    binding: &crate::launch::stock::ExternalInputBinding,
) -> anyhow::Result<crate::app_server::external_input::Envelope> {
    external::envelope(message, binding)
}

/// Spill at delivery against a given repository and document store (tests).
#[cfg(test)]
pub(crate) fn spill_frozen_external(
    repository: &crate::management::v2::agent_bus_state::AgentBusMessageRepository,
    frozen: &crate::app_server::external_input::Envelope,
    message: &AgentBusMessage,
    limit: crate::app_server::external_input::size::ReceiverLimit,
    documents: crate::documents::DocumentStore,
    force: bool,
) -> anyhow::Result<Option<crate::app_server::external_input::Envelope>> {
    let settings = crate::config::documents::DocumentSettings::default();
    external::spill_frozen(repository, frozen, message, limit, &settings, move || Ok(documents), force)
}

pub(crate) fn validate_external_recovery_target(
    message: &AgentBusMessage,
    owner: &str,
    roster: &crate::agent_management::AgentManagementSnapshot,
) -> anyhow::Result<()> {
    external::validate_target(message, owner, roster)
}

/// r219: finishes the durable record of a polled legacy Task Service message
/// ([`AgentBusMessage::is_removed_task_service_message`]) that both bridges
/// ACK without submission. A record that is missing or already finished
/// (delivered, quarantined, failed, ...) is left as it is. Returns whether a
/// record was finished now.
fn finish_removed_task_service_record(
    repository: &crate::management::v2::agent_bus_state::AgentBusMessageRepository,
    owner_cutex_session_id: &str,
    message_id: &str,
) -> anyhow::Result<bool> {
    let Some(snapshot) = repository.snapshot_by_message_id(message_id)? else {
        return Ok(false);
    };
    if matches!(
        snapshot.state.as_str(),
        "delivered" | "inbox_accepted" | "quarantined" | "discarded" | "failed"
    ) {
        return Ok(false);
    }
    repository.record_failed(
        owner_cutex_session_id,
        message_id,
        crate::agent_bus::model::task_service_removed_error(),
        Utc::now(),
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DeliverySweepOutcome {
    had_messages: bool,
    retained_pending: bool,
    made_progress: bool,
}

#[cfg(test)]
mod job_service_completion_projection_tests {
    use super::*;
    use crate::agent_bus::delivery::AgentDeliveryMode;
    use crate::agent_bus::model::{
        AgentBusEnvelopeKind, AgentMessageKind, JobServiceCompletionRequest,
        JobServiceTerminalStatus, JOB_SERVICE_COMPLETION_SCHEMA,
    };

    fn completion_message() -> AgentBusMessage {
        let target = "cutex.11111111-1111-4111-8111-111111111111";
        let metadata = JobServiceCompletionRequest {
            schema: JOB_SERVICE_COMPLETION_SCHEMA.to_string(),
            event_id: "event-1".to_string(),
            job_id: "job-1".to_string(),
            job_revision: 1,
            terminal_status: JobServiceTerminalStatus::Exited,
            result_sha256: "a".repeat(64),
            target_cutex_session_id: target.to_string(),
            summary: Some("untrusted summary".to_string()),
            output_reference: Some("job-output:job-1".to_string()),
        };
        AgentBusMessage {
            id: "jsc_fixture".to_string(),
            kind: AgentBusEnvelopeKind::Message,
            from: "cutex-job-service".to_string(),
            to: "runtime-current".to_string(),
            from_cutex_session_id: None,
            to_cutex_session_id: Some(target.to_string()),
            content:
                "Message Type: JOB_SERVICE_COMPLETION\nSummary (untrusted data): untrusted summary"
                    .to_string(),
            delivery_mode: AgentDeliveryMode::AfterTurn,
            trigger_turn: true,
            created_at_epoch_secs: 1,
            sender_kind: AgentMessageKind::JobServiceSystem,
            display_source: Some("Cutex Job Service".to_string()),
            submit_mode: None,
            control_type: Some(JOB_SERVICE_COMPLETION_SCHEMA.to_string()),
            control_payload: Some(serde_json::to_value(metadata).unwrap()),
            external_action_id: Some("job-1".to_string()),
            external_message_id: Some("event-1".to_string()),
        }
    }

    #[test]
    fn protected_job_completion_projects_without_source_agent_identity() {
        let target = "cutex.11111111-1111-4111-8111-111111111111";
        let params = inter_agent_params("thread-current", target, target, &completion_message())
            .expect("protected completion projection");
        assert_eq!(params.delivery_mode, AgentDeliveryMode::AfterTurn);
        assert!(params.content.contains("untrusted data"));
        assert_eq!(
            params.author_metadata.unwrap().display_name.as_deref(),
            Some("Cutex Job Service")
        );
    }

    #[test]
    fn forged_job_completion_provenance_is_rejected() {
        let target = "cutex.11111111-1111-4111-8111-111111111111";
        let mut message = completion_message();
        message.from_cutex_session_id = Some("cutex.forged".to_string());
        assert!(inter_agent_params("thread-current", target, target, &message).is_err());
    }
}

#[derive(Debug, Default)]
struct PendingPollBackoff {
    next_delay: Option<Duration>,
}

impl PendingPollBackoff {
    fn wait_after(
        &mut self,
        outcome: DeliverySweepOutcome,
        initial_delay: Duration,
        maximum_delay: Duration,
    ) -> Option<Duration> {
        if outcome.made_progress {
            self.next_delay = None;
        }
        if !outcome.retained_pending {
            self.next_delay = None;
            return None;
        }
        let cap = maximum_delay.max(initial_delay);
        let delay = self.next_delay.unwrap_or(initial_delay).min(cap);
        self.next_delay = Some(delay.saturating_mul(2).min(cap));
        Some(delay)
    }
}

pub trait RuntimeAgentBus: Send + Sync + 'static {
    fn register(&self, request: &AgentBusRegisterRequest) -> anyhow::Result<()>;
    fn unregister(&self, agent_id: &str) -> anyhow::Result<bool>;
    fn poll(&self, agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>>;
    fn ack(&self, agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize>;
}

impl RuntimeAgentBus for AgentBusHttpClient {
    fn register(&self, request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
        AgentBusHttpClient::register(self, request)
    }

    fn poll(&self, agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
        AgentBusHttpClient::poll(self, agent_id)
    }

    fn unregister(&self, agent_id: &str) -> anyhow::Result<bool> {
        AgentBusHttpClient::unregister(self, agent_id)
    }

    fn ack(&self, agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
        AgentBusHttpClient::ack(self, agent_id, message_ids)
    }
}

pub trait InterAgentMessageSubmitter: Send + Sync + 'static {
    fn thread_busy(&self, thread_id: &str) -> anyhow::Result<bool>;

    fn delivery_may_start(&self, session: &str, id: &str) -> anyhow::Result<bool> {
        crate::management::v2::admission::delivery_may_start(session, id)
    }

    fn submit_inter_agent_message(
        &self,
        params: &ThreadInterAgentMessageParams,
    ) -> anyhow::Result<String>;

    fn inter_agent_message_status(
        &self,
        params: &ThreadInterAgentMessageStatusParams,
    ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse>;
}

impl InterAgentMessageSubmitter for AppServerCommands {
    fn thread_busy(&self, thread_id: &str) -> anyhow::Result<bool> {
        let read = self.thread_read(&super::commands::ThreadReadParams {
            thread_id: thread_id.into(), include_turns: false,
        })?;
        let status = read.pointer("/thread/status/type").and_then(serde_json::Value::as_str)
            .context("thread/read without status")?;
        Ok(status == "active")
    }

    fn submit_inter_agent_message(
        &self,
        params: &ThreadInterAgentMessageParams,
    ) -> anyhow::Result<String> {
        let response = self.thread_inter_agent_message(params)?;
        let submission_id = response.submission_id.trim().to_string();
        if submission_id.is_empty() {
            anyhow::bail!("thread/inter_agent_message returned an empty submissionId");
        }
        Ok(submission_id)
    }

    fn inter_agent_message_status(
        &self,
        params: &ThreadInterAgentMessageStatusParams,
    ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
        self.thread_inter_agent_message_status(params)
            .map_err(Into::into)
    }
}

#[derive(Debug, Clone)]
pub struct AppServerAgentBusBridgeOptions {
    /// Stock outbound-only prototype: maintain registration, never poll/ack.
    pub registration_only: bool,
    /// Trusted launcher only; client still proves exact pinned occurrence.
    pub external_input_generation: Option<u64>,
    pub registration: AgentBusRegisterRequest,
    pub cutex_session_id: String,
    pub thread_id: String,
    pub poll_interval: Duration,
    pub retry_interval: Duration,
    pub registration_refresh_interval: Duration,
}

impl AppServerAgentBusBridgeOptions {
    pub fn new(registration: AgentBusRegisterRequest, thread_id: impl Into<String>) -> Self {
        let thread_id = thread_id.into();
        Self {
            registration_only: false,
            external_input_generation: None,
            registration,
            cutex_session_id: default_cutex_session_id_for_codex_session(&thread_id),
            thread_id,
            poll_interval: DEFAULT_POLL_INTERVAL,
            retry_interval: DEFAULT_RETRY_INTERVAL,
            registration_refresh_interval: DEFAULT_REGISTRATION_REFRESH_INTERVAL,
        }
    }

    pub fn with_cutex_session_id(mut self, cutex_session_id: impl Into<String>) -> Self {
        self.cutex_session_id = cutex_session_id.into();
        self
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.registration.id.trim().is_empty() {
            anyhow::bail!("agent-bus bridge registration id must not be empty");
        }
        if self.thread_id.trim().is_empty() {
            anyhow::bail!("agent-bus bridge threadId must not be empty");
        }
        if self.cutex_session_id.trim().is_empty() {
            anyhow::bail!("agent-bus bridge cutexSessionId must not be empty");
        }
        if self.registration.session_id.as_deref() != Some(self.thread_id.as_str()) {
            anyhow::bail!("agent-bus registration session_id must match the app-server threadId");
        }
        if self.registration.pid == 0 {
            anyhow::bail!("agent-bus registration pid must be the app-server process id");
        }
        if self.poll_interval.is_zero()
            || self.retry_interval.is_zero()
            || self.registration_refresh_interval.is_zero()
        {
            anyhow::bail!("agent-bus bridge intervals must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerAgentBusBridgeStatus {
    pub runtime_agent_id: String,
    pub thread_id: String,
    pub running: bool,
    pub registered: bool,
    pub pending_ack_count: usize,
    pub submitted_count: u64,
    pub acknowledged_count: u64,
    pub last_poll_at: Option<String>,
    pub last_message_id: Option<String>,
    pub last_submission_id: Option<String>,
    pub last_error: Option<String>,
}

pub struct AppServerAgentBusBridge {
    stop_tx: mpsc::SyncSender<()>,
    worker: Option<JoinHandle<()>>,
    heartbeat_stop_tx: mpsc::SyncSender<()>,
    heartbeat_worker: Option<JoinHandle<()>>,
    status: Arc<Mutex<AppServerAgentBusBridgeStatus>>,
    shutdown_bus: Arc<dyn RuntimeAgentBus>,
    registration: AgentBusRegisterRequest,
    runtime_agent_id: String,
}

impl AppServerAgentBusBridge {
    pub fn spawn(
        bus: Arc<dyn RuntimeAgentBus>,
        submitter: Arc<dyn InterAgentMessageSubmitter>,
        options: AppServerAgentBusBridgeOptions,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_liveness(bus, submitter, options, Arc::new(AtomicBool::new(true)))
    }

    pub fn spawn_with_liveness(
        bus: Arc<dyn RuntimeAgentBus>,
        submitter: Arc<dyn InterAgentMessageSubmitter>,
        options: AppServerAgentBusBridgeOptions,
        runtime_alive: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        options.validate()?;
        if !runtime_alive.load(Ordering::Acquire) {
            anyhow::bail!("app-server runtime disconnected before agent-bus bridge startup");
        }
        bus.register(&options.registration)
            .context("failed to register app-server runtime on the agent bus")?;
        let runtime_agent_id = options.registration.id.clone();
        let registration = options.registration.clone();
        let status = Arc::new(Mutex::new(AppServerAgentBusBridgeStatus {
            runtime_agent_id: runtime_agent_id.clone(),
            thread_id: options.thread_id.clone(),
            running: true,
            registered: true,
            pending_ack_count: 0,
            submitted_count: 0,
            acknowledged_count: 0,
            last_poll_at: None,
            last_message_id: None,
            last_submission_id: None,
            last_error: None,
        }));
        let (heartbeat_stop_tx, heartbeat_stop_rx) = mpsc::sync_channel(1);
        let heartbeat_bus = bus.clone();
        let heartbeat_registration = options.registration.clone();
        let heartbeat_refresh_interval = options.registration_refresh_interval;
        let heartbeat_retry_interval = options.retry_interval;
        let heartbeat_status = status.clone();
        let heartbeat_runtime_alive = runtime_alive.clone();
        let heartbeat_worker = thread::spawn(move || {
            run_registration_worker(
                heartbeat_bus,
                heartbeat_registration,
                heartbeat_refresh_interval,
                heartbeat_retry_interval,
                heartbeat_stop_rx,
                heartbeat_status,
                heartbeat_runtime_alive,
            )
        });
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let worker_status = status.clone();
        let shutdown_bus = bus.clone();
        let worker = thread::spawn(move || {
            run_bridge_worker(
                bus,
                submitter,
                options,
                stop_rx,
                worker_status,
                runtime_alive,
            )
        });
        Ok(Self {
            stop_tx,
            worker: Some(worker),
            heartbeat_stop_tx,
            heartbeat_worker: Some(heartbeat_worker),
            status,
            shutdown_bus,
            registration,
            runtime_agent_id,
        })
    }

    pub fn status(&self) -> anyhow::Result<AppServerAgentBusBridgeStatus> {
        self.status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| anyhow::anyhow!("agent-bus bridge status lock was poisoned"))
    }

    /// Synchronously republishes the bridge's frozen identity after Agent Bus
    /// recovery. Registration is idempotent for the same runtime endpoint and
    /// does not create a child or alter the native thread/generation.
    pub fn refresh_registration(&self) -> anyhow::Result<AppServerAgentBusBridgeStatus> {
        match self.shutdown_bus.register(&self.registration) {
            Ok(()) => {
                mark_registration(&self.status, true);
                self.status()
            }
            Err(error) => {
                mark_registration(&self.status, false);
                mark_error(
                    &self.status,
                    format!("agent-bus registration refresh failed: {error:#}"),
                );
                Err(error)
            }
        }
    }

    pub fn shutdown(mut self) -> anyhow::Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> anyhow::Result<()> {
        #[cfg(feature = "stock-launch-test-hook")]
        super::private_restart_phase("bridge.stop_signal");
        let _ = self.stop_tx.try_send(());
        let _ = self.heartbeat_stop_tx.try_send(());
        let heartbeat_panicked = self
            .heartbeat_worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        if self.worker.is_some() {
            // Removing the volatile registration wakes an in-flight long poll,
            // allowing the worker to observe the stop signal immediately.
            #[cfg(feature = "stock-launch-test-hook")]
            super::private_restart_phase("bridge.unregister.begin");
            let _ = self.shutdown_bus.unregister(&self.runtime_agent_id);
            #[cfg(feature = "stock-launch-test-hook")]
            super::private_restart_phase("bridge.unregister.end");
        }
        let worker_panicked = if let Some(worker) = self.worker.take() {
            #[cfg(feature = "stock-launch-test-hook")]
            super::private_restart_phase("bridge.worker_join.begin");
            let panicked = worker.join().is_err();
            #[cfg(feature = "stock-launch-test-hook")]
            super::private_restart_phase("bridge.worker_join.end");
            panicked
        } else {
            false
        };
        anyhow::ensure!(!heartbeat_panicked, "agent-bus registration worker panicked");
        anyhow::ensure!(!worker_panicked, "agent-bus bridge worker panicked");
        Ok(())
    }
}

impl Drop for AppServerAgentBusBridge {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn run_registration_worker(
    bus: Arc<dyn RuntimeAgentBus>,
    registration: AgentBusRegisterRequest,
    refresh_interval: Duration,
    retry_interval: Duration,
    stop_rx: mpsc::Receiver<()>,
    status: Arc<Mutex<AppServerAgentBusBridgeStatus>>,
    runtime_alive: Arc<AtomicBool>,
) {
    let mut wait = refresh_interval;
    loop {
        if !runtime_alive.load(Ordering::Acquire) || stop_requested(&stop_rx, wait) {
            break;
        }
        if !runtime_alive.load(Ordering::Acquire) {
            break;
        }
        #[cfg(feature = "stock-launch-test-hook")]
        super::private_restart_phase("bridge.refresh_register.begin");
        match bus.register(&registration) {
            Ok(()) => {
                #[cfg(feature = "stock-launch-test-hook")]
                super::private_restart_phase("bridge.refresh_register.end");
                mark_registration(&status, true);
                wait = refresh_interval;
            }
            Err(error) => {
                mark_registration(&status, false);
                mark_error(
                    &status,
                    format!("agent-bus registration refresh failed: {error:#}"),
                );
                wait = retry_interval;
            }
        }
    }
}

fn run_bridge_worker(
    bus: Arc<dyn RuntimeAgentBus>,
    submitter: Arc<dyn InterAgentMessageSubmitter>,
    options: AppServerAgentBusBridgeOptions,
    stop_rx: mpsc::Receiver<()>,
    status: Arc<Mutex<AppServerAgentBusBridgeStatus>>,
    runtime_alive: Arc<AtomicBool>,
) {
    let mut pending_acks = BTreeSet::new();
    let recipient_label = canonical_recipient_label(
        options.registration.base_name.as_deref(),
        &options.registration.name,
        &options.registration.id,
    )
    .to_string();
    let mut pending_poll_backoff = PendingPollBackoff::default();
    let mut external_client = None;
    loop {
        if !runtime_alive.load(Ordering::Acquire) {
            mark_error(&status, "app-server runtime disconnected".to_string());
            break;
        }
        if stop_requested(&stop_rx, Duration::ZERO) {
            break;
        }
        if !pending_acks.is_empty() {
            if let Err(error) = retry_pending_acks(
                bus.as_ref(),
                &options.registration.id,
                &mut pending_acks,
                &status,
            ) {
                mark_error(&status, error.to_string());
                if stop_requested(&stop_rx, options.retry_interval) {
                    break;
                }
                continue;
            }
        }
        if options.registration_only {
            if stop_requested(&stop_rx, options.poll_interval) {
                break;
            }
            continue;
        }
        let poll_started = Instant::now();
        match bus.poll(&options.registration.id) {
            Ok(messages) => {
                mark_poll(&status);
                let delivery = if let Some(generation) = options.external_input_generation {
                    external::deliver(
                        bus.as_ref(),
                        &options,
                        generation,
                        messages,
                        &status,
                        &mut external_client,
                    )
                } else {
                    deliver_polled_messages(
                        bus.as_ref(),
                        submitter.as_ref(),
                        &options.registration.id,
                        &recipient_label,
                        &options.cutex_session_id,
                        &options.thread_id,
                        messages,
                        &mut pending_acks,
                        &status,
                    )
                };
                // Input delivery and ACK have already run. Display failures do
                // not alter their result; recovery also runs on an empty poll.
                if let Some(generation) = options.external_input_generation {
                    if let Err(error) = external::deliver_presentations(&options, generation) {
                        mark_error(&status, format!("presentation recovery: {error}"));
                    }
                }
                let outcome = match delivery {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        mark_error(&status, error.to_string());
                        if stop_requested(&stop_rx, options.retry_interval) {
                            break;
                        }
                        continue;
                    }
                };
                let idle_wait = if let Some(delay) = pending_poll_backoff.wait_after(
                    outcome,
                    options.poll_interval,
                    options.retry_interval,
                ) {
                    delay
                } else if outcome.had_messages {
                    Duration::ZERO
                } else {
                    options.poll_interval.saturating_sub(poll_started.elapsed())
                };
                if stop_requested(&stop_rx, idle_wait) {
                    break;
                }
            }
            Err(error) => {
                mark_registration(&status, false);
                mark_error(&status, format!("agent-bus poll failed: {error:#}"));
                if stop_requested(&stop_rx, options.retry_interval) {
                    break;
                }
            }
        }
    }
    if let Ok(mut status) = status.lock() {
        status.running = false;
        status.registered = false;
        status.pending_ack_count = pending_acks.len();
    }
    if let Err(error) = bus.unregister(&options.registration.id) {
        mark_error(&status, format!("agent-bus unregister failed: {error:#}"));
    }
}

fn deliver_polled_messages(
    bus: &dyn RuntimeAgentBus,
    submitter: &dyn InterAgentMessageSubmitter,
    runtime_agent_id: &str,
    recipient_label: &str,
    cutex_session_id: &str,
    thread_id: &str,
    messages: Vec<AgentBusMessage>,
    pending_acks: &mut BTreeSet<String>,
    status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>,
) -> anyhow::Result<DeliverySweepOutcome> {
    let mut outcome = DeliverySweepOutcome {
        had_messages: !messages.is_empty(),
        ..DeliverySweepOutcome::default()
    };
    let mut prepared = Vec::new();
    for message in messages {
        if message.control_type.as_deref() == Some(crate::agent_bus::job_completion::SCHEMA_V2) {
            // A legacy submitter would put the canonical mechanical snapshot
            // into model input. Leave it pending, with no transport ACK.
            mark_error(
                status,
                "Job v2 requires reviewed ExternalInput v2 owner; pending without downgrade".into(),
            );
            agent_bus_message_repository()?.record_external_input_error(
                &message.id,
                "Job v2 requires reviewed ExternalInput v2 owner; no legacy fallback",
            )?;
            outcome.retained_pending = true;
            continue;
        }
        if pending_acks.contains(&message.id) {
            continue;
        }
        if message.is_removed_task_service_message() {
            // r219: a Task Service message an older daemon queued is never
            // submitted and raises no owner notice: finish its record, ACK.
            finish_removed_task_service_record(
                agent_bus_message_repository()?,
                cutex_session_id,
                &message.id,
            )?;
            let ids = [message.id.clone()];
            let acked = bus.ack(runtime_agent_id, &ids)?;
            mark_acknowledged(status, acked, pending_acks.len());
            outcome.made_progress = true;
            continue;
        }
        match inter_agent_params(thread_id, recipient_label, cutex_session_id, &message) {
            Ok(params) => {
                let computed = inter_agent_semantic_sha256(&params);
                let digest = agent_bus_message_repository()?
                    .semantic_sha256(&message.id)?
                    .unwrap_or_else(|| computed.clone());
                if digest != computed {
                    let quarantined = quarantine_or_audit_live_message(
                        cutex_session_id,
                        &message.id,
                        serde_json::json!({"code":"canonical_semantic_digest_mismatch","message":"durable envelope no longer matches its committed digest"}),
                        Utc::now(),
                    )?;
                    if quarantined {
                        let ids = [message.id.clone()];
                        let acked = bus.ack(runtime_agent_id, &ids)?;
                        mark_acknowledged(status, acked, pending_acks.len());
                        outcome.made_progress = true;
                    }
                    continue;
                }
                prepared.push((message, params, digest));
            }
            Err(error) => {
                let quarantined = quarantine_or_audit_live_message(
                    cutex_session_id,
                    &message.id,
                    serde_json::json!({"code":"canonical_envelope_invalid","message":error.to_string()}),
                    Utc::now(),
                )?;
                if quarantined {
                    let ids = [message.id.clone()];
                    let acked = bus.ack(runtime_agent_id, &ids)?;
                    mark_acknowledged(status, acked, pending_acks.len());
                    outcome.made_progress = true;
                }
            }
        }
    }
    if prepared.is_empty() {
        return Ok(outcome);
    }

    for batch in prepared.chunks(256) {
        let initial = query_delivery_statuses_isolated(submitter, thread_id, batch)?;
        let mut malformed = BTreeSet::new();
        for (message, params, _digest) in batch {
            match &initial[params.message_id.as_str()] {
                Ok(status) if status.state == ThreadInterAgentMessageDeliveryState::Unknown => {}
                Ok(_) => continue,
                Err(error) => {
                    quarantine_status_error(
                        bus,
                        runtime_agent_id,
                        cutex_session_id,
                        message,
                        error,
                        pending_acks,
                        status,
                    )?;
                    outcome.made_progress = true;
                    malformed.insert(message.id.clone());
                    continue;
                }
            }
            let input_lock = crate::management::v2::user_input::session_input_lock(cutex_session_id)?;
            let _input_guard = input_lock.lock().map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
            let starts_turn = params.delivery_mode.trigger_turn() && !submitter.thread_busy(thread_id)?;
            if params.delivery_mode.trigger_turn() {
                let busy = !starts_turn;
                let joins_running = busy && params.delivery_mode == crate::agent_bus::delivery::AgentDeliveryMode::Soon;
                if !joins_running && (busy || !submitter.delivery_may_start(cutex_session_id, &message.id)?) {
                    outcome.retained_pending = true;
                    continue;
                }
            }
            let prepared_turn = starts_turn.then(|| crate::management::v2::admission::prepare_turn(cutex_session_id, "delivery", None));
            match submitter.submit_inter_agent_message(params) {
                Ok(submission_id) => {
                    if let Some(prepared) = prepared_turn { prepared.submitted(); }
                    let _ = agent_bus_message_repository()?.record_a2_submission(
                        cutex_session_id,
                        &message.id,
                        &submission_id,
                        Utc::now(),
                    )?;
                    if starts_turn {
                        agent_bus_message_repository()?.record_admission_started_idle(cutex_session_id, &message.id)?;
                    }
                    mark_submitted(status, &message.id, &submission_id, pending_acks.len());
                    outcome.made_progress = true;
                }
                Err(error) => {
                    mark_error(
                        status,
                        format!("A2 submission failed for {}: {error:#}", message.id),
                    );
                }
            }
        }

        let current = query_delivery_statuses_isolated(submitter, thread_id, batch)?;
        for (message, params, _digest) in batch {
            if malformed.contains(&message.id) {
                continue;
            }
            let native = match &current[params.message_id.as_str()] {
                Ok(status) => status,
                Err(error) => {
                    quarantine_status_error(
                        bus,
                        runtime_agent_id,
                        cutex_session_id,
                        message,
                        error,
                        pending_acks,
                        status,
                    )?;
                    outcome.made_progress = true;
                    continue;
                }
            };
            match native.state {
                ThreadInterAgentMessageDeliveryState::ContextPersisted => {
                    let receipt = native
                        .receipt
                        .as_ref()
                        .context("context_persisted status omitted its typed A4 receipt")?;
                    let _ = agent_bus_message_repository()?.record_delivered(
                        cutex_session_id,
                        &message.id,
                        receipt,
                        Utc::now(),
                    )?;
                    pending_acks.insert(message.id.clone());
                    let ids = [message.id.clone()];
                    let acked = bus.ack(runtime_agent_id, &ids).with_context(|| {
                        format!(
                            "failed to acknowledge A4-proven agent-bus message {}",
                            message.id
                        )
                    })?;
                    pending_acks.remove(&message.id);
                    mark_acknowledged(status, acked, pending_acks.len());
                    outcome.made_progress = true;
                }
                ThreadInterAgentMessageDeliveryState::Conflict => {
                    let quarantined = quarantine_or_audit_live_message(
                    cutex_session_id,
                    &message.id,
                    serde_json::json!({"code":"native_semantic_conflict","message":"receiver reported the message ID with different semantics"}),
                    Utc::now(),
                )?;
                    if quarantined {
                        let ids = [message.id.clone()];
                        let acked = bus.ack(runtime_agent_id, &ids)?;
                        mark_acknowledged(status, acked, pending_acks.len());
                        outcome.made_progress = true;
                    }
                }
                ThreadInterAgentMessageDeliveryState::Unknown
                | ThreadInterAgentMessageDeliveryState::Pending
                | ThreadInterAgentMessageDeliveryState::RetryableError => {
                    outcome.retained_pending = true;
                }
            }
        }
    }
    Ok(outcome)
}

fn query_delivery_statuses_isolated<'a>(
    submitter: &dyn InterAgentMessageSubmitter,
    thread_id: &str,
    prepared: &'a [(AgentBusMessage, ThreadInterAgentMessageParams, String)],
) -> anyhow::Result<BTreeMap<&'a str, Result<super::commands::ThreadInterAgentMessageStatus, String>>>
{
    match query_delivery_statuses(submitter, thread_id, prepared) {
        Ok(statuses) => Ok(statuses
            .into_iter()
            .map(|(id, status)| (id, Ok(status)))
            .collect()),
        Err(error) if prepared.len() > 1 && is_malformed_status_error(&error) => {
            let mut isolated = BTreeMap::new();
            for item in prepared {
                match query_delivery_statuses(submitter, thread_id, std::slice::from_ref(item)) {
                    Ok(mut statuses) => {
                        let id = item.1.message_id.as_str();
                        isolated.insert(
                            id,
                            Ok(statuses
                                .remove(id)
                                .expect("single status response contains its requested id")),
                        );
                    }
                    Err(error) if is_malformed_status_error(&error) => {
                        isolated.insert(item.1.message_id.as_str(), Err(error.to_string()));
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(isolated)
        }
        Err(error) if is_malformed_status_error(&error) => Ok(BTreeMap::from([(
            prepared[0].1.message_id.as_str(),
            Err(error.to_string()),
        )])),
        Err(error) => Err(error),
    }
}

fn is_malformed_status_error(error: &anyhow::Error) -> bool {
    error.to_string().starts_with("malformed A4 status:")
}

/// A permanent failure must leave the live queue even when an old producer
/// never wrote a durable record. Preserve an audit fact before authorizing ACK.
fn quarantine_or_audit_live_message(
    owner: &str,
    message_id: &str,
    error: serde_json::Value,
    at: chrono::DateTime<Utc>,
) -> anyhow::Result<bool> {
    if !agent_bus_message_repository()?.record_quarantined(owner, message_id, error.clone(), at)? {
        crate::agent_bus::audit::append_agent_bus_audit_record(serde_json::json!({
            "event": "live_message_terminal", "timestamp": at.to_rfc3339(),
            "message_id": message_id, "to_cutex_session_id": owner,
            "durable": false, "error": error,
        }))?;
    }
    eprintln!("warning: terminating agent-bus message {message_id}: {error}");
    Ok(true)
}

fn quarantine_status_error(
    bus: &dyn RuntimeAgentBus,
    runtime_agent_id: &str,
    cutex_session_id: &str,
    message: &AgentBusMessage,
    error: &str,
    pending_acks: &BTreeSet<String>,
    status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>,
) -> anyhow::Result<()> {
    let quarantined = quarantine_or_audit_live_message(
        cutex_session_id,
        &message.id,
        serde_json::json!({"code":"native_status_malformed","message":error}),
        Utc::now(),
    )?;
    if quarantined {
        let ids = [message.id.clone()];
        let acked = bus.ack(runtime_agent_id, &ids)?;
        mark_acknowledged(status, acked, pending_acks.len());
    }
    mark_error(
        status,
        format!("malformed A4 status for {}: {error}", message.id),
    );
    Ok(())
}

fn query_delivery_statuses<'a>(
    submitter: &dyn InterAgentMessageSubmitter,
    thread_id: &str,
    prepared: &'a [(AgentBusMessage, ThreadInterAgentMessageParams, String)],
) -> anyhow::Result<BTreeMap<&'a str, super::commands::ThreadInterAgentMessageStatus>> {
    let params = ThreadInterAgentMessageStatusParams {
        thread_id: thread_id.to_string(),
        messages: prepared
            .iter()
            .map(|(_, params, digest)| ThreadInterAgentMessageStatusQuery {
                message_id: params.message_id.clone(),
                semantic_sha256: digest.clone(),
            })
            .collect(),
    };
    let response = submitter.inter_agent_message_status(&params)?;
    if response.schema != INTER_AGENT_STATUS_SCHEMA
        || response.thread_id != thread_id
        || response.statuses.len() != prepared.len()
    {
        anyhow::bail!("malformed A4 status: response batch identity is invalid");
    }
    let mut result = BTreeMap::new();
    for ((_, requested, digest), status) in prepared.iter().zip(response.statuses) {
        if status.message_id != requested.message_id || status.semantic_sha256 != *digest {
            anyhow::bail!("malformed A4 status: response changed request identity or order");
        }
        if status.state == ThreadInterAgentMessageDeliveryState::ContextPersisted {
            let receipt = status
                .receipt
                .as_ref()
                .context("malformed A4 status: context_persisted omitted receipt")?;
            if receipt.receipt_id.trim().is_empty()
                || receipt.response_item_id != requested.message_id
                || receipt.turn_id.trim().is_empty()
                || receipt
                    .thread_id
                    .as_deref()
                    .is_some_and(|value| value != thread_id)
                || receipt
                    .message_id
                    .as_deref()
                    .is_some_and(|value| value != requested.message_id)
                || receipt
                    .semantic_sha256
                    .as_deref()
                    .is_some_and(|value| value != digest)
            {
                anyhow::bail!("malformed A4 status: context_persisted receipt is invalid");
            }
        } else if status.receipt.is_some() {
            anyhow::bail!("malformed A4 status: non-A4 state included a receipt");
        }
        result.insert(requested.message_id.as_str(), status);
    }
    Ok(result)
}

fn retry_pending_acks(
    bus: &dyn RuntimeAgentBus,
    runtime_agent_id: &str,
    pending_acks: &mut BTreeSet<String>,
    status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>,
) -> anyhow::Result<()> {
    let message_ids = pending_acks.iter().cloned().collect::<Vec<_>>();
    let acked = bus.ack(runtime_agent_id, &message_ids)?;
    pending_acks.clear();
    mark_acknowledged(status, acked, 0);
    Ok(())
}

pub fn inter_agent_params(
    thread_id: &str,
    recipient_label: &str,
    recipient_cutex_session_id: &str,
    message: &AgentBusMessage,
) -> anyhow::Result<ThreadInterAgentMessageParams> {
    let recipient_metadata =
        Some(RegistryParticipantMetadataResolver.resolve(recipient_cutex_session_id));
    if message.sender_kind.is_alarm_system() {
        return alarm_inter_agent_params(thread_id, recipient_cutex_session_id, message);
    }
    if message.kind == AgentBusEnvelopeKind::Message
        && (message.from == AGENT_MANAGEMENT_SYSTEM_SENDER
            || message.control_type.as_deref() == Some(AGENT_MANAGEMENT_START_CONTROL_TYPE))
    {
        return agent_management_inter_agent_params(
            thread_id,
            recipient_label,
            recipient_metadata,
            message,
        );
    }
    if message.kind == AgentBusEnvelopeKind::Message
        && message.sender_kind.is_agent()
        && crate::agent_bus::peer_tell::is_peer_tell(message)
    {
        // Cross-host tell: a service source, never a local agent author.
        let (_, content) = crate::agent_bus::peer_tell::model_text(message)?;
        return Ok(ThreadInterAgentMessageParams {
            thread_id: thread_id.to_string(),
            message_id: model_visible_message_id(&message.id),
            author: agent_path_for_bus_label(crate::agent_bus::peer_tell::SOURCE_ID),
            author_metadata: Some(system_participant(crate::agent_bus::peer_tell::SOURCE_ID)),
            recipient: "/root".to_string(),
            recipient_metadata,
            other_recipients: Vec::new(),
            content,
            delivery_mode: message.delivery_mode.clone(),
        });
    }
    if message.kind == AgentBusEnvelopeKind::Message && message.sender_kind.is_agent() {
        if message.id.trim().is_empty() {
            anyhow::bail!("agent-bus message id must not be empty");
        }
        if message.content.trim().is_empty() {
            anyhow::bail!("agent-bus message {} has empty content", message.id);
        }
        return Ok(ThreadInterAgentMessageParams {
            thread_id: thread_id.to_string(),
            message_id: model_visible_message_id(&message.id),
            author: agent_path_for_bus_label(&message.from),
            author_metadata: message
                .from_cutex_session_id
                .as_deref()
                .map(|session_id| RegistryParticipantMetadataResolver.resolve(session_id)),
            recipient: "/root".to_string(),
            recipient_metadata,
            other_recipients: Vec::new(),
            content: format!(
                "Message Type: MESSAGE\nTask name: {recipient_label}\nSender: {}\nPayload:\n{}",
                message.from, message.content
            ),
            delivery_mode: message.delivery_mode.clone(),
        });
    }
    if message.kind == AgentBusEnvelopeKind::Message && message.sender_kind.is_job_service_system()
    {
        return job_service_inter_agent_params(
            thread_id,
            recipient_cutex_session_id,
            recipient_metadata,
            message,
        );
    }
    anyhow::bail!(
        "agent-bus message {} is not an inter-agent message",
        message.id
    )
}

/// Stable system projection, also used by Claude. This is deliberately independent
/// of the alarm store: the scheduler commits `queued` after the Bus accepts it.
pub fn alarm_inter_agent_params(
    thread_id: &str,
    recipient_cutex_session_id: &str,
    message: &AgentBusMessage,
) -> anyhow::Result<ThreadInterAgentMessageParams> {
    use crate::agent_bus::{alarms::ALARM_SYSTEM_SENDER, delivery::AgentDeliveryMode};
    if !(message.kind == AgentBusEnvelopeKind::Message
            && message.sender_kind.is_alarm_system()
            && message.from == ALARM_SYSTEM_SENDER
            && message.from_cutex_session_id.is_none()
            && message.to_cutex_session_id.as_deref() == Some(recipient_cutex_session_id)
            && message.control_type.is_none()
            && message.control_payload.is_none()
            && message.delivery_mode == AgentDeliveryMode::AfterTurn
            && message.external_message_id.as_deref().is_some_and(|id| id.starts_with("alarm-"))
            && !message.id.trim().is_empty()
            && !message.content.trim().is_empty()) {
        return Err(crate::agent_bus::alarms::InvalidAlarmProvenance(
            format!("alarm message {} has invalid system provenance", message.id)
        ).into());
    }
    Ok(ThreadInterAgentMessageParams {
        thread_id: thread_id.into(),
        message_id: model_visible_message_id(&message.id),
        author: agent_path_for_bus_label(ALARM_SYSTEM_SENDER),
        author_metadata: Some(system_participant(ALARM_SYSTEM_SENDER)),
        recipient: "/root".into(),
        recipient_metadata: Some(RegistryParticipantMetadataResolver.resolve(recipient_cutex_session_id)),
        other_recipients: Vec::new(),
        content: format!("Message Type: ALARM\nPayload:\n{}", message.content),
        delivery_mode: AgentDeliveryMode::AfterTurn,
    })
}

fn job_service_inter_agent_params(
    thread_id: &str,
    recipient_cutex_session_id: &str,
    recipient_metadata: Option<ParticipantPresentationMetadata>,
    message: &AgentBusMessage,
) -> anyhow::Result<ThreadInterAgentMessageParams> {
    use crate::agent_bus::model::{JobServiceCompletionRequest, JOB_SERVICE_COMPLETION_SCHEMA};

    if message.control_type.as_deref() == Some(crate::agent_bus::job_completion::SCHEMA_V2) {
        let frozen = crate::agent_bus::job_completion::FrozenProjection::from_message(message)?;
        anyhow::ensure!(
            frozen.request.target_cutex_session_id == recipient_cutex_session_id,
            "Job v2 recipient mismatch"
        );
        return Ok(ThreadInterAgentMessageParams {
            thread_id: thread_id.into(),
            message_id: model_visible_message_id(&message.id),
            author: agent_path_for_bus_label("cutex-job-service"),
            author_metadata: Some(system_participant("Cutex Job Service")),
            recipient: "/root".into(),
            recipient_metadata,
            other_recipients: Vec::new(),
            content: message.content.clone(),
            delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn,
        });
    }

    if message.from != "cutex-job-service"
        || message.from_cutex_session_id.is_some()
        || message.to_cutex_session_id.as_deref() != Some(recipient_cutex_session_id)
        || message.control_type.as_deref() != Some(JOB_SERVICE_COMPLETION_SCHEMA)
        || message.delivery_mode != crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
    {
        anyhow::bail!(
            "Job Service message {} has invalid protected provenance",
            message.id
        );
    }
    let metadata: JobServiceCompletionRequest = serde_json::from_value(
        message
            .control_payload
            .clone()
            .context("Job Service completion is missing structured metadata")?,
    )
    .context("Job Service completion metadata is invalid")?;
    if metadata.schema != JOB_SERVICE_COMPLETION_SCHEMA
        || metadata.target_cutex_session_id != recipient_cutex_session_id
        || message.external_message_id.as_deref() != Some(metadata.event_id.as_str())
    {
        anyhow::bail!(
            "Job Service message {} has inconsistent metadata",
            message.id
        );
    }
    Ok(ThreadInterAgentMessageParams {
        thread_id: thread_id.to_string(),
        message_id: model_visible_message_id(&message.id),
        author: agent_path_for_bus_label("cutex-job-service"),
        author_metadata: Some(system_participant("Cutex Job Service")),
        recipient: "/root".to_string(),
        recipient_metadata,
        other_recipients: Vec::new(),
        content: message.content.clone(),
        delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn,
    })
}

fn agent_management_metadata(
    message: &AgentBusMessage,
) -> anyhow::Result<AgentManagementMessageMetadata> {
    if message.from != AGENT_MANAGEMENT_SYSTEM_SENDER {
        anyhow::bail!(
            "Agent Management agent-bus message {} has a noncanonical sender",
            message.id,
        );
    }
    // Agent Management retains the existing Agent Bus wire kind; the opaque
    // in-process principal and reserved control record select this projection.
    if !message.sender_kind.is_agent() {
        anyhow::bail!(
            "Agent Management agent-bus message {} has an invalid sender kind",
            message.id,
        );
    }
    if message.id.trim().is_empty() {
        anyhow::bail!("agent-bus message id must not be empty");
    }
    if message.content.trim().is_empty() {
        anyhow::bail!("agent-bus message {} has empty content", message.id);
    }
    if !matches!(
        message.control_type.as_deref(),
        Some(AGENT_MANAGEMENT_START_CONTROL_TYPE | AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE)
    ) {
        anyhow::bail!(
            "Agent Management agent-bus message {} has an invalid control type",
            message.id
        );
    }
    let control_payload = message.control_payload.clone().with_context(|| {
        format!(
            "Agent Management agent-bus message {} is missing control metadata",
            message.id
        )
    })?;
    let metadata: AgentManagementMessageMetadata = serde_json::from_value(control_payload)
        .with_context(|| {
            format!(
                "Agent Management agent-bus message {} has invalid control metadata",
                message.id
            )
        })?;
    if metadata.schema != AgentManagementSchema::V1 {
        anyhow::bail!(
            "Agent Management agent-bus message {} has an inconsistent metadata schema",
            message.id
        );
    }
    message
        .external_message_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| {
            format!(
                "Agent Management agent-bus message {} is missing its external message ID",
                message.id
            )
        })?;
    Ok(metadata)
}

fn agent_management_inter_agent_params(
    thread_id: &str,
    recipient_label: &str,
    recipient_metadata: Option<ParticipantPresentationMetadata>,
    message: &AgentBusMessage,
) -> anyhow::Result<ThreadInterAgentMessageParams> {
    let metadata = agent_management_metadata(message)?;
    Ok(ThreadInterAgentMessageParams {
        thread_id: thread_id.to_string(),
        message_id: model_visible_message_id(&message.id),
        author: agent_path_for_bus_label(AGENT_MANAGEMENT_SYSTEM_SENDER),
        author_metadata: Some(system_participant("Cutex Agent Management")),
        recipient: "/root".to_string(),
        recipient_metadata,
        other_recipients: Vec::new(),
        content: if message.control_type.as_deref() == Some(AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE) {
            format!(
                "Message Type: NOTICE\nTask name: {recipient_label}\nSender: {AGENT_MANAGEMENT_SYSTEM_SENDER}\nPayload:\n{}",
                message.content,
            )
        } else {
            format!(
                "Message Type: MESSAGE\nTask name: {recipient_label}\nSender: {AGENT_MANAGEMENT_SYSTEM_SENDER}\nRequested By Director: {}\nPayload:\n{}",
                metadata.requested_by_director.as_str(),
                message.content,
            )
        },
        delivery_mode: message.delivery_mode.clone(),
    })
}

fn system_participant(display_name: &str) -> ParticipantPresentationMetadata {
    ParticipantPresentationMetadata {
        display_name: Some(display_name.to_string()),
        ..Default::default()
    }
}

/// Projects an Agent Bus record ID into the bounded app-server item-ID namespace.
/// Existing bridge IDs remain byte-for-byte stable when they already fit. Only
/// oversized IDs use an `amsg_`-prefixed, domain-separated SHA-256 projection
/// with 236 bits of collision strength so cute-codex retains the native item ID.
pub fn model_visible_message_id(agent_bus_message_id: &str) -> String {
    let current = format!("{MODEL_VISIBLE_MESSAGE_ID_PREFIX}{agent_bus_message_id}");
    if current.chars().count() <= MODEL_VISIBLE_MESSAGE_ID_MAX_CHARS {
        return current;
    }

    let digest_input = format!("{MODEL_VISIBLE_MESSAGE_ID_HASH_DOMAIN}{current}");
    let digest = crate::role_revision::sha256_bytes(digest_input.as_bytes());
    let digest_chars = MODEL_VISIBLE_MESSAGE_ID_MAX_CHARS - MODEL_VISIBLE_MESSAGE_ID_PREFIX.len();
    format!(
        "{MODEL_VISIBLE_MESSAGE_ID_PREFIX}{}",
        &digest.as_str()[..digest_chars]
    )
}

pub fn inter_agent_semantic_sha256(params: &ThreadInterAgentMessageParams) -> String {
    fn field(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    let mut hasher = Sha256::new();
    hasher.update(INTER_AGENT_SEMANTIC_HASH_DOMAIN);
    field(&mut hasher, params.message_id.as_bytes());
    field(&mut hasher, params.author.as_bytes());
    field(&mut hasher, params.recipient.as_bytes());
    hasher.update((params.other_recipients.len() as u64).to_be_bytes());
    for recipient in &params.other_recipients {
        field(&mut hasher, recipient.as_bytes());
    }
    field(&mut hasher, params.delivery_mode.event_label().as_bytes());
    field(&mut hasher, params.content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn agent_path_for_bus_label(label: &str) -> String {
    let mut segment = String::new();
    let mut previous_underscore = false;
    for character in label.chars() {
        let character = if character.is_ascii_alphanumeric() {
            character.to_ascii_lowercase()
        } else {
            '_'
        };
        if character == '_' && previous_underscore {
            continue;
        }
        segment.push(character);
        previous_underscore = character == '_';
        if segment.len() >= 48 {
            break;
        }
    }
    let segment = segment.trim_matches('_');
    let segment = if segment.is_empty() || segment == "root" {
        "external_agent"
    } else {
        segment
    };
    format!("/root/{segment}")
}

fn stop_requested(stop_rx: &mpsc::Receiver<()>, timeout: Duration) -> bool {
    match stop_rx.recv_timeout(timeout) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => true,
        Err(mpsc::RecvTimeoutError::Timeout) => false,
    }
}

fn mark_registration(status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>, registered: bool) {
    if let Ok(mut status) = status.lock() {
        status.registered = registered;
    }
}

fn mark_poll(status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>) {
    if let Ok(mut status) = status.lock() {
        status.last_poll_at = Some(Utc::now().to_rfc3339());
    }
}

fn mark_submitted(
    status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>,
    message_id: &str,
    submission_id: &str,
    pending_ack_count: usize,
) {
    if let Ok(mut status) = status.lock() {
        status.submitted_count = status.submitted_count.saturating_add(1);
        status.last_message_id = Some(message_id.to_string());
        status.last_submission_id = Some(submission_id.to_string());
        status.pending_ack_count = pending_ack_count;
    }
}

fn mark_acknowledged(
    status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>,
    acknowledged: usize,
    pending_ack_count: usize,
) {
    if let Ok(mut status) = status.lock() {
        status.acknowledged_count = status
            .acknowledged_count
            .saturating_add(acknowledged as u64);
        status.pending_ack_count = pending_ack_count;
    }
}

fn mark_error(status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>, error: String) {
    if let Ok(mut status) = status.lock() {
        status.last_error = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bus::delivery::AgentDeliveryMode;
    use crate::agent_bus::model::AgentMessageKind;
    use crate::agent_bus::model::AgentRegistrationClass;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Condvar;

    // The bridge calls process-global repositories and admission hooks. Each
    // test gets a separate process and home, including its worker threads.
    pub(super) fn test_runs_in_private_home(name: &str) -> bool {
        const CHILD: &str = "CUTEX_BUS_TEST_CHILD";
        if std::env::var(CHILD).ok().as_deref() == Some(name) { return false; }
        let parent = std::env::var_os("CUTEX_TEST_PRIVATE_HOME").map(std::path::PathBuf::from)
            .expect("bridge tests require the private test home");
        let root = parent.join(format!("bus-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".cutex-test-private-home"), b"bus isolated test\n").unwrap();
        let listing = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--list"]).output().unwrap();
        assert!(listing.status.success()
            && String::from_utf8_lossy(&listing.stdout).lines().any(|line| line == format!("{name}: test")),
            "isolated bridge test was not found: {name}");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("HOME", &root).env("CUTEX_TEST_PRIVATE_HOME", &root).env(CHILD, name)
            .status().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(status.success(), "isolated bridge test failed: {name}");
        true
    }

    #[test]
    fn isolated_private_home_exercises_real_a4_repository_boundary() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::isolated_private_home_exercises_real_a4_repository_boundary") { return; }
        stable_recipient_digest_delivers_once_with_a4_and_one_ack();
    }

    #[derive(Default)]
    struct FakeBus {
        events: Mutex<Vec<String>>,
        ack_results: Mutex<VecDeque<anyhow::Result<usize>>>,
    }

    impl RuntimeAgentBus for FakeBus {
        fn register(&self, _request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
            self.events
                .lock()
                .expect("events lock")
                .push("register".to_string());
            Ok(())
        }

        fn unregister(&self, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn poll(&self, _agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            Ok(Vec::new())
        }

        fn ack(&self, _agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
            self.events
                .lock()
                .expect("events lock")
                .push(format!("ack:{}", message_ids.join(",")));
            self.ack_results
                .lock()
                .expect("ack results lock")
                .pop_front()
                .unwrap_or_else(|| Ok(message_ids.len()))
        }
    }

    struct FakeSubmitter {
        events: Arc<Mutex<Vec<String>>>,
        result: Mutex<Option<anyhow::Result<String>>>,
    }

    struct SelectiveStatusSubmitter {
        events: Arc<Mutex<Vec<String>>>,
        unavailable: bool,
    }

    struct MalformedFirstStatusSubmitter;

    struct ConflictFirstStatusSubmitter;

    impl InterAgentMessageSubmitter for ConflictFirstStatusSubmitter {
        fn delivery_may_start(&self, _session: &str, _id: &str) -> anyhow::Result<bool> { Ok(true) }

        fn thread_busy(&self, _thread_id: &str) -> anyhow::Result<bool> { Ok(false) }

        fn submit_inter_agent_message(
            &self,
            params: &ThreadInterAgentMessageParams,
        ) -> anyhow::Result<String> {
            anyhow::bail!("unexpected submit for {}", params.message_id)
        }

        fn inter_agent_message_status(
            &self,
            params: &ThreadInterAgentMessageStatusParams,
        ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
            Ok(ThreadInterAgentMessageStatusResponse {
                schema: INTER_AGENT_STATUS_SCHEMA.to_string(),
                thread_id: params.thread_id.clone(),
                statuses: params
                    .messages
                    .iter()
                    .map(|query| {
                        let conflict = query.message_id.ends_with("message-1");
                        let state = if conflict {
                            ThreadInterAgentMessageDeliveryState::Conflict
                        } else {
                            ThreadInterAgentMessageDeliveryState::ContextPersisted
                        };
                        super::super::commands::ThreadInterAgentMessageStatus {
                            message_id: query.message_id.clone(),
                            state,
                            semantic_sha256: query.semantic_sha256.clone(),
                            receipt: (!conflict).then(|| InterAgentContextPersistedReceipt {
                                schema: None,
                                receipt_id: format!("a4r_{}", query.message_id),
                                thread_id: None,
                                message_id: None,
                                semantic_sha256: None,
                                response_item_id: query.message_id.clone(),
                                turn_id: "turn-1".to_string(),
                                rollout_ordinal: 1,
                            }),
                        }
                    })
                    .collect(),
            })
        }
    }

    impl InterAgentMessageSubmitter for MalformedFirstStatusSubmitter {
        fn delivery_may_start(&self, _session: &str, _id: &str) -> anyhow::Result<bool> { Ok(true) }

        fn thread_busy(&self, _thread_id: &str) -> anyhow::Result<bool> { Ok(false) }

        fn submit_inter_agent_message(
            &self,
            params: &ThreadInterAgentMessageParams,
        ) -> anyhow::Result<String> {
            anyhow::bail!("unexpected submit for {}", params.message_id)
        }

        fn inter_agent_message_status(
            &self,
            params: &ThreadInterAgentMessageStatusParams,
        ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
            Ok(ThreadInterAgentMessageStatusResponse {
                schema: INTER_AGENT_STATUS_SCHEMA.to_string(),
                thread_id: params.thread_id.clone(),
                statuses: params
                    .messages
                    .iter()
                    .map(|query| {
                        let malformed = query.message_id.ends_with("message-1");
                        super::super::commands::ThreadInterAgentMessageStatus {
                            message_id: query.message_id.clone(),
                            state: ThreadInterAgentMessageDeliveryState::ContextPersisted,
                            semantic_sha256: query.semantic_sha256.clone(),
                            receipt: Some(InterAgentContextPersistedReceipt {
                                schema: None,
                                receipt_id: format!("a4r_{}", query.message_id),
                                thread_id: None,
                                message_id: None,
                                semantic_sha256: None,
                                response_item_id: if malformed {
                                    "wrong-response-item".to_string()
                                } else {
                                    query.message_id.clone()
                                },
                                turn_id: "turn-1".to_string(),
                                rollout_ordinal: 1,
                            }),
                        }
                    })
                    .collect(),
            })
        }
    }

    impl InterAgentMessageSubmitter for SelectiveStatusSubmitter {
        fn delivery_may_start(&self, _session: &str, _id: &str) -> anyhow::Result<bool> { Ok(true) }

        fn thread_busy(&self, _thread_id: &str) -> anyhow::Result<bool> { Ok(false) }

        fn submit_inter_agent_message(
            &self,
            params: &ThreadInterAgentMessageParams,
        ) -> anyhow::Result<String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("submit:{}", params.message_id));
            Ok(format!("a2_{}", params.message_id))
        }

        fn inter_agent_message_status(
            &self,
            params: &ThreadInterAgentMessageStatusParams,
        ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
            if self.unavailable {
                anyhow::bail!("method not found: thread/inter_agent_message/status");
            }
            let submitted = self.events.lock().unwrap().clone();
            Ok(ThreadInterAgentMessageStatusResponse {
                schema: INTER_AGENT_STATUS_SCHEMA.to_string(),
                thread_id: params.thread_id.clone(),
                statuses: params
                    .messages
                    .iter()
                    .map(|query| {
                        let is_later = query.message_id.ends_with("message-2");
                        let was_submitted = submitted
                            .iter()
                            .any(|event| event == &format!("submit:{}", query.message_id));
                        let state = if is_later && was_submitted {
                            ThreadInterAgentMessageDeliveryState::ContextPersisted
                        } else if is_later {
                            ThreadInterAgentMessageDeliveryState::Unknown
                        } else {
                            ThreadInterAgentMessageDeliveryState::Pending
                        };
                        super::super::commands::ThreadInterAgentMessageStatus {
                            message_id: query.message_id.clone(),
                            state,
                            semantic_sha256: query.semantic_sha256.clone(),
                            receipt: (state
                                == ThreadInterAgentMessageDeliveryState::ContextPersisted)
                                .then(|| InterAgentContextPersistedReceipt {
                                    schema: Some(INTER_AGENT_STATUS_SCHEMA.to_string()),
                                    receipt_id: format!("a4r_{}", query.message_id),
                                    thread_id: Some(params.thread_id.clone()),
                                    message_id: Some(query.message_id.clone()),
                                    semantic_sha256: Some(query.semantic_sha256.clone()),
                                    response_item_id: query.message_id.clone(),
                                    turn_id: "turn-later".to_string(),
                                    rollout_ordinal: 9,
                                }),
                        }
                    })
                    .collect(),
            })
        }
    }

    impl InterAgentMessageSubmitter for FakeSubmitter {
        fn delivery_may_start(&self, _session: &str, _id: &str) -> anyhow::Result<bool> { Ok(true) }

        fn thread_busy(&self, _thread_id: &str) -> anyhow::Result<bool> { Ok(false) }

        fn submit_inter_agent_message(
            &self,
            params: &ThreadInterAgentMessageParams,
        ) -> anyhow::Result<String> {
            self.events
                .lock()
                .expect("events lock")
                .push(format!("submit:{}", params.message_id));
            let mut configured = self.result.lock().expect("result lock");
            let result = configured
                .take()
                .unwrap_or_else(|| Ok("submission-1".to_string()));
            if result.is_ok() {
                *configured = Some(Ok("__a4_persisted__".to_string()));
            }
            result
        }

        fn inter_agent_message_status(
            &self,
            params: &ThreadInterAgentMessageStatusParams,
        ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
            let submitted = self
                .result
                .lock()
                .expect("result lock")
                .as_ref()
                .is_some_and(|result| {
                    result
                        .as_ref()
                        .is_ok_and(|value| value == "__a4_persisted__")
                });
            let state = if submitted {
                ThreadInterAgentMessageDeliveryState::ContextPersisted
            } else {
                ThreadInterAgentMessageDeliveryState::Unknown
            };
            Ok(ThreadInterAgentMessageStatusResponse {
                schema: INTER_AGENT_STATUS_SCHEMA.to_string(),
                thread_id: params.thread_id.clone(),
                statuses: params
                    .messages
                    .iter()
                    .map(
                        |query| super::super::commands::ThreadInterAgentMessageStatus {
                            message_id: query.message_id.clone(),
                            state,
                            semantic_sha256: query.semantic_sha256.clone(),
                            receipt: (state
                                == ThreadInterAgentMessageDeliveryState::ContextPersisted)
                                .then(|| InterAgentContextPersistedReceipt {
                                    schema: None,
                                    receipt_id: format!("a4r_{}", query.message_id),
                                    thread_id: None,
                                    message_id: None,
                                    semantic_sha256: None,
                                    response_item_id: query.message_id.clone(),
                                    turn_id: "turn-1".to_string(),
                                    rollout_ordinal: 1,
                                }),
                        },
                    )
                    .collect(),
            })
        }
    }

    #[test]
    fn alarm_projection_has_stable_system_digest_and_legacy_delivery() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::alarm_projection_has_stable_system_digest_and_legacy_delivery") { return; }
        let mut message = test_message(AgentDeliveryMode::AfterTurn);
        message.id = "alarm-fixture".into();
        message.sender_kind = AgentMessageKind::AlarmSystem;
        message.from = crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        message.to_cutex_session_id = Some("cutex.thread-1".into());
        message.external_message_id = Some("alarm-fixture".into());
        let params = inter_agent_params("thread-1", "renamable recipient", "cutex.thread-1", &message).unwrap();
        assert_eq!(params.content, "Message Type: ALARM\nPayload:\nhello");
        assert_eq!(params.author_metadata.as_ref().unwrap().display_name.as_deref(), Some("Cutex Alarm"));
        assert_eq!(inter_agent_semantic_sha256(&params), "e9c18344a81082bcc4bf8dd5d9dfdffc0b3506ab03975a76a1d65a664bed3990");
        let renamed = inter_agent_params("new-thread", "new name", "cutex.thread-1", &message).unwrap();
        assert_eq!(inter_agent_semantic_sha256(&params), inter_agent_semantic_sha256(&renamed));
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus { inner: FakeBus::default(), events: events.clone() };
        let submitter = FakeSubmitter { events: events.clone(), result: Mutex::new(Some(Ok("alarm-submission".into()))) };
        let outcome = deliver_polled_messages(&bus, &submitter, "runtime-1", "renamed", "cutex.thread-1", "thread-1",
            vec![message.clone()], &mut BTreeSet::new(), &test_status()).unwrap();
        assert!(outcome.made_progress);
        assert_eq!(*events.lock().unwrap(), ["submit:amsg_alarm-fixture", "ack:alarm-fixture"]);
        message.from_cutex_session_id = Some("cutex.forged".into());
        assert!(inter_agent_params("thread-1", "receiver", "cutex.thread-1", &message).is_err());
    }

    #[test]
    fn malformed_live_only_message_is_audited_and_acked_without_native_submission() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::malformed_live_only_message_is_audited_and_acked_without_native_submission") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus { inner: FakeBus::default(), events: events.clone() };
        let submitter = FakeSubmitter { events: events.clone(), result: Mutex::new(None) };
        let mut message = test_message(AgentDeliveryMode::AfterTurn);
        message.id = format!("live-malformed-{}", uuid::Uuid::new_v4());
        message.sender_kind = AgentMessageKind::AlarmSystem;
        let outcome = deliver_polled_messages(&bus, &submitter, "runtime-1", "receiver", "cutex.thread-1", "thread-1",
            vec![message.clone()], &mut BTreeSet::new(), &test_status()).unwrap();
        assert!(outcome.made_progress);
        assert_eq!(*events.lock().unwrap(), [format!("ack:{}", message.id)]);
        let audit = std::fs::read_to_string(crate::config::paths::runtime_dir().unwrap().join("agent-bus-audit.jsonl")).unwrap();
        assert!(audit.lines().any(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["event"] == "live_message_terminal" && value["message_id"] == message.id
        }));
    }

    #[test]
    fn inter_agent_params_wrap_payload_once_and_preserve_native_paths() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::inter_agent_params_wrap_payload_once_and_preserve_native_paths") { return; }
        let mut message = test_message(AgentDeliveryMode::Interrupt);
        message.id = "11111111-2222-3333-4444-555555555555".to_string();
        message.content = "Message Type: NEW_TASK\nline two\nμ-tail\n".to_string();
        let params = inter_agent_params("thread-1", "receiver-agent", "cutex.receiver", &message)
            .expect("map message");

        assert_eq!(params.thread_id, "thread-1");
        assert_eq!(
            params.message_id,
            "amsg_11111111-2222-3333-4444-555555555555"
        );
        assert_eq!(params.author, "/root/sender_agent");
        assert_eq!(params.recipient, "/root");
        assert!(params.other_recipients.is_empty());
        assert_eq!(
            params.content,
            "Message Type: MESSAGE\nTask name: receiver-agent\nSender: Sender Agent\nPayload:\nMessage Type: NEW_TASK\nline two\nμ-tail\n"
        );
        assert_eq!(params.delivery_mode, AgentDeliveryMode::Interrupt);
    }

    #[test]
    fn semantic_digest_matches_frozen_cross_runtime_golden_vector() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::semantic_digest_matches_frozen_cross_runtime_golden_vector") { return; }
        let params = ThreadInterAgentMessageParams {
            thread_id: "presentation-only-thread".to_string(),
            message_id: "mail_a4_restart_1".to_string(),
            author: "/root/sender".to_string(),
            author_metadata: Some(ParticipantPresentationMetadata {
                display_name: Some("Sender presentation".to_string()),
                ..Default::default()
            }),
            recipient: "/root/recipient".to_string(),
            recipient_metadata: Some(ParticipantPresentationMetadata {
                display_name: Some("Recipient presentation".to_string()),
                ..Default::default()
            }),
            other_recipients: vec!["/root/observer".to_string()],
            content: "hello from sender".to_string(),
            delivery_mode: AgentDeliveryMode::Passive,
        };

        assert_eq!(
            inter_agent_semantic_sha256(&params),
            "8b677efc23d0fe2c4b11d23d4fa0c2af7049d9afa37aa5de2b214b9d6c83bdef"
        );
    }

    #[test]
    fn model_visible_message_id_preserves_valid_ids_and_projects_long_ids_stably() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::model_visible_message_id_preserves_valid_ids_and_projects_long_ids_stably") { return; }
        let uuid = "11111111-2222-3333-4444-555555555555";
        assert_eq!(
            model_visible_message_id(uuid),
            "amsg_11111111-2222-3333-4444-555555555555"
        );

        let pathological = format!("tsc_tsn-{}", "a".repeat(64));
        assert_eq!(format!("amsg_{pathological}").len(), 77);
        let projected = model_visible_message_id(&pathological);
        assert_eq!(
            projected.chars().count(),
            MODEL_VISIBLE_MESSAGE_ID_MAX_CHARS
        );
        assert!(projected.starts_with(MODEL_VISIBLE_MESSAGE_ID_PREFIX));
        assert!(projected[MODEL_VISIBLE_MESSAGE_ID_PREFIX.len()..]
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()));
        assert_eq!(projected, model_visible_message_id(&pathological));

        let distinct = format!("tsc_tsn-{}", "b".repeat(64));
        assert_ne!(projected, model_visible_message_id(&distinct));
    }

    #[test]
    fn every_bridge_path_uses_the_same_bounded_message_id_projection() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::every_bridge_path_uses_the_same_bounded_message_id_projection") { return; }
        let pathological = format!("tsc_tsn-{}", "c".repeat(64));
        let expected = model_visible_message_id(&pathological);
        let mut messages = vec![
            test_message(AgentDeliveryMode::Soon),
            agent_management_message(),
        ];

        for message in &mut messages {
            message.id = pathological.clone();
            let params =
                inter_agent_params("thread-1", "receiver-agent", "cutex.receiver", message)
                    .expect("map long Agent Bus message ID");
            assert_eq!(params.message_id, expected);
            assert!(params.message_id.chars().count() <= MODEL_VISIBLE_MESSAGE_ID_MAX_CHARS);
            assert_eq!(message.id, pathological);
        }
    }

    #[test]
    fn agent_management_start_projects_system_sender_and_typed_director_provenance() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::agent_management_start_projects_system_sender_and_typed_director_provenance") { return; }
        let message = agent_management_message();
        let params = inter_agent_params("thread-1", "receiver-agent", "cutex.receiver", &message)
            .expect("map authorized Agent Management start message");

        assert_eq!(params.thread_id, "thread-1");
        assert_eq!(params.message_id, "amsg_agent-management-message-1");
        assert_eq!(params.author, "/root/agentmanagementsystem");
        assert_eq!(params.recipient, "/root");
        assert!(params.other_recipients.is_empty());
        assert_eq!(
            params.content,
            "Message Type: MESSAGE\nTask name: receiver-agent\nSender: AgentManagementSystem\nRequested By Director: cutex.director-r11\nPayload:\ncustom start body"
        );
        assert_eq!(params.delivery_mode, AgentDeliveryMode::AfterTurn);
    }

    #[test]
    fn forged_or_inconsistent_agent_management_envelopes_are_rejected() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::forged_or_inconsistent_agent_management_envelopes_are_rejected") { return; }
        let valid = agent_management_message();
        let mut cases = Vec::new();

        let mut message = valid.clone();
        message.from = "cutex.director-r11".to_string();
        cases.push(("sender", message));

        let mut message = valid.clone();
        message.control_type = Some("cutex.agent_management.start.v0".to_string());
        cases.push(("control type", message));

        let mut message = valid.clone();
        message.control_payload = None;
        cases.push(("missing metadata", message));

        let mut message = valid.clone();
        message.control_payload.as_mut().unwrap()["extra"] = serde_json::json!(true);
        cases.push(("unknown metadata", message));

        let mut message = valid.clone();
        message.control_payload.as_mut().unwrap()["requested_by_director"] = serde_json::json!("");
        cases.push(("invalid Director provenance", message));

        let mut message = valid.clone();
        message.external_message_id = None;
        cases.push(("external message ID", message));

        let mut message = valid.clone();
        message.sender_kind = AgentMessageKind::Owner;
        cases.push(("sender kind", message));

        let mut message = valid;
        message.kind = AgentBusEnvelopeKind::Control;
        cases.push(("envelope kind", message));

        for (label, message) in cases {
            assert!(
                inter_agent_params("thread-1", "receiver-agent", "cutex.receiver", &message)
                    .is_err(),
                "accepted invalid Agent Management {label}"
            );
        }
    }

    #[test]
    fn every_delivery_mode_keeps_its_mode_and_renders_message() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::every_delivery_mode_keeps_its_mode_and_renders_message") { return; }
        for delivery_mode in [
            AgentDeliveryMode::AfterTurn,
            AgentDeliveryMode::Soon,
            AgentDeliveryMode::Passive,
            AgentDeliveryMode::Interrupt,
        ] {
            let message = test_message(delivery_mode.clone());
            let params =
                inter_agent_params("thread-1", "receiver-agent", "cutex.receiver", &message)
                    .expect("map message");

            assert_eq!(params.delivery_mode, delivery_mode);
            assert_eq!(
                params.content,
                "Message Type: MESSAGE\nTask name: receiver-agent\nSender: Sender Agent\nPayload:\nhello"
            );
        }
    }

    #[test]
    fn canonical_recipient_label_uses_frozen_fallback_order() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::canonical_recipient_label_uses_frozen_fallback_order") { return; }
        let mut registration = test_options().registration;
        registration.id = "runtime-id".to_string();
        registration.name = "registration-name".to_string();
        registration.base_name = Some("stable-base-name".to_string());
        let label = |registration: &AgentBusRegisterRequest| {
            canonical_recipient_label(
                registration.base_name.as_deref(),
                &registration.name,
                &registration.id,
            )
            .to_string()
        };
        assert_eq!(label(&registration), "stable-base-name");

        registration.base_name = Some(" \t".to_string());
        assert_eq!(label(&registration), "registration-name");

        registration.base_name = None;
        assert_eq!(label(&registration), "registration-name");

        registration.name = " \n".to_string();
        assert_eq!(label(&registration), "runtime-id");
    }

    fn legacy_task_service_message(
        id: &str,
        sender_kind: AgentMessageKind,
        control_type: &str,
    ) -> AgentBusMessage {
        let mut message = test_message(AgentDeliveryMode::AfterTurn);
        message.id = id.to_string();
        message.from = "cutex-task-service".to_string();
        message.content = "Message Type: TASK_SERVICE_ASSIGNMENT".to_string();
        message.sender_kind = sender_kind;
        message.control_type = Some(control_type.to_string());
        message.control_payload = Some(serde_json::json!({"legacy": true}));
        message
    }

    /// r219: a Task Service message an older daemon queued is ACKed without a
    /// native submission (and the legacy bridge raises no owner notice); an
    /// ordinary message in the same page still delivers.
    #[test]
    fn legacy_task_service_messages_are_acked_without_submission() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::legacy_task_service_messages_are_acked_without_submission") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let submitter = FakeSubmitter {
            events: events.clone(),
            result: Mutex::new(Some(Ok("submission-1".to_string()))),
        };
        let status = test_status();
        let mut pending_acks = BTreeSet::new();
        let system = legacy_task_service_message(
            "legacy-system",
            AgentMessageKind::TaskServiceSystem,
            "cutex.task_service.assignment.v2",
        );
        let control = legacy_task_service_message(
            "legacy-control",
            AgentMessageKind::Agent,
            "cutex.task_service.watchdog.v1",
        );
        assert!(system.is_removed_task_service_message());
        assert!(control.is_removed_task_service_message());
        assert!(!test_message(AgentDeliveryMode::Soon).is_removed_task_service_message());
        assert!(
            inter_agent_params("thread-1", "agent", "cutex.thread-1", &system).is_err(),
            "no projection exists for a Task Service message any more"
        );

        let outcome = deliver_polled_messages(
            &bus,
            &submitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![system, control, test_message(AgentDeliveryMode::Soon)],
            &mut pending_acks,
            &status,
        )
        .expect("legacy messages do not block the page");

        let events = events.lock().expect("events lock").clone();
        assert_eq!(
            events,
            [
                "ack:legacy-system",
                "ack:legacy-control",
                "submit:amsg_message-1",
                "ack:message-1"
            ]
        );
        assert!(outcome.made_progress);
        assert!(!outcome.retained_pending);
        assert!(pending_acks.is_empty());
        let status = status.lock().expect("status lock");
        assert_eq!(status.submitted_count, 1);
        assert_eq!(status.acknowledged_count, 3);
    }

    /// r219: the bridges finish a polled legacy Task Service record terminally
    /// (`task_service_removed`, not retryable) unless it is missing or already
    /// finished; a delivered record stays delivered.
    #[test]
    fn removed_task_service_record_is_finished_only_while_unfinished() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::removed_task_service_record_is_finished_only_while_unfinished") { return; }
        use crate::management::v2::agent_bus_state::{
            AgentBusMessageRepository, AgentBusQueuedMessage,
        };

        let root = std::env::temp_dir().join(format!(
            "cutex-r219-bridge-task-record-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let owner = "cutex.00000000-0000-0000-0000-0000000000c1";
        let queue = |id: &str| {
            let mut envelope = legacy_task_service_message(
                id,
                AgentMessageKind::TaskServiceSystem,
                "cutex.task_service.completion.v1",
            );
            envelope.to_cutex_session_id = Some(owner.to_string());
            assert!(repository
                .record_queued(AgentBusQueuedMessage {
                    owner_cutex_session_id: owner.into(),
                    message_id: id.into(),
                    from_cutex_session_id: None,
                    to_cutex_session_id: owner.into(),
                    from_runtime_agent_id: None,
                    to_runtime_agent_id: Some("runtime-1".into()),
                    delivery_mode: "after_turn".into(),
                    content: envelope.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: envelope,
                    semantic_sha256: "a".repeat(64),
                })
                .unwrap());
        };

        assert!(!finish_removed_task_service_record(&repository, owner, "missing").unwrap());

        queue("legacy-pending");
        assert!(finish_removed_task_service_record(&repository, owner, "legacy-pending").unwrap());
        let snapshot = repository
            .snapshot_by_message_id("legacy-pending")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.state, "failed");
        let error = snapshot.error.unwrap();
        assert_eq!(error["code"], "task_service_removed");
        assert_eq!(error["retryable"], false);
        // A repeated poll of the same id leaves the terminal record alone.
        assert!(!finish_removed_task_service_record(&repository, owner, "legacy-pending").unwrap());

        queue("legacy-delivered");
        let receipt = InterAgentContextPersistedReceipt {
            schema: Some(INTER_AGENT_STATUS_SCHEMA.to_string()),
            receipt_id: "a4r_legacy".to_string(),
            thread_id: Some("thread-1".to_string()),
            message_id: Some("amsg_legacy-delivered".to_string()),
            semantic_sha256: Some("a".repeat(64)),
            response_item_id: "amsg_legacy-delivered".to_string(),
            turn_id: "turn-1".to_string(),
            rollout_ordinal: 1,
        };
        assert!(repository
            .record_delivered(owner, "legacy-delivered", &receipt, Utc::now())
            .unwrap());
        assert!(!finish_removed_task_service_record(&repository, owner, "legacy-delivered").unwrap());
        assert_eq!(
            repository
                .snapshot_by_message_id("legacy-delivered")
                .unwrap()
                .unwrap()
                .state,
            "delivered"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn admission_hold_keeps_legacy_wake_pending_without_submit_or_ack() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::admission_hold_keeps_legacy_wake_pending_without_submit_or_ack") { return; }
        struct HeldSubmitter(FakeSubmitter);
        impl InterAgentMessageSubmitter for HeldSubmitter {
            fn thread_busy(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
            fn delivery_may_start(&self, session: &str, id: &str) -> anyhow::Result<bool> {
                assert_eq!((session, id), ("cutex.thread-1", "message-1"));
                assert!(crate::management::v2::user_input::session_input_lock(session)?.try_lock().is_err());
                Ok(false)
            }
            fn submit_inter_agent_message(&self, _: &ThreadInterAgentMessageParams) -> anyhow::Result<String> {
                panic!("held delivery must stay outside the native inbox")
            }
            fn inter_agent_message_status(&self, params: &ThreadInterAgentMessageStatusParams)
                -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
                self.0.inter_agent_message_status(params)
            }
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus { inner: FakeBus::default(), events: events.clone() };
        let submitter = HeldSubmitter(FakeSubmitter { events: events.clone(), result: Mutex::new(None) });
        let mut pending_acks = BTreeSet::new();
        let outcome = deliver_polled_messages(&bus, &submitter, "runtime-1", "agent", "cutex.thread-1", "thread-1",
            vec![test_message(AgentDeliveryMode::AfterTurn)], &mut pending_acks, &test_status()).unwrap();
        assert!(outcome.retained_pending);
        assert!(!outcome.made_progress);
        assert!(events.lock().unwrap().is_empty());
        assert!(pending_acks.is_empty());
    }

    #[test]
    fn failed_submission_is_not_acknowledged() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::failed_submission_is_not_acknowledged") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let submitter = FakeSubmitter {
            events: events.clone(),
            result: Mutex::new(Some(Err(anyhow::anyhow!("submission failed")))),
        };
        let status = test_status();
        let mut pending_acks = BTreeSet::new();

        deliver_polled_messages(
            &bus,
            &submitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![test_message(AgentDeliveryMode::Soon)],
            &mut pending_acks,
            &status,
        )
        .expect("retryable A2 failure remains pending without blocking the sweep");
        assert_eq!(
            *events.lock().expect("events lock"),
            vec!["submit:amsg_message-1"]
        );
        assert!(pending_acks.is_empty());
    }

    #[test]
    fn failed_ack_retries_without_resubmitting() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::failed_ack_retries_without_resubmitting") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let inner = FakeBus::default();
        inner
            .ack_results
            .lock()
            .expect("ack results lock")
            .extend([Err(anyhow::anyhow!("ack failed")), Ok(1)]);
        let bus = RecordingBus {
            inner,
            events: events.clone(),
        };
        let submitter = FakeSubmitter {
            events: events.clone(),
            result: Mutex::new(Some(Ok("submission-1".to_string()))),
        };
        let status = test_status();
        let mut pending_acks = BTreeSet::new();

        assert!(deliver_polled_messages(
            &bus,
            &submitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![test_message(AgentDeliveryMode::Passive)],
            &mut pending_acks,
            &status,
        )
        .is_err());
        assert!(pending_acks.contains("message-1"));
        retry_pending_acks(&bus, "runtime-1", &mut pending_acks, &status)
            .expect("retry acknowledgement");

        assert_eq!(
            *events.lock().expect("events lock"),
            vec!["submit:amsg_message-1", "ack:message-1", "ack:message-1"]
        );
        assert!(pending_acks.is_empty());
    }

    #[test]
    fn pending_after_turn_does_not_block_later_soon_a4_and_selective_ack() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::pending_after_turn_does_not_block_later_soon_a4_and_selective_ack") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let submitter = SelectiveStatusSubmitter {
            events: events.clone(),
            unavailable: false,
        };
        let older = test_message(AgentDeliveryMode::AfterTurn);
        let mut later = test_message(AgentDeliveryMode::Soon);
        later.id = "message-2".to_string();
        let status = test_status();
        let mut pending_acks = BTreeSet::new();

        deliver_polled_messages(
            &bus,
            &submitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![older, later],
            &mut pending_acks,
            &status,
        )
        .expect("independent A4 item must progress");

        assert_eq!(
            *events.lock().unwrap(),
            vec!["submit:amsg_message-2", "ack:message-2"]
        );
        assert!(pending_acks.is_empty());
    }

    #[test]
    fn malformed_status_is_isolated_and_does_not_block_unrelated_a4() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::malformed_status_is_isolated_and_does_not_block_unrelated_a4") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let mut malformed = test_message(AgentDeliveryMode::AfterTurn);
        malformed.id = "message-1".to_string();
        let mut valid = test_message(AgentDeliveryMode::Soon);
        valid.id = "message-2".to_string();
        let status = test_status();
        let mut pending_acks = BTreeSet::new();

        deliver_polled_messages(
            &bus,
            &MalformedFirstStatusSubmitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![malformed, valid],
            &mut pending_acks,
            &status,
        )
        .expect("malformed status must be isolated from the valid A4 item");

        // Permanent malformed status has no durable record in this fixture;
        // its audit fact permits ACK, while the unrelated A4 still progresses.
        assert_eq!(*events.lock().unwrap(), vec!["ack:message-1", "ack:message-2"]);
        assert!(pending_acks.is_empty());
        assert!(status
            .lock()
            .unwrap()
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("message-1")));
    }

    #[test]
    fn conflict_is_durably_quarantined_while_unrelated_a4_progresses() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::conflict_is_durably_quarantined_while_unrelated_a4_progresses") { return; }
        let prefix = uuid::Uuid::new_v4();
        let conflict_id = format!("{prefix}-message-1");
        let valid_id = format!("{prefix}-message-2");
        let mut conflict = test_message(AgentDeliveryMode::AfterTurn);
        conflict.id = conflict_id.clone();
        conflict.from_cutex_session_id = Some("cutex.source".to_string());
        conflict.to_cutex_session_id = Some("cutex.thread-1".to_string());
        let mut valid = test_message(AgentDeliveryMode::Soon);
        valid.id = valid_id.clone();
        valid.from_cutex_session_id = Some("cutex.source".to_string());
        valid.to_cutex_session_id = Some("cutex.thread-1".to_string());
        let params = inter_agent_params("thread-1", "agent", "cutex.thread-1", &conflict).unwrap();
        agent_bus_message_repository()
            .unwrap()
            .record_queued(
                crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
                    owner_cutex_session_id: "cutex.thread-1".to_string(),
                    message_id: conflict_id.clone(),
                    from_cutex_session_id: Some("cutex.source".to_string()),
                    to_cutex_session_id: "cutex.thread-1".to_string(),
                    from_runtime_agent_id: Some("runtime-source".to_string()),
                    to_runtime_agent_id: Some("runtime-1".to_string()),
                    delivery_mode: "after_turn".to_string(),
                    content: conflict.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: conflict.clone(),
                    semantic_sha256: inter_agent_semantic_sha256(&params),
                },
            )
            .unwrap();
        let valid_params =
            inter_agent_params("thread-1", "agent", "cutex.thread-1", &valid).unwrap();
        agent_bus_message_repository()
            .unwrap()
            .record_queued(
                crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
                    owner_cutex_session_id: "cutex.thread-1".to_string(),
                    message_id: valid_id.clone(),
                    from_cutex_session_id: Some("cutex.source".to_string()),
                    to_cutex_session_id: "cutex.thread-1".to_string(),
                    from_runtime_agent_id: Some("runtime-source".to_string()),
                    to_runtime_agent_id: Some("runtime-1".to_string()),
                    delivery_mode: "soon".to_string(),
                    content: valid.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: valid.clone(),
                    semantic_sha256: inter_agent_semantic_sha256(&valid_params),
                },
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let status = test_status();
        let mut pending_acks = BTreeSet::new();

        deliver_polled_messages(
            &bus,
            &ConflictFirstStatusSubmitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![conflict, valid],
            &mut pending_acks,
            &status,
        )
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            vec![format!("ack:{conflict_id}"), format!("ack:{valid_id}")]
        );
        let snapshot = agent_bus_message_repository()
            .unwrap()
            .snapshot("cutex.thread-1")
            .unwrap();
        let quarantined = snapshot
            .iter()
            .find(|record| record["messageId"] == conflict_id)
            .expect("conflicting durable record remains observable");
        assert_eq!(quarantined["state"], "quarantined");
        assert_eq!(quarantined["error"]["code"], "native_semantic_conflict");
        let delivered = snapshot
            .iter()
            .find(|record| record["messageId"] == valid_id)
            .expect("valid durable record remains observable");
        assert_eq!(delivered["state"], "delivered");
        assert_eq!(
            delivered["a4Receipt"]["receiptId"],
            format!("a4r_amsg_{valid_id}")
        );
    }

    #[test]
    fn stable_recipient_digest_delivers_once_with_a4_and_one_ack() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::stable_recipient_digest_delivers_once_with_a4_and_one_ack") { return; }
        let message_id = format!("recipient-consistency-{}", uuid::Uuid::new_v4());
        let mut message = test_message(AgentDeliveryMode::AfterTurn);
        message.id = message_id.clone();
        message.from_cutex_session_id = Some("cutex.source".to_string());
        message.to_cutex_session_id = Some("cutex.thread-1".to_string());
        let registration = AgentBusRegisterRequest {
            id: "runtime-1".to_string(),
            name: "Agent Display.9f2a".to_string(),
            base_name: Some("agent-stable".to_string()),
            ..test_options().registration
        };
        let recipient_label = canonical_recipient_label(
            registration.base_name.as_deref(),
            &registration.name,
            &registration.id,
        );
        assert_eq!(recipient_label, "agent-stable");
        assert_eq!(registration.name, "Agent Display.9f2a");
        let mut params =
            inter_agent_params("thread-1", recipient_label, "cutex.thread-1", &message).unwrap();
        // Simulate a pre-short-ID queued record. Hash the literal baseline
        // presentation; delivery below must accept it without quarantine.
        let baseline = "Message Type: MESSAGE\nTask name: agent-stable\nSender: Sender Agent\nPayload:\nhello";
        assert_eq!(params.content, baseline);
        params.content = baseline.to_string();
        agent_bus_message_repository()
            .unwrap()
            .record_queued(
                crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
                    owner_cutex_session_id: "cutex.thread-1".to_string(),
                    message_id: message_id.clone(),
                    from_cutex_session_id: Some("cutex.source".to_string()),
                    to_cutex_session_id: "cutex.thread-1".to_string(),
                    from_runtime_agent_id: Some("runtime-source".to_string()),
                    to_runtime_agent_id: Some(registration.id.clone()),
                    delivery_mode: "after_turn".to_string(),
                    content: message.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: message.clone(),
                    semantic_sha256: inter_agent_semantic_sha256(&params),
                },
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let submitter = ImmediateA4Submitter::default();
        let status = test_status();
        let mut pending_acks = BTreeSet::new();

        let outcome = deliver_polled_messages(
            &bus,
            &submitter,
            &registration.id,
            recipient_label,
            "cutex.thread-1",
            "thread-1",
            vec![message],
            &mut pending_acks,
            &status,
        )
        .unwrap();

        assert!(outcome.made_progress);
        assert!(!outcome.retained_pending);
        assert_eq!(*events.lock().unwrap(), vec![format!("ack:{message_id}")]);
        assert_eq!(submitter.submitted.lock().unwrap().len(), 1);
        assert_eq!(status.lock().unwrap().submitted_count, 1);
        assert_eq!(status.lock().unwrap().acknowledged_count, 1);
        let snapshot = agent_bus_message_repository()
            .unwrap()
            .snapshot("cutex.thread-1")
            .unwrap();
        let delivered = snapshot
            .iter()
            .find(|record| record["messageId"] == message_id)
            .unwrap();
        assert_eq!(delivered["state"], "delivered");
        assert_eq!(
            delivered["a4Receipt"]["receiptId"],
            format!("a4r_amsg_{message_id}")
        );
        assert!(delivered.get("error").is_none());
    }

    #[test]
    fn altered_semantic_envelope_is_quarantined_and_never_submitted() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::altered_semantic_envelope_is_quarantined_and_never_submitted") { return; }
        let message_id = format!("recipient-tamper-{}", uuid::Uuid::new_v4());
        let mut canonical = test_message(AgentDeliveryMode::Soon);
        canonical.id = message_id.clone();
        canonical.from_cutex_session_id = Some("cutex.source".to_string());
        canonical.to_cutex_session_id = Some("cutex.thread-1".to_string());
        let params =
            inter_agent_params("thread-1", "agent-stable", "cutex.thread-1", &canonical).unwrap();
        agent_bus_message_repository()
            .unwrap()
            .record_queued(
                crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
                    owner_cutex_session_id: "cutex.thread-1".to_string(),
                    message_id: message_id.clone(),
                    from_cutex_session_id: Some("cutex.source".to_string()),
                    to_cutex_session_id: "cutex.thread-1".to_string(),
                    from_runtime_agent_id: Some("runtime-source".to_string()),
                    to_runtime_agent_id: Some("runtime-1".to_string()),
                    delivery_mode: "soon".to_string(),
                    content: canonical.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: canonical.clone(),
                    semantic_sha256: inter_agent_semantic_sha256(&params),
                },
            )
            .unwrap();
        let mut altered = canonical;
        altered.content = "tampered after durable commit".to_string();
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let submitter = ImmediateA4Submitter::default();
        let mut pending_acks = BTreeSet::new();

        deliver_polled_messages(
            &bus,
            &submitter,
            "runtime-1",
            "agent-stable",
            "cutex.thread-1",
            "thread-1",
            vec![altered],
            &mut pending_acks,
            &test_status(),
        )
        .unwrap();

        assert!(submitter.submitted.lock().unwrap().is_empty());
        assert_eq!(*events.lock().unwrap(), vec![format!("ack:{message_id}")]);
        let snapshot = agent_bus_message_repository()
            .unwrap()
            .snapshot("cutex.thread-1")
            .unwrap();
        let quarantined = snapshot
            .iter()
            .find(|record| record["messageId"] == message_id)
            .unwrap();
        assert_eq!(quarantined["state"], "quarantined");
        assert_eq!(
            quarantined["error"]["code"],
            "canonical_semantic_digest_mismatch"
        );
    }

    #[test]
    fn ordinary_a4_fact_before_ack_replays_without_resubmission() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::ordinary_a4_fact_before_ack_replays_without_resubmission") { return; }
        let message_id = format!("{}-message-2", uuid::Uuid::new_v4());
        let mut message = test_message(AgentDeliveryMode::Soon);
        message.id = message_id.clone();
        message.from_cutex_session_id = Some("cutex.source".to_string());
        message.to_cutex_session_id = Some("cutex.thread-1".to_string());
        let params = inter_agent_params("thread-1", "agent", "cutex.thread-1", &message).unwrap();
        agent_bus_message_repository()
            .unwrap()
            .record_queued(
                crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
                    owner_cutex_session_id: "cutex.thread-1".to_string(),
                    message_id: message_id.clone(),
                    from_cutex_session_id: Some("cutex.source".to_string()),
                    to_cutex_session_id: "cutex.thread-1".to_string(),
                    from_runtime_agent_id: Some("runtime-source".to_string()),
                    to_runtime_agent_id: Some("runtime-1".to_string()),
                    delivery_mode: "soon".to_string(),
                    content: message.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: message.clone(),
                    semantic_sha256: inter_agent_semantic_sha256(&params),
                },
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let inner = FakeBus::default();
        inner
            .ack_results
            .lock()
            .unwrap()
            .extend([Err(anyhow::anyhow!("crash before ACK commit")), Ok(1)]);
        let bus = RecordingBus {
            inner,
            events: events.clone(),
        };
        let status = test_status();
        let mut pre_crash_acks = BTreeSet::new();

        assert!(deliver_polled_messages(
            &bus,
            &ConflictFirstStatusSubmitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![message.clone()],
            &mut pre_crash_acks,
            &status,
        )
        .is_err());
        assert!(pre_crash_acks.contains(&message_id));

        let mut reopened_acks = BTreeSet::new();
        deliver_polled_messages(
            &bus,
            &ConflictFirstStatusSubmitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![message],
            &mut reopened_acks,
            &status,
        )
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            vec![format!("ack:{message_id}"), format!("ack:{message_id}")]
        );
        assert!(reopened_acks.is_empty());
        let snapshot = agent_bus_message_repository()
            .unwrap()
            .snapshot("cutex.thread-1")
            .unwrap();
        let delivered = snapshot
            .iter()
            .find(|record| record["messageId"] == message_id)
            .unwrap();
        assert_eq!(delivered["state"], "delivered");
        assert_eq!(
            delivered["a4Receipt"]["receiptId"],
            format!("a4r_amsg_{message_id}")
        );
    }

    #[test]
    fn missing_a4_status_method_never_falls_back_to_a2_or_ack() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::missing_a4_status_method_never_falls_back_to_a2_or_ack") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let submitter = SelectiveStatusSubmitter {
            events: events.clone(),
            unavailable: true,
        };
        let status = test_status();
        let mut pending_acks = BTreeSet::new();

        assert!(deliver_polled_messages(
            &bus,
            &submitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![test_message(AgentDeliveryMode::Soon)],
            &mut pending_acks,
            &status,
        )
        .is_err());
        assert!(events.lock().unwrap().is_empty());
        assert!(pending_acks.is_empty());
    }

    #[test]
    fn pending_ack_dedupe_is_keyed_by_original_bus_id() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::pending_ack_dedupe_is_keyed_by_original_bus_id") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = RecordingBus {
            inner: FakeBus::default(),
            events: events.clone(),
        };
        let submitter = FakeSubmitter {
            events: events.clone(),
            result: Mutex::new(Some(Ok("submission-1".to_string()))),
        };
        let status = test_status();
        let mut pending_acks = BTreeSet::from(["message-1".to_string()]);

        deliver_polled_messages(
            &bus,
            &submitter,
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![test_message(AgentDeliveryMode::AfterTurn)],
            &mut pending_acks,
            &status,
        )
        .expect("skip pending message");

        assert!(events.lock().expect("events lock").is_empty());
        assert_eq!(pending_acks, BTreeSet::from(["message-1".to_string()]));
        assert_eq!(status.lock().expect("status lock").submitted_count, 0);
    }

    #[test]
    fn pending_poll_backoff_is_bounded_deterministic_and_progress_resets_it() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::pending_poll_backoff_is_bounded_deterministic_and_progress_resets_it") { return; }
        let pending = DeliverySweepOutcome {
            had_messages: true,
            retained_pending: true,
            made_progress: false,
        };
        let mut backoff = PendingPollBackoff::default();
        assert_eq!(
            backoff.wait_after(
                pending,
                Duration::from_millis(20),
                Duration::from_millis(70)
            ),
            Some(Duration::from_millis(20))
        );
        assert_eq!(
            backoff.wait_after(
                pending,
                Duration::from_millis(20),
                Duration::from_millis(70)
            ),
            Some(Duration::from_millis(40))
        );
        assert_eq!(
            backoff.wait_after(
                pending,
                Duration::from_millis(20),
                Duration::from_millis(70)
            ),
            Some(Duration::from_millis(70))
        );
        assert_eq!(
            backoff.wait_after(
                DeliverySweepOutcome {
                    made_progress: true,
                    ..pending
                },
                Duration::from_millis(20),
                Duration::from_millis(70),
            ),
            Some(Duration::from_millis(20))
        );
        assert_eq!(
            backoff.wait_after(
                DeliverySweepOutcome {
                    had_messages: true,
                    retained_pending: false,
                    made_progress: true,
                },
                Duration::from_millis(20),
                Duration::from_millis(70),
            ),
            None
        );
    }

    #[test]
    fn pending_native_delivery_backs_off_then_reaches_a4_once() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::pending_native_delivery_backs_off_then_reaches_a4_once") { return; }
        let mut message = test_message(AgentDeliveryMode::AfterTurn);
        message.id = format!("pending-backoff-{}", uuid::Uuid::new_v4());
        message.from_cutex_session_id = Some("cutex.source".to_string());
        message.to_cutex_session_id = Some("cutex.thread-1".to_string());
        let params = inter_agent_params("thread-1", "agent", "cutex.thread-1", &message).unwrap();
        agent_bus_message_repository()
            .unwrap()
            .record_queued(
                crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
                    owner_cutex_session_id: "cutex.thread-1".to_string(),
                    message_id: message.id.clone(),
                    from_cutex_session_id: Some("cutex.source".to_string()),
                    to_cutex_session_id: "cutex.thread-1".to_string(),
                    from_runtime_agent_id: Some("runtime-source".to_string()),
                    to_runtime_agent_id: Some("runtime-1".to_string()),
                    delivery_mode: "after_turn".to_string(),
                    content: message.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: message.clone(),
                    semantic_sha256: inter_agent_semantic_sha256(&params),
                },
            )
            .unwrap();
        let message_id = message.id.clone();
        let bus = Arc::new(RepeatingWorkerBus {
            message,
            poll_count: AtomicUsize::new(0),
            ack_count: AtomicUsize::new(0),
            acked: AtomicBool::new(false),
        });
        let submitter = Arc::new(ControlledPendingSubmitter {
            context_persisted: AtomicBool::new(false),
            submit_count: AtomicUsize::new(0),
        });
        let mut options = test_options();
        options.poll_interval = Duration::from_millis(120);
        options.retry_interval = Duration::from_millis(480);
        options.registration_refresh_interval = Duration::from_secs(60);
        let bridge = AppServerAgentBusBridge::spawn(bus.clone(), submitter.clone(), options)
            .expect("start pending bridge");

        let deadline = Instant::now() + Duration::from_secs(2);
        while submitter.submit_count.load(Ordering::Acquire) != 1 {
            assert!(
                Instant::now() < deadline,
                "pending message was not submitted"
            );
            thread::sleep(Duration::from_millis(2));
        }
        let polls_after_pending = bus.poll_count.load(Ordering::Acquire);
        thread::sleep(Duration::from_millis(40));
        assert_eq!(
            bus.poll_count.load(Ordering::Acquire),
            polls_after_pending,
            "pending A4 status must not trigger an immediate Agent Bus repoll"
        );

        submitter.context_persisted.store(true, Ordering::Release);
        while bus.ack_count.load(Ordering::Acquire) != 1 {
            assert!(
                Instant::now() < deadline,
                "pending message did not reach A4"
            );
            thread::sleep(Duration::from_millis(5));
        }
        bridge.shutdown().expect("stop pending bridge");
        assert_eq!(submitter.submit_count.load(Ordering::Acquire), 1);
        assert_eq!(bus.ack_count.load(Ordering::Acquire), 1);
        let snapshot = agent_bus_message_repository()
            .unwrap()
            .snapshot("cutex.thread-1")
            .unwrap();
        let delivered = snapshot
            .iter()
            .find(|record| record["messageId"] == message_id)
            .expect("pending item retains its durable A4 delivery fact");
        assert_eq!(delivered["state"], "delivered");
        assert_eq!(
            delivered["a4Receipt"]["receiptId"],
            format!("a4r_amsg_{message_id}")
        );
    }

    #[test]
    fn fully_acked_nonempty_page_immediately_drains_next_page() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::fully_acked_nonempty_page_immediately_drains_next_page") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut first = test_message(AgentDeliveryMode::Soon);
        first.id = format!("acked-first-{}", uuid::Uuid::new_v4());
        let mut second = test_message(AgentDeliveryMode::Soon);
        second.id = format!("acked-second-{}", uuid::Uuid::new_v4());
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let bus = Arc::new(WorkerBus {
            events: events.clone(),
            polls: Mutex::new(VecDeque::from([vec![first], vec![second], Vec::new()])),
        });
        let submitter = Arc::new(ImmediateA4Submitter::default());
        let mut options = test_options();
        options.poll_interval = Duration::from_secs(1);
        options.retry_interval = Duration::from_secs(2);
        options.registration_refresh_interval = Duration::from_secs(60);
        let bridge =
            AppServerAgentBusBridge::spawn(bus, submitter, options).expect("start backlog bridge");
        let deadline = Instant::now() + Duration::from_secs(2);
        while bridge.status().unwrap().acknowledged_count != 2 {
            assert!(Instant::now() < deadline, "backlog did not drain");
            thread::sleep(Duration::from_millis(5));
        }
        bridge.shutdown().unwrap();

        let events = events.lock().unwrap();
        let first_ack = events
            .iter()
            .position(|event| event == &format!("ack:{first_id}"))
            .unwrap();
        let second_ack = events
            .iter()
            .position(|event| event == &format!("ack:{second_id}"))
            .unwrap();
        assert_eq!(events[first_ack + 1], "poll");
        assert_eq!(second_ack, first_ack + 2);
    }

    #[test]
    fn pending_after_turn_allows_later_soon_selective_progress_in_worker() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::pending_after_turn_allows_later_soon_selective_progress_in_worker") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut pending = test_message(AgentDeliveryMode::AfterTurn);
        pending.id = "worker-pending-message-1".to_string();
        let mut later = test_message(AgentDeliveryMode::Soon);
        later.id = "worker-later-message-2".to_string();
        let bus = Arc::new(WorkerBus {
            events: events.clone(),
            polls: Mutex::new(VecDeque::from([
                vec![pending.clone()],
                vec![pending, later.clone()],
                Vec::new(),
            ])),
        });
        let submitter = Arc::new(SelectiveStatusSubmitter {
            events: events.clone(),
            unavailable: false,
        });
        let mut options = test_options();
        options.poll_interval = Duration::from_millis(40);
        options.retry_interval = Duration::from_millis(160);
        options.registration_refresh_interval = Duration::from_secs(60);
        let bridge = AppServerAgentBusBridge::spawn(bus, submitter, options)
            .expect("start selective bridge");
        let deadline = Instant::now() + Duration::from_secs(2);
        while bridge.status().unwrap().acknowledged_count != 1 {
            assert!(
                Instant::now() < deadline,
                "later soon item did not progress"
            );
            thread::sleep(Duration::from_millis(5));
        }
        bridge.shutdown().unwrap();

        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "submit:amsg_worker-later-message-2")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "submit:amsg_worker-pending-message-1")
                .count(),
            0,
            "already-pending native delivery must not be resubmitted"
        );
        assert!(events
            .iter()
            .any(|event| event == "ack:worker-later-message-2"));
        assert!(!events
            .iter()
            .any(|event| event == "ack:worker-pending-message-1"));
    }

    #[test]
    fn bridge_shutdown_interrupts_pending_poll_backoff_promptly() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::bridge_shutdown_interrupts_pending_poll_backoff_promptly") { return; }
        let mut message = test_message(AgentDeliveryMode::AfterTurn);
        message.id = format!("pending-shutdown-{}", uuid::Uuid::new_v4());
        let bus = Arc::new(RepeatingWorkerBus {
            message,
            poll_count: AtomicUsize::new(0),
            ack_count: AtomicUsize::new(0),
            acked: AtomicBool::new(false),
        });
        let submitter = Arc::new(ControlledPendingSubmitter {
            context_persisted: AtomicBool::new(false),
            submit_count: AtomicUsize::new(0),
        });
        let mut options = test_options();
        options.poll_interval = Duration::from_secs(1);
        options.retry_interval = Duration::from_secs(2);
        options.registration_refresh_interval = Duration::from_secs(60);
        let bridge = AppServerAgentBusBridge::spawn(bus, submitter.clone(), options)
            .expect("start pending shutdown bridge");
        let deadline = Instant::now() + Duration::from_secs(2);
        while submitter.submit_count.load(Ordering::Acquire) != 1 {
            assert!(
                Instant::now() < deadline,
                "pending message was not submitted"
            );
            thread::sleep(Duration::from_millis(2));
        }

        let shutdown_started = Instant::now();
        bridge.shutdown().expect("stop bridge during pending wait");
        assert!(shutdown_started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn bridge_worker_registers_delivers_and_stops_cleanly() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::bridge_worker_registers_delivers_and_stops_cleanly") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = Arc::new(WorkerBus {
            events: events.clone(),
            polls: Mutex::new(VecDeque::from([
                vec![test_message(AgentDeliveryMode::Soon)],
                Vec::new(),
            ])),
        });
        let submitter = Arc::new(FakeSubmitter {
            events: events.clone(),
            result: Mutex::new(Some(Ok("submission-1".to_string()))),
        });
        let mut options = test_options();
        options.poll_interval = Duration::from_millis(10);
        options.retry_interval = Duration::from_millis(10);
        options.registration_refresh_interval = Duration::from_secs(1);

        let bridge = AppServerAgentBusBridge::spawn(bus, submitter, options)
            .expect("start agent-bus bridge");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if bridge.status().expect("bridge status").acknowledged_count == 1 {
                break;
            }
            assert!(Instant::now() < deadline, "bridge did not deliver message");
            thread::sleep(Duration::from_millis(10));
        }
        bridge.shutdown().expect("stop bridge");

        let events = events.lock().expect("events lock");
        assert_eq!(
            &events[..4],
            ["register", "poll", "submit:amsg_message-1", "ack:message-1"]
        );
    }

    #[test]
    fn refreshed_exact_registration_can_receive_a_message() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::refreshed_exact_registration_can_receive_a_message") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = Arc::new(WorkerBus {
            events: events.clone(),
            polls: Mutex::new(VecDeque::from([Vec::new()])),
        });
        let submitter = Arc::new(FakeSubmitter {
            events: events.clone(),
            result: Mutex::new(Some(Ok("submission-after-recovery".to_string()))),
        });
        let mut options = test_options();
        options.poll_interval = Duration::from_secs(1);
        options.registration_refresh_interval = Duration::from_secs(60);
        let bridge = AppServerAgentBusBridge::spawn(bus.clone(), submitter.clone(), options)
            .expect("start exact runtime bridge");

        let refreshed = bridge
            .refresh_registration()
            .expect("re-register exact recovered runtime");
        assert!(refreshed.registered);

        let status = test_status();
        let mut pending_acks = BTreeSet::new();
        deliver_polled_messages(
            bus.as_ref(),
            submitter.as_ref(),
            "runtime-1",
            "agent",
            "cutex.thread-1",
            "thread-1",
            vec![test_message(AgentDeliveryMode::Soon)],
            &mut pending_acks,
            &status,
        )
        .expect("deliver a message after recovery refresh");
        bridge.shutdown().expect("stop bridge");

        let events = events.lock().expect("events lock");
        let register_positions = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| (event == "register").then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(register_positions.len(), 2);
        let submit_position = events
            .iter()
            .position(|event| event == "submit:amsg_message-1")
            .expect("native submission");
        assert!(register_positions[1] < submit_position);
        assert!(events.iter().any(|event| event == "ack:message-1"));
    }

    #[test]
    fn bridge_worker_stops_when_app_server_disconnects() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::bridge_worker_stops_when_app_server_disconnects") { return; }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bus = Arc::new(WorkerBus {
            events: events.clone(),
            polls: Mutex::new(VecDeque::from([Vec::new()])),
        });
        let submitter = Arc::new(FakeSubmitter {
            events,
            result: Mutex::new(None),
        });
        let mut options = test_options();
        options.poll_interval = Duration::from_millis(10);
        let runtime_alive = Arc::new(AtomicBool::new(true));
        let bridge = AppServerAgentBusBridge::spawn_with_liveness(
            bus,
            submitter,
            options,
            runtime_alive.clone(),
        )
        .expect("start agent-bus bridge");

        runtime_alive.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !bridge.status().expect("bridge status").running {
                break;
            }
            assert!(Instant::now() < deadline, "bridge did not stop");
            thread::sleep(Duration::from_millis(10));
        }
        bridge.shutdown().expect("join bridge");
    }

    #[test]
    fn bridge_shutdown_wakes_a_blocking_poll() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::bridge_shutdown_wakes_a_blocking_poll") { return; }
        let bus = Arc::new(BlockingPollBus::default());
        let observed_bus = bus.clone();
        let submitter = Arc::new(FakeSubmitter {
            events: Arc::new(Mutex::new(Vec::new())),
            result: Mutex::new(None),
        });
        let bridge = AppServerAgentBusBridge::spawn(bus, submitter, test_options())
            .expect("start agent-bus bridge");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !observed_bus.poll_started.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "bridge did not start polling");
            thread::sleep(Duration::from_millis(5));
        }

        let shutdown_started = Instant::now();
        bridge.shutdown().expect("stop blocking bridge");

        assert!(shutdown_started.elapsed() < Duration::from_millis(500));
        assert!(*observed_bus.unregistered.lock().expect("unregistered lock"));
    }

    #[test]
    fn registration_refresh_continues_while_delivery_poll_is_blocked() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::registration_refresh_continues_while_delivery_poll_is_blocked") { return; }
        let bus = Arc::new(BlockingPollBus::default());
        let observed_bus = bus.clone();
        let submitter = Arc::new(FakeSubmitter {
            events: Arc::new(Mutex::new(Vec::new())),
            result: Mutex::new(None),
        });
        let mut options = test_options();
        options.registration_refresh_interval = Duration::from_millis(10);
        let bridge = AppServerAgentBusBridge::spawn(bus, submitter, options)
            .expect("start agent-bus bridge");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !observed_bus.poll_started.load(Ordering::Acquire)
            || observed_bus.registration_count.load(Ordering::SeqCst) < 2
        {
            assert!(
                Instant::now() < deadline,
                "registration did not refresh while poll was blocked"
            );
            thread::sleep(Duration::from_millis(5));
        }
        bridge.shutdown().expect("stop bridge");
    }

    struct RecordingBus {
        inner: FakeBus,
        events: Arc<Mutex<Vec<String>>>,
    }

    struct WorkerBus {
        events: Arc<Mutex<Vec<String>>>,
        polls: Mutex<VecDeque<Vec<AgentBusMessage>>>,
    }

    struct RepeatingWorkerBus {
        message: AgentBusMessage,
        poll_count: AtomicUsize,
        ack_count: AtomicUsize,
        acked: AtomicBool,
    }

    struct ControlledPendingSubmitter {
        context_persisted: AtomicBool,
        submit_count: AtomicUsize,
    }

    #[derive(Default)]
    struct ImmediateA4Submitter {
        submitted: Mutex<BTreeSet<String>>,
    }

    impl InterAgentMessageSubmitter for ControlledPendingSubmitter {
        fn delivery_may_start(&self, _session: &str, _id: &str) -> anyhow::Result<bool> { Ok(true) }

        fn thread_busy(&self, _thread_id: &str) -> anyhow::Result<bool> { Ok(false) }

        fn submit_inter_agent_message(
            &self,
            params: &ThreadInterAgentMessageParams,
        ) -> anyhow::Result<String> {
            self.submit_count.fetch_add(1, Ordering::AcqRel);
            Ok(format!("a2_{}", params.message_id))
        }

        fn inter_agent_message_status(
            &self,
            params: &ThreadInterAgentMessageStatusParams,
        ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
            let submitted = self.submit_count.load(Ordering::Acquire) > 0;
            let persisted = self.context_persisted.load(Ordering::Acquire);
            Ok(status_response(params, |query| {
                if persisted {
                    context_persisted_status(params, query, "turn-pending")
                } else {
                    pending_status(
                        query,
                        if submitted {
                            ThreadInterAgentMessageDeliveryState::Pending
                        } else {
                            ThreadInterAgentMessageDeliveryState::Unknown
                        },
                    )
                }
            }))
        }
    }

    impl InterAgentMessageSubmitter for ImmediateA4Submitter {
        fn delivery_may_start(&self, _session: &str, _id: &str) -> anyhow::Result<bool> { Ok(true) }

        fn thread_busy(&self, _thread_id: &str) -> anyhow::Result<bool> { Ok(false) }

        fn submit_inter_agent_message(
            &self,
            params: &ThreadInterAgentMessageParams,
        ) -> anyhow::Result<String> {
            self.submitted
                .lock()
                .unwrap()
                .insert(params.message_id.clone());
            Ok(format!("a2_{}", params.message_id))
        }

        fn inter_agent_message_status(
            &self,
            params: &ThreadInterAgentMessageStatusParams,
        ) -> anyhow::Result<ThreadInterAgentMessageStatusResponse> {
            let submitted = self.submitted.lock().unwrap().clone();
            Ok(status_response(params, |query| {
                if submitted.contains(&query.message_id) {
                    context_persisted_status(params, query, "turn-immediate")
                } else {
                    pending_status(query, ThreadInterAgentMessageDeliveryState::Unknown)
                }
            }))
        }
    }

    fn status_response(
        params: &ThreadInterAgentMessageStatusParams,
        mut status_for: impl FnMut(
            &ThreadInterAgentMessageStatusQuery,
        ) -> super::super::commands::ThreadInterAgentMessageStatus,
    ) -> ThreadInterAgentMessageStatusResponse {
        ThreadInterAgentMessageStatusResponse {
            schema: INTER_AGENT_STATUS_SCHEMA.to_string(),
            thread_id: params.thread_id.clone(),
            statuses: params.messages.iter().map(&mut status_for).collect(),
        }
    }

    fn pending_status(
        query: &ThreadInterAgentMessageStatusQuery,
        state: ThreadInterAgentMessageDeliveryState,
    ) -> super::super::commands::ThreadInterAgentMessageStatus {
        super::super::commands::ThreadInterAgentMessageStatus {
            message_id: query.message_id.clone(),
            state,
            semantic_sha256: query.semantic_sha256.clone(),
            receipt: None,
        }
    }

    fn context_persisted_status(
        params: &ThreadInterAgentMessageStatusParams,
        query: &ThreadInterAgentMessageStatusQuery,
        turn_id: &str,
    ) -> super::super::commands::ThreadInterAgentMessageStatus {
        super::super::commands::ThreadInterAgentMessageStatus {
            message_id: query.message_id.clone(),
            state: ThreadInterAgentMessageDeliveryState::ContextPersisted,
            semantic_sha256: query.semantic_sha256.clone(),
            receipt: Some(InterAgentContextPersistedReceipt {
                schema: Some(INTER_AGENT_STATUS_SCHEMA.to_string()),
                receipt_id: format!("a4r_{}", query.message_id),
                thread_id: Some(params.thread_id.clone()),
                message_id: Some(query.message_id.clone()),
                semantic_sha256: Some(query.semantic_sha256.clone()),
                response_item_id: query.message_id.clone(),
                turn_id: turn_id.to_string(),
                rollout_ordinal: 1,
            }),
        }
    }

    #[derive(Default)]
    struct BlockingPollBus {
        poll_started: AtomicBool,
        registration_count: AtomicUsize,
        unregistered: Mutex<bool>,
        wake: Condvar,
    }

    impl RuntimeAgentBus for BlockingPollBus {
        fn register(&self, _request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
            self.registration_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn unregister(&self, _agent_id: &str) -> anyhow::Result<bool> {
            *self.unregistered.lock().expect("unregistered lock") = true;
            self.wake.notify_all();
            Ok(true)
        }

        fn poll(&self, _agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            self.poll_started.store(true, Ordering::Release);
            let unregistered = self.unregistered.lock().expect("unregistered lock");
            let _ = self
                .wake
                .wait_timeout_while(unregistered, Duration::from_secs(5), |value| !*value)
                .map_err(|_| anyhow::anyhow!("unregistered lock was poisoned"))?;
            Ok(Vec::new())
        }

        fn ack(&self, _agent_id: &str, _message_ids: &[String]) -> anyhow::Result<usize> {
            Ok(0)
        }
    }

    impl RuntimeAgentBus for WorkerBus {
        fn register(&self, _request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
            self.events
                .lock()
                .expect("events lock")
                .push("register".to_string());
            Ok(())
        }

        fn unregister(&self, _agent_id: &str) -> anyhow::Result<bool> {
            self.events
                .lock()
                .expect("events lock")
                .push("unregister".to_string());
            Ok(true)
        }

        fn poll(&self, _agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            self.events
                .lock()
                .expect("events lock")
                .push("poll".to_string());
            Ok(self
                .polls
                .lock()
                .expect("polls lock")
                .pop_front()
                .unwrap_or_default())
        }

        fn ack(&self, _agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
            self.events
                .lock()
                .expect("events lock")
                .push(format!("ack:{}", message_ids.join(",")));
            Ok(message_ids.len())
        }
    }

    impl RuntimeAgentBus for RepeatingWorkerBus {
        fn register(&self, _request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
            Ok(())
        }

        fn unregister(&self, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn poll(&self, _agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            self.poll_count.fetch_add(1, Ordering::AcqRel);
            if self.acked.load(Ordering::Acquire) {
                Ok(Vec::new())
            } else {
                Ok(vec![self.message.clone()])
            }
        }

        fn ack(&self, _agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
            self.ack_count.fetch_add(1, Ordering::AcqRel);
            self.acked.store(true, Ordering::Release);
            Ok(message_ids.len())
        }
    }

    impl RuntimeAgentBus for RecordingBus {
        fn register(&self, request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
            self.inner.register(request)
        }

        fn unregister(&self, agent_id: &str) -> anyhow::Result<bool> {
            self.inner.unregister(agent_id)
        }

        fn poll(&self, agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            self.inner.poll(agent_id)
        }

        fn ack(&self, agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
            self.events
                .lock()
                .expect("events lock")
                .push(format!("ack:{}", message_ids.join(",")));
            self.inner
                .ack_results
                .lock()
                .expect("ack results lock")
                .pop_front()
                .unwrap_or_else(|| Ok(message_ids.len()))
                .with_context(|| format!("ack failed for {agent_id}"))
        }
    }

    fn test_message(delivery_mode: AgentDeliveryMode) -> AgentBusMessage {
        AgentBusMessage {
            id: "message-1".to_string(),
            kind: AgentBusEnvelopeKind::Message,
            from: "Sender Agent".to_string(),
            to: "runtime-1".to_string(),
            from_cutex_session_id: None,
            to_cutex_session_id: None,
            content: "hello".to_string(),
            delivery_mode,
            trigger_turn: true,
            created_at_epoch_secs: 1,
            sender_kind: AgentMessageKind::Agent,
            display_source: None,
            submit_mode: None,
            control_type: None,
            control_payload: None,
            external_action_id: None,
            external_message_id: None,
        }
    }

    fn agent_management_message() -> AgentBusMessage {
        let metadata = AgentManagementMessageMetadata {
            schema: AgentManagementSchema::V1,
            requested_by_director: crate::role_revision::CutexSessionId::new("cutex.director-r11")
                .unwrap(),
            requested_by_operator: None,
        };
        AgentBusMessage {
            id: "agent-management-message-1".to_string(),
            kind: AgentBusEnvelopeKind::Message,
            from: AGENT_MANAGEMENT_SYSTEM_SENDER.to_string(),
            to: "runtime-1".to_string(),
            from_cutex_session_id: None,
            to_cutex_session_id: None,
            content: "custom start body".to_string(),
            delivery_mode: AgentDeliveryMode::AfterTurn,
            trigger_turn: true,
            created_at_epoch_secs: 1,
            sender_kind: AgentMessageKind::Agent,
            display_source: Some("Agent Management System".to_string()),
            submit_mode: None,
            control_type: Some(AGENT_MANAGEMENT_START_CONTROL_TYPE.to_string()),
            control_payload: Some(serde_json::to_value(metadata).unwrap()),
            external_action_id: None,
            external_message_id: Some("agent-management:create:start".to_string()),
        }
    }

    fn test_status() -> Arc<Mutex<AppServerAgentBusBridgeStatus>> {
        Arc::new(Mutex::new(AppServerAgentBusBridgeStatus {
            runtime_agent_id: "runtime-1".to_string(),
            thread_id: "thread-1".to_string(),
            running: true,
            registered: true,
            pending_ack_count: 0,
            submitted_count: 0,
            acknowledged_count: 0,
            last_poll_at: None,
            last_message_id: None,
            last_submission_id: None,
            last_error: None,
        }))
    }

    fn test_options() -> AppServerAgentBusBridgeOptions {
        AppServerAgentBusBridgeOptions::new(
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
        )
    }

    #[test]
    fn stock_registration_only_refreshes_without_poll_submit_or_ack() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::stock_registration_only_refreshes_without_poll_submit_or_ack") { return; }
        struct RegistrationOnlyBus {
            registered: mpsc::Sender<()>,
            polls: AtomicUsize,
            acks: AtomicUsize,
        }
        impl RuntimeAgentBus for RegistrationOnlyBus {
            fn register(&self, _: &AgentBusRegisterRequest) -> anyhow::Result<()> {
                self.registered.send(())?;
                Ok(())
            }
            fn unregister(&self, _: &str) -> anyhow::Result<bool> {
                Ok(true)
            }
            fn poll(&self, _: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
                self.polls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("stock must not poll")
            }
            fn ack(&self, _: &str, _: &[String]) -> anyhow::Result<usize> {
                self.acks.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("stock must not ack")
            }
        }
        let (tx, rx) = mpsc::channel();
        let bus = Arc::new(RegistrationOnlyBus {
            registered: tx,
            polls: AtomicUsize::new(0),
            acks: AtomicUsize::new(0),
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let submitter = Arc::new(FakeSubmitter {
            events: events.clone(),
            result: Mutex::new(None),
        });
        let mut options = test_options();
        options.registration_only = true;
        options.registration_refresh_interval = Duration::from_millis(1);
        options.poll_interval = Duration::from_millis(1);
        let bridge = AppServerAgentBusBridge::spawn_with_liveness(
            bus.clone(),
            submitter,
            options,
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap(); // initial public register
        rx.recv_timeout(Duration::from_secs(2)).unwrap(); // worker refresh reached
        bridge.shutdown().unwrap();
        assert_eq!(bus.polls.load(Ordering::SeqCst), 0);
        assert_eq!(bus.acks.load(Ordering::SeqCst), 0);
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn bridge_options_require_native_thread_identity_and_app_server_pid() {
        if test_runs_in_private_home("app_server::bus_bridge::tests::bridge_options_require_native_thread_identity_and_app_server_pid") { return; }
        let mut options = test_options();
        assert!(options.validate().is_ok());

        options.registration.session_id = Some("thread-2".to_string());
        assert!(options.validate().is_err());
        options.registration.session_id = Some("thread-1".to_string());
        options.registration.pid = 0;
        assert!(options.validate().is_err());
    }
}
