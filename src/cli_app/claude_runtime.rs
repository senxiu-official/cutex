//! Management's read-only connection to already running Claude adapters.
//! Recovery reconnects observers; it never spawns an adapter or a model turn.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
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
    } else if let Some(requester) = super::session_runtime::stop_requested_by() {
        params["requestedBy"] = requester;
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
    fn handle_background_work(&self, context: &AppServerRuntimeEventContext,
        work: Option<cutex::app_server::background_work::BackgroundWork>) -> anyhow::Result<()> {
        cutex::management::v2::repository::publish_background_work(context, work);
        Ok(())
    }
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
            // 054 §2.11: owner-insert stages advance the input ledger record
            // (never forwarded raw to clients).
            cutex::management::v2::agent_bus_state::claude_input_stage(
                &context.cutex_session_id, &notification.method, notification.params.as_ref());
            // r276 agent pause: the tool calls the adapter holds.
            if notification.method == super::tool_gate::CLAUDE_CHANGED {
                super::tool_gate::note_changed(&context.cutex_session_id, &context.thread_id, notification.params.as_ref());
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
            let bytes = cutex::management::v2::runtime_journal::retained_cost(&frame);
            cutex::management::v2::runtime_journal::submit(frame, bytes, |_| Ok(()))?;
        }
        return Ok(());
    }
    if let NativeEventDisposition::Publish(pending) = disposition {
        let bytes = cutex::management::v2::runtime_journal::retained_cost(&pending);
        let captured_context = context.clone();
        let captured_event = event.clone();
        cutex::management::v2::runtime_journal::submit(pending, bytes, move |envelope| {
            complete_claude_event(&captured_context, &captured_event, synchronize_before_publish, envelope)
        })?;
        // A subAgentActivity `started` names a subagent transcript as a verified descendant.
        for frame in subagent_frames(context, &native, event, None)? {
            let bytes = cutex::management::v2::runtime_journal::retained_cost(&frame);
            cutex::management::v2::runtime_journal::submit(frame, bytes, |_| Ok(()))?;
        }
        return Ok(());
    }
    // Ignored protocol events still take the original bookkeeping path.
    complete_claude_event(context, event, synchronize_before_publish, None)
}

fn complete_claude_event(
    context: &AppServerRuntimeEventContext,
    event: &cutex::app_server::client::AppServerEvent,
    synchronize_before_publish: bool,
    envelope: Option<cutex::management::v2::model::EventEnvelope>,
) -> anyhow::Result<()> {
        if let Some(envelope) = envelope {
        // Token usage goes into the usage ledger under the session's
        // profile, as for Codex runtimes: GET /v2/usage and the quota
        // of a Claude API account (API usage) read it.
        let profile = context.current_profile();
        // A failed turn keeps its error under this profile (turn-error-surface).
        cutex::management::v2::activity::record_activity_event_for_profile(
            &envelope,
            profile.id.as_deref(),
            profile.name.as_deref(),
        )?;
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
            // 054 S24: what waited for this turn to end goes out now, before
            // the next turn starts (not at the next 5 s observer pass).
            wake_delivery(&context.cutex_session_id);
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
    notify_parent_with(cutex_session_id, params, |request| {
        let config = cutex::config::store::load_codez_config();
        cutex::agent_bus::client::send_claude_task_notice(&config, request).map(drop)
    })
}

/// [`notify_parent`] with the bus send injected. A queued notice wakes the
/// parent's delivery at once (r274.2): a system sender emits no
/// `messageQueued`, so an idle parent would otherwise wait for the next
/// observer pass.
fn notify_parent_with(
    cutex_session_id: &str,
    params: &serde_json::Value,
    send: impl FnOnce(&cutex::agent_bus::model::AgentBusSendRequest) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(notice) = cutex::agent_bus::claude_task::ClaudeTaskNotice::from_task_updated(params) else {
        return Ok(());
    };
    let request = cutex::agent_bus::claude_task::send_request(cutex_session_id, &notice)?;
    send(&request)?;
    wake_delivery(cutex_session_id);
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
        // The stream may have missed its last turn/completed before losing
        // the socket. Recover that terminal fact before trying the owner queue,
        // even if Agent Bus registration is temporarily unavailable.
        if let Err(error) = reconcile_observed_completion(id, record.runtime_generation) {
            eprintln!("Claude completed turn for {id} was not reconciled: {error:#}");
        }
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

fn reconcile_observed_completion(id: &str, generation: u64) -> anyhow::Result<()> {
    let store = cutex::asks::store::AskStore::open_default()?;
    let Some(lease) = store.inspect(|state| state.leases.get(id).filter(|lease| lease.turn_id.is_some()).cloned())? else {
        return Ok(());
    };
    let Some(status) = manager().status(id)?.filter(|status| status.connected && status.runtime_generation == generation) else {
        return Ok(());
    };
    let thread = if cutex::management::v2::input_ledger::enabled() {
        // 054 §2.12 b (S17): one turn, once per runtime generation and
        // lease turn; later checks belong to the lease reconcile.
        let Some(turn_id) = lease.turn_id.clone() else { return Ok(()) };
        static READ: OnceLock<Mutex<std::collections::HashSet<(String, u64, String)>>> = OnceLock::new();
        let first = READ.get_or_init(Default::default).lock()
            .map_err(|_| anyhow::anyhow!("Claude completion reconcile lock poisoned"))?
            .insert((id.to_string(), generation, turn_id.clone()));
        if !first {
            return Ok(());
        }
        let handle = manager().handle(id)?;
        let key = cutex::management::v2::turn_index::Key::new(id, &status.thread_id, "live", generation);
        let mut call = |method: &str, params: serde_json::Value| {
            handle.request(method, params).map_err(|error| match error {
                cutex::app_server::client::AppServerClientError::Rpc(error) => cutex::management::v2::history::HistoryCallError::Rpc(
                    serde_json::json!({"code": error.code, "message": error.message, "data": error.data})),
                other => cutex::management::v2::history::HistoryCallError::Unavailable(other.to_string()),
            })
        };
        match cutex::management::v2::history::read_complete_turn(&key, &status.thread_id, &turn_id, &mut call) {
            Ok(Some(turn)) => serde_json::json!({"thread": {"id": status.thread_id, "turns": [turn.turn()]}}),
            Ok(None) => return Ok(()),
            Err(_) => anyhow::bail!("the lease turn {turn_id} could not be read"),
        }
    } else {
        manager().commands(id)?.thread_read(&cutex::app_server::commands::ThreadReadParams {
            thread_id: status.thread_id.clone(), include_turns: true,
        })?
    };
    if manager().status(id)?.is_some_and(|status| status.connected && status.runtime_generation == generation) {
        reconcile_observed_history(&store, &lease, &status.thread_id, &thread)?;
    }
    Ok(())
}

pub(super) fn reconcile_observed_history(
    store: &cutex::asks::store::AskStore,
    lease: &cutex::asks::model::AdmissionLease,
    thread_id: &str,
    response: &serde_json::Value,
) -> anyhow::Result<()> {
    // An absent/unreadable history is not a synthetic completion. The
    // scheduler independently handles leases from an earlier runtime generation.
    if response.pointer("/thread/id").and_then(serde_json::Value::as_str) != Some(thread_id) {
        return Ok(());
    }
    let turn = response.pointer("/thread/turns").and_then(serde_json::Value::as_array)
        .and_then(|turns| turns.iter().find(|turn| turn.get("id").and_then(serde_json::Value::as_str) == lease.turn_id.as_deref()));
    if let Some(turn) = turn {
        cutex::asks::observe::observe_bound_completion(store, lease, turn, chrono::Utc::now(),
            &|| cutex::management::v2::quota_continue::enabled_default(&lease.cutex_session_id))?;
    }
    Ok(())
}

// Rebuildable settlement sidecar failures must not abort ordinary delivery.
// Close remains independently fail-closed; this never retries or fabricates facts.
fn persist_settlement_observation(
    id: &str,
    message_id: &str,
    persist: impl FnOnce() -> anyhow::Result<bool>,
) -> bool {
    match persist() {
        Ok(_) => true,
        Err(error) => {
            eprintln!("Claude settlement persistence for {id}/{message_id}: {error:#}");
            false
        }
    }
}

/// Rebuildable evidence only. Called by the bounded post-delivery phase, never
/// from ordinary delivery/poll or while holding a session input/store guard.
fn observe_settlement_message(id: &str, message_id: &str, generation: u64) -> anyhow::Result<()> {
    let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let Some(manager) = MANAGER.get() else { return Ok(()); };
    if manager.status(id)?.is_none_or(|status| status.runtime_generation != generation) { return Ok(()); }
    observe_settlement_message_with(repository, id, message_id,
        Instant::now() + settlement::OBSERVATION_BUDGET, generation,
        |snapshot, deadline| settlement::collect_background(snapshot, deadline),
        || Ok(manager.status(id)?.is_some_and(|status| status.runtime_generation == generation)))
}

fn observe_settlement_message_with(
    repository: &cutex::management::v2::agent_bus_state::AgentBusMessageRepository,
    id: &str, message_id: &str, deadline: Instant, generation: u64,
    collect: impl FnOnce(&cutex::management::v2::agent_bus_state::AgentBusMessageSnapshot, Instant)
        -> anyhow::Result<cutex::management::v2::agent_bus_settlement_model::Observation>,
    binding_current: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let Some(snapshot) = repository.snapshot_by_message_id(message_id)? else {return Ok(());};
    anyhow::ensure!(snapshot.to_cutex_session_id == id, "settlement recipient changed");
    if cutex::management::v2::agent_bus_state::AgentBusMessageRepository::settlement_observation_failed(&snapshot, generation) {
        return Ok(());
    }
    let mut failed = false;
    let evidence = match collect(&snapshot, deadline) {
        Ok(cutex::management::v2::agent_bus_settlement_model::Observation::Terminal(evidence)) => Some(evidence),
        Ok(cutex::management::v2::agent_bus_settlement_model::Observation::Ineligible) => None,
        Err(error) => {
            eprintln!("Claude settlement observation for {id}/{message_id}: {error:#}");
            failed = true;
            None // Unavailable observations never create close eligibility.
        }
    };
    let terminal = evidence.is_some();
    let persisted = persist_settlement_observation(id, message_id, || {
        repository.observe_settlement(message_id, evidence, chrono::Utc::now())
    });
    if terminal && persisted { repository.clear_settlement_observation_failure(&snapshot)?; }
    if failed && binding_current()? {
        // CAS retains receipt/status changes that raced the failed read. No
        // automatic close, replay, discard or fabricated delivery receipt.
        repository.record_settlement_observation_failure(&snapshot, generation)?;
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
    deliver_messages_with(id,bus,attempts,
        |generation|super::asks_claude::adapter_capabilities(id,generation),claude_message_params)
}

fn deliver_messages_with(
    id:&str,bus:&cutex::agent_bus::client::AgentBusHttpClient,
    attempts:&mut HashMap<String,(u64,Instant)>,
    capabilities:impl Fn(u64)->Option<serde_json::Value>,
    project:impl Fn(&cutex::session::model::CutexSessionStore,&str,&str,&cutex::agent_bus::model::AgentBusMessage,
        Option<&cutex::management::v2::agent_bus_state::AgentBusMessageMeta>,bool)->anyhow::Result<serde_json::Value>,
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
    let capabilities = capabilities(record.runtime_generation);
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
    // Both the regular pass and event wakes share this serial observer. Its
    // outer wait handles idling; a 2 s long poll here delays every later seat.
    let polled = bus.poll_now(runtime)?;
    // 054 S10: Job Service notices, planned as the Codex bridge plans them
    // (batched, awaited ones passive). Native ingress only; not a ledger
    // feature (the ledger mapping of these writes stays behind its switch).
    let jobs_on = native;
    let (job_hold, job_batch_passive) = if jobs_on {
        cutex::app_server::bus_bridge::job_notice_plan_for(&polled, id)
    } else {
        Default::default()
    };
    for message in polled {
        // Service controls need their own typed target/seat checks and receipts.
        // A cross-host tell is an ordinary passive note with a peer control record.
        // An Agent Management start / notice is validated against its action
        // (the Codex bridge's provenance check) when its input is built.
        let management = cutex::app_server::bus_bridge::is_agent_management_message(&message);
        let job = jobs_on && message.sender_kind.is_job_service_system();
        if !submits(&message, jobs_on) {
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
        let unfrozen = repository.snapshot_by_message_id(&message.id)?.is_some_and(|snapshot| snapshot.external_input.is_none());
        // A job notice a delegation awaits, or batched behind the newest one,
        // rides along passive; the rest of a still-arriving batch waits.
        let job_passive = job && (cutex::app_server::bus_bridge::job_notice_awaited(&message, id)
            || (unfrozen && job_batch_passive.contains(&message.id)));
        if job && !job_passive && unfrozen && job_hold.contains(&message.id) { continue; }
        let built = if job {
            // The legacy payload is never sent (native only); this checks
            // the Job Service provenance before anything is frozen.
            cutex::app_server::bus_bridge::job_notice_envelope(&message, id, thread, record.runtime_generation, job_passive, false)
                .map(|_| serde_json::json!({"threadId": thread, "messageId": message.id}))
        } else {
            project(&sessions, thread, id, &message, meta.as_ref(), v2)
        };
        let payload = match built {
            Ok(payload) => payload,
            Err(error) if management || job => {
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
        if native&&!job&&unfrozen {
            // A malformed/unsupported projection is a message failure, not a
            // broken adapter connection. Validate before any native request or
            // turn admission so it cannot put the whole seat into backoff.
            if let Err(error)=super::claude_ingress::envelope(id,thread,record.runtime_generation,&payload) {
                if !cutex::app_server::bus_bridge::quarantine_invalid_external_projection(id,&message,&format!("{error:#}"))? {
                    return Err(error);
                }
                bus.ack(runtime,&[message.id.clone()])?;
                eprintln!("warning: Claude message {} projection quarantined: {error:#}",message.id);
                continue;
            }
        }
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
        if repository.owner_delivery_blocked(id, &message.id)? {
            // A carried identity belongs to its owner's atomic request. The
            // observer may recover genuine native proof, but must never submit
            // an Unknown carried item as an independent turn or passive note.
            if let Some(envelope) = repository.snapshot_by_message_id(&message.id)?.and_then(|snapshot| snapshot.external_input) {
                reconcile_native_message(id, record.runtime_generation, &envelope)?;
                let snapshot = repository.snapshot_by_message_id(&message.id)?;
                if snapshot.as_ref().is_some_and(|snapshot| snapshot.state == "delivered") {
                    bus.ack(runtime, &[message.id.clone()])?;
                } else if manager().owner_carry_return_supported(id, record.runtime_generation)
                    && repository.owner_carried(id, &message.id)?
                    && snapshot.and_then(|snapshot| snapshot.external_input_last_observed).is_none_or(|status| super::claude_ingress::may_be_returned(&status))
                    && super::claude_ingress::return_carry(&handle, &envelope, record.runtime_generation)?
                    && repository.return_unread_owner_carry(id, &message.id, &envelope)? {
                    super::owner_carries::OwnerCarries::note_returned(id, &message.id);
                    wake_delivery(id);
                }
            }
            continue;
        }
        let active = manager().status(id)?.filter(|status| status.connected && status.runtime_generation == record.runtime_generation);
        let busy_turn = active.as_ref().and_then(|status| status.active_turn_id.as_deref());
        let wait = claude_task_wait(&message, cutex::agent_bus::delivery::busy_wait_for(&message));
        busy_delivery::note(&message, wait, busy_turn);
        let age = Duration::from_secs((chrono::Utc::now().timestamp().max(0) as u64).saturating_sub(message.created_at_epoch_secs));
        let timing = busy_delivery::timing(age, wait, active.as_ref().map(|status| status.active_turn_id.is_some()),
            frozen.is_none() && existing.is_none() && !job_passive
                && busy_delivery::candidate(&message, capabilities.as_ref().is_some_and(super::claude_ingress::offers_soon)));
        if timing == cutex::agent_bus::delivery::WakeTiming::Hold { continue; }
        let mut already_accepted = existing.is_some();
        // A passive job notice rides along (no turn of its own), as on Codex.
        let triggers_turn = message.delivery_mode.trigger_turn() && !job_passive;
        if existing.is_none() && triggers_turn {
            let accepted = match frozen.as_ref() {
                Some(envelope) => {
                    let status = super::claude_ingress::status(&handle, envelope, record.runtime_generation)?;
                    // 054 RC8: keep what the native runtime said (held / retry / conflict).
                    if cutex::management::v2::agent_bus_state::input_ledger_active() {
                        repository.observe_external_input(&message.id, envelope, &status)?;
                        // Still withheld by the runtime: no new sending, no
                        // local inbox receipt until it takes the input.
                        if native_withheld(&status) { continue; }
                    }
                    status.delivery_state != cutex::app_server::external_input::DeliveryState::Unknown
                }
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
        let prepared_turn = (busy_turn.is_none() && !already_accepted && triggers_turn)
            .then(|| cutex::management::v2::admission::prepare_turn(id, "delivery", None));
        let mut receipt = if let Some(existing) = existing {
            existing
        } else if native || frozen.is_some() {
            ensure!(native, "native Claude input was frozen but this runtime lacks its protocol; no legacy fallback");
            let frozen_input = repository.freeze_external_input(id, &message.id, |canonical| {
                if job {
                    return cutex::app_server::bus_bridge::job_notice_envelope(canonical, id, thread, record.runtime_generation,
                        job_passive, timing == cutex::agent_bus::delivery::WakeTiming::NextStep);
                }
                let mut projection = project(&sessions, thread, id, canonical, meta.as_ref(), true)?;
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
                Err(error) if (management || job)
                    && cutex::app_server::bus_bridge::quarantine_terminal_external_input(id, &message, &error)? => {
                    bus.ack(runtime, &[message.id.clone()])?;
                    eprintln!("warning: Claude Agent Management message {} quarantined: {error:#}", message.id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            ensure!(envelope.owner_id == id && envelope.thread_id == thread, "frozen Claude input binding changed");
            // 054 §2.9: frozen, native IO starts now.
            repository.input_ledger_sending(id, &message.id);
            let submitted = super::claude_ingress::submit(&handle, &envelope, record.runtime_generation);
            // 054 RC8 (S12): the local inbox receipt says nothing about a
            // native hold / retry / conflict. With the ledger on, read the
            // native status once and keep it; a withheld input gets no
            // receipt and no ack (the next sweep checks it again).
            if cutex::management::v2::agent_bus_state::input_ledger_active() {
                if let Ok(status) = super::claude_ingress::status(&handle, &envelope, record.runtime_generation) {
                    repository.observe_external_input(&message.id, &envelope, &status)?;
                    if native_withheld(&status) { continue; }
                }
            }
            submitted?
        } else { repository.input_ledger_sending(id, &message.id); match handle.request("claude/input/submit", payload) {
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

/// Whether the Claude observer submits a polled bus message (the rest stay
/// on the bus): agent messages and alarms, cross-host tells, Claude task
/// notices, Agent Management messages and, with `jobs` (native ingress),
/// Job Service notices.
fn submits(message: &cutex::agent_bus::model::AgentBusMessage, jobs: bool) -> bool {
    let management = cutex::app_server::bus_bridge::is_agent_management_message(message);
    let job = jobs && message.sender_kind.is_job_service_system();
    (message.sender_kind.is_agent() || message.sender_kind.is_alarm_system() || job)
        && (message.control_type.is_none() || cutex::agent_bus::peer_tell::is_peer_tell(message)
            || cutex::agent_bus::claude_task::is_claude_task(message)
            || cutex::agent_bus::rename_notice::is_notice(message) || management || job)
        && message.kind == cutex::agent_bus::model::AgentBusEnvelopeKind::Message
}

/// The native runtime holds the input, asks for a retry, or refuses it
/// (RC8): not a context receipt, whatever the local inbox says.
fn native_withheld(status: &cutex::app_server::external_input::Status) -> bool {
    use cutex::app_server::external_input::{DeliveryState, ProcessingState};
    matches!(status.delivery_state, DeliveryState::RetryableError | DeliveryState::Conflict)
        || status.processing.state == ProcessingState::Held
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
    if cutex::agent_bus::rename_notice::is_notice(message) {
        anyhow::ensure!(v2,"passive formal-name notices need a Claude runtime with passive input support");
        anyhow::ensure!(message.to_cutex_session_id.as_deref()==Some(id),"rename notice recipient mismatch");
        let text=cutex::agent_bus::rename_notice::model_text(message)?;
        return Ok(serde_json::json!({"threadId":thread,"messageId":message.id,
            "source":{"kind":"service","id":cutex::agent_bus::rename_notice::SOURCE_ID},
            "eventType":cutex::agent_bus::rename_notice::EVENT_TYPE,"text":text,
            "metadata":{"sourceName":cutex::agent_bus::rename_notice::SOURCE_ID,"cutexSessionId":id,"deliveryMode":"passive"}}));
    }
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
    DELIVERY_WAKE_TARGETS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
    DELIVERY_WAKE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
    invalidate_settlement_observations(id);
    let _guard = OBSERVATION_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Claude observer lock poisoned"))?;
    manager().disconnect(id)?;
    Ok(())
}

/// Claude sessions to deliver bus messages to now (054 S24): a message was
/// queued for them, or their turn ended. Not a ledger feature: no ledger
/// state is read or written here; the regular 5 s pass stays the backstop.
static DELIVERY_WAKE: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static DELIVERY_WAKE_SIGNAL: Condvar = Condvar::new();
/// Some Claude session took deliveries at the observer's last pass: only
/// then does [`follow_queued_messages`] keep its event subscription (none,
/// and so no shared-journal following, on a host without Claude agents).
static DELIVERY_WAKE_TARGETS: Mutex<BTreeMap<String, RuntimeIdentity>> = Mutex::new(BTreeMap::new());

/// Recheck the observer's successful occurrences before scheduling a retry.
/// A stop, retirement or replacement invalidates its old delayed wake even
/// while the regular observer has not reached its next candidate scan.
fn wanted_delivery_targets() -> BTreeMap<String, u64> {
    let targets = DELIVERY_WAKE_TARGETS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    if targets.is_empty() { return BTreeMap::new(); }
    let Ok(sessions) = cutex::session::store::load_cutex_session_store_shared() else { return BTreeMap::new() };
    targets.into_iter().filter_map(|(id, identity)| {
        let record = sessions.sessions.get(&id)?;
        let same = !record.is_retired() && record.runtime_generation == identity.generation
            && record.runtime_pid == Some(identity.pid)
            && record.claude_session.as_ref()?.connection_id == identity.connection_id;
        let connected = manager().status(&id).ok().flatten().is_some_and(|status|
            status.connected && status.runtime_generation == identity.generation);
        (same && connected).then_some((id, identity.generation))
    }).collect()
}

pub(super) fn wake_delivery(id: &str) {
    let mut woken = DELIVERY_WAKE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if woken.insert(id.to_string()) {
        DELIVERY_WAKE_SIGNAL.notify_one();
    }
}

/// Sessions woken before `until` (empty: the pass is due).
fn wait_for_delivery_wake(until: Instant) -> Vec<String> {
    let mut woken = DELIVERY_WAKE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if !woken.is_empty() {
            return std::mem::take(&mut *woken).into_iter().collect();
        }
        let now = Instant::now();
        if now >= until {
            return Vec::new();
        }
        woken = DELIVERY_WAKE_SIGNAL.wait_timeout(woken, until - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner).0;
    }
}

/// Sessions woken so far, without waiting (r275: served inside the pass).
fn take_delivery_wakes() -> Vec<String> {
    let mut woken = DELIVERY_WAKE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *woken).into_iter().collect()
}

/// One step of an observer pass.
enum PassStep<'a> {
    /// The regular visit of a candidate (observe, then deliver).
    Visit(&'a str),
    /// A delivery for a session woken meanwhile.
    Woken(String),
}

/// Wall time of one observer pass and its slowest visit (r275 timing log).
struct PassTiming {
    started: Instant,
    sessions: usize,
    woken: usize,
    slowest: Option<(String, Duration)>,
}

/// A pass at least this long is logged.
const SLOW_PASS: Duration = Duration::from_secs(2);
/// At most one slow-pass line per this interval.
const PASS_LOG_EVERY: Duration = Duration::from_secs(600);

impl PassTiming {
    fn start() -> Self {
        Self { started: Instant::now(), sessions: 0, woken: 0, slowest: None }
    }

    fn visited(&mut self, id: &str, took: Duration) {
        self.sessions += 1;
        if self.slowest.as_ref().is_none_or(|(_, slowest)| took > *slowest) {
            self.slowest = Some((id.to_string(), took));
        }
    }

    /// The stderr line of a pass over [`SLOW_PASS`], else `None`.
    fn slow_line(&self, total: Duration) -> Option<String> {
        if total <= SLOW_PASS {
            return None;
        }
        let (id, took) = self.slowest.as_ref().map_or(("-", Duration::ZERO), |(id, took)| (id.as_str(), *took));
        Some(format!("Claude observer pass: {} ms, {} sessions, {} woken served; slowest {id} {} ms",
            total.as_millis(), self.sessions, self.woken, took.as_millis()))
    }
}

/// Visits `ids` in order. Before each visit it serves the sessions woken
/// meanwhile (`take_wakes`), so a wake raised during a long pass waits for
/// the current visit only, not for the whole pass. A session already
/// visited in this pass is delivered again when woken.
fn visit_serving_wakes(
    ids: &[String],
    timing: &mut PassTiming,
    mut take_wakes: impl FnMut() -> Vec<String>,
    mut step: impl FnMut(PassStep<'_>),
) {
    for id in ids {
        for woken in take_wakes() {
            timing.woken += 1;
            step(PassStep::Woken(woken));
        }
        let at = Instant::now();
        step(PassStep::Visit(id));
        timing.visited(id, at.elapsed());
    }
}

/// A woken delivery: only for a session whose last pass delivered (one in
/// backoff waits for its regular visit). Same gates as every delivery.
fn deliver_woken(
    id: &str,
    healthy: &mut HashSet<String>,
    bus: &cutex::agent_bus::client::AgentBusHttpClient,
    attempts: &mut HashMap<String, (u64, Instant)>,
) {
    if !healthy.contains(id) || !wanted_delivery_targets().contains_key(id) {
        return;
    }
    if let Err(error) = deliver_messages(id, bus, attempts) {
        eprintln!("Claude delivery on wake for {id}: {error:#}");
        healthy.remove(id);
        DELIVERY_WAKE_TARGETS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
    }
}

// The durable queued event precedes the bus's in-memory enqueue. A
// non-waiting poll can therefore miss it; retry once after this interval.
const QUEUED_WAKE_AGAIN: Duration = Duration::from_millis(250);

struct PendingDeliveryWake {
    generation: u64,
    at: Instant,
    last: Instant,
}

#[derive(Default)]
struct QueuedDeliveryWakes(BTreeMap<String, PendingDeliveryWake>);

impl QueuedDeliveryWakes {
    fn retain(&mut self, wanted: &BTreeMap<String, u64>) {
        self.0.retain(|id, pending| wanted.get(id) == Some(&pending.generation));
    }
    fn queued(&mut self, id: &str, generation: u64, now: Instant) {
        let at = now + QUEUED_WAKE_AGAIN;
        // One entry per wanted occurrence, independent of message volume.
        // Preserve the first deadline under a continuous stream; remember
        // its tail too so the final event still gets a full delayed retry.
        self.0.entry(id.to_string()).and_modify(|pending| pending.last = at)
            .or_insert(PendingDeliveryWake { generation, at, last: at });
    }
    fn due(&mut self, now: Instant) -> Vec<String> {
        let mut ready = Vec::new();
        self.0.retain(|id, pending| {
            if pending.at > now { return true; }
            ready.push(id.clone());
            if pending.last > now {
                pending.at = pending.last;
                true
            } else { false }
        });
        ready
    }
    fn wait(&self, now: Instant) -> Duration {
        self.0.values().map(|pending| pending.at.saturating_duration_since(now))
            .min().unwrap_or(Duration::from_millis(500)).min(Duration::from_millis(500))
    }
}

fn wake_for_event(event: &cutex::management::v2::model::EventEnvelope,
    wanted: &BTreeMap<String, u64>, again: &mut QueuedDeliveryWakes) {
    if let Some(&generation) = wanted.get(&event.cutex_session_id).filter(|_|
        event.cutex.as_ref().is_some_and(|cutex| cutex.method == "cutex/agentBus/messageQueued")) {
        wake_delivery(&event.cutex_session_id);
        again.queued(&event.cutex_session_id, generation, Instant::now());
    }
}

/// Follows the Management event stream (other processes' events arrive via
/// the shared journal watcher) from now on while `wanted()`, waking delivery
/// for every bus message queued. Never returns. A lagged or lost
/// subscription only costs the wake (the 5 s pass delivers).
fn follow_queued_messages(repository: &cutex::management::v2::repository::EventRepository, wanted: impl Fn() -> BTreeMap<String, u64>) {
    let mut subscription: Option<cutex::management::v2::repository::EventSubscription> = None;
    let mut again = QueuedDeliveryWakes::default();
    loop {
        let wanted = wanted();
        again.retain(&wanted);
        if wanted.is_empty() {
            subscription = None;
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        for id in again.due(Instant::now()) { wake_delivery(&id); }
        let Some(live) = subscription.as_ref() else {
            let subscribed = repository.checkpoint().ok().and_then(|checkpoint| {
                repository.page_and_subscribe(cutex::management::v2::repository::ReplayQuery {
                    stream_id: Some(checkpoint.stream_id.clone()),
                    after: checkpoint.cursor.clone(),
                    limit: 1,
                    cutex_session_id: None,
                }, 1024).ok()
            });
            if subscribed.is_none() {
                std::thread::sleep(Duration::from_secs(1));
            }
            subscription = subscribed;
            continue;
        };
        let lost = match live.receiver.recv_timeout(again.wait(Instant::now())) {
            Ok(event) => {
                wake_for_event(&event, &wanted, &mut again);
                for event in live.receiver.try_iter().take(1024) {
                    wake_for_event(&event, &wanted, &mut again);
                }
                false
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
        };
        if lost || live.lagged.load(Ordering::SeqCst) {
            subscription = None;
        }
    }
}

pub(super) fn spawn_observer() -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("cutex-claude-delivery-wake".into())
        .spawn(|| match management_v2_repository() {
            Ok(repository) => follow_queued_messages(repository, wanted_delivery_targets),
            Err(error) => eprintln!("Claude delivery wake: {error:#}"),
        })?;
    cutex::management::v2::agent_bus_settlement_model::register_collector(settlement::collect);
    cutex::management::v2::agent_bus_settlement_model::register_runtime_fence(settlement::runtime_fence);
    let settlement_worker = settlement::ObservationWorker::spawn(|id, message, generation| {
        if let Err(error) = observe_settlement_message(id, message, generation) {
            eprintln!("Claude settlement source for {id}/{message}: {error:#}");
        }
    })?;
    std::thread::Builder::new()
        .name("cutex-claude-observer".into())
        .spawn(move || {
            let mut store_failure = None;
            let mut last_pass_log: Option<Instant> = None;
            let mut reconciliation_attempts = HashMap::<String, (u64, Instant)>::new();
            let mut previous_candidates = HashMap::<String, RuntimeIdentity>::new();
            let mut outbox_attempt = None::<Instant>;
            let mut settlement_observations = settlement::BackgroundObservations::default();
            // Sessions whose last pass delivered (a woken delivery skips the
            // ones in backoff; the regular pass handles those).
            let mut healthy = HashSet::<String>::new();
            loop {
                let mut timing = PassTiming::start();
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
                            healthy.remove(&removed);
                            let _ = disconnect(&removed);
                            reconciliation_attempts.remove(&removed);
                            eprintln!("Claude observer inactive for {removed}: no eligible local live runtime");
                        }
                        settlement_observations.retain_candidates(&wanted.keys().cloned().collect::<Vec<_>>());
                        settlement_observations.after_deliveries(settlement::background_enabled(), || {
                        let mut delivered = Vec::new();
                        let ids = wanted.keys().cloned().collect::<Vec<_>>();
                        visit_serving_wakes(&ids, &mut timing, take_delivery_wakes, |step| {
                            let id = match step {
                                PassStep::Woken(id) => {
                                    deliver_woken(&id, &mut healthy, &bus, &mut reconciliation_attempts);
                                    return;
                                }
                                PassStep::Visit(id) => id,
                            };
                            if !with_state(|state| state.due(id, Instant::now())) { return; }
                            with_state(|state| state.attempted(id));
                            let observation = observe_inner(id);
                            let stage = if observation.is_err() { "observe" } else { "delivery" };
                            let result = observation.and_then(|()| deliver_messages(id, &bus, &mut reconciliation_attempts));
                            match result {
                                Ok(()) => {
                                    record_observation_result(id, wanted.get(id), stage, &Ok(()));
                                    healthy.insert(id.to_string());
                                    delivered.push(id.to_string());
                                },
                                Err(error) => {
                                    healthy.remove(id);
                                    // Includes an offline occurrence discovered
                                    // after the candidate scan, and failed
                                    // connection checks while the PID survives.
                                    invalidate_settlement_observations(id);
                                    let offline = error.downcast_ref::<cutex::runtime::claude::ClaudeObservationOffline>().is_some();
                                    record_observation_result(id, wanted.get(id), stage, &Err(error));
                                    if offline { reconciliation_attempts.remove(id); return; }
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
                                        }
                                        Ok(false) => {}
                                        Err(_) => {} // Preserve the original failure and its retry state.
                                    }
                                }
                            }
                        });
                        delivered
                        }, |id| {
                            cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
                                .pending_native_envelopes(id)
                                .map(|envelopes| envelopes.into_iter().map(|envelope| envelope.message.id).collect())
                        }, |id, message, _scheduling_deadline| {
                            if let Some(identity) = previous_candidates.get(id) {
                                settlement_worker.schedule(id, message, identity.generation);
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
                if let Some(line) = timing.slow_line(timing.started.elapsed()) {
                    // A pass that served a wake always logs; other slow
                    // passes log once per PASS_LOG_EVERY. Bus probes do not
                    // wait, but observation or delivery RPCs may still stall.
                    if timing.woken > 0 || last_pass_log.is_none_or(|at: Instant| at.elapsed() >= PASS_LOG_EVERY) {
                        eprintln!("{line}");
                        last_pass_log = Some(Instant::now());
                    }
                }
                *DELIVERY_WAKE_TARGETS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = healthy.iter()
                    .filter_map(|id| previous_candidates.get(id).map(|identity| (id.clone(), identity.clone()))).collect();
                // The next pass in 5 s; meanwhile deliver at once to a
                // session a bus message was queued for or whose turn ended.
                let until = Instant::now() + Duration::from_secs(5);
                loop {
                    let woken = wait_for_delivery_wake(until);
                    if woken.is_empty() {
                        break;
                    }
                    let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(
                        &cutex::config::store::load_codez_config(),
                    );
                    for id in woken {
                        deliver_woken(&id, &mut healthy, &bus, &mut reconciliation_attempts);
                    }
                }
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
    if !matches!(notification.method.as_str(), "item/started" | "item/completed") {
        return Ok(());
    }
    let item = &params["item"];
    if item["type"] != "functionCallOutput" || item["name"] != "external_event" {
        return Ok(());
    }
    let (Some(message), Some(turn)) = (item["id"].as_str(), params["turnId"].as_str()) else {
        return Ok(());
    };
    let repository = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let snapshot = repository.snapshot_by_message_id(message)?;
    if let Some(envelope) = snapshot.as_ref().and_then(|snapshot| snapshot.external_input.as_ref()) {
        // Native r8 timeline items are an audit projection at claim time:
        // the bus waits for the separately validated native context-persisted
        // receipt. r274.2 §9.1: the ledger record moves to the timeline now
        // (`accepted`, this turn, this item), any view and delivery mode
        // (notes, alarms, notices, Claude Tasks, peer tells; claude-r35 also
        // emits busy `soon` items at claim time).
        if envelope.owner_id == id && params["threadId"].as_str() == Some(envelope.thread_id.as_str()) {
            repository.input_ledger_native_item(id, message, turn)?;
        }
        return Ok(());
    }
    if snapshot.is_none() {
        // Not a bus message: a delegation results event (its native message
        // id), e.g. a `soon` result the claude-r35 hook claimed.
        cutex::asks::ledger::results_item_shown(id, message, turn);
        return Ok(());
    }
    if notification.method != "item/completed" {
        return Ok(());
    }
    if !matches!(item.pointer("/externalInputView/schema").and_then(serde_json::Value::as_str),
        Some(cutex::agent_bus::projection::AGENT_MESSAGE_VIEW_SCHEMA | cutex::agent_bus::alarms::ALARM_VIEW_SCHEMA))
    {
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
    fn reconnect_history_replays_only_exact_terminal_bound_turn() {
        use cutex::asks::model::{AdmissionLease, LeaseHolder};
        use serde_json::json;
        let root = std::env::temp_dir().join(format!("claude-reconnect-{}", uuid::Uuid::new_v4()));
        let repository = cutex::management::v2::user_input::UserInputRepository::open(&root).unwrap();
        let store = repository.ask_store();
        let lease = AdmissionLease {
            cutex_session_id: "reconnect-owner".into(), holder: LeaseHolder::Owner,
            native_message_id: "old-message".into(), runtime_generation: 7,
            turn_id: Some("old-turn".into()), since: chrono::Utc::now().to_rfc3339(),
        };
        for response in [
            json!({"thread":{"id":"other","turns":[{"id":"old-turn","status":"completed"}]}}),
            json!({"thread":{"id":"thread","turns":[{"id":"old-turn","status":"inProgress"}]}}),
            json!({"thread":{"id":"thread","turns":[{"id":"other","status":"completed"}]}}),
            json!({"thread":{"id":"thread","turns":[]}}),
            json!({"thread":{"id":"thread","turns":[{"id":"old-turn"}]}}),
        ] {
            store.with_state(|state| { state.leases.insert(lease.cutex_session_id.clone(), lease.clone()); }).unwrap();
            reconcile_observed_history(&store, &lease, "thread", &response).unwrap();
            assert_eq!(store.inspect(|s| s.leases.get(&lease.cutex_session_id).cloned()).unwrap(), Some(lease.clone()));
        }
        let completed = json!({"thread":{"id":"thread","turns":[{"id":"old-turn","status":"completed"}]}});
        store.with_state(|state| state.leases.get_mut(&lease.cutex_session_id).unwrap().native_message_id = "replacement".into()).unwrap();
        reconcile_observed_history(&store, &lease, "thread", &completed).unwrap();
        assert_eq!(store.inspect(|s| s.leases[&lease.cutex_session_id].native_message_id.clone()).unwrap(), "replacement");
        store.with_state(|state| { state.leases.insert(lease.cutex_session_id.clone(), lease.clone()); }).unwrap();
        reconcile_observed_history(&store, &lease, "thread", &completed).unwrap();
        assert!(!store.inspect(|s| s.leases.contains_key(&lease.cutex_session_id)).unwrap());
        // A duplicate history read is harmless.
        reconcile_observed_history(&store, &lease, "thread", &completed).unwrap();
        drop(store);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

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
                if let Ok(request) = cutex::http::server::read_simple_http_request(&mut stream) {
                    assert!(request.path.contains("ack=1&wait_ms=0"), "observer must not long-poll: {}", request.path);
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
    fn invalid_native_projection_is_quarantined_before_later_message_processing() {
        use serde_json::json;
        use cutex::management::v2::agent_bus_state::{agent_bus_message_repository,AgentBusQueuedMessage};
        use cutex::session::{model::CutexSessionRecord,store};
        let owner=format!("cutex.{}",uuid::Uuid::new_v4());
        let mut record=CutexSessionRecord::new(owner.clone(),Some("projection-thread".into()),"private".into(),"/private".into(),None).unwrap();
        record.current_runtime_agent_id=Some("projection-runtime".into());record.runtime_generation=1;
        let path=store::cutex_sessions_path().unwrap();
        store::with_locked_session_rows(&path,|rows|{rows.insert_session(&owner,record);rows.commit()?;Ok(())}).unwrap();
        let messages:Vec<cutex::agent_bus::model::AgentBusMessage>=(0..2).map(|n|serde_json::from_value(json!({
            "id":format!("projection-{n}-{}",uuid::Uuid::new_v4()),"kind":"message","from":"sender","to":"projection-runtime",
            "fromCutexSessionId":"cutex.sender","toCutexSessionId":owner,"content":"unchanged canonical content",
            "deliveryMode":"passive","triggerTurn":false,"createdAtEpochSecs":1,"senderKind":"agent"
        })).unwrap()).collect();
        let repository=agent_bus_message_repository().unwrap();
        for message in &messages {
            let params=cutex::app_server::bus_bridge::inter_agent_params("projection-thread","receiver",&owner,message).unwrap();
            repository.record_queued(AgentBusQueuedMessage {owner_cutex_session_id:owner.clone(),message_id:message.id.clone(),
                from_cutex_session_id:message.from_cutex_session_id.clone(),to_cutex_session_id:owner.clone(),
                from_runtime_agent_id:None,to_runtime_agent_id:Some("projection-runtime".into()),delivery_mode:"passive".into(),
                content:message.content.clone(),queued_at:chrono::Utc::now(),canonical_envelope:message.clone(),
                semantic_sha256:cutex::app_server::bus_bridge::inter_agent_semantic_sha256(&params)}).unwrap();
        }
        let listener=std::net::TcpListener::bind(("127.0.0.1",0)).unwrap();
        let port=listener.local_addr().unwrap().port();listener.set_nonblocking(true).unwrap();
        let wire=messages.clone();let acknowledgments=Arc::new(Mutex::new(Vec::<String>::new()));let acked=acknowledgments.clone();
        let server=std::thread::spawn(move|| {
            let deadline=Instant::now()+Duration::from_secs(5);
            while Instant::now()<deadline {
                let Ok((mut stream,_))=listener.accept() else {std::thread::sleep(Duration::from_millis(10));continue};
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let request=cutex::http::server::read_simple_http_request(&mut stream).unwrap();
                let reply=if request.method=="GET" {json!({"messages":wire})} else {
                    let body:serde_json::Value=serde_json::from_slice(&request.body).unwrap();
                    acked.lock().unwrap().extend(body["messageIds"].as_array().unwrap().iter().map(|id|id.as_str().unwrap().to_owned()));
                    json!({"ok":true,"acked":1})
                };
                cutex::http::server::write_json_response(&mut stream,200,"OK",&reply).unwrap();
                if !acked.lock().unwrap().is_empty() {break}
            }
        });
        let bus=cutex::agent_bus::client::AgentBusHttpClient::new(format!("http://127.0.0.1:{port}"),None);
        let projected=std::cell::RefCell::new(Vec::new());
        let error=deliver_messages_with(&owner,&bus,&mut HashMap::new(),|_|Some(json!({
            "externalInputVersion":1,"externalInputVersions":[1,2],"externalInputDeliveries":["after_turn","passive"],
            "input":{"views":true,"deliveryModes":["passive"]}
        })),|sessions,thread,id,message,meta,v2| {
            projected.borrow_mut().push(message.id.clone());
            if message.id==messages[0].id {return Ok(json!({"threadId":thread,"messageId":message.id,
                "source":{"kind":"service","id":"Future Service"},"eventType":"unrecognized_future_projection",
                "text":"never sent","metadata":{"deliveryMode":"passive"}}))}
            claude_message_params(sessions,thread,id,message,meta,v2)
        }).unwrap_err();
        server.join().unwrap();
        assert_eq!(*projected.borrow(),messages.iter().map(|message|message.id.clone()).collect::<Vec<_>>());
        assert!(error.to_string().contains("runtime generation changed: expected 1, actual none"),
            "the later valid input reached its native generation fence: {error:#}");
        assert_eq!(*acknowledgments.lock().unwrap(),vec![messages[0].id.clone()]);
        let invalid=repository.snapshot_by_message_id(&messages[0].id).unwrap().unwrap();
        assert_eq!(invalid.state,"quarantined");assert_eq!(invalid.error.unwrap()["code"],"external_projection_invalid");
        assert_eq!(repository.snapshot_by_message_id(&messages[1].id).unwrap().unwrap().state,"pending");
        for message in &messages {
            assert_eq!(serde_json::to_value(repository.canonical_message(&message.id).unwrap()).unwrap(),serde_json::to_value(message).unwrap());
        }
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

#[cfg(test)]
mod delivery_wake_tests {
    use super::*;

    /// The tests in this process that wake or take the shared wake set
    /// (`isolated_*` ones run in a child process).
    static WAKE_SET: Mutex<()> = Mutex::new(());

    fn wake_set() -> std::sync::MutexGuard<'static, ()> {
        WAKE_SET.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn queued_wakes_coalesce_bursts_without_postponing_the_first_or_losing_the_tail() {
        let now = Instant::now();
        let mut again = QueuedDeliveryWakes::default();
        for _ in 0..100_000 { again.queued("a", 1, now); }
        again.queued("b", 2, now);
        assert_eq!(again.0.len(), 2);
        again.queued("a", 1, now + Duration::from_millis(249));
        assert!(again.due(now + Duration::from_millis(249)).is_empty());
        assert_eq!(again.due(now + QUEUED_WAKE_AGAIN), vec!["a", "b"]);
        assert_eq!(again.0.len(), 1);
        assert_eq!(again.wait(now + QUEUED_WAKE_AGAIN), Duration::from_millis(249));
        assert_eq!(again.due(now + Duration::from_millis(499)), vec!["a"]);
        assert!(again.0.is_empty());
    }

    #[test]
    fn queued_wakes_cancel_removed_or_replaced_occurrences_before_waking() {
        let now = Instant::now();
        let mut again = QueuedDeliveryWakes::default();
        for (id, generation) in [("stopped", 1), ("replaced", 1), ("active", 2)] {
            again.queued(id, generation, now);
        }
        again.retain(&BTreeMap::from([("replaced".to_string(), 2), ("active".to_string(), 2)]));
        assert_eq!(again.due(now + QUEUED_WAKE_AGAIN), vec!["active"]);
        again.queued("active", 2, now);
        again.retain(&BTreeMap::new());
        assert!(again.due(now + QUEUED_WAKE_AGAIN).is_empty());
    }

    /// 054 S24: the observer waits at most until its next pass, and returns
    /// at once with the sessions woken meanwhile (from any thread).
    #[test]
    fn delivery_wake_returns_woken_sessions_or_the_pass_deadline() {
        let _wake_set = wake_set();
        let started = Instant::now();
        assert!(wait_for_delivery_wake(started + Duration::from_millis(60)).is_empty());
        assert!(started.elapsed() >= Duration::from_millis(60));
        let waker = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            let mut woken = DELIVERY_WAKE.lock().unwrap();
            woken.insert("cutex.wake-b".into());
            woken.insert("cutex.wake-a".into());
            DELIVERY_WAKE_SIGNAL.notify_one();
        });
        let started = Instant::now();
        let woken = wait_for_delivery_wake(started + Duration::from_secs(10));
        waker.join().unwrap();
        assert_eq!(woken, ["cutex.wake-a", "cutex.wake-b"]);
        assert!(started.elapsed() < Duration::from_secs(5), "woken, not the pass deadline");
        assert!(wait_for_delivery_wake(Instant::now()).is_empty(), "taken once");
    }

    /// r275: a wake raised while the pass visits session 1 of 3 is served
    /// before session 2 (not after the whole pass), and a session already
    /// visited in this pass is delivered again when woken. Only this test's
    /// own ids are taken from the shared wake set.
    #[test]
    fn a_wake_during_a_visit_is_served_before_the_next_session() {
        let _wake_set = wake_set();
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let ids = (1..=3).map(|n| format!("cutex.pass-{tag}-{n}")).collect::<Vec<_>>();
        let mine = ids.clone();
        let take = || {
            let mut woken = DELIVERY_WAKE.lock().unwrap();
            mine.iter().filter(|id| woken.remove(id.as_str())).cloned().collect::<Vec<_>>()
        };
        let mut steps = Vec::new();
        let mut timing = PassTiming::start();
        visit_serving_wakes(&ids, &mut timing, take, |step| match step {
            PassStep::Visit(id) => {
                steps.push(format!("visit {id}"));
                if id == ids[0] { wake_delivery(&ids[2]); }
                if id == ids[1] {
                    std::thread::sleep(Duration::from_millis(40));
                    wake_delivery(&ids[0]);
                }
            }
            PassStep::Woken(id) => steps.push(format!("woken {id}")),
        });
        assert_eq!(steps, [
            format!("visit {}", ids[0]), format!("woken {}", ids[2]), format!("visit {}", ids[1]),
            format!("woken {}", ids[0]), format!("visit {}", ids[2]),
        ]);
        assert_eq!((timing.sessions, timing.woken), (3, 2));
        assert_eq!(timing.slowest.as_ref().map(|(id, _)| id.as_str()), Some(ids[1].as_str()));
        // One line, only for a pass over 2 s.
        assert_eq!(timing.slow_line(Duration::from_millis(1999)), None);
        assert_eq!(timing.slow_line(Duration::from_secs(2)), None);
        let line = timing.slow_line(Duration::from_millis(2500)).unwrap();
        let slowest = timing.slowest.as_ref().unwrap().1.as_millis();
        assert_eq!(line, format!("Claude observer pass: 2500 ms, 3 sessions, 2 woken served; slowest {} {slowest} ms", ids[1]));
        assert!(slowest >= 40);
    }

    /// r274.2: a queued Claude Task notice wakes the parent's delivery (a
    /// system sender emits no `messageQueued`); a failed send or a frame
    /// without a valid notice wakes nothing. Only this test's own id is
    /// taken from the wake set.
    #[test]
    fn a_queued_claude_task_notice_wakes_the_parent() {
        let _wake_set = wake_set();
        let parent = format!("cutex.ctask-wake-{}", uuid::Uuid::new_v4());
        let woken = |id: &str| DELIVERY_WAKE.lock().unwrap().remove(id);
        let finished = serde_json::json!({"threadId": "t", "task": {"background": true, "status": "completed",
            "taskId": "afa1b240b52a47bb4", "taskType": "local_agent",
            "notifyParent": {"description": "bg", "noticeId": "afa1b240b52a47bb4", "originTurnId": "3216c31d",
                "status": "completed", "summary": "bg-done", "summaryTruncated": false, "taskId": "afa1b240b52a47bb4",
                "taskType": "local_agent", "toolUseId": "toolu_01Fr", "transcript": "claude:3c57b94d:task:afa1b240b52a47bb4"}}});
        assert!(notify_parent_with(&parent, &finished, |_| Err(anyhow::anyhow!("bus down"))).is_err());
        assert!(!woken(&parent), "a notice the bus did not take wakes nothing");
        assert!(notify_parent_with(&parent, &serde_json::json!({"task": {}}), |_| panic!("no notice, no send")).is_ok());
        assert!(!woken(&parent));
        let mut sent = None;
        notify_parent_with(&parent, &finished, |request| {
            sent = request.external_message_id.clone();
            Ok(())
        }).unwrap();
        assert_eq!(sent, Some(cutex::agent_bus::claude_task::message_id(&parent, "afa1b240b52a47bb4")));
        assert!(woken(&parent), "the queued notice wakes the parent's delivery");
    }

    const JOB_OWNER: &str = "cutex.11111111-1111-4111-8111-111111111111";

    fn job_notice(id: &str) -> cutex::agent_bus::model::AgentBusMessage {
        use cutex::agent_bus::model::{JobServiceCompletionRequest, JobServiceTerminalStatus, JOB_SERVICE_COMPLETION_SCHEMA};
        let request = JobServiceCompletionRequest {
            schema: JOB_SERVICE_COMPLETION_SCHEMA.to_string(), event_id: format!("event-{id}"), job_id: format!("job-{id}"),
            job_revision: 1, terminal_status: JobServiceTerminalStatus::Exited, result_sha256: "a".repeat(64),
            target_cutex_session_id: JOB_OWNER.to_string(), summary: Some("untrusted summary".to_string()),
            output_reference: Some(format!("job-output:job-{id}")),
        };
        serde_json::from_value(serde_json::json!({
            "id":id,"kind":"message","from":"cutex-job-service","to":"runtime","toCutexSessionId":JOB_OWNER,
            "content":"Message Type: JOB_SERVICE_COMPLETION\nSummary (untrusted data): untrusted summary",
            "deliveryMode":"after_turn","triggerTurn":true,"createdAtEpochSecs":1,"senderKind":"job_service_system",
            "displaySource":"Cutex Job Service","controlType":JOB_SERVICE_COMPLETION_SCHEMA,
            "controlPayload":request,"externalActionId":format!("job-{id}"),"externalMessageId":format!("event-{id}")
        })).unwrap()
    }

    /// S10 with the ledger switch off: a Claude runtime with native ingress
    /// takes Job Service notices (they used to stay on the bus); without
    /// native ingress they still wait there (no legacy route).
    #[test]
    fn job_notices_are_submitted_to_native_claude_runtimes() {
        let notice = job_notice("jsc_submits");
        assert!(submits(&notice, true));
        assert!(!submits(&notice, false));
        let agent: cutex::agent_bus::model::AgentBusMessage = serde_json::from_value(serde_json::json!({
            "id":"bus-agent","kind":"message","from":"peer","to":"runtime","toCutexSessionId":JOB_OWNER,
            "content":"hi","deliveryMode":"after_turn","triggerTurn":true,"createdAtEpochSecs":1,"senderKind":"agent"
        })).unwrap();
        assert!(submits(&agent, true) && submits(&agent, false));
        let mut controlled = agent.clone();
        controlled.control_type = Some("cutex.unknown.v1".into());
        assert!(!submits(&controlled, true));
    }

    /// Ledger switch OFF (isolated home): a bus message queued for a session
    /// wakes its delivery at once (no 5 s pass, no settle thread), the Job
    /// Service notice path (plan, envelope, freeze, sending, local inbox
    /// receipt) works, and the input ledger is never written (its file is
    /// not even created).
    #[test]
    fn isolated_switch_off_wake_and_job_notice_write_no_ledger() {
        const CHILD: &str = "CUTEX_CLAUDE_UNGATE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = std::env::temp_dir().join(format!("cutex-claude-ungate-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(".cutex-test-private-home"), b"ungate test").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cli_app::claude_runtime::delivery_wake_tests::isolated_switch_off_wake_and_job_notice_write_no_ledger", "--nocapture"])
                .env("HOME", &root).env("CUTEX_TEST_PRIVATE_HOME", &root).env(CHILD, "1")
                .status().unwrap();
            let ran = root.join("ungate-test-done").is_file();
            std::fs::remove_dir_all(root).unwrap();
            assert!(result.success() && ran);
            return;
        }
        use cutex::management::v2::agent_bus_state::{agent_bus_message_repository, AgentBusQueuedMessage};
        use serde_json::json;
        cutex::management::v2::input_ledger::set_enabled_override(Some(false));
        assert!(!cutex::management::v2::input_ledger::enabled());
        let ledger_file = cutex::config::paths::runtime_dir().unwrap().join("management-v2")
            .join(cutex::management::v2::input_ledger::DATABASE_FILE);
        let repository = agent_bus_message_repository().unwrap();
        let events = management_v2_repository().unwrap();
        std::thread::spawn(move || follow_queued_messages(events, || BTreeMap::from([(JOB_OWNER.to_string(), 1)])));
        let queue = |message: &cutex::agent_bus::model::AgentBusMessage, from: Option<&str>| {
            repository.record_queued(AgentBusQueuedMessage {
                owner_cutex_session_id: JOB_OWNER.into(), message_id: message.id.clone(),
                from_cutex_session_id: from.map(str::to_string), to_cutex_session_id: JOB_OWNER.into(),
                from_runtime_agent_id: None, to_runtime_agent_id: Some("runtime".into()),
                delivery_mode: "after_turn".into(), content: message.content.clone(), queued_at: chrono::Utc::now(),
                canonical_envelope: message.clone(), semantic_sha256: cutex::app_server::bus_bridge::inter_agent_semantic_sha256(
                    &cutex::app_server::bus_bridge::inter_agent_params("thread", "ignored", JOB_OWNER, message).unwrap()),
            }).unwrap()
        };
        // S24: queued -> woken, well before a 5 s pass would find it.
        const SENDER: &str = "cutex.22222222-2222-4222-8222-222222222222";
        let note: cutex::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
            "id":"bus-note-1","kind":"message","from":"peer","to":"runtime","fromCutexSessionId":SENDER,
            "toCutexSessionId":JOB_OWNER,"content":"a note","deliveryMode":"after_turn","triggerTurn":true,
            "createdAtEpochSecs":1,"senderKind":"agent"
        })).unwrap();
        // The follower subscribes from "now": queue until it sees one.
        let started = Instant::now();
        let mut woken = Vec::new();
        for attempt in 0..20 {
            let mut message = note.clone();
            message.id = format!("bus-note-{attempt}");
            assert!(queue(&message, Some(SENDER)));
            let queued_at = Instant::now();
            woken = wait_for_delivery_wake(queued_at + Duration::from_millis(500));
            if !woken.is_empty() {
                assert!(queued_at.elapsed() < Duration::from_millis(500));
                break;
            }
        }
        assert_eq!(woken, [JOB_OWNER], "a queued bus message wakes delivery (waited {:?})", started.elapsed());
        // S10: the Job Service notice path, switch off.
        let notice = job_notice("jsc_switch_off");
        assert!(submits(&notice, true));
        let (hold, passive) = cutex::app_server::bus_bridge::job_notice_plan_for(std::slice::from_ref(&notice), JOB_OWNER);
        assert!(hold.is_empty() && passive.is_empty());
        assert!(!cutex::app_server::bus_bridge::job_notice_awaited(&notice, JOB_OWNER));
        assert!(queue(&notice, None));
        let envelope = repository.freeze_external_input(JOB_OWNER, &notice.id, |canonical|
            cutex::app_server::bus_bridge::job_notice_envelope(canonical, JOB_OWNER, "thread", 7, false, false)).unwrap();
        assert_eq!((envelope.message.event_type.as_str(), envelope.message.source.id.as_str()), ("job_completion", "cutex-job-service"));
        repository.input_ledger_sending(JOB_OWNER, &notice.id);
        let receipt = json!({"threadId":"thread","messageId":notice.id,"durable":true,"receiptScope":"local-inbox","state":"queued"});
        repository.record_local_inbox(JOB_OWNER, &notice.id, &receipt).unwrap();
        assert_eq!(repository.message_state(&notice.id).unwrap(), Some(("inbox_accepted".into(), true)));
        // Turn end wakes too (in process, no ledger).
        wake_delivery(JOB_OWNER);
        assert_eq!(wait_for_delivery_wake(Instant::now() + Duration::from_secs(1)), [JOB_OWNER]);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!ledger_file.exists(), "switch off: the input ledger is never opened or written");
        std::fs::write(std::path::Path::new(&std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap()).join("ungate-test-done"), b"ok").unwrap();
    }
}
