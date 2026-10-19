//! Management's read-only connection to already running Claude adapters.
//! Recovery reconnects observers; it never spawns an adapter or a model turn.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cutex::app_server::manager::{AppServerRuntimeEventContext, AppServerRuntimeManager};
use cutex::management::v2::model::{AppServerSchema, AppServerSchemaChannel};
use cutex::management::v2::native_events::{
    pending_event_from_app_server, NativeEventContext, NativeEventDisposition,
};
use cutex::management::v2::repository::management_v2_repository;

static MANAGER: OnceLock<AppServerRuntimeManager> = OnceLock::new();
static OBSERVATION_LOCK: Mutex<()> = Mutex::new(());

#[path = "claude_busy_delivery.rs"]
mod busy_delivery;

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
    fn event_ledger(&self) -> Option<Arc<cutex::app_server::single_writer::Ledger>> {
        cutex::app_server::single_writer::process_ledger()
    }

    fn handle_event(
        &self,
        context: &AppServerRuntimeEventContext,
        event: &cutex::app_server::client::AppServerEvent,
    ) -> anyhow::Result<()> {
        handle_claude_event(context, event)
    }
}

fn handle_claude_event(
    context: &AppServerRuntimeEventContext,
    event: &cutex::app_server::client::AppServerEvent,
) -> anyhow::Result<()> {
    let native = NativeEventContext {
        cutex_session_id: context.cutex_session_id.clone(),
        thread_id: context.thread_id.clone(),
        host_id: cutex::platform::host::current_host_name(),
        runtime_generation: context.runtime_generation,
        runtime_backend: "claude_subscription".into(),
        schema: AppServerSchema {
            protocol: "claude-app-server-subset".into(), major_version: 1,
            version: context.schema.version.clone(), sha256: context.schema.sha256.clone(),
            channel: AppServerSchemaChannel::Experimental,
            // Per-event identity stays small; the full feature surface is
            // the session resource's native.schema.capabilities.features.
            capabilities: serde_json::json!({"persistentRuntime":true,"history":"paginated","sameTurnSteering":false}),
            extensions: vec!["claude-runtime-v1".into()],
        },
    };
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
        let envelope = management_v2_repository()?.append(pending)?;
        // A subAgentActivity `started` names a subagent transcript as a verified descendant.
        for frame in subagent_frames(context, &native, event, None)? {
            management_v2_repository()?.append(frame)?;
        }
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
    }
    if !synchronize_before_publish {
        super::app_server_state_sync::handle_runtime_event(context, event)?;
    }
    // Quota auto-continue first: a usage-limit failure holds the agent
    // (`blocked(quota)`) before the asks observer puts a failed
    // delegation back in the queue; a completed turn cancels.
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
    if let Err(error) = super::app_server_user_input::handle_runtime_event(&context.cutex_session_id, event) {
        eprintln!("Claude queued input for {} was not flushed: {error:#}", context.cutex_session_id);
    }
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
    if manager().status(id)?.is_some_and(|s| {
        s.connected && s.runtime_generation == observation.record.runtime_generation
    }) {
        return Ok(());
    }
    manager().disconnect(id)?;
    let record = observation.record.clone();
    manager().connect_adapter(observation)?;
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
    if let Err(error) = manager().start_agent_bus_bridge(
        id,
        Arc::new(cutex::agent_bus::client::AgentBusHttpClient::from_config(
            &cutex::config::store::load_codez_config(),
        )),
        options,
    ) {
        manager().disconnect(id)?;
        return Err(error);
    }
    // Safety blocks the adapter reported while nobody observed it.
    cutex::management::v2::safety_checks::reconcile(id, &thread);
    // Like a Codex connect: input queued while no observer was connected
    // (offline submit, daemon restart) goes out once the thread is idle.
    if let Err(error) = super::app_server_user_input::flush_queued_if_idle(id) {
        eprintln!("Claude queued input for {id} was not flushed: {error:#}");
    }
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
        if !(message.sender_kind.is_agent() || message.sender_kind.is_alarm_system())
            || (message.control_type.is_some() && !cutex::agent_bus::peer_tell::is_peer_tell(&message))
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
        let meta = repository.message_meta(&message.id)?;
        let payload = match claude_message_params(&sessions, thread, id, &message, meta.as_ref(), v2) {
            Ok(payload) => payload,
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
        let wait = cutex::agent_bus::delivery::wake_busy_wait();
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
        let prepared_turn = (busy_turn.is_none() && !already_accepted && message.delivery_mode.trigger_turn())
            .then(|| cutex::management::v2::admission::prepare_turn(id, "delivery", None));
        let mut receipt = if let Some(existing) = existing {
            existing
        } else if native || frozen.is_some() {
            ensure!(native, "native Claude input was frozen but this runtime lacks its protocol; no legacy fallback");
            let envelope = repository.freeze_external_input(id, &message.id, |canonical| {
                let mut projection = claude_message_params(&sessions, thread, id, canonical, meta.as_ref(), true)?;
                if timing == cutex::agent_bus::delivery::WakeTiming::NextStep {
                    projection["metadata"]["deliveryMode"] = serde_json::json!("soon");
                }
                super::claude_ingress::envelope(id, thread, record.runtime_generation, &projection)
            })?;
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

pub(super) fn disconnect(id: &str) -> anyhow::Result<()> {
    let _guard = OBSERVATION_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Claude observer lock poisoned"))?;
    manager().disconnect(id)?;
    Ok(())
}

pub(super) fn spawn_observer() -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("cutex-claude-observer".into())
        .spawn(|| {
            let mut tracked = HashSet::<String>::new();
            let mut failures = HashMap::<String, String>::new();
            let mut reconciliation_attempts = HashMap::<String, (u64, Instant)>::new();
            loop {
                // Borrow the cached store (no whole-store copy every 5 s).
                let host = cutex::platform::host::current_host_name();
                let result = cutex::session::store::with_cutex_session_store(|store| {
                    store
                        .sessions
                        .values()
                        .filter(|r| {
                            r.claude_session.is_some()
                                && r.runtime_pid.is_some()
                                && !r.is_retired()
                                && cutex::runtime::lifecycle::cutex_session_host_is_local(
                                    &r.host_id, &host,
                                )
                        })
                        .map(|r| r.cutex_session_id.clone())
                        .collect::<HashSet<_>>()
                });
                match result {
                    Ok(wanted) => {
                        let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(
                            &cutex::config::store::load_codez_config(),
                        );
                        for removed in tracked.difference(&wanted) {
                            let _ = disconnect(removed);
                            failures.remove(removed);
                            reconciliation_attempts.remove(removed);
                        }
                        for id in &wanted {
                            match observe(id).and_then(|()| deliver_messages(id, &bus, &mut reconciliation_attempts)) {
                                Ok(()) => {
                                    failures.remove(id);
                                }
                                Err(error) => {
                                    // A dead adapter (PID gone, endpoint
                                    // refused) is offline: clear its stale
                                    // claim; the next pass stops observing it.
                                    match cutex::runtime::claude::reconcile_dead_adapter(id) {
                                        Ok(true) => {
                                            eprintln!("Claude adapter of {id} is gone; cleared its stale runtime claim");
                                            let _ = disconnect(id);
                                            failures.remove(id);
                                            continue;
                                        }
                                        Ok(false) => {}
                                        Err(reconcile) => eprintln!(
                                            "Claude dead-adapter check for {id} failed: {reconcile:#}"
                                        ),
                                    }
                                    let detail = format!("{error:#}");
                                    if failures.get(id) != Some(&detail) {
                                        eprintln!(
                                            "Claude observation unavailable for {id}: {detail}"
                                        );
                                        failures.insert(id.clone(), detail);
                                    }
                                }
                            }
                        }
                        tracked = wanted;
                    }
                    Err(error) => {
                        eprintln!("Claude observer could not read session store: {error:#}")
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            launched_profile: None, launched_profile_id: None, usage: Default::default(),
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
