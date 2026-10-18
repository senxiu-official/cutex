use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::{fs, path::Path};

use anyhow::Context;

use cutex::agent_bus::client::AgentBusHttpClient;
use cutex::agent_bus::groups::normalize_registered_agent_groups;
use cutex::agent_bus::identity::fnv1a_hex;
use cutex::agent_bus::identity::sanitize_session_component;
use cutex::agent_bus::model::AgentBusRegisterRequest;
#[cfg(not(test))]
use cutex::app_server::activity_bridge::spawn_activity_projector;
use cutex::app_server::bus_bridge::AppServerAgentBusBridgeOptions;
use cutex::app_server::client::AppServerEndpoint;
use cutex::app_server::manager::AppServerRuntimeConnectResult;
use cutex::app_server::manager::AppServerRuntimeEventContext;
use cutex::app_server::manager::AppServerRuntimeManager;
use cutex::app_server::runtime::cleanup_runtime_binding_files;
use cutex::app_server::runtime::thread_resume_params_for_session_with_model_provider;
use cutex::app_server::runtime::AppServerRuntimeLayout;
use cutex::management::v2::activity::record_activity_event;
use cutex::management::v2::model::AppServerSchema;
use cutex::management::v2::model::AppServerSchemaChannel;
use cutex::management::v2::native_events::pending_event_from_app_server;
use cutex::management::v2::native_events::NativeEventContext;
use cutex::management::v2::native_events::NativeEventDisposition;
use cutex::management::v2::repository::management_v2_repository;
use cutex::management::v2::subagent_lineage;
use cutex::management::v2::subagent_lineage::ForeignThreadRoute;
use cutex::management::v2::subagent_lineage::LineageReadError;
use cutex::management::v2::usage::record_usage_event_for_runtime;
use cutex::platform::host::current_host_name;
use cutex::platform::process::{process_is_running, process_started_at};
use cutex::profiles::model::CodezConfig;
use cutex::runtime::alden::find_live_cute_alden_session_by_name;
use cutex::runtime::lifecycle::cutex_session_host_is_local;
use cutex::runtime::process_scope::terminate_managed_agent_scope;
use cutex::session::model::CutexAppServerRuntimeBinding;
use cutex::session::model::CutexAppServerTransport;
use cutex::session::model::CutexSessionRecord;
use cutex::session::service::clear_cutex_session_runtime_record;
use cutex::session::service::cutex_session_display_name;
use cutex::session::service::cutex_session_key_for_user_id;
use cutex::session::service::cutex_session_launch_cwd;
use cutex::session::service::persist_cutex_session_store_and_im_record;
use cutex::session::store::load_cutex_session_store;

use super::app_server_state_sync;
use super::app_server_user_input;
use cutex::management::v2::server_requests as app_server_pending_requests;

static APP_SERVER_RUNTIME_MANAGER: OnceLock<AppServerRuntimeManager> = OnceLock::new();
static APP_SERVER_BACKGROUND_DIAGNOSTICS: AtomicBool = AtomicBool::new(true);

pub(crate) fn suppress_background_diagnostics() {
    APP_SERVER_BACKGROUND_DIAGNOSTICS.store(false, Ordering::Release);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeEventSinkDisposition {
    Published,
    /// Addressed to a thread whose sub-agent lineage is not yet known; held
    /// until it resolves (see `subagent_lineage`).
    BufferedOtherThreadEvent,
    /// Addressed to a thread that is not a sub-agent of the bound thread.
    IgnoredForeignThreadEvent,
}

/// Which thread a published event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishScope {
    /// The session's bound thread (or thread-less runtime events).
    BoundThread,
    /// A verified Codex sub-agent (descendant) thread of the bound thread.
    SubagentThread,
}

fn native_event_context(context: &AppServerRuntimeEventContext) -> NativeEventContext {
    NativeEventContext {
        cutex_session_id: context.cutex_session_id.clone(),
        thread_id: context.thread_id.clone(),
        host_id: current_host_name(),
        runtime_generation: context.runtime_generation,
        runtime_backend: context.runtime_backend.clone(),
        schema: AppServerSchema {
            protocol: "codex-app-server".to_string(),
            major_version: 2,
            version: context.schema.version.clone(),
            sha256: context.schema.sha256.clone(),
            channel: AppServerSchemaChannel::Experimental,
            capabilities: serde_json::json!({ "experimentalApi": true }),
            extensions: if cutex::launch::stock::schema_has_external_input(
                &context.schema.version,
                context.schema.sha256.as_str(),
            ) {
                vec!["external-input-v1".to_string()]
            } else if context.schema.sha256 == cutex::launch::stock::STOCK_SCHEMA_SHA256 {
                Vec::new()
            } else {
                vec!["cutex-inter-agent-v2".to_string()]
            },
        },
    }
}

fn learn_subagent_lineage(
    context: &AppServerRuntimeEventContext,
    event: &cutex::app_server::client::AppServerEvent,
) {
    if let cutex::app_server::client::AppServerEvent::Notification(notification) = event {
        subagent_lineage::learn_from_notification(
            &context.cutex_session_id,
            &context.thread_id,
            &notification.raw,
        );
    }
}

/// Routes one runtime event. Bound-thread events publish as before. Events
/// for other threads publish (native `threadId` intact) only when the thread
/// is a verified sub-agent of the bound thread; unknown threads are buffered
/// while `resolve_lineage` looks them up, and unrelated threads are ignored.
fn handle_runtime_native_event(
    context: &AppServerRuntimeEventContext,
    event: &cutex::app_server::client::AppServerEvent,
    mut publish: impl FnMut(
        &cutex::app_server::client::AppServerEvent,
        cutex::management::v2::model::PendingEvent,
        PublishScope,
    ) -> anyhow::Result<()>,
    mut resolve_lineage: impl FnMut(&str),
) -> anyhow::Result<RuntimeEventSinkDisposition> {
    if super::thread_titles::claim_event(event) {
        return Ok(RuntimeEventSinkDisposition::IgnoredForeignThreadEvent);
    }
    let native_context = native_event_context(context);
    let session = context.cutex_session_id.as_str();
    let root = context.thread_id.as_str();
    let disposition = match pending_event_from_app_server(&native_context, event)? {
        NativeEventDisposition::Publish(pending) => {
            learn_subagent_lineage(context, event);
            publish(event, pending, PublishScope::BoundThread)?;
            super::thread_titles::on_runtime_event(context, event);
            RuntimeEventSinkDisposition::Published
        }
        NativeEventDisposition::OtherThread { thread_id, pending } => {
            match subagent_lineage::route_foreign_event(session, root, &thread_id, event) {
                ForeignThreadRoute::Descendant => {
                    learn_subagent_lineage(context, event);
                    publish(event, pending, PublishScope::SubagentThread)?;
                    RuntimeEventSinkDisposition::Published
                }
                ForeignThreadRoute::Buffered { resolve } => {
                    if resolve {
                        resolve_lineage(&thread_id);
                    }
                    RuntimeEventSinkDisposition::BufferedOtherThreadEvent
                }
                ForeignThreadRoute::Rejected => {
                    RuntimeEventSinkDisposition::IgnoredForeignThreadEvent
                }
            }
        }
    };
    if matches!(
        event,
        cutex::app_server::client::AppServerEvent::Disconnected { .. }
    ) {
        subagent_lineage::clear_pending(session, root);
        return Ok(disposition);
    }
    // Publish buffered sub-agent events whose lineage is now verified (by
    // this event, by the background resolver, or by a Management read).
    loop {
        let (ready, dropped) = subagent_lineage::take_ready_events(session, root);
        if dropped > 0 && APP_SERVER_BACKGROUND_DIAGNOSTICS.load(Ordering::Acquire) {
            eprintln!(
                "\x1b[33mwarning:\x1b[0m dropped {dropped} app-server event(s) for threads that are not sub-agents of {root}"
            );
        }
        if ready.is_empty() {
            break;
        }
        for buffered in &ready {
            if let NativeEventDisposition::OtherThread { pending, .. } =
                pending_event_from_app_server(&native_context, buffered)?
            {
                learn_subagent_lineage(context, buffered);
                publish(buffered, pending, PublishScope::SubagentThread)?;
            }
        }
    }
    Ok(disposition)
}

/// Looks up a thread's ancestry off the event worker (a synchronous request
/// on the worker would stall the bounded event channel).
fn spawn_subagent_lineage_resolver(cutex_session_id: String, root_thread_id: String, thread_id: String) {
    let spawned = std::thread::Builder::new()
        .name("cutex-subagent-lineage".to_string())
        .spawn(move || {
            // Ok(true) records the verified edges and Ok(false) marks the
            // thread rejected; buffered events are then flushed or dropped on
            // the session's next runtime event. Errors retry later.
            let _ = subagent_lineage::verify_descendant(
                &cutex_session_id,
                &root_thread_id,
                &thread_id,
                |current| {
                    let handle = runtime_manager()
                        .handle(&cutex_session_id)
                        .map_err(|error| LineageReadError::Unavailable(format!("{error:#}")))?;
                    match handle.request_raw_message(serde_json::json!({
                        "id": format!("cutex-lineage-{}", uuid::Uuid::new_v4()),
                        "method": "thread/read",
                        "params": { "threadId": current, "includeTurns": false },
                    })) {
                        Ok(response) => Ok(response.pointer("/result/thread").cloned()),
                        Err(cutex::app_server::client::AppServerClientError::Rpc(_)) => Ok(None),
                        Err(error) => Err(LineageReadError::Unavailable(error.to_string())),
                    }
                },
            );
        });
    if let Err(error) = spawned {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to start sub-agent lineage lookup: {error}");
    }
}

fn publish_runtime_event(
    context: &AppServerRuntimeEventContext,
    event: &cutex::app_server::client::AppServerEvent,
    pending: cutex::management::v2::model::PendingEvent,
    scope: PublishScope,
) -> anyhow::Result<()> {
    let synchronize_before_publish = matches!(
        event,
        cutex::app_server::client::AppServerEvent::ServerRequest(_)
    ) || matches!(
        event,
        cutex::app_server::client::AppServerEvent::Notification(notification)
            if notification.method == "serverRequest/resolved"
    );
    if scope == PublishScope::SubagentThread {
        // Sub-agent events are stream-only: keep pending server-request
        // bookkeeping (a sub-agent approval stays answerable) but never let
        // them touch the bound session's record, activity, usage or queue.
        if synchronize_before_publish {
            app_server_state_sync::handle_runtime_event(context, event)?;
        }
        management_v2_repository()?.append(pending)?;
        return Ok(());
    }
    if synchronize_before_publish {
        app_server_state_sync::handle_runtime_event(context, event)?;
    }
    let envelope = management_v2_repository()?.append(pending)?;
    if let Err(err) = record_activity_event(&envelope) {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to update session activity: {err:#}");
    }
    // Resolved by profile id: after a rename a live runtime reports under the
    // new name (docs/profile-id-and-runtimes.md).
    let profile = context.current_profile();
    if let Err(err) =
        record_usage_event_for_runtime(&envelope, profile.id.as_deref(), profile.name.as_deref(), &context.usage)
    {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to update session usage: {err:#}");
    }
    cutex::management::v2::profile_quota_native::ingest_runtime_event(&envelope, profile.key());
    if !synchronize_before_publish {
        app_server_state_sync::handle_runtime_event(context, event)?;
    }
    // Asks (委托): attempt activity, answers and questions; before the owner
    // queue flush so a finished ask releases the admission lease first.
    super::asks_runtime::on_runtime_event(&context.cutex_session_id, context.runtime_generation, event);
    if let cutex::app_server::client::AppServerEvent::Notification(notification) = event {
        if notification.method == "turn/completed" && notification.params.as_ref().and_then(|p| p.get("threadId")).and_then(serde_json::Value::as_str) == Some(context.thread_id.as_str()) {
            if let Some(turn) = notification.params.as_ref().and_then(|p| p.pointer("/turn/id")).and_then(serde_json::Value::as_str) {
                cutex::browser_relay::turn_completed(&context.cutex_session_id, &context.thread_id, context.runtime_generation, turn);
            }
        }
    }
    app_server_user_input::handle_runtime_event(&context.cutex_session_id, event)?;
    Ok(())
}

struct ManagementEventSink;
impl cutex::app_server::manager::AppServerRuntimeEventSink for ManagementEventSink {
    fn should_defer_journal_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
        origin: &cutex::app_server::client::AppServerHandle,
    ) -> bool {
        super::side_chats::defer_diagnostic(context, event, origin)
    }
    fn should_journal_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
        origin: &cutex::app_server::client::AppServerHandle,
    ) -> bool {
        !super::side_chats::is_transient_event(context, event, origin)
    }
    fn handle_event_from_connection(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
        origin: &cutex::app_server::client::AppServerHandle,
    ) -> anyhow::Result<()> {
        if super::side_chats::claim_event(context, event, origin) {
            return Ok(());
        }
        self.handle_event(context, event)
    }
    fn event_ledger(&self) -> Option<Arc<cutex::app_server::single_writer::Ledger>> {
        cutex::app_server::single_writer::process_ledger()
    }
    fn handle_unowned_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
        origin: &cutex::app_server::client::AppServerHandle,
    ) -> anyhow::Result<()> {
        // Ephemeral threads this process opened on the connection (side
        // chats, generated titles) still get their events; everything else
        // is journaled by the process that owns the runtime's events.
        if !super::side_chats::claim_event(context, event, origin) {
            super::thread_titles::claim_event(event);
        }
        Ok(())
    }
    fn handle_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
    ) -> anyhow::Result<()> {
        handle_runtime_native_event(
            context,
            event,
            |event, pending, scope| publish_runtime_event(context, event, pending, scope),
            |thread_id| {
                spawn_subagent_lineage_resolver(
                    context.cutex_session_id.clone(),
                    context.thread_id.clone(),
                    thread_id.to_string(),
                )
            },
        )?;
        Ok(())
    }
}

pub(crate) fn runtime_manager() -> &'static AppServerRuntimeManager {
    APP_SERVER_RUNTIME_MANAGER.get_or_init(|| {
        let manager = AppServerRuntimeManager::new(Arc::new(ManagementEventSink));
        #[cfg(not(test))]
        {
            let emit_background_diagnostics =
                APP_SERVER_BACKGROUND_DIAGNOSTICS.load(Ordering::Acquire);
            if let Err(error) =
                spawn_activity_projector(manager.clone(), emit_background_diagnostics)
            {
                if emit_background_diagnostics {
                    eprintln!(
                        "\x1b[33mwarning:\x1b[0m failed to start Cutex activity projector: {error:#}"
                    );
                }
            }
        }
        manager
    })
}

pub(crate) fn connect_runtime(
    config: &CodezConfig,
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    runtime_agent_id: &str,
) -> anyhow::Result<AppServerRuntimeConnectResult> {
    connect_runtime_with_model_provider(config, record, binding, runtime_agent_id, None)
}

pub(crate) fn connect_runtime_with_model_provider(
    config: &CodezConfig,
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    runtime_agent_id: &str,
    model_provider: Option<&str>,
) -> anyhow::Result<AppServerRuntimeConnectResult> {
    if record.is_retired() {
        anyhow::bail!(
            "cannot connect a runtime for retired cutex session {}",
            record.cutex_session_id
        );
    }
    let manager = runtime_manager();
    let mut resume =
        thread_resume_params_for_session_with_model_provider(record, model_provider)?;
    cutex::memory::attach_launch_augmentation_to_thread_resume(&mut resume, record);
    let runtime_backend = serde_json::to_value(record.runtime_backend)?
        .as_str()
        .context("runtime backend did not serialize as a string")?
        .to_string();
    let connected = manager.connect_binding(
        &record.cutex_session_id,
        binding,
        resume,
        record.runtime_generation,
        &runtime_backend,
    )?;
    let registration = runtime_agent_registration(record, binding, runtime_agent_id)?;
    if let Err(error) = manager.start_agent_bus_bridge(
        &record.cutex_session_id,
        Arc::new(AgentBusHttpClient::from_config(config)),
        AppServerAgentBusBridgeOptions::new(
            registration,
            record.codex_session_id.clone().unwrap_or_default(),
        )
        .with_cutex_session_id(record.cutex_session_id.clone()),
    ) {
        let _ = manager.disconnect(&record.cutex_session_id);
        return Err(error);
    }
    if let Err(error) = app_server_user_input::flush_queued_if_idle(&record.cutex_session_id) {
        let _ = manager.disconnect(&record.cutex_session_id);
        return Err(error.context("failed to flush queued app-server user input"));
    }
    Ok(connected)
}

pub(crate) fn start_connected_runtime_bridge(
    config: &CodezConfig,
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    runtime_agent_id: &str,
) -> anyhow::Result<()> {
    let thread_id = record
        .codex_session_id
        .clone()
        .context("new runtime record has no persisted thread id")?;
    let registration = runtime_agent_registration(record, binding, runtime_agent_id)?;
    runtime_manager().start_agent_bus_bridge(
        &record.cutex_session_id,
        Arc::new(AgentBusHttpClient::from_config(config)),
        AppServerAgentBusBridgeOptions::new(registration, thread_id)
            .with_cutex_session_id(record.cutex_session_id.clone()),
    )?;
    Ok(())
}

pub(crate) fn disconnect_runtime(cutex_session_id: &str) -> anyhow::Result<bool> {
    // A runtime this process lets go of (stop, cutover, failed launch) is not
    // adopted back by the late-adoption observer while it is being stopped.
    cutex::app_server::late_adoption::note_local_disconnect(cutex_session_id);
    let disconnected = runtime_manager().disconnect(cutex_session_id)?;
    app_server_pending_requests::clear_session(cutex_session_id)?;
    Ok(disconnected)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct AppServerAdoptionSummary {
    pub(crate) adopted: usize,
    pub(crate) cleared_stale: usize,
    pub(crate) skipped: usize,
    pub(crate) failures: Vec<String>,
}

/// Decision made before `cutex/runtime/online` is allowed to create a new
/// manager-owned app-server occurrence.  A persisted binding is an ownership
/// claim, not disposable error state: a live claim must either reconnect or
/// stop with an explicit cutover requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PersistedRuntimeRecoveryAction {
    Launch,
    Reconnect {
        runtime_agent_id: String,
    },
    ClearStaleAndLaunch,
    CutoverRequired {
        reason: &'static str,
        pid: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedRuntimeRecoveryOutcome {
    NoClaim,
    RecoveredExact,
    ClearedDeadClaim,
}

const MAX_PROCESS_BINDING_START_SKEW: chrono::Duration = chrono::Duration::seconds(10);
const EXACT_ENDPOINT_OWNERSHIP_ATTEMPTS: usize = 6;
const EXACT_ENDPOINT_OWNERSHIP_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Provider lifecycle recovery for one exact durable ownership claim. This
/// function never launches a child: it either reconnects the proven existing
/// occurrence, CAS-clears a conclusively dead claim, or fails closed.
pub(crate) fn recover_persisted_runtime_for_lifecycle(
    config: &CodezConfig,
    expected: &CutexSessionRecord,
) -> anyhow::Result<ManagedRuntimeRecoveryOutcome> {
    if expected.explicit_launch.is_some() {
        // Native-launch agents recover through their own launch contract.
        return super::native_management_lifecycle::recover(expected);
    }
    match classify_local_persisted_runtime_recovery(expected, process_is_running) {
        PersistedRuntimeRecoveryAction::Launch => Ok(ManagedRuntimeRecoveryOutcome::NoClaim),
        PersistedRuntimeRecoveryAction::ClearStaleAndLaunch => {
            let scope_stop = terminate_managed_agent_scope(&expected.cutex_session_id, true)
                .context("failed to clean a stale managed Agent process scope")?;
            if !scope_stop.stopped {
                anyhow::bail!(
                    "stale managed Agent process scope remained active: {}",
                    scope_stop.detail
                );
            }
            clear_stale_persisted_runtime(expected)?;
            Ok(ManagedRuntimeRecoveryOutcome::ClearedDeadClaim)
        }
        PersistedRuntimeRecoveryAction::CutoverRequired { reason, pid } => {
            anyhow::bail!(
                "runtime recovery requires owner action ({reason}, pid={})",
                pid.map(|pid| pid.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        }
        PersistedRuntimeRecoveryAction::Reconnect { runtime_agent_id } => {
            let binding = expected
                .app_server_runtime
                .as_ref()
                .context("runtime reconnect claim omitted its app-server binding")?;
            verify_exact_live_runtime_claim(expected, binding)?;
            let already_connected = runtime_manager()
                .status(&expected.cutex_session_id)?
                .is_some_and(|status| status.connected);
            if already_connected {
                if runtime_manager()
                    .agent_bus_bridge_status(&expected.cutex_session_id)?
                    .is_none()
                {
                    start_connected_runtime_bridge(config, expected, binding, &runtime_agent_id)?;
                }
            } else if let Err(error) = connect_runtime(config, expected, binding, &runtime_agent_id)
            {
                return Err(error.context("failed to reconnect exact claimed runtime"));
            }
            let verification = verify_recovered_runtime_state(expected, binding, &runtime_agent_id);
            if let Err(error) = verification {
                if already_connected {
                    return Err(error);
                }
                let disconnect = disconnect_runtime(&expected.cutex_session_id);
                return Err(match disconnect {
                    Ok(_) => error,
                    Err(disconnect_error) => error.context(format!(
                        "failed to disconnect rejected recovered runtime: {disconnect_error:#}"
                    )),
                });
            }
            Ok(ManagedRuntimeRecoveryOutcome::RecoveredExact)
        }
    }
}

fn verify_recovered_runtime_state(
    expected: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    runtime_agent_id: &str,
) -> anyhow::Result<()> {
    if !persisted_runtime_ownership_matches(expected)? {
        anyhow::bail!("durable runtime ownership changed during reconnect");
    }
    verify_exact_live_runtime_claim(expected, binding)?;
    let status = runtime_manager()
        .status(&expected.cutex_session_id)?
        .context("reconnected runtime is absent from the manager")?;
    if !status.connected
        || status.runtime_generation != expected.runtime_generation
        || expected.codex_session_id.as_deref() != Some(status.thread_id.as_str())
    {
        anyhow::bail!("reconnected runtime identity/generation does not match the durable claim");
    }
    let registration = runtime_manager()
        .refresh_agent_bus_registration(&expected.cutex_session_id)?
        .context("reconnected runtime has no Agent Bus bridge")?;
    if !registration.registered || registration.runtime_agent_id != runtime_agent_id {
        anyhow::bail!("reconnected runtime Agent Bus identity is not exact");
    }
    Ok(())
}

pub(super) fn verify_exact_live_runtime_claim(
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
) -> anyhow::Result<()> {
    let initial_process_started_at = process_started_at(binding.pid)?;
    verify_exact_live_runtime_claim_with_probe(
        record,
        binding,
        initial_process_started_at,
        EXACT_ENDPOINT_OWNERSHIP_ATTEMPTS,
        EXACT_ENDPOINT_OWNERSHIP_RETRY_DELAY,
        || binding_endpoint_is_owned_by_process(binding, binding.pid),
    )
}

/// `/proc/net/unix` and `/proc/<pid>/fd` are two separately observed kernel
/// tables. A listener with clients churning through accept can transiently
/// expose an incomplete pathname-row snapshot even though both objects remain
/// live. Re-observe that evidence for a short bounded window before turning a
/// durable ownership claim into an owner-action failure. Every attempt must
/// still prove the same PID identity; no fallback ownership rule is accepted.
fn verify_exact_live_runtime_claim_with_probe(
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    initial_process_started_at: chrono::DateTime<chrono::Utc>,
    attempts: usize,
    retry_delay: Duration,
    mut endpoint_is_owned: impl FnMut() -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    for attempt in 1..=attempts {
        if endpoint_is_owned()? {
            let current_process_started_at = process_started_at(binding.pid)?;
            anyhow::ensure!(
                current_process_started_at == initial_process_started_at,
                "runtime PID identity changed during endpoint ownership verification"
            );
            return validate_exact_live_runtime_claim_evidence(
                record,
                binding,
                current_process_started_at,
                true,
            );
        }
        if attempt < attempts {
            std::thread::sleep(retry_delay);
        }
    }
    validate_exact_live_runtime_claim_evidence(record, binding, initial_process_started_at, false)
}

fn validate_exact_live_runtime_claim_evidence(
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    process_started_at: chrono::DateTime<chrono::Utc>,
    endpoint_owned: bool,
) -> anyhow::Result<()> {
    if record.runtime_generation == 0 {
        anyhow::bail!("live runtime claim has no generation");
    }
    if record.app_server_launch_claim_id.is_some() {
        anyhow::bail!("live runtime claim overlaps an unresolved launch claim");
    }
    if record.runtime_pid != Some(binding.pid) || binding.pid == 0 {
        anyhow::bail!("live runtime PID does not match the durable binding");
    }
    if !record
        .current_runtime_agent_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        anyhow::bail!("live runtime claim has no exact runtime endpoint identity");
    }
    let binding_started_at = chrono::DateTime::parse_from_rfc3339(&binding.started_at)
        .context("runtime binding started_at is invalid")?
        .with_timezone(&chrono::Utc);
    let start_skew = binding_started_at.signed_duration_since(process_started_at);
    if start_skew < chrono::Duration::zero() || start_skew > MAX_PROCESS_BINDING_START_SKEW {
        anyhow::bail!("runtime PID creation time does not match the durable binding");
    }
    if !endpoint_owned {
        anyhow::bail!(
            "runtime endpoint is not live and owned by the claimed PID {} ({})",
            binding.pid,
            binding.endpoint
        );
    }
    Ok(())
}

fn binding_endpoint_is_owned_by_process(
    binding: &CutexAppServerRuntimeBinding,
    pid: u32,
) -> anyhow::Result<bool> {
    let layout = AppServerRuntimeLayout::from_binding(binding)?;
    if !layout.endpoint_ready() {
        return Ok(false);
    }
    match binding.transport {
        CutexAppServerTransport::UnixSocket => {
            #[cfg(target_os = "linux")]
            {
                let socket_path = binding
                    .endpoint
                    .strip_prefix("unix://")
                    .context("runtime Unix endpoint is invalid")?;
                return linux_unix_socket_is_owned_by_process(Path::new(socket_path), pid);
            }
            #[cfg(target_os = "macos")]
            {
                let socket_path = binding
                    .endpoint
                    .strip_prefix("unix://")
                    .context("runtime Unix endpoint is invalid")?;
                return macos_unix_socket_is_owned_by_process(Path::new(socket_path), pid);
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            anyhow::bail!("Unix socket PID ownership verification is unsupported on this platform")
        }
        CutexAppServerTransport::LoopbackWebSocket => {
            #[cfg(windows)]
            {
                return windows_loopback_listener_is_owned_by_process(&binding.endpoint, pid);
            }
            #[cfg(not(windows))]
            anyhow::bail!(
                "loopback WebSocket PID ownership verification is unsupported on this platform"
            )
        }
    }
}

/// macOS has no `/proc/net/unix` or descriptor links: the kernel-backed peer
/// PID of a one-shot connection (`LOCAL_PEERPID`, the Linux fallback's
/// `SO_PEERCRED`) must be the claimed PID, owned by this user, on an
/// endpoint that did not change while it was inspected.
#[cfg(target_os = "macos")]
fn macos_unix_socket_is_owned_by_process(path: &Path, pid: u32) -> anyhow::Result<bool> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let resolved_path = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve runtime socket {}", path.display()))?;
    let metadata = fs::metadata(&resolved_path)
        .with_context(|| format!("failed to inspect runtime socket {}", path.display()))?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Ok(false);
    }
    let Ok(stream) = std::os::unix::net::UnixStream::connect(&resolved_path) else {
        return Ok(false);
    };
    let peer = cutex::platform::macos_proc::unix_peer(&stream);
    drop(stream);
    let endpoint_is_stable = fs::canonicalize(path).ok().as_ref() == Some(&resolved_path)
        && fs::metadata(path).is_ok_and(|current| {
            current.dev() == metadata.dev() && current.ino() == metadata.ino()
        });
    Ok(peer.is_some_and(|(peer_pid, uid)| peer_pid == pid && uid == unsafe { libc::geteuid() })
        && endpoint_is_stable)
}

#[cfg(target_os = "linux")]
fn linux_unix_socket_is_owned_by_process(path: &Path, pid: u32) -> anyhow::Result<bool> {
    linux_unix_socket_is_owned_by_process_with_listener_rows(
        path,
        pid,
        linux_unix_socket_listener_inodes,
    )
}

#[cfg(target_os = "linux")]
fn linux_unix_socket_is_owned_by_process_with_listener_rows(
    path: &Path,
    pid: u32,
    mut listener_rows: impl FnMut(&Path) -> anyhow::Result<Vec<String>>,
) -> anyhow::Result<bool> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    // Native 0.156 uses a rendezvous symlink to its protected listener.
    let resolved_path = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve runtime socket {}", path.display()))?;
    let metadata = fs::metadata(&resolved_path)
        .with_context(|| format!("failed to inspect runtime socket {}", path.display()))?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Ok(false);
    }
    let endpoint_is_stable = || {
        fs::canonicalize(path).ok().as_ref() == Some(&resolved_path)
            && fs::metadata(path).is_ok_and(|current| {
                current.dev() == metadata.dev() && current.ino() == metadata.ino()
            })
    };

    if let [inode] = listener_rows(&resolved_path)?.as_slice() {
        let expected_link = format!("socket:[{inode}]");
        let fd_dir = format!("/proc/{pid}/fd");
        for entry in fs::read_dir(&fd_dir)
            .with_context(|| format!("failed to inspect runtime process descriptors {fd_dir}"))?
        {
            let Ok(target) = fs::read_link(entry?.path()) else {
                continue;
            };
            if target.to_string_lossy() == expected_link {
                // Reject an alias or socket replaced while ownership was inspected.
                return Ok(endpoint_is_stable());
            }
        }
    }

    // The pathname and descriptor tables above are separately observed snapshots.
    // On Linux, a one-shot connection provides kernel-backed peer credentials for
    // the same endpoint, which avoids turning a transient table miss into an
    // owner-action failure without weakening the claimed-PID requirement.
    Ok(
        linux_unix_socket_peer_credentials(&resolved_path)
            .is_some_and(|credentials| {
                credentials.pid == pid as libc::pid_t
                    && credentials.uid == unsafe { libc::geteuid() }
            })
            && endpoint_is_stable(),
    )
}

#[cfg(target_os = "linux")]
fn linux_unix_socket_listener_inodes(resolved_path: &Path) -> anyhow::Result<Vec<String>> {
    let unix_sockets = fs::read_to_string("/proc/net/unix")?;
    // `/proc/net/unix` may repeat the bound pathname on accepted stream
    // sockets. Only the stream row in the unconnected/listening state owns
    // the endpoint; connected rows must not participate in listener identity.
    let mut listener_inodes = unix_sockets
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let flags = u32::from_str_radix(fields.get(3)?, 16).ok()?;
            (fields.get(7).copied() == Some(resolved_path.to_string_lossy().as_ref())
                && fields.get(4).copied() == Some("0001")
                && fields.get(5).copied() == Some("01")
                && flags & 0x0001_0000 != 0)
                .then(|| fields.get(6).map(|inode| inode.to_string()))
                .flatten()
        })
        .collect::<Vec<_>>();
    listener_inodes.sort_unstable();
    listener_inodes.dedup();
    Ok(listener_inodes)
}

#[cfg(target_os = "linux")]
fn linux_unix_socket_peer_credentials(path: &Path) -> Option<libc::ucred> {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(path).ok()?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut credentials_len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut libc::ucred as *mut libc::c_void,
            &mut credentials_len,
        )
    };
    drop(stream);
    (result == 0 && credentials_len == std::mem::size_of::<libc::ucred>() as libc::socklen_t)
        .then_some(credentials)
}

#[cfg(windows)]
fn windows_loopback_listener_is_owned_by_process(url: &str, pid: u32) -> anyhow::Result<bool> {
    use std::process::{Command, Stdio};

    let url = url::Url::parse(url).context("runtime loopback endpoint is invalid")?;
    let host = url
        .host_str()
        .context("runtime endpoint omitted its host")?;
    if !matches!(host, "127.0.0.1" | "::1" | "localhost") {
        anyhow::bail!("runtime endpoint is not loopback");
    }
    let port = url.port().context("runtime endpoint omitted its port")?;
    let script = format!(
        "Get-NetTCPConnection -State Listen -LocalPort {port} -ErrorAction Stop | Select-Object -ExpandProperty OwningProcess"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .output()
        .context("failed to query loopback listener ownership")?;
    if !output.status.success() {
        anyhow::bail!("loopback listener ownership query failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .any(|owner| owner == pid))
}

pub(crate) fn classify_persisted_runtime_recovery(
    record: &CutexSessionRecord,
    process_is_live: impl Fn(u32) -> bool,
) -> PersistedRuntimeRecoveryAction {
    let binding_pid = record
        .app_server_runtime
        .as_ref()
        .map(|binding| binding.pid);
    let binding_is_live = binding_pid.is_some_and(&process_is_live);
    let runtime_pid_is_live = record.runtime_pid.is_some_and(&process_is_live);
    let alden_pid_is_live = record.alden_pid.is_some_and(&process_is_live);
    let live_child_pid = binding_pid
        .filter(|_| binding_is_live)
        .or(record.runtime_pid.filter(|_| runtime_pid_is_live))
        .or(record.alden_pid.filter(|_| alden_pid_is_live));

    if record
        .app_server_launch_claim_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return PersistedRuntimeRecoveryAction::CutoverRequired {
            reason: "pending_launch_ownership_unknown",
            pid: live_child_pid,
        };
    }

    if record.app_server_runtime.is_some() {
        if binding_is_live {
            let runtime_agent_id = record
                .current_runtime_agent_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            return match runtime_agent_id {
                Some(runtime_agent_id) => PersistedRuntimeRecoveryAction::Reconnect {
                    runtime_agent_id: runtime_agent_id.to_string(),
                },
                None => PersistedRuntimeRecoveryAction::CutoverRequired {
                    reason: "live_binding_missing_runtime_agent_id",
                    pid: live_child_pid,
                },
            };
        }
        if runtime_pid_is_live || alden_pid_is_live {
            return PersistedRuntimeRecoveryAction::CutoverRequired {
                reason: "stale_binding_has_live_child",
                pid: live_child_pid,
            };
        }
        return PersistedRuntimeRecoveryAction::ClearStaleAndLaunch;
    }

    if record
        .current_runtime_agent_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return PersistedRuntimeRecoveryAction::CutoverRequired {
            reason: "binding_missing_runtime_agent_id_present",
            pid: live_child_pid.or(record.runtime_pid).or(record.alden_pid),
        };
    }
    if runtime_pid_is_live || alden_pid_is_live {
        return PersistedRuntimeRecoveryAction::CutoverRequired {
            reason: "binding_missing_live_child",
            pid: live_child_pid,
        };
    }
    if record.runtime_pid.is_some() || record.alden_pid.is_some() {
        return PersistedRuntimeRecoveryAction::ClearStaleAndLaunch;
    }
    PersistedRuntimeRecoveryAction::Launch
}

pub(crate) fn classify_local_persisted_runtime_recovery(
    record: &CutexSessionRecord,
    process_is_live: impl Fn(u32) -> bool,
) -> PersistedRuntimeRecoveryAction {
    let action = classify_persisted_runtime_recovery(record, process_is_live);
    if matches!(
        action,
        PersistedRuntimeRecoveryAction::Launch
            | PersistedRuntimeRecoveryAction::ClearStaleAndLaunch
    ) {
        if let Some(pid) = record
            .alden_session_name
            .as_deref()
            .and_then(find_live_cute_alden_session_by_name)
            .map(|session| session.pid)
        {
            return PersistedRuntimeRecoveryAction::CutoverRequired {
                reason: "stale_binding_has_live_tui",
                pid: Some(pid),
            };
        }
    }
    action
}

/// Remove only a binding that was classified stale from the same durable
/// snapshot that was inspected.  A changed generation or ownership field is a
/// concurrent lifecycle action and must block a new launch.
pub(crate) fn clear_stale_persisted_runtime(expected: &CutexSessionRecord) -> anyhow::Result<()> {
    // Boot-aware: a PID recorded in a previous boot (or reused since) is not
    // this occurrence even when some process runs under that number.
    let previous_boot = cutex::session::boot_fence::with_host_probe(|host| {
        cutex::session::boot_fence::binding_is_from_previous_boot(expected, host)
    });
    clear_stale_persisted_runtime_if_dead(
        expected,
        |pid| cutex::session::boot_fence::occurrence_pid_is_live(expected, pid),
        previous_boot,
    )
}

pub(crate) fn persisted_runtime_ownership_matches(
    expected: &CutexSessionRecord,
) -> anyhow::Result<bool> {
    let store = load_cutex_session_store()?;
    let Some(key) = cutex_session_key_for_user_id(&store, &expected.cutex_session_id) else {
        return Ok(false);
    };
    let Some(current) = store.sessions.get(&key) else {
        return Ok(false);
    };
    Ok(runtime_ownership_snapshot_matches(current, expected))
}

pub(crate) fn restore_recovery_snapshot_if_owned(
    expected: &CutexSessionRecord,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let key =
        cutex_session_key_for_user_id(&store, &expected.cutex_session_id).ok_or_else(|| {
            anyhow::anyhow!("cutex session disappeared: {}", expected.cutex_session_id)
        })?;
    let current = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared: {key}"))?;
    if !runtime_ownership_snapshot_matches(&current, expected) {
        anyhow::bail!("runtime ownership changed concurrently; reconnect rollback was not applied");
    }
    if current == *expected {
        return Ok(());
    }
    store.sessions.insert(key.clone(), expected.clone());
    persist_cutex_session_store_and_im_record(&store, &key)
}

fn clear_stale_persisted_runtime_if_dead(
    expected: &CutexSessionRecord,
    process_is_live: impl Fn(u32) -> bool,
    previous_boot: bool,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let key =
        cutex_session_key_for_user_id(&store, &expected.cutex_session_id).ok_or_else(|| {
            anyhow::anyhow!("cutex session disappeared: {}", expected.cutex_session_id)
        })?;
    let current = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared: {key}"))?;
    if !runtime_ownership_snapshot_matches(&current, expected) {
        anyhow::bail!("runtime changed concurrently; stale cleanup was not applied");
    }
    if let Some(pid) = runtime_occurrence_pids(&current)
        .into_iter()
        .find(|pid| process_is_live(*pid))
    {
        anyhow::bail!("runtime process became live during stale cleanup: {pid}");
    }
    if let Some(binding) = current.app_server_runtime.as_ref() {
        let layout = AppServerRuntimeLayout::from_binding(binding)
            .context("failed to reconstruct stale app-server endpoint")?;
        // An endpoint of a previous boot that answers now (a reused loopback
        // port or socket path) belongs to another process.
        if !previous_boot && binding_endpoint_is_reachable(&layout)? {
            anyhow::bail!(
                "stale app-server endpoint remains reachable during cleanup: {}",
                binding.endpoint
            );
        }
        cleanup_runtime_binding_files(binding)?;
    }
    clear_cutex_session_runtime_record(&mut store, &key, true)?;
    persist_cutex_session_store_and_im_record(&store, &key)
}

fn binding_endpoint_is_reachable(layout: &AppServerRuntimeLayout) -> anyhow::Result<bool> {
    match layout.endpoint() {
        #[cfg(unix)]
        AppServerEndpoint::UnixSocket { socket_path } => {
            Ok(std::os::unix::net::UnixStream::connect(socket_path).is_ok())
        }
        AppServerEndpoint::LoopbackWebSocket { .. } => Ok(layout.endpoint_ready()),
    }
}

fn runtime_ownership_snapshot_matches(
    current: &CutexSessionRecord,
    expected: &CutexSessionRecord,
) -> bool {
    current.host_id == expected.host_id
        && current.explicit_launch == expected.explicit_launch
        && current.runtime_backend == expected.runtime_backend
        && current.profile == expected.profile
        && current.pending_launch_id == expected.pending_launch_id
        && current.app_server_launch_claim_id == expected.app_server_launch_claim_id
        && current.runtime_generation == expected.runtime_generation
        && current.app_server_runtime == expected.app_server_runtime
        && current.current_runtime_agent_id == expected.current_runtime_agent_id
        && current.runtime_pid == expected.runtime_pid
        && current.alden_pid == expected.alden_pid
        && current.alden_session_name == expected.alden_session_name
}

fn runtime_occurrence_pids(record: &CutexSessionRecord) -> Vec<u32> {
    let mut occurrence_pids = record
        .app_server_runtime
        .as_ref()
        .map(|binding| binding.pid)
        .into_iter()
        .chain(record.runtime_pid)
        .chain(record.alden_pid)
        .filter(|pid| *pid != 0)
        .collect::<Vec<_>>();
    occurrence_pids.sort_unstable();
    occurrence_pids.dedup();
    occurrence_pids
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersistedRuntimeAdoptionAction {
    Retired,
    Remote,
    Recover(PersistedRuntimeRecoveryAction),
}

fn classify_persisted_runtime_adoption(
    record: &CutexSessionRecord,
    current_host: &str,
    process_is_live: impl Fn(u32) -> bool,
) -> PersistedRuntimeAdoptionAction {
    if record.is_retired() {
        return PersistedRuntimeAdoptionAction::Retired;
    }
    if !cutex_session_host_is_local(&record.host_id, current_host) {
        return PersistedRuntimeAdoptionAction::Remote;
    }
    PersistedRuntimeAdoptionAction::Recover(classify_local_persisted_runtime_recovery(
        record,
        process_is_live,
    ))
}

/// Synchronous part of startup recovery, run before Management listens:
/// clears every local binding that is proven not running (recorded before
/// this boot, exited, or its PID reused), so no client ever reads a durable
/// runtime claim of a previous boot. Only cheap evidence is used (record
/// timestamps, boot time, /proc or process creation time); live bindings
/// and anything needing a reconnect are left to [`adopt_persisted_runtimes`]
/// in the background. Stops early after `budget` (the background pass
/// clears the rest the same way).
pub(crate) fn clear_dead_runtimes_at_startup(
    current_host: &str,
    budget: Duration,
) -> anyhow::Result<AppServerAdoptionSummary> {
    let started = std::time::Instant::now();
    let store = load_cutex_session_store()?;
    let mut summary = AppServerAdoptionSummary::default();
    let candidates = cutex::session::boot_fence::with_host_probe(|host| {
        store
            .sessions
            .values()
            .filter(|record| {
                !record.is_retired()
                    && record.claude_session.is_none()
                    && cutex_session_host_is_local(&record.host_id, current_host)
                    && (record.app_server_runtime.is_some()
                        || record.runtime_pid.is_some()
                        || record.alden_pid.is_some())
            })
            .filter(|record| {
                // Explicit launches: only a proven-gone binding. Others: a
                // proven-gone binding or (legacy, no binding) dead PIDs.
                cutex::session::boot_fence::binding_is_proven_gone(record, host)
                    || (record.explicit_launch.is_none()
                        && record.app_server_runtime.is_none()
                        && ![record.runtime_pid, record.alden_pid]
                            .into_iter()
                            .flatten()
                            .any(|pid| cutex::session::boot_fence::occurrence_pid_is_live_with(record, pid, host)))
            })
            .cloned()
            .collect::<Vec<_>>()
    });
    drop(store);
    for record in candidates {
        if started.elapsed() >= budget {
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        }
        if record.explicit_launch.is_some() {
            match super::native_stop::stop_and_commit(&record, false) {
                Ok(outcome) if outcome.stopped => {
                    summary.cleared_stale = summary.cleared_stale.saturating_add(1)
                }
                Ok(outcome) => summary.failures.push(format!(
                    "{}: exited runtime cleanup incomplete: {}",
                    record.cutex_session_id, outcome.detail
                )),
                Err(error) => summary.failures.push(format!(
                    "{}: exited runtime claim kept: {error:#}",
                    record.cutex_session_id
                )),
            }
            continue;
        }
        let live = |pid: u32| cutex::session::boot_fence::occurrence_pid_is_live(&record, pid);
        match classify_persisted_runtime_adoption(&record, current_host, live) {
            PersistedRuntimeAdoptionAction::Recover(
                PersistedRuntimeRecoveryAction::ClearStaleAndLaunch,
            ) => match clear_stale_persisted_runtime(&record) {
                Ok(()) => summary.cleared_stale = summary.cleared_stale.saturating_add(1),
                Err(error) => summary.failures.push(format!(
                    "{}: failed to clear stale app-server binding: {error:#}",
                    record.cutex_session_id
                )),
            },
            _ => summary.skipped = summary.skipped.saturating_add(1),
        }
    }
    Ok(summary)
}

pub(crate) fn adopt_persisted_runtimes(
    config: &CodezConfig,
    current_host: &str,
) -> anyhow::Result<AppServerAdoptionSummary> {
    let store = load_cutex_session_store()?;
    let mut summary = AppServerAdoptionSummary::default();
    let session_keys = store.sessions.keys().cloned().collect::<Vec<_>>();
    for key in session_keys {
        let Some(record) = store.sessions.get(&key).cloned() else {
            continue;
        };
        if record.claude_session.is_some() {
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        }
        if record.explicit_launch.is_some() {
            let mut record = record;
            if !record.is_retired() && cutex_session_host_is_local(&record.host_id, current_host) {
                // A launch interrupted by a daemon crash leaves its claim (and a
                // Prepared intent) behind. Clear it when its owner is gone so
                // task delivery and the next start are not blocked.
                match super::management_context::auto_clear_dead_launch_claim(&record.cutex_session_id) {
                    Ok(()) => {
                        if let Some(current) = load_cutex_session_store()?.sessions.get(&key) {
                            record = current.clone();
                        }
                    }
                    Err(error) => summary.failures.push(format!(
                        "{}: launch claim kept: {error:#}",
                        record.cutex_session_id
                    )),
                }
            }
            let exited = record.app_server_runtime.as_ref().is_some_and(|binding| {
                !record.is_retired()
                    && cutex_session_host_is_local(&record.host_id, current_host)
                    && matches!(super::stock_lifecycle::stock_runtime_has_exited(&record, binding), Ok(true))
            });
            if record.is_retired()
                || !cutex_session_host_is_local(&record.host_id, current_host)
                || record.app_server_runtime.is_none()
            {
                summary.skipped = summary.skipped.saturating_add(1);
            } else if exited {
                // The runtime exited while the daemon was down (reboot,
                // crash): reconnecting cannot work, and keeping its claim
                // blocks package upgrades and archive. Clear it the way a
                // start does (proven process absence only; nothing is killed).
                match super::native_stop::stop_and_commit(&record, false) {
                    Ok(outcome) if outcome.stopped => {
                        summary.cleared_stale = summary.cleared_stale.saturating_add(1);
                    }
                    Ok(outcome) => summary.failures.push(format!(
                        "{}: exited runtime cleanup incomplete: {}",
                        record.cutex_session_id, outcome.detail
                    )),
                    Err(error) => summary.failures.push(format!(
                        "{}: exited runtime claim kept: {error:#}",
                        record.cutex_session_id
                    )),
                }
            } else {
                match super::stock_lifecycle::reconnect_ready_runtime(&record, &load_cutex_session_store()?) {
                    Ok(_) => summary.adopted = summary.adopted.saturating_add(1),
                    Err(error) => summary.failures.push(format!(
                        "{}: native reconnect failed; owner unchanged: {error:#}",
                        record.cutex_session_id
                    )),
                }
            }
            continue;
        }
        let live = |pid: u32| cutex::session::boot_fence::occurrence_pid_is_live(&record, pid);
        match classify_persisted_runtime_adoption(&record, current_host, live) {
            PersistedRuntimeAdoptionAction::Retired | PersistedRuntimeAdoptionAction::Remote => {
                summary.skipped = summary.skipped.saturating_add(1);
                continue;
            }
            PersistedRuntimeAdoptionAction::Recover(
                PersistedRuntimeRecoveryAction::ClearStaleAndLaunch,
            ) => {
                match clear_stale_persisted_runtime(&record) {
                    Ok(()) => {
                        summary.cleared_stale = summary.cleared_stale.saturating_add(1);
                    }
                    Err(error) => summary.failures.push(format!(
                        "{}: failed to clear stale app-server binding: {error:#}",
                        record.cutex_session_id
                    )),
                }
                continue;
            }
            PersistedRuntimeAdoptionAction::Recover(
                PersistedRuntimeRecoveryAction::CutoverRequired { reason, pid },
            ) => {
                summary.failures.push(format!(
                    "{}: cutover required before adoption ({reason}, pid={})",
                    record.cutex_session_id,
                    pid.map(|pid| pid.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                ));
                continue;
            }
            PersistedRuntimeAdoptionAction::Recover(PersistedRuntimeRecoveryAction::Launch) => {
                // A clean offline record has nothing to adopt.  Keeping this
                // branch explicit makes ownership classification total while
                // avoiding noisy startup output for ordinary offline sessions.
                continue;
            }
            PersistedRuntimeAdoptionAction::Recover(
                PersistedRuntimeRecoveryAction::Reconnect { runtime_agent_id },
            ) => {
                let Some(binding) = record.app_server_runtime.as_ref() else {
                    summary.failures.push(format!(
                        "{}: reconnect claim omitted its app-server binding",
                        record.cutex_session_id
                    ));
                    continue;
                };
                match connect_runtime(config, &record, binding, &runtime_agent_id) {
                    Ok(_) => summary.adopted = summary.adopted.saturating_add(1),
                    Err(error) => summary
                        .failures
                        .push(format!("{}: {error:#}", record.cutex_session_id)),
                }
            }
        }
    }
    Ok(summary)
}

/// Management only, after [`adopt_persisted_runtimes`]: keeps adopting
/// app-server runtimes that another cutex process started later. Agent
/// Management (`agent_create`, `member_runtime`, `member_replace`) runs in the
/// Agent Bus service, whose launches connect only that process; without this
/// Management reported such an agent offline (`session_offline` /
/// `app_server_unavailable` on submit) until its next restart. Like the
/// Claude observer it polls the shared store snapshot; an unchanged store
/// costs one revision query per tick. Never launches, stops or clears
/// anything: only an exact live claim is reconnected, through the same paths
/// as startup adoption.
pub(crate) fn observe_late_runtimes(current_host: &str) {
    use cutex::app_server::late_adoption::{adoption_candidate, LateAdoptionPlanner, TICK};
    let mut planner = LateAdoptionPlanner::default();
    let mut last_revision = None;
    loop {
        std::thread::sleep(TICK);
        let store = match cutex::session::store::load_cutex_session_store_shared() {
            Ok(store) => store,
            Err(_) => continue,
        };
        let revision = store.store_revision.get();
        if last_revision == Some(revision) && !planner.has_pending() {
            continue;
        }
        last_revision = Some(revision);
        let candidates = cutex::session::boot_fence::with_host_probe(|host| {
            store
                .sessions
                .values()
                .filter_map(|record| {
                    adoption_candidate(record, current_host, |record, pid| {
                        cutex::session::boot_fence::occurrence_pid_is_live_with(record, pid, host)
                    })
                })
                .collect::<Vec<_>>()
        });
        drop(store);
        let now = std::time::Instant::now();
        let due = planner.plan(candidates, now, |id| {
            cutex::app_server::late_adoption::disconnected_locally_since(id, now)
                || !matches!(runtime_manager().status(id), Ok(None))
        });
        for occurrence in due {
            match adopt_late_runtime(&occurrence, current_host) {
                Ok(true) => {
                    planner.adopted(&occurrence);
                    println!(
                        "app-server runtime adopted after startup: {} (pid {}, generation {})",
                        occurrence.cutex_session_id, occurrence.pid, occurrence.runtime_generation
                    );
                }
                // Changed or connected meanwhile: the next tick decides again.
                Ok(false) => planner.adopted(&occurrence),
                Err(error) => {
                    if planner.failed(&occurrence, format!("{error:#}"), std::time::Instant::now()) {
                        eprintln!(
                            "app-server late adoption warning: {}: {error:#}",
                            occurrence.cutex_session_id
                        );
                    }
                }
            }
        }
    }
}

/// Reconnects one settled occurrence. `Ok(false)`: nothing to do any more
/// (the claim changed or this process connected it meanwhile).
fn adopt_late_runtime(
    occurrence: &cutex::app_server::late_adoption::RuntimeOccurrence,
    current_host: &str,
) -> anyhow::Result<bool> {
    let store = cutex::session::store::load_cutex_session_store_shared()?;
    let Some(record) = store
        .sessions
        .values()
        .find(|record| record.cutex_session_id == occurrence.cutex_session_id)
        .cloned()
    else {
        return Ok(false);
    };
    let live = |record: &CutexSessionRecord, pid: u32| {
        cutex::session::boot_fence::occurrence_pid_is_live(record, pid)
    };
    if cutex::app_server::late_adoption::adoption_candidate(&record, current_host, live).as_ref()
        != Some(occurrence)
        || runtime_manager().status(&record.cutex_session_id)?.is_some()
    {
        return Ok(false);
    }
    let binding = record
        .app_server_runtime
        .as_ref()
        .context("adoption candidate lost its binding")?;
    if record.explicit_launch.is_some() {
        anyhow::ensure!(
            !super::stock_lifecycle::stock_runtime_has_exited(&record, binding)?,
            "runtime exited"
        );
        super::stock_lifecycle::reconnect_ready_runtime(&record, &store)?;
        return Ok(true);
    }
    match classify_persisted_runtime_adoption(&record, current_host, |pid| live(&record, pid)) {
        PersistedRuntimeAdoptionAction::Recover(PersistedRuntimeRecoveryAction::Reconnect {
            runtime_agent_id,
        }) => {
            verify_exact_live_runtime_claim(&record, binding)?;
            connect_runtime(
                &cutex::config::store::load_codez_config(),
                &record,
                binding,
                &runtime_agent_id,
            )?;
            Ok(true)
        }
        // Same rule as startup adoption: anything but an exact live claim
        // needs the owner (cutover) and is never touched here.
        other => anyhow::bail!("not adoptable without the owner: {other:?}"),
    }
}

pub(super) fn runtime_agent_registration(
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    runtime_agent_id: &str,
) -> anyhow::Result<AgentBusRegisterRequest> {
    let runtime_agent_id = runtime_agent_id.trim();
    if runtime_agent_id.is_empty() {
        anyhow::bail!("runtime_agent_id must not be empty");
    }
    let thread_id = record
        .codex_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("cutex session has no Codex session id"))?;
    let cwd = cutex_session_launch_cwd(record);
    let base_name = sanitize_session_component(&cutex_session_display_name(record), 48, "agent");
    let cwd_hash = fnv1a_hex(cwd);
    let path_key = cwd_hash[..7].to_string();
    let groups =
        normalize_registered_agent_groups(record.agent_groups.clone(), Some(&path_key), cwd);
    Ok(AgentBusRegisterRequest {
        id: runtime_agent_id.to_string(),
        name: format!("{base_name}.{path_key}"),
        base_name: Some(base_name),
        thread_name: record.thread_name.clone(),
        path_key: Some(path_key),
        session_id: Some(thread_id.to_string()),
        // A live registration describes the launched occurrence, not the
        // current durable/global profile selection. Legacy bindings retain an
        // explicit unknown marker rather than being relabeled.
        profile: binding
            .launched_profile
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-")
            .to_string(),
        cwd: cwd.to_string(),
        pid: binding.pid,
        host_id: Some(current_host_name()),
        groups,
        registration_class: record.registration_class,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::app_server::client::AppServerEvent;
    use cutex::app_server::journal::AppServerSchemaIdentity;
    use cutex::app_server::protocol::RpcNotification;
    use cutex::session::model::CutexAppServerTransport;

    #[test]
    fn stock_generic_recovery_routes_to_the_native_adapter() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".into(),
            None,
            "foreign-host".into(),
            "/missing".into(),
            None,
            "2026-08-07T00:00:00Z".into(),
        )
        .unwrap();
        record.explicit_launch = Some(
            serde_json::from_value(serde_json::json!({
                "version":999,"native_id":"invalid","native_home":"/missing",
                "bundle_manifest":"/missing","bundle_sha256":"a".repeat(64)
            }))
            .unwrap(),
        );
        // No claim and no binding: the native adapter reports nothing to
        // recover instead of refusing the generic entry point.
        assert_eq!(
            recover_persisted_runtime_for_lifecycle(&CodezConfig::default(), &record).unwrap(),
            ManagedRuntimeRecoveryOutcome::NoClaim
        );
        let mut changed = record.clone();
        changed.explicit_launch = None;
        assert!(!runtime_ownership_snapshot_matches(&changed, &record));
    }

    fn event_context() -> AppServerRuntimeEventContext {
        AppServerRuntimeEventContext {
            cutex_session_id: "cutex-1".to_string(),
            thread_id: "root-thread".to_string(),
            runtime_generation: 1,
            runtime_backend: "cute_alden".to_string(),
            launched_profile: None,
            launched_profile_id: None,
            usage: Default::default(),
            schema: AppServerSchemaIdentity {
                version: "test-schema".to_string(),
                sha256: "a".repeat(64),
            },
        }
    }

    fn notification(thread_id: &str) -> AppServerEvent {
        let raw = serde_json::json!({
            "method": "item/completed",
            "params": {
                "threadId": thread_id,
                "item": { "type": "agentMessage", "id": "item-1" }
            }
        });
        AppServerEvent::Notification(RpcNotification {
            method: "item/completed".to_string(),
            params: raw.get("params").cloned(),
            raw,
        })
    }

    fn unique_event_context() -> AppServerRuntimeEventContext {
        AppServerRuntimeEventContext {
            cutex_session_id: format!("cutex-sink-{}", uuid::Uuid::new_v4()),
            ..event_context()
        }
    }

    type Published = Vec<(Option<String>, PublishScope)>;

    fn sink(
        context: &AppServerRuntimeEventContext,
        event: &AppServerEvent,
        published: &mut Published,
        resolves: &mut Vec<String>,
    ) -> RuntimeEventSinkDisposition {
        handle_runtime_native_event(
            context,
            event,
            |_, pending, scope| {
                published.push((pending.correlation.thread_id.clone(), scope));
                Ok(())
            },
            |thread_id| resolves.push(thread_id.to_string()),
        )
        .expect("handle runtime event")
    }

    fn spawn_completed(sender: &str, child: &str) -> AppServerEvent {
        let raw = serde_json::json!({
            "method": "item/completed",
            "params": {
                "threadId": sender,
                "turnId": "turn-1",
                "item": {
                    "type": "collabAgentToolCall", "id": "call-1", "tool": "spawnAgent",
                    "status": "completed", "senderThreadId": sender,
                    "receiverThreadIds": [child], "agentsStates": {}
                }
            }
        });
        AppServerEvent::Notification(RpcNotification {
            method: "item/completed".to_string(),
            params: raw.get("params").cloned(),
            raw,
        })
    }

    #[test]
    fn runtime_sink_buffers_unknown_threads_and_ignores_rejected_ones() {
        let context = unique_event_context();
        let (mut published, mut resolves) = (Vec::new(), Vec::new());

        assert_eq!(
            sink(&context, &notification("root-thread"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::Published
        );
        assert_eq!(
            sink(&context, &notification("foreign-thread"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::BufferedOtherThreadEvent
        );
        assert_eq!(resolves, vec!["foreign-thread".to_string()]);
        assert_eq!(published.len(), 1);
        subagent_lineage::mark_rejected(&context.cutex_session_id, "root-thread", "foreign-thread");
        assert_eq!(
            sink(&context, &notification("foreign-thread"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::IgnoredForeignThreadEvent
        );
        assert_eq!(
            sink(&context, &notification("root-thread"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::Published
        );
        assert_eq!(
            published,
            vec![
                (Some("root-thread".to_string()), PublishScope::BoundThread),
                (Some("root-thread".to_string()), PublishScope::BoundThread),
            ]
        );
    }

    #[test]
    fn runtime_sink_flushes_subagent_events_with_native_thread_id_after_spawn_item() {
        let context = unique_event_context();
        let (mut published, mut resolves) = (Vec::new(), Vec::new());

        // The child starts emitting before the parent's spawn item completes.
        assert_eq!(
            sink(&context, &notification("child"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::BufferedOtherThreadEvent
        );
        assert_eq!(
            sink(&context, &notification("child"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::BufferedOtherThreadEvent
        );
        assert_eq!(resolves.len(), 1, "one lookup per unknown thread");
        assert!(published.is_empty());

        assert_eq!(
            sink(&context, &spawn_completed("root-thread", "child"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::Published
        );
        assert_eq!(
            published,
            vec![
                (Some("root-thread".to_string()), PublishScope::BoundThread),
                (Some("child".to_string()), PublishScope::SubagentThread),
                (Some("child".to_string()), PublishScope::SubagentThread),
            ]
        );
        // Grandchild spawned by the child: learned from the child's own item.
        assert_eq!(
            sink(&context, &spawn_completed("child", "grandchild"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::Published
        );
        assert_eq!(
            sink(&context, &notification("grandchild"), &mut published, &mut resolves),
            RuntimeEventSinkDisposition::Published
        );
        assert_eq!(
            published.last(),
            Some(&(Some("grandchild".to_string()), PublishScope::SubagentThread))
        );
    }

    #[test]
    fn runtime_sink_publishes_verified_subagent_server_requests() {
        let context = unique_event_context();
        let (mut published, mut resolves) = (Vec::new(), Vec::new());
        assert!(subagent_lineage::record_edge(
            &context.cutex_session_id,
            "root-thread",
            "child",
            "root-thread"
        ));
        let raw = serde_json::json!({
            "id": 41,
            "method": "item/commandExecution/requestApproval",
            "params": { "threadId": "child", "turnId": "turn-1", "itemId": "item-1" }
        });
        let request = AppServerEvent::ServerRequest(cutex::app_server::protocol::RpcServerRequest {
            id: serde_json::json!(41),
            method: "item/commandExecution/requestApproval".to_string(),
            params: raw.get("params").cloned(),
            raw,
        });
        assert_eq!(
            sink(&context, &request, &mut published, &mut resolves),
            RuntimeEventSinkDisposition::Published
        );
        assert_eq!(
            published,
            vec![(Some("child".to_string()), PublishScope::SubagentThread)]
        );
        assert!(resolves.is_empty());
    }

    fn runtime_binding(pid: u32) -> CutexAppServerRuntimeBinding {
        CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/app.sock".to_string(),
            pid,
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: Some("profile".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-07-10T00:00:00Z".to_string(),
        }
    }

    fn exact_claimed_record(pid: u32) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            Some("profile".to_string()),
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.app_server_runtime = Some(runtime_binding(pid));
        record.current_runtime_agent_id = Some("runtime-1".to_string());
        record.runtime_pid = Some(pid);
        record.runtime_generation = 7;
        record
    }

    #[test]
    fn exact_live_claim_requires_pid_start_and_endpoint_ownership_evidence() {
        let record = exact_claimed_record(4242);
        let binding = record.app_server_runtime.as_ref().unwrap();
        let process_started_at = chrono::DateTime::parse_from_rfc3339("2026-07-09T23:59:55Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        validate_exact_live_runtime_claim_evidence(&record, binding, process_started_at, true)
            .expect("exact claim evidence");

        let reused_pid_started_at = chrono::DateTime::parse_from_rfc3339("2026-07-10T00:00:01Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(validate_exact_live_runtime_claim_evidence(
            &record,
            binding,
            reused_pid_started_at,
            true
        )
        .is_err());
        assert!(validate_exact_live_runtime_claim_evidence(
            &record,
            binding,
            process_started_at,
            false
        )
        .is_err());
    }

    #[test]
    fn exact_live_claim_rejects_generation_pid_and_partial_claim_mismatch() {
        let mut record = exact_claimed_record(4242);
        let binding = record.app_server_runtime.clone().unwrap();
        let process_started_at = chrono::DateTime::parse_from_rfc3339("2026-07-09T23:59:55Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        record.runtime_generation = 0;
        assert!(validate_exact_live_runtime_claim_evidence(
            &record,
            &binding,
            process_started_at,
            true
        )
        .is_err());
        record.runtime_generation = 7;
        record.runtime_pid = Some(4343);
        assert!(validate_exact_live_runtime_claim_evidence(
            &record,
            &binding,
            process_started_at,
            true
        )
        .is_err());
        record.runtime_pid = Some(4242);
        record.app_server_launch_claim_id = Some("partial-launch".to_string());
        assert!(validate_exact_live_runtime_claim_evidence(
            &record,
            &binding,
            process_started_at,
            true
        )
        .is_err());
    }

    fn exact_claim_for_current_process() -> (
        CutexSessionRecord,
        CutexAppServerRuntimeBinding,
        chrono::DateTime<chrono::Utc>,
    ) {
        let pid = std::process::id();
        let mut record = exact_claimed_record(pid);
        let started_at = process_started_at(pid).expect("current process identity");
        record.app_server_runtime.as_mut().unwrap().started_at = started_at.to_rfc3339();
        let binding = record.app_server_runtime.clone().unwrap();
        (record, binding, started_at)
    }

    #[test]
    fn exact_live_claim_retries_transient_endpoint_ownership_evidence() {
        let (record, binding, process_started_at) = exact_claim_for_current_process();
        let mut observations = 0usize;
        verify_exact_live_runtime_claim_with_probe(
            &record,
            &binding,
            process_started_at,
            2,
            Duration::ZERO,
            || {
                observations += 1;
                Ok(observations != 1)
            },
        )
        .expect("second stable observation satisfies exact ownership");
        assert_eq!(observations, 2);
    }

    #[test]
    fn exact_live_claim_does_not_accept_persistent_non_ownership() {
        let (record, binding, process_started_at) = exact_claim_for_current_process();
        let mut observations = 0usize;
        let error = verify_exact_live_runtime_claim_with_probe(
            &record,
            &binding,
            process_started_at,
            3,
            Duration::ZERO,
            || {
                observations += 1;
                Ok(false)
            },
        )
        .err()
        .expect("persistent absence remains rejected");
        assert_eq!(observations, 3);
        assert!(error.to_string().contains("runtime endpoint"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unix_socket_claim_requires_the_claimed_process_to_own_the_live_endpoint() {
        let root = std::env::temp_dir().join(format!(
            "own-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir(&root).expect("runtime ownership fixture directory");
        let socket = root.join("app.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .expect("bind runtime ownership fixture socket");
        assert!(macos_unix_socket_is_owned_by_process(&socket, std::process::id()).unwrap());
        assert!(!macos_unix_socket_is_owned_by_process(&socket, 1).unwrap());
        let alias = root.join("rendezvous");
        std::os::unix::fs::symlink(&socket, &alias).unwrap();
        assert!(macos_unix_socket_is_owned_by_process(&alias, std::process::id()).unwrap());
        drop(listener);
        assert!(!macos_unix_socket_is_owned_by_process(&socket, std::process::id()).unwrap());
        fs::remove_file(&alias).unwrap();
        fs::remove_file(&socket).expect("remove fixture socket");
        fs::remove_dir(&root).expect("remove runtime ownership fixture directory");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unix_socket_claim_requires_the_claimed_process_to_own_the_live_inode() {
        let root = std::env::temp_dir().join(format!(
            "own-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir(&root).expect("runtime ownership fixture directory");
        let socket = root.join("app.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .expect("bind runtime ownership fixture socket");

        assert!(
            linux_unix_socket_is_owned_by_process(&socket, std::process::id())
                .expect("inspect exact socket owner")
        );
        assert!(linux_unix_socket_is_owned_by_process(&socket, u32::MAX).is_err());

        drop(listener);
        assert!(
            !linux_unix_socket_is_owned_by_process(&socket, std::process::id())
                .expect("closed socket inode is stale")
        );
        fs::remove_file(&socket).expect("remove fixture socket");
        fs::remove_dir(&root).expect("remove runtime ownership fixture directory");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unix_socket_claim_resolves_native_rendezvous_alias() {
        let root = std::env::temp_dir().join(format!(
            "alias-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir(&root).unwrap();
        let socket = root.join("physical.sock");
        let alias = root.join("rendezvous");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::os::unix::fs::symlink("physical.sock", &alias).unwrap();
        assert!(linux_unix_socket_is_owned_by_process(&alias, std::process::id()).unwrap());
        assert!(
            !linux_unix_socket_is_owned_by_process(&alias, std::process::id().saturating_sub(1))
                .unwrap_or(false)
        );
        drop(listener);
        assert!(!linux_unix_socket_is_owned_by_process(&alias, std::process::id()).unwrap());
        fs::remove_file(&socket).unwrap();
        assert!(linux_unix_socket_is_owned_by_process(&alias, std::process::id()).is_err());
        fs::write(&socket, "not a socket").unwrap();
        assert!(!linux_unix_socket_is_owned_by_process(&alias, std::process::id()).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unix_socket_claim_tolerates_connected_rows_for_the_listener_path() {
        let root = std::env::temp_dir().join(format!(
            "listen-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir(&root).expect("runtime listener fixture directory");
        let socket = root.join("app.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .expect("bind runtime listener fixture socket");
        let client_one =
            std::os::unix::net::UnixStream::connect(&socket).expect("connect first client");
        let (server_one, _) = listener.accept().expect("accept first client");
        let client_two =
            std::os::unix::net::UnixStream::connect(&socket).expect("connect second client");
        let (server_two, _) = listener.accept().expect("accept second client");

        let socket_path = socket.to_string_lossy();
        let matching_rows = fs::read_to_string("/proc/net/unix")
            .expect("read Unix socket table")
            .lines()
            .skip(1)
            .filter(|line| line.split_whitespace().nth(7) == Some(socket_path.as_ref()))
            .count();
        assert!(
            matching_rows >= 3,
            "fixture did not expose listener plus connected pathname rows"
        );
        assert!(
            linux_unix_socket_is_owned_by_process(&socket, std::process::id())
                .expect("inspect listener ownership with connected clients")
        );

        drop((client_one, server_one, client_two, server_two, listener));
        fs::remove_file(&socket).expect("remove fixture socket");
        fs::remove_dir(&root).expect("remove runtime listener fixture directory");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unix_socket_claim_uses_peer_credentials_when_listener_rows_are_transient() {
        let root = std::env::temp_dir().join(format!(
            "peer-credential-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir(&root).expect("runtime peer credential fixture directory");
        let socket = root.join("app.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .expect("bind runtime peer credential socket");

        assert!(linux_unix_socket_is_owned_by_process_with_listener_rows(
            &socket,
            std::process::id(),
            |_| Ok(Vec::new()),
        )
        .expect("inspect peer credential ownership"));
        assert!(!linux_unix_socket_is_owned_by_process_with_listener_rows(
            &socket,
            std::process::id().saturating_sub(1),
            |_| Ok(Vec::new()),
        )
        .expect("reject a mismatched peer PID"));

        drop(listener);
        fs::remove_file(&socket).expect("remove fixture socket");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_registration_uses_app_server_pid_and_real_thread_identity() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            Some("profile".to_string()),
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.display_name_hint = Some("Runtime Agent".to_string());
        record.agent_groups = vec!["waveline".to_string()];
        record.registration_class = AgentRegistrationClass::Persistent;
        let binding = runtime_binding(4242);

        let registration =
            runtime_agent_registration(&record, &binding, "runtime-1").expect("registration");
        assert_eq!(registration.id, "runtime-1");
        assert_eq!(registration.pid, 4242);
        assert_eq!(
            registration.session_id.as_deref(),
            Some("018f0000-0000-7000-8000-000000000001")
        );
        assert_eq!(registration.base_name.as_deref(), Some("runtime-agent"));
        assert!(registration.name.starts_with("runtime-agent."));
        assert!(registration.groups.iter().any(|group| group == "waveline"));
        assert!(
            registration
                .groups
                .iter()
                .any(|group| group.starts_with("project:"))
        );
        assert_eq!(
            registration.registration_class,
            AgentRegistrationClass::Persistent
        );
        assert_eq!(registration.profile, "profile");
    }

    #[test]
    fn legacy_runtime_registration_reports_unknown_profile() {
        let record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-08-07T00:00:00Z".to_string(),
        )
        .expect("session record");
        let mut binding = runtime_binding(4242);
        binding.launched_profile = None;

        let registration =
            runtime_agent_registration(&record, &binding, "runtime-1").expect("registration");

        assert_eq!(registration.profile, "-");
    }

    #[test]
    fn running_occurrence_profile_wins_over_durable_inheritance() {
        let record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-08-07T00:00:00Z".to_string(),
        )
        .expect("session record");
        let mut binding = runtime_binding(4242);
        binding.launched_profile = Some("profile-a".to_string());

        let registration =
            runtime_agent_registration(&record, &binding, "runtime-1").expect("registration");
        assert_eq!(registration.profile, "profile-a");
    }

    #[test]
    fn adoption_recovers_live_and_clears_only_fully_dead_local_bindings() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.app_server_runtime = Some(runtime_binding(4242));
        record.current_runtime_agent_id = Some("runtime-1".to_string());
        record.runtime_pid = Some(4242);

        assert_eq!(
            classify_persisted_runtime_adoption(&record, "devbox", |_| true),
            PersistedRuntimeAdoptionAction::Recover(PersistedRuntimeRecoveryAction::Reconnect {
                runtime_agent_id: "runtime-1".to_string(),
            })
        );
        assert_eq!(
            classify_persisted_runtime_adoption(&record, "devbox", |_| false),
            PersistedRuntimeAdoptionAction::Recover(
                PersistedRuntimeRecoveryAction::ClearStaleAndLaunch
            )
        );

        record.alden_pid = Some(5151);
        assert_eq!(
            classify_persisted_runtime_adoption(&record, "devbox", |pid| pid == 5151),
            PersistedRuntimeAdoptionAction::Recover(
                PersistedRuntimeRecoveryAction::CutoverRequired {
                    reason: "stale_binding_has_live_child",
                    pid: Some(5151),
                }
            )
        );

        record.host_id = "winbox".to_string();
        assert_eq!(
            classify_persisted_runtime_adoption(&record, "devbox", |_| {
                panic!("remote PIDs must not be probed locally")
            }),
            PersistedRuntimeAdoptionAction::Remote
        );
    }

    #[test]
    fn retired_runtime_is_skipped_before_host_or_pid_probes() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-retired".to_string(),
            Some("018f0000-0000-7000-8000-000000000099".to_string()),
            "winbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        record.app_server_runtime = Some(runtime_binding(4242));
        record.runtime_pid = Some(4242);

        assert_eq!(
            classify_persisted_runtime_adoption(&record, "devbox", |_| {
                panic!("retired PIDs must not be probed")
            }),
            PersistedRuntimeAdoptionAction::Retired
        );
    }

    #[test]
    fn live_binding_recovery_keeps_the_persisted_runtime_agent_identity() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.app_server_runtime = Some(runtime_binding(4242));
        record.current_runtime_agent_id = Some("runtime-1".to_string());
        record.runtime_pid = Some(4242);
        record.runtime_generation = 7;

        assert_eq!(
            classify_persisted_runtime_recovery(&record, |pid| pid == 4242),
            PersistedRuntimeRecoveryAction::Reconnect {
                runtime_agent_id: "runtime-1".to_string()
            }
        );
        assert_eq!(record.runtime_generation, 7);
        assert_eq!(record.profile, None);
    }

    #[test]
    fn live_binding_without_identity_requires_cutover_instead_of_launch() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.app_server_runtime = Some(runtime_binding(4242));
        record.runtime_generation = 7;

        assert_eq!(
            classify_persisted_runtime_recovery(&record, |pid| pid == 4242),
            PersistedRuntimeRecoveryAction::CutoverRequired {
                reason: "live_binding_missing_runtime_agent_id",
                pid: Some(4242),
            }
        );
    }

    #[test]
    fn dead_binding_is_the_only_persisted_state_allowed_to_clear_and_launch() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.app_server_runtime = Some(runtime_binding(4242));
        record.runtime_pid = Some(4242);
        assert_eq!(
            classify_persisted_runtime_recovery(&record, |_| false),
            PersistedRuntimeRecoveryAction::ClearStaleAndLaunch
        );

        record.app_server_runtime = None;
        record.current_runtime_agent_id = None;
        record.alden_pid = None;
        assert_eq!(
            classify_persisted_runtime_recovery(&record, |_| false),
            PersistedRuntimeRecoveryAction::ClearStaleAndLaunch
        );

        record.runtime_pid = None;
        assert_eq!(
            classify_persisted_runtime_recovery(&record, |_| false),
            PersistedRuntimeRecoveryAction::Launch
        );
    }

    #[test]
    fn missing_binding_with_live_process_requires_cutover() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.runtime_pid = Some(4242);

        assert_eq!(
            classify_persisted_runtime_recovery(&record, |pid| pid == 4242),
            PersistedRuntimeRecoveryAction::CutoverRequired {
                reason: "binding_missing_live_child",
                pid: Some(4242),
            }
        );
    }

    /// Management startup (before it listens): bindings of a previous boot,
    /// reused PIDs and dead legacy PIDs are cleared synchronously; a live
    /// binding of this boot and a live legacy PID are kept for adoption.
    #[cfg(unix)]
    #[test]
    fn startup_clears_dead_and_previous_boot_bindings_before_serving() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("startup-boot-fence").unwrap();
        let host = cutex::platform::host::current_host_name();
        let boot = cutex::platform::process::system_boot_time().unwrap();
        let mut live_child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let mut dead_child = std::process::Command::new("true").spawn().unwrap();
        dead_child.wait().unwrap();
        let me = std::process::id();
        let my_start = cutex::platform::process::process_started_at(me).unwrap();
        let live_start = cutex::platform::process::process_started_at(live_child.id()).unwrap();
        let record = |id: &str, pid: Option<u32>, started_at: Option<String>| {
            let mut record = CutexSessionRecord::new_at(
                id.to_string(),
                None,
                host.clone(),
                "/tmp/project".to_string(),
                None,
                "2026-10-12T00:00:00Z".to_string(),
            )
            .unwrap();
            record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::Persistent;
            record.agent_enabled = true;
            record.runtime_generation = 3;
            if let (Some(pid), Some(started_at)) = (pid, started_at) {
                let dir = cutex::config::paths::runtime_dir().unwrap().join("app-server").join(id);
                std::fs::create_dir_all(&dir).unwrap();
                let mut binding = runtime_binding(pid);
                binding.runtime_dir = dir.display().to_string();
                binding.endpoint = format!("unix://{}", dir.join("app.sock").display());
                binding.diagnostic_journal_path = dir.join("events.jsonl").display().to_string();
                binding.started_at = started_at;
                record.app_server_runtime = Some(binding);
                record.current_runtime_agent_id = Some(format!("runtime-{id}"));
            }
            record
        };
        let mut store = cutex::session::model::CutexSessionStore::default();
        // A previous boot: its PID now belongs to a live, unrelated process.
        let mut old_boot = record("cutex.old-boot", Some(me), Some((boot - chrono::Duration::hours(2)).to_rfc3339()));
        old_boot.runtime_pid = Some(me);
        // This boot, live and matching.
        let live = record("cutex.live", Some(live_child.id()), Some(live_start.to_rfc3339()));
        // This boot, the PID reused by a process created at another time.
        let reused = record("cutex.reused", Some(me), Some((my_start + chrono::Duration::hours(1)).to_rfc3339()));
        // Legacy records without a binding: the plain process check.
        let mut legacy_dead = record("cutex.legacy-dead", None, None);
        legacy_dead.runtime_pid = Some(dead_child.id());
        let mut legacy_live = record("cutex.legacy-live", None, None);
        legacy_live.runtime_pid = Some(live_child.id());
        for record in [old_boot, live, reused, legacy_dead, legacy_live] {
            store.sessions.insert(record.cutex_session_id.clone(), record);
        }
        cutex::session::store::save_cutex_session_store(&store).unwrap();

        let summary = clear_dead_runtimes_at_startup(&host, Duration::from_secs(10)).unwrap();
        assert!(summary.failures.is_empty(), "{:?}", summary.failures);
        assert_eq!(summary.cleared_stale, 3);
        let after = load_cutex_session_store().unwrap();
        let get = |id: &str| after.sessions.get(id).unwrap().clone();
        for id in ["cutex.old-boot", "cutex.reused", "cutex.legacy-dead"] {
            let record = get(id);
            assert!(
                record.app_server_runtime.is_none()
                    && record.runtime_pid.is_none()
                    && record.current_runtime_agent_id.is_none(),
                "{id} must be offline before Management serves"
            );
        }
        assert!(get("cutex.live").app_server_runtime.is_some());
        assert_eq!(get("cutex.legacy-live").runtime_pid, Some(live_child.id()));
        assert!(cutex::platform::process::process_is_running(me));
        let _ = live_child.kill();
        let _ = live_child.wait();
        drop(home);
    }

    #[test]
    fn stale_cleanup_snapshot_rejects_a_concurrent_owner_change() {
        let mut expected = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        expected.app_server_runtime = Some(runtime_binding(4242));
        expected.current_runtime_agent_id = Some("runtime-1".to_string());
        expected.runtime_pid = Some(4242);
        expected.alden_pid = Some(5151);
        expected.runtime_generation = 7;

        assert!(runtime_ownership_snapshot_matches(&expected, &expected));
        assert_eq!(runtime_occurrence_pids(&expected), vec![4242, 5151]);

        let mut concurrent = expected.clone();
        concurrent.runtime_generation = 8;
        concurrent.current_runtime_agent_id = Some("runtime-2".to_string());
        assert!(!runtime_ownership_snapshot_matches(&concurrent, &expected));

        let mut profile_changed = expected.clone();
        profile_changed.profile = Some("profile-b".to_string());
        assert!(!runtime_ownership_snapshot_matches(
            &profile_changed,
            &expected
        ));

        let mut heartbeat_changed = expected.clone();
        heartbeat_changed.pending_launch_id = Some("new-heartbeat-launch".to_string());
        assert!(!runtime_ownership_snapshot_matches(
            &heartbeat_changed,
            &expected
        ));

        let mut launch_claim_changed = expected.clone();
        launch_claim_changed.app_server_launch_claim_id = Some("launch-2".to_string());
        assert!(!runtime_ownership_snapshot_matches(
            &launch_claim_changed,
            &expected
        ));
    }

    #[test]
    fn legacy_pending_launch_id_does_not_block_a_clean_offline_record() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-08-07T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.pending_launch_id = Some("legacy-heartbeat-launch".to_string());

        assert_eq!(
            classify_persisted_runtime_recovery(&record, |_| false),
            PersistedRuntimeRecoveryAction::Launch
        );
    }

    #[test]
    fn app_server_launch_claim_blocks_an_unfenced_generation() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-08-07T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.app_server_launch_claim_id = Some("app-server-launch-1".to_string());

        assert_eq!(
            classify_persisted_runtime_recovery(&record, |_| false),
            PersistedRuntimeRecoveryAction::CutoverRequired {
                reason: "pending_launch_ownership_unknown",
                pid: None,
            }
        );
    }
}
