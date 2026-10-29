//! Management's read-only connection to already running Claude adapters.
//! Recovery reconnects observers; it never spawns an adapter or a model turn.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cutex::app_server::manager::{AppServerRuntimeEventContext, AppServerRuntimeManager};
use cutex::management::v2::model::{AppServerSchema, AppServerSchemaChannel};
use cutex::management::v2::native_events::{
    pending_event_from_app_server, NativeEventContext, NativeEventDisposition,
};
use cutex::management::v2::repository::management_v2_repository;
use cutex::management::v2::claude_observer::{candidate_identities, with_state, RuntimeIdentity};

static MANAGER: OnceLock<AppServerRuntimeManager> = OnceLock::new();
static OBSERVATION_LOCK: Mutex<()> = Mutex::new(());

#[path = "claude_busy_delivery.rs"]
mod busy_delivery;

#[path = "claude_settlement.rs"]
mod settlement;

pub(super) fn lifecycle(id: &str, online: bool) -> anyhow::Result<serde_json::Value> {
    use anyhow::{ensure, Context};
    use serde_json::json;
    let store = cutex::session::store::load_cutex_session_store_shared()?;
    let record = store.sessions.get(id).context("Claude session not found")?;
    ensure!(record.claude_session.is_some(), "not a Claude session");
    ensure!(
        cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &cutex::platform::host::current_host_name()
        ),
        "use SSH for this remote Claude runtime"
    );
    let method = if online {
        "cutex/runtime/online"
    } else {
        "cutex/runtime/close"
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut params =
        json!({"expectedRuntimeGeneration":record.runtime_generation,"reason":"cutex_cli"});
    if online {
        params["openVisibleTerminal"] = json!(false);
    }
    let encoded: String = url::form_urlencoded::byte_serialize(id.as_bytes()).collect();
    let response: serde_json::Value =
        super::management_control_plane::ManagementControlClient::connect()?.request_with_timeout(
            "POST",
            &format!("/v2/sessions/{encoded}/cutex/requests"),
            Some(&serde_json::to_vec(
                &json!({"requestId":request_id,"method":method,"params":params}),
            )?),
            Duration::from_secs(120),
        )?;
    ensure!(
        response["contractVersion"] == 2
            && response["requestId"] == request_id
            && response["cutexSessionId"] == id
            && response["cutex"]["method"] == method,
        "Claude management response identity mismatch; inspect this session before retrying"
    );
    Ok(response)
}

pub(super) fn manager() -> &'static AppServerRuntimeManager {
    MANAGER.get_or_init(|| AppServerRuntimeManager::new(Arc::new(ClaudeEventSink)))
}

/// Claude observers follow the same single journal writer handoff as Codex
/// runtimes: the Agent Bus process observes the adapters it starts, and
/// Management observes them too (`app_server::single_writer`).
struct ClaudeEventSink;

impl cutex::app_server::manager::AppServerRuntimeEventSink for ClaudeEventSink {
    fn handle_live_connection_boundary(&self, context: &AppServerRuntimeEventContext) -> anyhow::Result<()> {
        management_v2_repository()?.invalidate_live(&context.cutex_session_id, context.runtime_generation);
        Ok(())
    }
    fn event_ledger(&self) -> Option<Arc<cutex::app_server::single_writer::Ledger>> {
        cutex::app_server::single_writer::process_ledger()
    }

    fn handle_live_event_from_connection(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
        _origin: &cutex::app_server::client::AppServerHandle,
    ) -> anyhow::Result<()> {
        if let Some((pending, bound)) = cutex::management::v2::native_events::live_text_pending(
            &claude_native_context(context), event,
            |thread| cutex::management::v2::subagent_lineage::is_descendant(
                &context.cutex_session_id, &context.thread_id, thread),
        )? {
            management_v2_repository()?.publish_live(pending.clone());
            if bound { cutex::management::v2::activity::record_live_output_signal(&pending)?; }
        }
        Ok(())
    }

    fn handle_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
    ) -> anyhow::Result<()> {
        handle_claude_event(context, event)
    }
}

fn claude_native_context(context: &AppServerRuntimeEventContext) -> NativeEventContext {
    NativeEventContext {
        cutex_session_id: context.cutex_session_id.clone(),
        thread_id: context.thread_id.clone(),
        host_id: cutex::platform::host::current_host_name(),
        runtime_generation: context.runtime_generation,
        runtime_backend: "claude_subscription".into(),
        schema: AppServerSchema {
            protocol: "claude-app-server-subset".into(), major_version: 1,
            version: context.schema.version.clone(), sha256: context.schema.sha256.clone(),
            channel: AppServerSchemaChannel::Experimental,
            capabilities: serde_json::json!({"persistentRuntime":true,"history":"paginated","sameTurnSteering":false,
                "features":{"fullTextItemTerminal":context.full_text_item_terminal}}),
            extensions: vec!["claude-runtime-v1".into()],
        },
    }
}

fn handle_claude_event(
    context: &AppServerRuntimeEventContext,
    event: &cutex::app_server::client::AppServerEvent,
) -> anyhow::Result<()> {
    if context.full_text_item_terminal && cutex::management::v2::native_events::is_live_text_delta(event) { return Ok(()); }
    if let cutex::app_server::client::AppServerEvent::Notification(notification) = event {
        if notification.method.starts_with("claude/") {
            // Adapter extensions (`claudeExtensions`): only a background
            // subagent / command's end matters here (claude-r23): one "Claude
            // Task" message to the agent, deduplicated by notice id. Never
            // published, journaled into the session or fatal to this observer.
            if let Some(params) = claude_task_notice_params(&context.thread_id, &notification.method, notification.params.as_ref()) {
                if let Err(error) = notify_parent(&context.cutex_session_id, params) {
                    eprintln!("Claude task notice for {} not queued: {error:#}", context.cutex_session_id);
                }
            }
            // Status changes only, as cutex/claudeTask/changed (project log).
            if let ("claude/task/updated", Some(params)) = (notification.method.as_str(), notification.params.as_ref()) {
                if params.get("threadId").and_then(serde_json::Value::as_str) == Some(context.thread_id.as_str()) {
                    if let Err(error) = cutex::management::v2::project_events::record_claude_task(&context.cutex_session_id, params) {
                        eprintln!("Claude task change for {} not recorded: {error:#}", context.cutex_session_id);
                    }
                }
            }
            return Ok(());
        }
    }
    let native = claude_native_context(context);
    // Same bookkeeping order as Codex runtimes: a server request is
    // recorded as pending before it becomes visible on the stream.
    let synchronize_before_publish = matches!(event, cutex::app_server::client::AppServerEvent::ServerRequest(_))
        || matches!(event, cutex::app_server::client::AppServerEvent::Notification(n) if n.method == "serverRequest/resolved");
    if synchronize_before_publish {
        super::app_server_state_sync::handle_runtime_event(context, event)?;
    }
    let disposition = pending_event_from_app_server(&native, event)?;
    if let NativeEventDisposition::OtherThread { thread_id, pending } = disposition {
        // A subagent transcript frame: stream-only, nothing else observes it.
        for frame in subagent_frames(context, &native, event, Some((&thread_id, pending)))? {
            management_v2_repository()?.append(frame)?;
        }
        return Ok(());
    }
    if let NativeEventDisposition::Publish(pending) = disposition {
        let envelope = cutex::management::v2::native_events::append_observed_event(pending)?;
        // A subAgentActivity `started` names a subagent transcript as a verified descendant.
        for frame in subagent_frames(context, &native, event, None)? {
            management_v2_repository()?.append(frame)?;
        }
        if let Some(envelope) = envelope {
        cutex::management::v2::activity::record_activity_event(&envelope)?;
        // Token usage goes into the usage ledger under the session's
        // profile, as for Codex runtimes: GET /v2/usage and the quota
        // of a Claude API account (API usage) read it.
        let profile = context.current_profile();
        if let Err(error) = cutex::management::v2::usage::record_usage_event_for_profile(
            &envelope,
            profile.id.as_deref(),
            profile.name.as_deref(),
        ) {
            eprintln!("Claude usage for {} not recorded: {error:#}", context.cutex_session_id);
        }
        // Claude has no native rollout to derive bootstrap's
        // cutexState.latestTokenUsage from (Codex: launch::native_token_usage):
        // keep the bound thread's newest usage report instead.
        if let Err(error) = cutex::management::v2::claude_token_usage::record_event(&envelope, &context.thread_id) {
            eprintln!("Claude latest token usage for {} not kept: {error:#}", context.cutex_session_id);
        }
        // Likewise cutexState.latestPlan (Codex: launch::native_plan): the
        // bound thread's newest checklist (`turn/plan/updated`).
        if let Err(error) = cutex::management::v2::claude_plan::record_event(&envelope, &context.thread_id) {
            eprintln!("Claude latest plan for {} not kept: {error:#}", context.cutex_session_id);
        }
    }
    }
    if !synchronize_before_publish {
        super::app_server_state_sync::handle_runtime_event(context, event)?;
    }
    // Quota auto-continue first: a usage-limit failure holds the agent
    // (`blocked(quota)`) before the asks observer puts a failed
    // delegation back in the queue; a completed turn cancels.
    super::model_overload_retry::on_runtime_event(&context.cutex_session_id, &context.thread_id, context.runtime_generation, event);
    super::claude_quota_continue::on_runtime_event(&context.cutex_session_id, &context.thread_id, context.runtime_generation, event);
    // Asks next: a turn end releases the ask lease before the owner
    // queue decides whether it may flush (as for Codex runtimes).
    super::asks_runtime::on_runtime_event(&context.cutex_session_id, context.runtime_generation, event);
    if let cutex::app_server::client::AppServerEvent::Notification(notification) = event {
        // The adapter starts turns only with an open input gate: a
        // runtimeGate pause (asks / ask results refused) ends.
        if notification.method == "turn/started" && notification.params.as_ref().and_then(|p| p.get("threadId")).and_then(serde_json::Value::as_str) == Some(context.thread_id.as_str()) {
            cutex::asks::control::runtime_gate_turn_started_default(&context.cutex_session_id);
        }
    }
    if let cutex::app_server::client::AppServerEvent::Notification(notification) = event {
        if notification.method == "turn/completed" && notification.params.as_ref().and_then(|p| p.get("threadId")).and_then(serde_json::Value::as_str) == Some(context.thread_id.as_str()) {
            if let Some(turn) = notification.params.as_ref().and_then(|p| p.pointer("/turn/id")).and_then(serde_json::Value::as_str) {
                cutex::browser_relay::turn_completed(&context.cutex_session_id, &context.thread_id, context.runtime_generation, turn);
            }
        }
    }
    if let cutex::app_server::client::AppServerEvent::Notification(notification) = event {
        // A declined Claude Code safety check (full access) becomes an owner
        // notice; a later block state (granted, used) updates it.
        if notification.method.starts_with("thread/safetyCheck/") {
            if let Some(params) = notification.params.as_ref() {
                cutex::management::v2::safety_checks::on_runtime_notification(
                    &context.cutex_session_id, &context.thread_id, &notification.method, params);
            }
        }
    }
    if let Err(error) = record_agent_message_item(&context.cutex_session_id, context.runtime_generation, event) {
        eprintln!("Claude agent message receipt for {} not recorded: {error:#}", context.cutex_session_id);
    }
    // Flush the Management input queue when the Claude turn ends. A
    // failed item is reported (`cutex/userInput/failed`) and stays
    // queued; it must not drop this observer connection.
    if let Err(error) = super::app_server_user_input::handle_runtime_event_for_generation(&context.cutex_session_id, context.runtime_generation, event) {
        eprintln!("Claude queued input for {} was not flushed: {error:#}", context.cutex_session_id);
    }
    Ok(())
}

/// The params of a bound-thread `claude/task/updated` that carries `notifyParent`.
fn claude_task_notice_params<'a>(
    thread_id: &str,
    method: &str,
    params: Option<&'a serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    let params = params.filter(|_| method == "claude/task/updated")?;
    (params.get("threadId").and_then(serde_json::Value::as_str) == Some(thread_id)
        && params.pointer("/task/notifyParent").is_some_and(serde_json::Value::is_object))
        .then_some(params)
}

/// `notifyParent` of a `claude/task/updated` (adapter `input.backgroundSubagents`)
/// becomes one Agent Bus message from "Claude Task" to the same agent. The bus
/// keeps its stable `ctask-` id once (a repeated notice is deduplicated there).
fn notify_parent(cutex_session_id: &str, params: &serde_json::Value) -> anyhow::Result<()> {
    let Some(notice) = cutex::agent_bus::claude_task::ClaudeTaskNotice::from_task_updated(params) else {
        return Ok(());
    };
    let request = cutex::agent_bus::claude_task::send_request(cutex_session_id, &notice)?;
    let config = cutex::config::store::load_codez_config();
    cutex::agent_bus::client::send_claude_task_notice(&config, &request)?;
    Ok(())
}

/// Capability `claude-subagent-transcripts`: a foreground subagent's
/// transcript frames (`params.threadId` = `claude:<session>:task:<id>`, adapter
/// `features.subagentTranscripts`) are published stream-only, with the native
/// thread id intact, once that thread is a verified descendant of the bound
/// thread (taught by the parent's `subAgentActivity` `started` item, or by a
/// `thread/read` lineage check). They never touch the session record,
/// activity, usage, asks or the queue. Unverified frames wait briefly
/// (bounded, `subagent_lineage`); unrelated ones are dropped.
///
/// `other` is an event for another thread; None = a bound-thread event that
/// may teach an edge. Returns the subagent frames to publish, in order.
fn subagent_frames(
    context: &AppServerRuntimeEventContext,
    native: &NativeEventContext,
    event: &cutex::app_server::client::AppServerEvent,
    other: Option<(&str, cutex::management::v2::model::PendingEvent)>,
) -> anyhow::Result<Vec<cutex::management::v2::model::PendingEvent>> {
    use cutex::app_server::client::AppServerEvent;
    use cutex::management::v2::subagent_lineage::{self, ForeignThreadRoute};
    let (session, root) = (context.cutex_session_id.as_str(), context.thread_id.as_str());
    let mut frames = Vec::new();
    match (other, event) {
        (Some((thread_id, pending)), AppServerEvent::Notification(_)) => {
            if subagent_lineage::route_foreign_event(session, root, thread_id, event) == ForeignThreadRoute::Descendant {
                frames.push(pending);
            }
        }
        (None, AppServerEvent::Notification(notification)) => {
            subagent_lineage::learn_from_notification(session, root, &notification.raw);
        }
        _ => {}
    }
    loop {
        let (ready, _) = subagent_lineage::take_ready_events(session, root);
        if ready.is_empty() {
            break;
        }
        for buffered in &ready {
            if let NativeEventDisposition::OtherThread { pending, .. } = pending_event_from_app_server(native, buffered)? {
                frames.push(pending);
            }
        }
    }
    Ok(frames)
}

pub(super) fn observe(id: &str) -> anyhow::Result<()> {
    // Explicit starts bypass an old occurrence's backoff and update diagnostics too.
    let identity = cutex::session::store::with_cutex_session_store(|store| {
        store.sessions.get(id).and_then(|record| Some(RuntimeIdentity {
            generation: record.runtime_generation,
            pid: record.runtime_pid?,
            connection_id: record.claude_session.as_ref()?.connection_id.clone(),
        }))
    })?;
    if let Some(identity) = &identity {
        with_state(|state| { state.activate(id, identity.clone(), Instant::now()); state.attempted(id); });
    }
    let result = observe_inner(id);
    record_observation_result(id, identity.as_ref(), "observe", &result);
    result
}

fn record_observation_result(id: &str, identity: Option<&RuntimeIdentity>, stage: &str, result: &anyhow::Result<()>) {
    let Some(identity) = identity else { return };
    with_state(|state| match result {
        Ok(()) => { if state.complete(id, identity, None, Instant::now()) { eprintln!("Claude observation recovered for {id}"); } }
        Err(error) => {
            let detail = format!("{error:#}");
            let stage = if detail.starts_with("Claude bus registration pending:") { "registration" } else { stage };
            let changed = state.complete(id, identity, Some((stage, &detail)), Instant::now());
            if error.downcast_ref::<cutex::runtime::claude::ClaudeObservationOffline>().is_some() {
                if state.suspend(id, identity, Instant::now()) {
                    eprintln!("Claude observer inactive for {id}: {detail}");
                }
            } else if changed {
                eprintln!("Claude observation unavailable for {id}: {detail}");
            }
        }
    });
}

fn observe_inner(id: &str) -> anyhow::Result<()> {
    let _guard = OBSERVATION_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Claude observer lock poisoned"))?;
    let observation = match cutex::runtime::claude::observation(id) {
        Ok(observation) => observation,
        Err(error) => {
            manager().disconnect(id)?;
            return Err(error);
        }
    };
    let connected = manager().status(id)?.is_some_and(|s| {
        s.connected && s.runtime_generation == observation.record.runtime_generation
    });
    // A failed initial bus registration leaves the native stream connected.
    // Retry only the missing bridge; an existing bridge retries its heartbeat.
    if connected && manager().agent_bus_status(id)?.is_some_and(|bridge| bridge.running) {
        return Ok(());
    }
    let record = observation.record.clone();
    if !connected {
        manager().disconnect(id)?;
        manager().connect_adapter(observation)?;
    }
    cutex::runtime::claude::refresh_mcp_binding(id)?;
    let name = cutex::session::service::cutex_session_display_name(&record);
    let thread = record.app_server_thread_id().unwrap().to_string();
    let registration = cutex::agent_bus::model::AgentBusRegisterRequest {
        id: record
            .current_runtime_agent_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Claude runtime registration identity missing"))?,
        name: name.clone(),
        base_name: Some(name),
        thread_name: record.thread_name.clone(),
        path_key: None,
        session_id: Some(thread.clone()),
        profile: record.profile.clone().unwrap_or_else(||"Claude".into()),
        cwd: record.cwd.clone(),
        pid: record.runtime_pid.unwrap(),
        host_id: Some(record.host_id.clone()),
        groups: record.agent_groups.clone(),
        registration_class: record.registration_class,
    };
    let mut options =
        cutex::app_server::bus_bridge::AppServerAgentBusBridgeOptions::new(registration, thread.clone())
            .with_cutex_session_id(id);
    // Registration and heartbeat only until Claude's different ingress receipt
    // semantics are mapped explicitly. Never invoke Codex's insertion methods.
    options.registration_only = true;
    manager().ensure_agent_bus_bridge(
        id,
        Arc::new(cutex::agent_bus::client::AgentBusHttpClient::from_config(
            &cutex::config::store::load_codez_config(),
        )),
        options,
    ).map_err(|error| anyhow::anyhow!("Claude bus registration pending: {error:#}"))?;
    // Safety blocks the adapter reported while nobody observed it.
    cutex::management::v2::safety_checks::reconcile(id, &thread);
    // Like a Codex connect: input queued while no observer was connected
    // (offline submit, daemon restart) goes out once the thread is idle.
    if let Err(error) = super::app_server_user_input::flush_queued_if_idle(id) {
        eprintln!("Claude queued input for {id} was not flushed: {error:#}");
    }
    Ok(())
}

// Rebuildable settlement sidecar failures must not abort ordinary delivery.
// Close remains independently fail-closed; this never retries or fabricates facts.
fn persist_settlement_observation(
    id: &str,
    message_id: &str,
    persist: impl FnOnce() -> anyhow::Result<bool>,
) {
    if let Err(error) = persist() {
        eprintln!("Claude settlement persistence for {id}/{message_id}: {error:#}");
    }
}

/// Rebuildable evidence only. Called by the bounded post-delivery phase, never
/// from ordinary delivery/poll or while holding a session input/store guard.
fn observe_settlement_message(id: &str, message_id: &str, deadline: Instant) -> anyhow::Result<()> {
    let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
    observe_settlement_message_with(repository, id, message_id, deadline, settlement::collect_background)
}

fn observe_settlement_message_with(
    repository: &cutex::management::v2::agent_bus_state::AgentBusMessageRepository,
    id: &str, message_id: &str, deadline: Instant,
    collect: impl FnOnce(&cutex::management::v2::agent_bus_state::AgentBusMessageSnapshot, Instant)
        -> anyhow::Result<cutex::management::v2::agent_bus_settlement_model::Observation>,
) -> anyhow::Result<()> {
    let Some(snapshot) = repository.snapshot_by_message_id(message_id)? else {return Ok(());};
    anyhow::ensure!(snapshot.to_cutex_session_id == id, "settlement recipient changed");
    let evidence = match collect(&snapshot, deadline) {
        Ok(cutex::management::v2::agent_bus_settlement_model::Observation::Terminal(evidence)) => Some(evidence),
        Ok(cutex::management::v2::agent_bus_settlement_model::Observation::Ineligible) => None,
        Err(error) => {
            eprintln!("Claude settlement observation for {id}/{message_id}: {error:#}");
            None // Unavailable observations never create close eligibility.
        }
    };
    persist_settlement_observation(id, message_id, || {
        repository.observe_settlement(message_id, evidence, chrono::Utc::now())
    });
    Ok(())
}

fn reconciliation_due(
    attempts: &mut HashMap<String, (u64, Instant)>,
    id: &str,
    generation: u64,
    now: Instant,
) -> bool {
    if attempts.get(id).is_some_and(|(previous, at)| {
        *previous == generation && now.duration_since(*at) < Duration::from_secs(60)
    }) {
        return false;
    }
    attempts.insert(id.to_string(), (generation, now));
    true
}

fn deliver_messages(
    id: &str,
    bus: &cutex::agent_bus::client::AgentBusHttpClient,
    attempts: &mut HashMap<String, (u64, Instant)>,
) -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    use cutex::agent_bus::delivery::AgentDeliveryMode;
    // The shared snapshot at the latest revision (no whole-store copy per
    // online Claude runtime per pass). It is read after `observe`, so the
    // record's runtime generation is the one every handle below is checked
    // against (`handle_for_generation`).
    let sessions = cutex::session::store::load_cutex_session_store_shared()?;
    let record = sessions.sessions.get(id).context("Claude session missing")?;
    let runtime = record.current_runtime_agent_id.as_deref().context("Claude offline")?;
    let thread = record.app_server_thread_id().context("Claude thread missing")?;
    let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let capabilities = super::asks_claude::adapter_capabilities(id, record.runtime_generation);
    let v2 = capabilities.as_ref().is_some_and(super::asks_claude::offers_agent_messages_v2);
    let native = capabilities.as_ref().is_some_and(super::claude_ingress::offers_native);
    if native && reconciliation_due(attempts, id, record.runtime_generation, Instant::now()) {
        // Notifications can be lost during observer restarts. Reconcile only;
        // neither unknown status nor transport failure schedules a replay.
        for envelope in repository.pending_native_envelopes(id)? {
            if let Err(error) = reconcile_native_message(id, record.runtime_generation, &envelope) {
                eprintln!("Claude receipt recovery for {id}/{}: {error:#}", envelope.message.id);
            }
        }
    }
    for message in bus.poll(runtime)? {
        // Service controls need their own typed target/seat checks and receipts.
        // A cross-host tell is an ordinary passive note with a peer control record.
        // An Agent Management start / notice is validated against its action
        // (the Codex bridge's provenance check) when its input is built.
        let management = cutex::app_server::bus_bridge::is_agent_management_message(&message);
        if !(message.sender_kind.is_agent() || message.sender_kind.is_alarm_system())
            || (message.control_type.is_some() && !cutex::agent_bus::peer_tell::is_peer_tell(&message)
                && !cutex::agent_bus::claude_task::is_claude_task(&message) && !management)
            || message.kind != cutex::agent_bus::model::AgentBusEnvelopeKind::Message {
            continue;
        }
        ensure!(message.to_cutex_session_id.as_deref() == Some(id), "Claude message target mismatch");
        ensure!(message.delivery_mode != AgentDeliveryMode::Interrupt, "unsupported Claude delivery mode");
        // Already handled (e.g. the ack was lost): never submit twice.
        if claude_message_is_settled(repository, id, &message)? {
            bus.ack(runtime, &[message.id])?;
            continue;
        }
        // Only the native ingress takes it (no legacy local-inbox receipt).
        if management && !native { continue; }
        let meta = repository.message_meta(&message.id)?;
        let payload = match claude_message_params(&sessions, thread, id, &message, meta.as_ref(), v2) {
            Ok(payload) => payload,
            Err(error) if management => {
                // Bad Management provenance is permanent: quarantined (the
                // requester gets its notice on the ACK), as on Codex.
                if !cutex::app_server::bus_bridge::quarantine_terminal_external_input(id, &message, &error)? {
                    return Err(error);
                }
                bus.ack(runtime, &[message.id.clone()])?;
                eprintln!("warning: Claude Agent Management message {} quarantined: {error:#}", message.id);
                continue;
            }
            Err(error) if error.is::<cutex::agent_bus::alarms::AlarmAwaitingQueueCommit>() => continue,
            Err(error) if message.sender_kind.is_alarm_system()
                && error.is::<cutex::agent_bus::alarms::InvalidAlarmProvenance>() => {
                // Bad system provenance is permanent. Keep a durable failure
                // or audit fact before withdrawing a legacy live-only item.
                if !repository.record_quarantined(id, &message.id,
                    serde_json::json!({"code":"alarm_provenance_invalid","message":error.to_string(),"retryable":false}), chrono::Utc::now())? {
                    cutex::agent_bus::audit::append_agent_bus_audit_record(serde_json::json!({
                        "event":"live_message_terminal", "timestamp":chrono::Utc::now().to_rfc3339(),
                        "message_id":message.id, "to_cutex_session_id":id,
                        "durable":false, "error":{"code":"alarm_provenance_invalid","message":error.to_string()}
                    }))?;
                }
                bus.ack(runtime, &[message.id.clone()])?;
                eprintln!("warning: Claude alarm {} terminated: {error:#}", message.id);
                continue;
            }
            Err(error) => return Err(error),
        };
        let handle = manager().handle_for_generation(id, record.runtime_generation)?;
        let frozen = repository.snapshot_by_message_id(&message.id)?.and_then(|snapshot| snapshot.external_input);
        // Probe accepted r7 ids before selecting the native route. Migration
        // must never reinterpret or resubmit an existing local inbox identity.
        let existing = if native && frozen.is_none() {
            super::claude_ingress::legacy_receipt(handle.request("claude/input/status",
                serde_json::json!({"threadId": thread, "messageId": message.id})))?
        } else { None };
        let input_lock = cutex::management::v2::user_input::session_input_lock(id)?;
        let _input_guard = input_lock.lock().map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
        let active = manager().status(id)?.filter(|status| status.connected && status.runtime_generation == record.runtime_generation);
        let busy_turn = active.as_ref().and_then(|status| status.active_turn_id.as_deref());
        let wait = claude_task_wait(&message, cutex::agent_bus::delivery::busy_wait_for(&message));
        busy_delivery::note(&message, wait, busy_turn);
        let age = Duration::from_secs((chrono::Utc::now().timestamp().max(0) as u64).saturating_sub(message.created_at_epoch_secs));
        let timing = busy_delivery::timing(age, wait, active.as_ref().map(|status| status.active_turn_id.is_some()),
            frozen.is_none() && existing.is_none()
                && busy_delivery::candidate(&message, capabilities.as_ref().is_some_and(super::claude_ingress::offers_soon)));
        if timing == cutex::agent_bus::delivery::WakeTiming::Hold { continue; }
        let mut already_accepted = existing.is_some();
        if existing.is_none() && message.delivery_mode.trigger_turn() {
            let accepted = match frozen.as_ref() {
                Some(envelope) => super::claude_ingress::status(&handle, envelope, record.runtime_generation)?
                    .delivery_state != cutex::app_server::external_input::DeliveryState::Unknown,
                None => false,
            };
            already_accepted = accepted;
            let busy = busy_turn.is_some();
            let joins_running = busy && (timing == cutex::agent_bus::delivery::WakeTiming::NextStep
                || frozen.as_ref().is_some_and(|envelope|
                    envelope.message.delivery == cutex::app_server::external_input::Delivery::Soon)
                || message.delivery_mode == AgentDeliveryMode::Soon);
            if !accepted && !joins_running && (busy
                || !cutex::management::v2::admission::delivery_may_start(id, &message.id)?) { continue; }
        }
        if !already_accepted && cutex::asks::pause_admission::is_held(
            &cutex::asks::store::AskStore::open_default()?, id,
        )? { continue; }
        let prepared_turn = (busy_turn.is_none() && !already_accepted && message.delivery_mode.trigger_turn())
            .then(|| cutex::management::v2::admission::prepare_turn(id, "delivery", None));
        let mut receipt = if let Some(existing) = existing {
            existing
        } else if native || frozen.is_some() {
            ensure!(native, "native Claude input was frozen but this runtime lacks its protocol; no legacy fallback");
            let frozen_input = repository.freeze_external_input(id, &message.id, |canonical| {
                let mut projection = claude_message_params(&sessions, thread, id, canonical, meta.as_ref(), true)?;
                if timing == cutex::agent_bus::delivery::WakeTiming::NextStep {
                    // Of the system alarms only approval notices are read early (023).
                    ensure!(!canonical.sender_kind.is_alarm_system()
                        || cutex::agent_bus::delivery::is_approval_notice(canonical),
                        "an alarm other than an approval notice is never read at the next step");
                    projection["metadata"]["deliveryMode"] = serde_json::json!("soon");
                }
                super::claude_ingress::envelope(id, thread, record.runtime_generation, &projection)
            });
            let envelope = match frozen_input {
                Ok(envelope) => envelope,
                // The durable canonical record failed the provenance check.
                Err(error) if management
                    && cutex::app_server::bus_bridge::quarantine_terminal_external_input(id, &message, &error)? => {
                    bus.ack(runtime, &[message.id.clone()])?;
                    eprintln!("warning: Claude Agent Management message {} quarantined: {error:#}", message.id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            ensure!(envelope.owner_id == id && envelope.thread_id == thread, "frozen Claude input binding changed");
            super::claude_ingress::submit(&handle, &envelope, record.runtime_generation)?
        } else { match handle.request("claude/input/submit", payload) {
            Ok(receipt) => receipt,
            // The id is known with other content (a retry after an upgrade
            // changed the projection): its first acceptance stands.
            Err(cutex::app_server::client::AppServerClientError::Rpc(error)) if error.code == -32010 => handle
                .request("claude/input/status", serde_json::json!({"threadId": thread, "messageId": message.id}))?,
            Err(error) => return Err(error.into()),
        }};
        ensure!(receipt["threadId"] == thread, "Claude inbox returned wrong thread");
        busy_delivery::decorate(&message.id, &mut receipt);
        manager().handle_for_generation(id, record.runtime_generation)?;
        repository.record_local_inbox(id, &message.id, &receipt)?;
        if let Some(prepared) = prepared_turn {
            prepared.submitted();
            repository.record_admission_started_idle(id, &message.id)?;
        }
        bus.ack(runtime, &[message.id])?;
    }
    Ok(())
}

fn claude_message_is_settled(
    repository: &cutex::management::v2::agent_bus_state::AgentBusMessageRepository,
    owner: &str,
    message: &cutex::agent_bus::model::AgentBusMessage,
) -> anyhow::Result<bool> {
    match repository.message_state(&message.id)? {
        Some((state, receipt)) => Ok(state != "pending" || receipt),
        None if message.sender_kind.is_alarm_system() => {
            // Historical live-only alarms have no canonical identity to freeze
            // or inbox receipt to commit. Do not submit them on every poll.
            cutex::agent_bus::audit::append_agent_bus_audit_record(serde_json::json!({
                "event":"live_message_terminal", "timestamp":chrono::Utc::now().to_rfc3339(),
                "message_id":message.id, "to_cutex_session_id":owner, "durable":false,
                "error":{"code":"canonical_message_absent","message":"alarm has no durable canonical record"}
            }))?;
            eprintln!("warning: terminating Claude live-only alarm {} without a durable record", message.id);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// `claude/input/submit` params for an agent message. claude-r5 adapters get
/// passive notes and the `cutex.agent-message.v1` view; older adapters get
/// every message as a turn-starting input (as before r208).
fn claude_message_params(
    sessions: &cutex::session::model::CutexSessionStore,
    thread: &str,
    id: &str,
    message: &cutex::agent_bus::model::AgentBusMessage,
    meta: Option<&cutex::management::v2::agent_bus_state::AgentBusMessageMeta>,
    v2: bool,
) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;
    use cutex::agent_bus::delivery::AgentDeliveryMode;
    if message.sender_kind.is_alarm_system() {
        let projection = cutex::app_server::bus_bridge::alarm_inter_agent_params(thread, id, message)?;
        cutex::agent_bus::alarms::authenticate_queued_message(
            message.external_message_id.as_deref().context("alarm identity missing")?, id, &message.content,
        )?;
        let mut payload = serde_json::json!({
            "threadId":thread, "messageId":message.id,
            "source":{"kind":"service","id":cutex::agent_bus::alarms::ALARM_SYSTEM_SENDER},
            "eventType":"alarm", "text":projection.content,
            "metadata":{"sourceName":cutex::agent_bus::alarms::ALARM_SYSTEM_SENDER,
                "cutexSessionId":id, "deliveryMode":"after-turn"}
        });
        if v2 {
            payload["view"] = serde_json::json!({"schema":cutex::agent_bus::alarms::ALARM_VIEW_SCHEMA,
                "data":{"sourceName":cutex::agent_bus::alarms::ALARM_SYSTEM_SENDER,"messageId":message.id}});
        }
        return Ok(payload);
    }
    if cutex::app_server::bus_bridge::is_agent_management_message(message) {
        // Agent Management start (a new agent's first instructions) or notice
        // (to the requester): a service source, no view (a v1 envelope), the
        // Codex envelope's text, validated against the current roster.
        let (event_type, text) = cutex::app_server::bus_bridge::agent_management_input(message, id)?;
        return Ok(management_input_params(thread, id, message, event_type, text));
    }
    if cutex::agent_bus::claude_task::is_claude_task(message) {
        // A background task of this agent ended: a service source, a wake.
        let (notice, text) = cutex::agent_bus::claude_task::model_text(message)?;
        let mut payload = serde_json::json!({"threadId":thread,"messageId":message.id,
            "source":{"kind":"service","id":cutex::agent_bus::claude_task::SOURCE_ID},
            "eventType":cutex::agent_bus::claude_task::EVENT_TYPE,"text":text,
            "metadata":{"sourceName":cutex::agent_bus::claude_task::SOURCE_ID,"cutexSessionId":id,
                "deliveryMode":"after-turn"}});
        if v2 {
            payload["view"] = serde_json::to_value(cutex::agent_bus::claude_task::view(message, &notice)?)?;
        }
        return Ok(payload);
    }
    if cutex::agent_bus::peer_tell::is_peer_tell(message) {
        // Cross-host tell: a service source naming the remote agent.
        let (facts, text) = cutex::agent_bus::peer_tell::model_text(message)?;
        let passive = v2 && message.delivery_mode == AgentDeliveryMode::Passive;
        let mut payload = serde_json::json!({"threadId":thread,"messageId":message.id,
            "source":{"kind":"service","id":cutex::agent_bus::peer_tell::SOURCE_ID},
            "eventType":cutex::agent_bus::peer_tell::EVENT_TYPE,"text":text,
            "metadata":{"sourceName":facts.sender_label(),"cutexSessionId":id,
                "deliveryMode": if passive { "passive" } else { "after-turn" }}});
        if v2 {
            payload["view"] = serde_json::to_value(cutex::agent_bus::peer_tell::view(message, &facts)?)?;
        }
        return Ok(payload);
    }
    let source = message.from_cutex_session_id.as_deref().context("message source missing")?;
    let fallback = sessions.sessions.get(source).and_then(|r| r.formal_agent_name.clone());
    let projection =
        cutex::agent_bus::projection::agent_message_projection(message, meta, fallback.as_deref())?;
    let passive = v2 && message.delivery_mode == AgentDeliveryMode::Passive;
    let source_name = projection.view.data["senderName"].as_str().unwrap_or(&message.from).to_string();
    let mut payload = serde_json::json!({"threadId":thread,"messageId":message.id,
        "source":{"kind":"agent","id":source},"eventType":"agent_message","text":projection.text,
        "metadata":{"sourceName":source_name,"cutexSessionId":id,
            "deliveryMode": if passive { "passive" } else { "after-turn" }}});
    if v2 {
        payload["view"] = serde_json::to_value(&projection.view)?;
    }
    Ok(payload)
}

fn management_input_params(
    thread: &str,
    id: &str,
    message: &cutex::agent_bus::model::AgentBusMessage,
    event_type: &str,
    text: String,
) -> serde_json::Value {
    use cutex::agent_bus::delivery::AgentDeliveryMode;
    let mode = match message.delivery_mode {
        AgentDeliveryMode::Passive => "passive",
        AgentDeliveryMode::Soon => "soon",
        _ => "after-turn",
    };
    serde_json::json!({"threadId":thread,"messageId":message.id,
        "source":{"kind":"service","id":cutex::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER},
        "eventType":event_type,"text":text,
        "metadata":{"sourceName":"Cutex Agent Management","cutexSessionId":id,"deliveryMode":mode}})
}

#[cfg(test)]
mod director_service_tests {
    #[test]
    fn offline_director_notice_renders_as_a_passive_service_without_agent_source(){
        let message:cutex::agent_bus::model::AgentBusMessage=serde_json::from_value(serde_json::json!({
            "id":"notice-1","from":cutex::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER,"to":"runtime-member",
            "toCutexSessionId":"cutex.22222222-2222-4222-8222-222222222222","content":"The Director changed",
            "deliveryMode":"passive","triggerTurn":false,"createdAtEpochSecs":1,
            "controlType":cutex::director_address::management::DIRECTOR_CHANGE_NOTICE_CONTROL_TYPE
        })).unwrap();
        assert!(message.from_cutex_session_id.is_none());
        let value=super::management_input_params("thread","cutex.22222222-2222-4222-8222-222222222222",&message,"management_notice",message.content.clone());
        assert_eq!(value["source"]["kind"],"service");
        assert_eq!(value["source"]["id"],cutex::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER);
        assert_eq!(value["metadata"]["deliveryMode"],"passive");
        assert_eq!(value["eventType"],"management_notice");
    }
}

/// A Claude task notice is meant for the agent's next step: no busy wait
/// (`soon` at once when the parent is busy, a new turn when it is idle).
fn claude_task_wait(
    message: &cutex::agent_bus::model::AgentBusMessage,
    wait: Option<Duration>,
) -> Option<Duration> {
    if cutex::agent_bus::claude_task::is_claude_task(message) {
        Some(Duration::ZERO)
    } else {
        wait
    }
}

/// Clear only unclosed sidecar observations. This path must work while the
/// runtime is offline: repository reads/writes only, never a native request.
fn invalidate_settlement_observations(id: &str) {
    let result = (|| -> anyhow::Result<()> {
        let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
        let now = chrono::Utc::now();
        for envelope in repository.pending_native_envelopes(id)? {
            if let Err(error) = repository.observe_settlement(&envelope.message.id, None, now) {
                // Try the remaining rows even if one sidecar write fails.
                eprintln!("Claude settlement invalidation for {id}/{}: {error:#}", envelope.message.id);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("Claude settlement invalidation for {id}: {error:#}");
    }
}

pub(super) fn disconnect(id: &str) -> anyhow::Result<()> {
    invalidate_settlement_observations(id);
    let _guard = OBSERVATION_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Claude observer lock poisoned"))?;
    manager().disconnect(id)?;
    Ok(())
}

pub(super) fn spawn_observer() -> anyhow::Result<()> {
    cutex::management::v2::agent_bus_settlement_model::register_collector(settlement::collect);
    cutex::management::v2::agent_bus_settlement_model::register_runtime_fence(settlement::runtime_fence);
    std::thread::Builder::new()
        .name("cutex-claude-observer".into())
        .spawn(|| {
            let mut store_failure = None;
            let mut reconciliation_attempts = HashMap::<String, (u64, Instant)>::new();
            let mut previous_candidates = HashMap::<String, RuntimeIdentity>::new();
            let mut outbox_attempt = None::<Instant>;
            let mut settlement_observations = settlement::BackgroundObservations::default();
            loop {
                // Independent of candidate discovery/connection success: a
                // fresh process and a fully offline host still repair outbox.
                if outbox_attempt.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
                    outbox_attempt = Some(Instant::now());
                    if let Err(error) = cutex::management::v2::agent_bus_state::agent_bus_message_repository()
                        .and_then(|repository| repository.flush_settlement_outbox()) {
                        eprintln!("Claude settlement outbox recovery: {error:#}");
                    }
                }
                // Borrow the cached store (no whole-store copy every 5 s).
                let host = cutex::platform::host::current_host_name();
                let result = cutex::session::store::with_cutex_session_store(|store| {
                    candidate_identities(store.sessions.values(), &host)
                });
                match result {
                    Ok(mut wanted) => {
                        store_failure = None;
                        // Probe the whole snapshot once. A PID claim alone is not a live runtime.
                        let pids = wanted.values().map(|identity| identity.pid).collect::<Vec<_>>();
                        let live = cutex::platform::process::running_process_ids(&pids);
                        wanted.retain(|_, identity| live.contains(&identity.pid));
                        // sync_candidates reactivates changed occurrences but
                        // returns only removed IDs. Invalidate the old evidence
                        // before observing a replacement generation/connection.
                        for (id, identity) in &wanted {
                            if previous_candidates.get(id).is_some_and(|previous| previous != identity) {
                                invalidate_settlement_observations(id);
                                reconciliation_attempts.remove(id);
                            }
                        }
                        previous_candidates = wanted.iter().map(|(id, identity)| (id.clone(), identity.clone())).collect();
                        let now = Instant::now();
                        let removed = with_state(|state| state.sync_candidates(&wanted, now));
                        let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(
                            &cutex::config::store::load_codez_config(),
                        );
                        for removed in removed {
                            let _ = disconnect(&removed);
                            reconciliation_attempts.remove(&removed);
                            eprintln!("Claude observer inactive for {removed}: no eligible local live runtime");
                        }
                        settlement_observations.retain_candidates(&wanted.keys().cloned().collect::<Vec<_>>());
                        settlement_observations.after_deliveries(settlement::background_enabled(), || {
                        let mut delivered = Vec::new();
                        for id in &wanted.keys().cloned().collect::<Vec<_>>() {
                            if !with_state(|state| state.due(id, Instant::now())) { continue; }
                            with_state(|state| state.attempted(id));
                            let observation = observe_inner(id);
                            let stage = if observation.is_err() { "observe" } else { "delivery" };
                            let result = observation.and_then(|()| deliver_messages(id, &bus, &mut reconciliation_attempts));
                            match result {
                                Ok(()) => {
                                    record_observation_result(id, wanted.get(id), stage, &Ok(()));
                                    delivered.push(id.clone());
                                },
                                Err(error) => {
                                    // Includes an offline occurrence discovered
                                    // after the candidate scan, and failed
                                    // connection checks while the PID survives.
                                    invalidate_settlement_observations(id);
                                    let offline = error.downcast_ref::<cutex::runtime::claude::ClaudeObservationOffline>().is_some();
                                    record_observation_result(id, wanted.get(id), stage, &Err(error));
                                    if offline { reconciliation_attempts.remove(id); continue; }
                                    // A dead adapter (PID gone, endpoint
                                    // refused) is offline: clear its stale
                                    // claim; the next pass stops observing it.
                                    match cutex::runtime::claude::reconcile_dead_adapter(id) {
                                        Ok(true) => {
                                            eprintln!("Claude adapter of {id} is gone; cleared its stale runtime claim");
                                            let _ = disconnect(id);
                                            // Drop immediately, including a lifecycle change
                                            // after the candidate scan. Start re-adds it.
                                            wanted.remove(id);
                                            reconciliation_attempts.remove(id);
                                            with_state(|state| { state.sync_candidates(&wanted, Instant::now()); });
                                            continue;
                                        }
                                        Ok(false) => {}
                                        Err(_) => {} // Preserve the original failure and its retry state.
                                    }
                                }
                            }
                        }
                        delivered
                        }, |id| {
                            cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
                                .pending_native_envelopes(id)
                                .map(|envelopes| envelopes.into_iter().map(|envelope| envelope.message.id).collect())
                        }, |id, message, deadline| {
                            if let Err(error) = observe_settlement_message(id, message, deadline) {
                                eprintln!("Claude settlement source for {id}/{message}: {error:#}");
                            }
                        });
                    }
                    Err(error) => {
                        let detail = format!("{error:#}");
                        if store_failure.as_ref() != Some(&detail) {
                            eprintln!("Claude observer could not read session store: {detail}");
                            store_failure = Some(detail);
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        })?;
    Ok(())
}

/// claude-r5 projects an agent message it read into context as a
/// `functionCallOutput` / `external_event` item whose id is the bus message
/// id; that item is the Claude "delivered" receipt (`messageDelivered`).
fn record_agent_message_item(
    id: &str,
    generation: u64,
    event: &cutex::app_server::client::AppServerEvent,
) -> anyhow::Result<()> {
    let cutex::app_server::client::AppServerEvent::Notification(notification) = event else {
        return Ok(());
    };
    let Some(params) = notification.params.as_ref() else { return Ok(()) };
    if notification.method == "thread/externalInput/statusChanged" {
        let Some(message) = params["messageId"].as_str() else { return Ok(()) };
        if let Some(envelope) = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
            .snapshot_by_message_id(message)?.and_then(|snapshot| snapshot.external_input)
        {
            ensure_native_event_binding(id, &params["threadId"], &envelope)?;
            reconcile_native_message(id, generation, &envelope)?;
        }
        return Ok(());
    }
    if notification.method != "item/completed" {
        return Ok(());
    }
    let item = &params["item"];
    if item["type"] != "functionCallOutput" || item["name"] != "external_event" {
        return Ok(());
    }
    if !matches!(item.pointer("/externalInputView/schema").and_then(serde_json::Value::as_str),
        Some(cutex::agent_bus::projection::AGENT_MESSAGE_VIEW_SCHEMA | cutex::agent_bus::alarms::ALARM_VIEW_SCHEMA))
    {
        return Ok(());
    }
    let (Some(message), Some(turn)) = (item["id"].as_str(), params["turnId"].as_str()) else {
        return Ok(());
    };
    let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
    if repository.snapshot_by_message_id(message)?.is_some_and(|snapshot| snapshot.external_input.is_some()) {
        // Native r8 timeline items are an audit projection at claim time. Wait
        // for the separately validated native context-persisted receipt.
        return Ok(());
    }
    if repository.record_context_injected(id, message, turn)? {
        let local = repository.snapshot_by_message_id(message)?.and_then(|snapshot| snapshot.local_inbox_receipt);
        busy_delivery::read(id, message, turn, local.as_ref());
    }
    Ok(())
}

fn ensure_native_event_binding(id: &str, thread: &serde_json::Value, envelope: &cutex::app_server::external_input::Envelope) -> anyhow::Result<()> {
    anyhow::ensure!(envelope.owner_id == id && thread.as_str() == Some(envelope.thread_id.as_str()), "Claude input notification binding mismatch");
    Ok(())
}

fn reconcile_native_message(id: &str, generation: u64, envelope: &cutex::app_server::external_input::Envelope) -> anyhow::Result<()> {
    anyhow::ensure!(envelope.owner_id == id, "Claude input owner changed");
    let handle = manager().handle_for_generation(id, generation)?;
    let status = super::claude_ingress::status(&handle, envelope, generation)?;
    if let Some(turn) = super::claude_ingress::persisted_turn(&status) {
        // Fence again after the response before publishing delivery.
        manager().handle_for_generation(id, generation)?;
        let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
        if repository.record_context_injected(id, &envelope.message.id, turn)? {
            let local = repository.snapshot_by_message_id(&envelope.message.id)?.and_then(|snapshot| snapshot.local_inbox_receipt);
            busy_delivery::read(id, &envelope.message.id, turn, local.as_ref());
        }
    } else {
        // Preserve validated native uncertainty for diagnostics, including
        // output-observed without an SDK acknowledgement after a restart.
        manager().handle_for_generation(id, generation)?;
        cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
            .observe_external_input(&envelope.message.id, envelope, &status)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_persistence_failure_is_best_effort_and_never_retries() {
        let calls = std::cell::Cell::new(0);
        for fail in [true, false] {
            persist_settlement_observation("synthetic-recipient", "synthetic-message", || {
                calls.set(calls.get() + 1);
                if fail { anyhow::bail!("synthetic sidecar unavailable"); }
                Ok(true)
            });
            // Both a failed attempt and later recovery return normally.
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn unchanged_settlement_persistence_is_not_retried() {
        let calls = std::cell::Cell::new(0);
        persist_settlement_observation("synthetic-recipient", "synthetic-message", || {
            calls.set(calls.get() + 1);
            Ok(false)
        });
        assert_eq!(calls.get(), 1);
    }

    fn claude_native(context: &AppServerRuntimeEventContext) -> NativeEventContext {
        NativeEventContext {
            cutex_session_id: context.cutex_session_id.clone(), thread_id: context.thread_id.clone(),
            host_id: "host".into(), runtime_generation: 1, runtime_backend: "claude_subscription".into(),
            schema: AppServerSchema {
                protocol: "claude-app-server-subset".into(), major_version: 1, version: "0.3.0+cutex".into(),
                sha256: "a".repeat(64), channel: AppServerSchemaChannel::Experimental,
                capabilities: serde_json::json!({}), extensions: vec![],
            },
        }
    }

    fn claude_event(method: &str, params: serde_json::Value) -> cutex::app_server::client::AppServerEvent {
        let raw = serde_json::json!({"method": method, "params": params});
        cutex::app_server::client::AppServerEvent::Notification(cutex::app_server::protocol::RpcNotification {
            method: method.into(), params: raw.get("params").cloned(), raw,
        })
    }

    fn frames_for(context: &AppServerRuntimeEventContext, event: &cutex::app_server::client::AppServerEvent) -> Vec<Option<String>> {
        let native = claude_native(context);
        let other = match pending_event_from_app_server(&native, event).unwrap() {
            NativeEventDisposition::OtherThread { thread_id, pending } => Some((thread_id, pending)),
            NativeEventDisposition::Publish(_) => None,
        };
        subagent_frames(context, &native, event, other.as_ref().map(|(t, p)| (t.as_str(), p.clone())))
            .unwrap().into_iter().map(|frame| frame.correlation.thread_id).collect()
    }

    /// claude-subagent-transcripts: transcript frames wait for the parent's
    /// activity item, then publish with their own thread id; others never do.
    #[test]
    fn subagent_transcript_frames_publish_after_the_parent_names_them() {
        let context = AppServerRuntimeEventContext {
            cutex_session_id: format!("cutex-claude-sub-{}", uuid::Uuid::new_v4()),
            thread_id: "root".into(), runtime_generation: 1, runtime_backend: "claude_subscription".into(),
            full_text_item_terminal: false, launched_profile: None, launched_profile_id: None, usage: Default::default(),
            schema: cutex::app_server::journal::AppServerSchemaIdentity { version: "0.3.0+cutex".into(), sha256: "a".repeat(64) },
        };
        let child = "claude:sid:task:task-a";
        let frame = claude_event("turn/started", serde_json::json!({"threadId": child, "turn": {"id": "subagent:c", "items": [], "status": "inProgress"}}));
        assert!(frames_for(&context, &frame).is_empty(), "buffered until the parent names it");
        let activity = claude_event("item/completed", serde_json::json!({"threadId": "root", "turnId": "t", "item": {
            "type": "subAgentActivity", "id": "a1", "kind": "started", "agentThreadId": child, "agentPath": "/claude/task-a"}}));
        assert_eq!(frames_for(&context, &activity), vec![Some(child.to_string())]);
        let item = claude_event("item/completed", serde_json::json!({"threadId": child, "turnId": "subagent:c", "item": {"type": "agentMessage", "id": "m", "text": "hi"}}));
        assert_eq!(frames_for(&context, &item), vec![Some(child.to_string())]);
        let stranger = claude_event("item/completed", serde_json::json!({"threadId": "claude:sid:task:other", "turnId": "x", "item": {"type": "agentMessage", "id": "m2", "text": "no"}}));
        assert!(frames_for(&context, &stranger).is_empty());
        // A bound-thread event that teaches nothing publishes no subagent frame.
        assert!(frames_for(&context, &claude_event("turn/started", serde_json::json!({"threadId": "root", "turn": {"id": "t"}}))).is_empty());
    }

    /// r247 hachimi (r23-live-check): the adapter's `claude/*` frames reach the
    /// observer once it asks for `claudeExtensions`. Only a bound-thread
    /// `claude/task/updated` with `notifyParent` becomes a notice; every other
    /// `claude/*` frame (a subagent-owned command's `turnId: null` included) is
    /// ignored before any store is touched and never ends the observer.
    #[test]
    fn claude_extension_frames_are_ignored_except_the_task_notice() {
        let context = AppServerRuntimeEventContext {
            cutex_session_id: format!("cutex-claude-ext-{}", uuid::Uuid::new_v4()),
            thread_id: "a43c4add".into(), runtime_generation: 1, runtime_backend: "claude_subscription".into(),
            full_text_item_terminal: false, launched_profile: None, launched_profile_id: None, usage: Default::default(),
            schema: cutex::app_server::journal::AppServerSchemaIdentity { version: "0.3.0+cutex".into(), sha256: "a".repeat(64) },
        };
        // Recorded on hachimi (adapter events seq 39 and 75), ids shortened.
        let subagent_command = serde_json::json!({"threadId": "a43c4add", "task": {"description": "Print eight steps",
            "ownedBySubagent": true, "status": "running", "taskId": "bvvxedjjm", "taskType": "local_bash",
            "toolUseId": "toolu_01Dn", "turnId": null}});
        let finished = serde_json::json!({"threadId": "a43c4add", "task": {"background": true,
            "description": "Background sleep loop test", "status": "completed", "taskId": "afa1b240b52a47bb4",
            "taskType": "local_agent", "toolUseId": "toolu_01Fr", "turnId": "3216c31d",
            "usage": {"duration_ms": 164130, "tool_uses": 1, "total_tokens": 11012},
            "notifyParent": {"description": "Background sleep loop test", "noticeId": "afa1b240b52a47bb4",
                "originTurnId": "3216c31d", "status": "completed", "summary": "bg-done", "summaryTruncated": false,
                "taskId": "afa1b240b52a47bb4", "taskType": "local_agent", "toolUseId": "toolu_01Fr",
                "transcript": "claude:3c57b94d:task:afa1b240b52a47bb4"}}});
        for (method, params) in [
            ("claude/task/updated", subagent_command.clone()),
            ("claude/runtime/event", serde_json::json!({"threadId": "a43c4add", "kind": "StreamEvent"})),
            ("claude/input/statusChanged", serde_json::json!({"messageId": "m", "durable": true})),
            ("claude/someFutureFrame", serde_json::json!({"threadId": "a43c4add", "turnId": ""})),
        ] {
            handle_claude_event(&context, &claude_event(method, params)).expect("ignored, observer stays connected");
        }
        assert!(claude_task_notice_params("a43c4add", "claude/task/updated", Some(&subagent_command)).is_none());
        let params = claude_task_notice_params("a43c4add", "claude/task/updated", Some(&finished)).expect("notice params");
        let notice = cutex::agent_bus::claude_task::ClaudeTaskNotice::from_task_updated(params).expect("valid notice");
        assert_eq!(notice.summary.as_deref(), Some("bg-done"));
        assert!(claude_task_notice_params("other-thread", "claude/task/updated", Some(&finished)).is_none());
        assert!(claude_task_notice_params("a43c4add", "claude/runtime/event", Some(&finished)).is_none());
    }

    /// PERF-03: one observer pass over several online Claude runtimes with an
    /// empty bus makes no whole-store copy (before: one per runtime per pass).
    #[test]
    fn delivery_pass_reads_the_shared_snapshot_without_copying_the_store() {
        const CHILD: &str = "CUTEX_CLAUDE_DELIVERY_CLONE_TEST_CHILD";
        const NAME: &str = "cli_app::claude_runtime::tests::delivery_pass_reads_the_shared_snapshot_without_copying_the_store";
        if std::env::var_os(CHILD).is_none() {
            let root = std::env::temp_dir().join(format!("cutex-claude-clone-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(".cutex-test-private-home"), b"clone test").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("HOME", &root).env("CUTEX_TEST_PRIVATE_HOME", &root).env(CHILD, "1")
                .status().unwrap();
            let ran = root.join("clone-test-done").is_file();
            std::fs::remove_dir_all(root).unwrap();
            assert!(result.success() && ran);
            return;
        }
        use cutex::session::model::{whole_store_clones_on_this_thread, CutexSessionRecord, CutexSessionStore};
        let mut store = CutexSessionStore::default();
        let ids: Vec<String> = (1..=3).map(|n| format!("cutex.00000000-0000-0000-0000-00000000000{n}")).collect();
        for (n, id) in ids.iter().enumerate() {
            let mut record = CutexSessionRecord::new(
                id.clone(), Some(uuid::Uuid::new_v4().to_string()), "private".into(), "/private".into(), None,
            ).unwrap();
            record.current_runtime_agent_id = Some(format!("runtime-{n}"));
            store.sessions.insert(id.clone(), record);
        }
        // Filler so a copy would be visible in the count, not just cheap.
        for n in 0..200 {
            let id = format!("cutex.10000000-0000-0000-0000-{n:012}");
            store.sessions.insert(id.clone(), CutexSessionRecord::new(
                id, None, "private".into(), "/private".into(), None).unwrap());
        }
        cutex::session::store::save_cutex_session_store(&store).unwrap();
        // An empty bus: every poll answers no messages.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                if cutex::http::server::read_simple_http_request(&mut stream).is_ok() {
                    let _ = cutex::http::server::write_json_response(
                        &mut stream, 200, "OK", &serde_json::json!({"messages": []}));
                }
            }
        });
        let bus = cutex::agent_bus::client::AgentBusHttpClient::new(format!("http://127.0.0.1:{port}"), None);
        let mut attempts = HashMap::new();
        // The counter sees whole-store copies on this thread.
        let probe = whole_store_clones_on_this_thread();
        drop(cutex::session::store::load_cutex_session_store().unwrap());
        assert_eq!(whole_store_clones_on_this_thread() - probe, 1);
        let before = whole_store_clones_on_this_thread();
        for _pass in 0..5 {
            for id in &ids {
                deliver_messages(id, &bus, &mut attempts).unwrap();
            }
        }
        let copies = whole_store_clones_on_this_thread() - before;
        eprintln!("whole-store copies for 5 passes x {} Claude runtimes: {copies}", ids.len());
        assert_eq!(copies, 0);
        std::fs::write(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").map(std::path::PathBuf::from).unwrap()
            .join("clone-test-done"), b"ok").unwrap();
    }

    /// claude-r23: a "Claude Task" notice (built from the adapter's notifyParent) is a
    /// service-source wake: a busy parent reads it at its next tool step at once
    /// (no busy wait, `soon`), an idle parent gets a turn at once.
    #[test]
    fn claude_task_notice_projection_and_next_step_timing() {
        use cutex::agent_bus::delivery::WakeTiming;
        use serde_json::json;
        let owner = "cutex.00000000-0000-0000-0000-000000000003";
        let params = json!({"threadId":"thread","task":{"taskId":"task-a","status":"completed","notifyParent":{
            "noticeId":"task-a","taskId":"task-a","taskType":"local_agent","status":"completed",
            "description":"Review docs","summary":"All good.","transcript":"claude:s:task:task-a"}}});
        let notice = cutex::agent_bus::claude_task::ClaudeTaskNotice::from_task_updated(&params).unwrap();
        let request = cutex::agent_bus::claude_task::send_request(owner, &notice).unwrap();
        let message: cutex::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
            "id": request.external_message_id, "kind":"message", "from":"Claude Task", "to":"runtime",
            "toCutexSessionId": owner, "content": request.content, "deliveryMode":"after_turn",
            "triggerTurn": true, "createdAtEpochSecs": 1, "senderKind":"agent",
            "controlType": request.control_type, "controlPayload": request.control_payload,
            "externalMessageId": request.external_message_id
        })).unwrap();
        let sessions = cutex::session::model::CutexSessionStore::default();
        let payload = claude_message_params(&sessions, "thread", owner, &message, None, true).unwrap();
        assert_eq!(payload["source"], json!({"kind":"service","id":"Claude Task"}));
        assert_eq!(payload["eventType"], "claude_task");
        assert_eq!(payload["metadata"]["deliveryMode"], "after-turn");
        assert_eq!(payload["view"]["schema"], "cutex.claude-task.v1");
        let text = payload["text"].as_str().unwrap();
        assert!(text.contains("Your background subagent finished.") && text.ends_with("Its final reply:\nAll good."), "{text}");
        // Frozen as a native envelope, after-turn or (busy, next step) soon.
        let envelope = super::super::claude_ingress::envelope(owner, "thread", 3, &payload).unwrap();
        assert_eq!(envelope.message.delivery, cutex::app_server::external_input::Delivery::AfterTurn);
        let mut soon = payload.clone();
        soon["metadata"]["deliveryMode"] = json!("soon");
        let envelope = super::super::claude_ingress::envelope(owner, "thread", 3, &soon).unwrap();
        assert_eq!(envelope.message.delivery, cutex::app_server::external_input::Delivery::Soon);
        let mut passive = payload.clone();
        passive["metadata"]["deliveryMode"] = json!("passive");
        assert!(super::super::claude_ingress::envelope(owner, "thread", 3, &passive).is_err());
        // Timing: no busy wait for the notice; an agent wake keeps the configured one.
        let wait = Some(Duration::from_secs(120));
        assert!(busy_delivery::candidate(&message, true));
        let notice_wait = claude_task_wait(&message, wait);
        assert_eq!(notice_wait, Some(Duration::ZERO));
        assert_eq!(busy_delivery::timing(Duration::ZERO, notice_wait, Some(true), true), WakeTiming::NextStep);
        assert_eq!(busy_delivery::timing(Duration::ZERO, notice_wait, Some(false), true), WakeTiming::NextStep);
        assert_eq!(busy_delivery::timing(Duration::ZERO, notice_wait, Some(true), false), WakeTiming::AfterTurn);
        let mut agent = message.clone();
        agent.control_type = None;
        agent.control_payload = None;
        assert_eq!(claude_task_wait(&agent, wait), wait);
        assert_eq!(busy_delivery::timing(Duration::ZERO, wait, Some(true), true), WakeTiming::Hold);
        // A forged notice (a sender session) is refused before any submission.
        let mut forged = message.clone();
        forged.from_cutex_session_id = Some("cutex.someone".into());
        assert!(claude_message_params(&sessions, "thread", owner, &forged, None, true).is_err());
        // No notice in an ordinary task update.
        assert!(cutex::agent_bus::claude_task::ClaudeTaskNotice::from_task_updated(
            &json!({"threadId":"thread","task":{"taskId":"task-a","status":"running"}})).is_none());
    }

    #[test]
    fn isolated_alarm_projection_freezes_service_identity_and_preserves_receipt_semantics() {
        const CHILD: &str = "CUTEX_CLAUDE_ALARM_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = std::env::temp_dir().join(format!("cutex-claude-alarm-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(".cutex-test-private-home"), b"alarm test").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cli_app::claude_runtime::tests::isolated_alarm_projection_freezes_service_identity_and_preserves_receipt_semantics", "--nocapture"])
                .env("HOME", &root).env("CUTEX_TEST_PRIVATE_HOME", &root).env(CHILD, "1")
                .status().unwrap();
            let ran = root.join("alarm-test-done").is_file();
            std::fs::remove_dir_all(root).unwrap();
            assert!(result.success() && ran);
            return;
        }
        use serde_json::json;
        use cutex::agent_bus::alarms::{self, Alarm, SetArgs};
        let owner = "cutex.00000000-0000-0000-0000-000000000001";
        let alarm: Alarm = serde_json::from_value(alarms::set(owner, SetArgs {
            alarm_id: "claude-test".into(), to: owner.into(), message: "reminder 私有".into(),
            at: None, delay_seconds: Some(60),
        }).unwrap()).unwrap();
        let message: cutex::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
            "id":alarm.message_id,"kind":"message","from":"Cutex Alarm","to":"runtime",
            "toCutexSessionId":owner,"content":alarm.message,"deliveryMode":"after_turn",
            "triggerTurn":true,"createdAtEpochSecs":1,"senderKind":"alarm_system",
            "externalMessageId":alarm.message_id
        })).unwrap();
        let sessions = cutex::session::model::CutexSessionStore::default();
        assert!(claude_message_params(&sessions, "thread", owner, &message, None, true)
            .unwrap_err().is::<alarms::AlarmAwaitingQueueCommit>());
        let path = cutex::config::paths::runtime_dir().unwrap().join("alarms/alarms.json");
        let mut alarms: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for a in alarms.as_object_mut().unwrap().values_mut() { a["state"] = json!("queued"); }
        std::fs::write(&path, serde_json::to_vec(&alarms).unwrap()).unwrap();
        let params = claude_message_params(&sessions, "thread", owner, &message, None, true).unwrap();
        assert_eq!(params["source"], json!({"kind":"service","id":"Cutex Alarm"}));
        assert_eq!(params["text"], "Message Type: ALARM\nPayload:\nreminder 私有");
        assert_eq!(params["metadata"]["deliveryMode"], "after-turn");
        assert_eq!(params["metadata"]["sourceName"], "Cutex Alarm");
        let legacy = claude_message_params(&sessions, "thread", owner, &message, None, false).unwrap();
        assert!(legacy.get("view").is_none());
        assert_eq!(legacy["text"], params["text"]);
        let projection = cutex::app_server::bus_bridge::inter_agent_params("thread", "ignored", owner, &message).unwrap();
        let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository().unwrap();
        assert!(claude_message_is_settled(repository, owner, &message).unwrap(), "live-only historical alarms must be withdrawn before native submission");
        let audit = std::fs::read_to_string(cutex::config::paths::runtime_dir().unwrap().join("agent-bus-audit.jsonl")).unwrap();
        assert!(audit.contains("canonical_message_absent") && audit.contains(&message.id));
        repository.record_queued(cutex::management::v2::agent_bus_state::AgentBusQueuedMessage {
            owner_cutex_session_id: owner.into(), message_id: message.id.clone(), from_cutex_session_id: None,
            to_cutex_session_id: owner.into(), from_runtime_agent_id: None, to_runtime_agent_id: Some("runtime".into()),
            delivery_mode: "after_turn".into(), content: message.content.clone(), queued_at: chrono::Utc::now(),
            canonical_envelope: message.clone(), semantic_sha256: cutex::app_server::bus_bridge::inter_agent_semantic_sha256(&projection),
        }).unwrap();
        assert!(!claude_message_is_settled(repository, owner, &message).unwrap());
        let frozen = repository.freeze_external_input(owner, &message.id, |_| super::super::claude_ingress::envelope(owner, "thread", 7, &params)).unwrap();
        assert_eq!(frozen.message.source.kind, cutex::app_server::external_input::SourceKind::Service);
        assert_eq!(frozen.message.event_type, "alarm");
        assert_eq!(frozen.message.delivery, cutex::app_server::external_input::Delivery::AfterTurn);
        assert_eq!(repository.freeze_external_input(owner, &message.id, |_| panic!("identity must stay frozen")).unwrap(), frozen);
        let receipt = json!({"threadId":"thread","messageId":message.id,"durable":true,"receiptScope":"local-inbox","state":"queued"});
        repository.record_local_inbox(owner, &message.id, &receipt).unwrap();
        assert_eq!(repository.message_state(&message.id).unwrap(), Some(("inbox_accepted".into(), true)));
        let event = cutex::app_server::client::AppServerEvent::Notification(cutex::app_server::protocol::RpcNotification {
            raw: serde_json::Value::Null,
            method: "item/completed".into(), params: Some(json!({"threadId":"thread","turnId":"turn",
                "item":{"type":"functionCallOutput","name":"external_event","id":message.id,
                    "externalInputView":params["view"]}})),
        });
        record_agent_message_item(owner, 7, &event).unwrap();
        assert_eq!(repository.message_state(&message.id).unwrap().unwrap().0, "inbox_accepted", "native claim-time items are not context receipts");
        let mut legacy_message = message.clone();
        legacy_message.id = "alarm-legacy-adapter".into();
        repository.record_queued(cutex::management::v2::agent_bus_state::AgentBusQueuedMessage {
            owner_cutex_session_id: owner.into(), message_id: legacy_message.id.clone(), from_cutex_session_id: None,
            to_cutex_session_id: owner.into(), from_runtime_agent_id: None, to_runtime_agent_id: Some("runtime".into()),
            delivery_mode: "after_turn".into(), content: message.content.clone(), queued_at: chrono::Utc::now(),
            canonical_envelope: legacy_message.clone(), semantic_sha256: cutex::app_server::bus_bridge::inter_agent_semantic_sha256(
                &cutex::app_server::bus_bridge::inter_agent_params("thread", "ignored", owner, &legacy_message).unwrap()),
        }).unwrap();
        let mut legacy_receipt = receipt.clone(); legacy_receipt["messageId"] = json!(legacy_message.id);
        repository.record_local_inbox(owner, &legacy_message.id, &legacy_receipt).unwrap();
        let mut legacy_event = event;
        if let cutex::app_server::client::AppServerEvent::Notification(notification) = &mut legacy_event {
            notification.params.as_mut().unwrap()["item"]["id"] = json!(legacy_message.id);
        }
        record_agent_message_item(owner, 7, &legacy_event).unwrap();
        assert_eq!(repository.message_state(&legacy_message.id).unwrap().unwrap().0, "delivered", "legacy v2 timeline items keep their context receipt semantics");
        if let Some(path) = std::env::var_os("CUTEX_ALARM_EVIDENCE_DIR") {
            let path = std::path::Path::new(&path);
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("claude-alarm-envelope.json"), serde_json::to_vec_pretty(&frozen).unwrap()).unwrap();
            std::fs::write(path.join("claude-alarm-legacy-params.json"), serde_json::to_vec_pretty(&params).unwrap()).unwrap();
        }
        std::fs::write(std::path::Path::new(&std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap()).join("alarm-test-done"), b"ok").unwrap();
    }

    #[test]
    fn isolated_agent_management_messages_become_native_inputs_or_quarantine() {
        const CHILD: &str = "CUTEX_CLAUDE_MANAGEMENT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = std::env::temp_dir().join(format!("cutex-claude-management-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(".cutex-test-private-home"), b"management test").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cli_app::claude_runtime::tests::isolated_agent_management_messages_become_native_inputs_or_quarantine", "--nocapture"])
                .env("HOME", &root).env("CUTEX_TEST_PRIVATE_HOME", &root).env(CHILD, "1")
                .status().unwrap();
            let ran = root.join("management-test-done").is_file();
            std::fs::remove_dir_all(root).unwrap();
            assert!(result.success() && ran);
            return;
        }
        use serde_json::json;
        use cutex::agent_management::{AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE, AGENT_MANAGEMENT_START_CONTROL_TYPE,
            AGENT_MANAGEMENT_SYSTEM_SENDER};
        const DIRECTOR: &str = "cutex.33333333-3333-4333-8333-333333333333";
        const WORKER: &str = "cutex.22222222-2222-4222-8222-222222222222";
        let management = |id: &str, control: &str, to: &str, mode: &str| -> cutex::agent_bus::model::AgentBusMessage {
            serde_json::from_value(json!({"id":id,"from":AGENT_MANAGEMENT_SYSTEM_SENDER,"to":"runtime",
                "fromCutexSessionId":DIRECTOR,"toCutexSessionId":to,"content":"Do the brief","deliveryMode":mode,
                "triggerTurn":mode != "passive","createdAtEpochSecs":1,"controlType":control,
                "controlPayload":{"schema":"cutex/agent-management/v1","requested_by_director":DIRECTOR},
                "externalMessageId":format!("agent-management:act-1:{id}")})).unwrap()
        };
        let sessions = cutex::session::model::CutexSessionStore::default();
        // A notice to its requester (no action needed): the Codex text, a
        // passive v1 service envelope.
        let notice = management("bus-notice-1", AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE, DIRECTOR, "passive");
        assert!(cutex::app_server::bus_bridge::is_agent_management_message(&notice));
        let params = claude_message_params(&sessions, "thread", DIRECTOR, &notice, None, true).unwrap();
        assert_eq!(params["source"], json!({"kind":"service","id":AGENT_MANAGEMENT_SYSTEM_SENDER}));
        assert_eq!(params["eventType"], "management_notice");
        assert_eq!(params["metadata"]["deliveryMode"], "passive");
        assert!(params["text"].as_str().unwrap().starts_with("Message Type: NOTICE\nFrom: Cutex Agent Management\nPayload:\n"));
        let envelope = super::super::claude_ingress::envelope(DIRECTOR, "thread", 7, &params).unwrap();
        assert_eq!((envelope.version, envelope.view.is_none()), (1, true));
        // A start message is an after-turn wake once its action matches; with
        // no create / replace action it is terminal and is quarantined.
        let start = management("bus-start-1", AGENT_MANAGEMENT_START_CONTROL_TYPE, WORKER, "after_turn");
        let error = claude_message_params(&sessions, "thread", WORKER, &start, None, true).unwrap_err();
        let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository().unwrap();
        repository.record_queued(cutex::management::v2::agent_bus_state::AgentBusQueuedMessage {
            owner_cutex_session_id: WORKER.into(), message_id: start.id.clone(), from_cutex_session_id: Some(DIRECTOR.into()),
            to_cutex_session_id: WORKER.into(), from_runtime_agent_id: None, to_runtime_agent_id: Some("runtime".into()),
            delivery_mode: "after_turn".into(), content: start.content.clone(), queued_at: chrono::Utc::now(),
            canonical_envelope: start.clone(), semantic_sha256: cutex::app_server::bus_bridge::inter_agent_semantic_sha256(
                &cutex::app_server::bus_bridge::inter_agent_params("thread", "ignored", WORKER, &start).unwrap()),
        }).unwrap();
        assert!(cutex::app_server::bus_bridge::quarantine_terminal_external_input(WORKER, &start, &error).unwrap());
        let snapshot = repository.snapshot_by_message_id(&start.id).unwrap().unwrap();
        assert_eq!(snapshot.state, "quarantined");
        assert_eq!(snapshot.error.unwrap()["code"], "management_start_provenance_invalid");
        // A retryable error is never quarantined.
        assert!(!cutex::app_server::bus_bridge::quarantine_terminal_external_input(
            WORKER, &start, &anyhow::anyhow!("store busy")).unwrap());
        // An ordinary agent message that claims the control type is not one.
        let mut forged = start.clone();
        forged.from = "worker".into();
        assert!(!cutex::app_server::bus_bridge::is_agent_management_message(&forged));
        std::fs::write(std::path::Path::new(&std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap()).join("management-test-done"), b"ok").unwrap();
    }

    #[test]
    fn native_receipt_recovery_is_throttled_but_new_generations_are_immediate() {
        let mut attempts = HashMap::new();
        let start = Instant::now();
        assert!(reconciliation_due(&mut attempts, "one", 1, start));
        assert!(!reconciliation_due(&mut attempts, "one", 1, start + Duration::from_secs(5)));
        assert!(!reconciliation_due(&mut attempts, "one", 1, start + Duration::from_secs(59)));
        assert!(reconciliation_due(&mut attempts, "two", 1, start));
        assert!(reconciliation_due(&mut attempts, "one", 1, start + Duration::from_secs(60)));
        assert!(reconciliation_due(&mut attempts, "one", 2, start + Duration::from_secs(61)));
    }
}
