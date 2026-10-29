//! The management process side of asks (委托): the runtime adapter the ask
//! scheduler uses, and the driver thread (asks design §5.3).
//!
//! Native hooks (cute-codex `feat/ask-hooks`, design §12.2) are detected per
//! runtime connection from the `initialize` response:
//! - N1 `externalInputAdmissions: ["exclusiveTurn"]`: without it asks are
//!   held `runtimeUnsupported` (never submitted through the merging path);
//! - N2 `externalInputGateControl: ["resume", "interruptKeepGate"]`: without
//!   it a requester stop of a running ask is refused (`n2_unavailable`) and
//!   owner resume only reopens the cutex admission.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;

use cutex::app_server::external_input::{
    Delivery, Envelope, ExclusiveSubmit, ExternalInputClient, HoldReason, MessageKey, ProcessingState,
};
use cutex::asks::scheduler::{AskRuntime, NativeMessageStatus, RespondOutcome, Submission, SubmitOutcome, TargetStatus};
use cutex::app_server::external_input::size::ReceiverLimit;
use cutex::asks::store::AskStore;

mod client_cache;

#[cfg(test)]
mod pause_tests;

use super::app_server_runtime;
use super::app_server_user_input;

/// Scheduler period.
const TICK: Duration = Duration::from_millis(1000);
/// A reconnect to the same session is not retried sooner.
const RECONNECT_RETRY: Duration = Duration::from_secs(30);

/// Reconnect (never launch) to the live runtime of `session`; returns its
/// runtime generation.
fn reconnect_live_runtime(session: &str) -> anyhow::Result<u64> {
    let record = record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
    anyhow::ensure!(cutex::asks::wake::runtime_binding_live(&record), "no live runtime binding");
    if record.explicit_launch.is_some() {
        super::management_context::online_management_v2_session(None, session, json!({}))
            .map_err(|error| anyhow::anyhow!("{} ({})", error.message, error.code))?;
    } else {
        let config = cutex::config::store::load_codez_config();
        super::app_server_runtime::recover_persisted_runtime_for_lifecycle(&config, &record)?;
    }
    Ok(record.runtime_generation)
}

#[derive(Default)]
pub(crate) struct ManagementAskRuntime {
    /// One ExternalInput connection per session and runtime generation.
    clients: client_cache::ClientCache<ExternalInputClient>,
    /// Last reconnect attempt per session (rate limit; one at a time).
    reconnects: std::sync::Arc<Mutex<HashMap<String, std::time::Instant>>>,
}

pub(super) fn record(session: &str) -> Option<cutex::session::model::CutexSessionRecord> {
    cutex::session::store::with_cutex_session_store(|store| store.sessions.get(session).cloned())
        .ok()
        .flatten()
}

fn envelope(client: &ExternalInputClient, submission: &Submission) -> anyhow::Result<Envelope> {
    build_envelope(client.binding(), client.supports_structured_view(), submission)
}

/// Same inputs give the same envelope (message_status rebuilds its key).
fn build_envelope(
    binding: &cutex::launch::stock::ExternalInputBinding,
    structured_view: bool,
    submission: &Submission,
) -> anyhow::Result<Envelope> {
    submission.envelope(binding, structured_view)
}

/// The native rejection of a message over the receiver's canonical byte
/// policy (cute-codex `ExternalInputContext::new`, checked before anything
/// is recorded): the same message is never accepted, so it is final. Not
/// the runtime-wide "policy blocks restored history" error.
fn rejected_by_size_policy(message: &str) -> bool {
    cutex::app_server::external_input::size::is_size_rejection(message)
}

/// A submit RPC error: final for a size-policy rejection (the connection is
/// fine), otherwise an unknown outcome (the connection is dropped).
fn submit_error_outcome(message: &str, size: (Option<u64>, Option<u64>)) -> SubmitOutcome {
    if rejected_by_size_policy(message) {
        return SubmitOutcome::TooLarge { item_bytes: size.0, limit: size.1 };
    }
    SubmitOutcome::Uncertain(message.to_string())
}

// Decide retirement while the session slot is still locked. Outcome mapping
// happens afterwards and must never remove a client by session name alone.
fn discard_submit_result<T>(result: &anyhow::Result<T>) -> bool {
    result.as_ref().err().is_some_and(|error| !rejected_by_size_policy(&format!("{error:#}")))
}

fn discard_soon_result<T>(result: &anyhow::Result<T>) -> bool {
    discard_submit_result(result) && result.as_ref().err()
        .is_some_and(|error| !format!("{error:#}").contains("does not support soon"))
}

/// What the receiver policy measures (exact serialization).
fn model_item_bytes(envelope: &Envelope) -> u64 {
    cutex::app_server::external_input::size::envelope_item_bytes(envelope) as u64
}

/// The receiver's item limit from the runtime binding (`None`: off).
fn canonical_limit(client: &ExternalInputClient) -> Option<u64> {
    ReceiverLimit::from_policy(&client.binding().canonical_byte_limit).item_limit()
}

fn hold_name(reason: &HoldReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

impl ManagementAskRuntime {
    fn with_client<T>(
        &self,
        session: &str,
        generation: u64,
        use_client: impl FnOnce(&ExternalInputClient) -> T,
    ) -> anyhow::Result<T> {
        self.with_client_policy(session, generation, use_client, |_| false)
    }

    fn with_client_policy<T>(
        &self,
        session: &str,
        generation: u64,
        use_client: impl FnOnce(&ExternalInputClient) -> T,
        discard: impl FnOnce(&T) -> bool,
    ) -> anyhow::Result<T> {
        self.clients.with_client(session, generation, || {
            let path = cutex::session::store::cutex_sessions_path()?;
            ExternalInputClient::connect_rpc_only(&path, session, generation)
        }, use_client, discard)
    }
}

/// Which transport carries an exact (generation + turn fenced) interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactInterruptRoute {
    /// Claude adapter: bound `thread/externalInput/interrupt`.
    Claude,
    /// DSH and Kimi (`dsh_session`, ACP adapters): generation-bound
    /// `turn/interrupt` (ACP cancel). They have no ExternalInput ingress.
    Acp,
    /// cute-codex: N2 `interruptKeepGate` over ExternalInput.
    Native,
}

#[cfg(test)]
pub(crate) fn exact_interrupt_route_for_tests(record: &cutex::session::model::CutexSessionRecord) -> ExactInterruptRoute {
    exact_interrupt_route(record)
}

fn exact_interrupt_route(record: &cutex::session::model::CutexSessionRecord) -> ExactInterruptRoute {
    if record.claude_session.is_some() {
        ExactInterruptRoute::Claude
    } else if record.dsh_session.is_some() {
        ExactInterruptRoute::Acp
    } else {
        ExactInterruptRoute::Native
    }
}

pub(crate) fn pause_admission_outcome(session:&str)->Option<SubmitOutcome> {
    gate_outcome(AskStore::open_default().and_then(|store|cutex::asks::pause_admission::gate(&store,session)))
}

fn gate_outcome(gate:anyhow::Result<cutex::asks::pause_admission::Gate>)->Option<SubmitOutcome> {
    use cutex::asks::pause_admission::Gate;
    match gate {
        Ok(Gate::Open)=>None,
        Ok(Gate::Paused)=>Some(SubmitOutcome::Busy{reason:"agent_paused".into()}),
        Ok(Gate::Deferred)=>Some(SubmitOutcome::Busy{reason:"authority_busy".into()}),
        Err(error)=>Some(SubmitOutcome::Busy{reason:format!("pause_admission_unavailable: {error:#}")}),
    }
}

/// [`pause_admission_outcome`] for a caller that already holds the session
/// lane right after the scheduler's lease write. That write wakes the
/// admission thread, whose short fence holds (`list_admission`) then collide
/// with this one-shot try-lock on every retry: the ask is refused
/// `authority_busy`, requeued, leased again, and starves (seen on DSH in the
/// private-daemon E2E). Retry the non-blocking gate for a bounded time
/// (never blocks on the fence, so a lifecycle action holding it still
/// defers this admission); a pause or any error is final at once.
pub(crate) fn pause_admission_outcome_in_lane(session:&str)->Option<SubmitOutcome> {
    const TRIES: u32 = 25;
    const STEP: Duration = Duration::from_millis(20);
    let store = match AskStore::open_default() {
        Ok(store) => store,
        Err(error) => return gate_outcome(Err(error)),
    };
    for attempt in 1..=TRIES {
        let gate = cutex::asks::pause_admission::gate(&store, session);
        if attempt < TRIES && matches!(gate, Ok(cutex::asks::pause_admission::Gate::Deferred)) {
            std::thread::sleep(STEP);
            continue;
        }
        return gate_outcome(gate);
    }
    unreachable!("the last attempt returns")
}

impl AskRuntime for ManagementAskRuntime {
    fn result_admission_lane(&self, session: &str) -> anyhow::Result<Option<std::sync::Arc<cutex::management::v2::session_coordination::OperationLane>>> {
        cutex::management::v2::session_coordination::session_lane(session).map(Some)
    }
    fn result_window_context(&self, session: &str) -> Option<cutex::asks::result_coalescing::Context> {
        cutex::asks::result_coalescing::context_for_session(session)
    }
    fn target_status(&self, session: &str) -> TargetStatus {
        let Some(record) = record(session) else {
            return TargetStatus::default();
        };
        if record.claude_session.is_some() {
            return super::asks_claude::target_status(session, &record);
        }
        if record.dsh_session.is_some() {
            return super::asks_dsh::target_status(session, &record);
        }
        let status = app_server_runtime::runtime_manager().status(session).ok().flatten();
        let online = status
            .as_ref()
            .is_some_and(|status| status.connected && status.runtime_generation == record.runtime_generation);
        if !online {
            // No connection here, but maybe a live runtime another cutex
            // process (the Agent Bus: agent_create, member_runtime) started.
            return TargetStatus {
                runtime_generation: record.runtime_generation,
                reconnectable: cutex::asks::wake::runtime_binding_live(&record),
                ..TargetStatus::default()
            };
        }
        let status = status.expect("online");
        let exclusive_admission = online
            && self
                .with_client(session, record.runtime_generation, ExternalInputClient::supports_exclusive_turn)
                .unwrap_or(false);
        TargetStatus {
            online,
            runtime_generation: record.runtime_generation,
            active_turn_id: status.active_turn_id,
            admission: None,
            plan_mode: false,
            profile_switch_pending: cutex::management::v2::profile_switch::pending_switch(session).is_some(),
            exclusive_admission,
            blocked: None,
            reconnectable: false,
            receiver_limit: self
                .with_client(session, record.runtime_generation, |client| {
                    ReceiverLimit::from_policy(&client.binding().canonical_byte_limit)
                })
                .unwrap_or_default(),
        }
    }

    fn submit_exclusive(&self, session: &str, generation: u64, submission: &Submission) -> SubmitOutcome {
        if record(session).is_some_and(|record| record.claude_session.is_some()) {
            return super::asks_claude::submit(session, generation, submission);
        }
        if record(session).is_some_and(|record| record.dsh_session.is_some()) {
            return super::asks_dsh::submit(session, generation, submission);
        }
        // Serialize with owner submit / flush / stop / revert.
        let lock = match app_server_user_input::session_input_lock(session) {
            Ok(lock) => lock,
            Err(error) => return SubmitOutcome::Unsupported(format!("{error:#}")),
        };
        let Ok(_guard) = lock.lock() else {
            return SubmitOutcome::Unsupported("session input lock poisoned".into());
        };
        if let Some(outcome)=pause_admission_outcome_in_lane(session){return outcome;}
        if let Ok(Some(status)) = app_server_runtime::runtime_manager().status(session) {
            if status.active_turn_id.is_some() {
                return SubmitOutcome::Busy { reason: "active_turn".into() };
            }
        }
        let mut size = (None, None);
        let result = self.with_client_policy(session, generation, |client| {
            if !client.supports_exclusive_turn() {
                return Ok(None);
            }
            let envelope = envelope(client, submission)?;
            size = (Some(model_item_bytes(&envelope)), canonical_limit(client));
            client.submit_exclusive(&envelope).map(Some)
        }, discard_submit_result);
        match result {
            // Connecting failed: nothing was sent.
            Err(error) => SubmitOutcome::Unsupported(format!("runtime unavailable: {error:#}")),
            Ok(Ok(None)) => SubmitOutcome::Unsupported(cutex::asks::scheduler::EXCLUSIVE_TURN.into()),
            Ok(Ok(Some(ExclusiveSubmit::Started { turn_id, .. }))) => SubmitOutcome::Started { turn_id },
            Ok(Ok(Some(ExclusiveSubmit::Busy { reason }))) => SubmitOutcome::Busy { reason },
            Ok(Ok(Some(ExclusiveSubmit::Replayed(status)))) => match status.processing.reason {
                Some(HoldReason::CanonicalSizePolicy) => SubmitOutcome::TooLarge { item_bytes: size.0, limit: size.1 },
                _ => SubmitOutcome::Uncertain("accepted without receipt; reconciling".into()),
            },
            Ok(Err(error)) => submit_error_outcome(&format!("{error:#}"), size),
        }
    }

    fn message_status(&self, session: &str, generation: u64, submission: &Submission) -> NativeMessageStatus {
        if record(session).is_some_and(|record| record.claude_session.is_some()) {
            return super::asks_claude::message_status(session, generation, submission);
        }
        if record(session).is_some_and(|record| record.dsh_session.is_some()) {
            return super::asks_dsh::message_status(session, generation, submission);
        }
        let result = self.with_client_policy(session, generation, |client| {
            let envelope = envelope(client, submission)?;
            let key = MessageKey {
                message_id: envelope.message.id.clone(),
                semantic_sha256: envelope.semantic_sha256.clone(),
            };
            client.status(&[key])
        }, Result::is_err);
        let status = match result {
            Ok(Ok(response)) => match response.statuses.into_iter().next() {
                Some(status) => status,
                None => return NativeMessageStatus::Unavailable("empty status".into()),
            },
            Ok(Err(error)) | Err(error) => {
                return NativeMessageStatus::Unavailable(format!("{error:#}"));
            }
        };
        let turn_id = status.receipt.as_ref().map(|receipt| receipt.turn_id.clone());
        if status.processing.state == ProcessingState::Held {
            return NativeMessageStatus::Held {
                reason: status.processing.reason.as_ref().map(hold_name).unwrap_or_default(),
                turn_id,
            };
        }
        match (status.delivery_state, turn_id) {
            (_, Some(turn_id)) => NativeMessageStatus::Persisted { turn_id },
            (cutex::app_server::external_input::DeliveryState::Unknown, None) => NativeMessageStatus::Absent,
            (cutex::app_server::external_input::DeliveryState::Pending, None) => NativeMessageStatus::Pending,
            (other, None) => NativeMessageStatus::Unavailable(format!("{other:?}")),
        }
    }

    fn interrupt_keep_gate(&self, session: &str, turn_id: &str) -> anyhow::Result<()> {
        let record = record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
        if record.claude_session.is_some() {
            return super::asks_claude::interrupt(session, turn_id);
        }
        if record.dsh_session.is_some() {
            return super::asks_dsh::interrupt(session, turn_id);
        }
        let supported = self.with_client(session, record.runtime_generation, ExternalInputClient::supports_gate_control)?;
        anyhow::ensure!(supported, "n2_unavailable: the runtime cannot interrupt without closing its external-input gate");
        self.with_client(session, record.runtime_generation, |client| client.interrupt_keep_gate(turn_id).map(|_| ()))?
    }

    fn interrupt_exact(&self, session: &str, generation: u64, turn_id: &str) -> anyhow::Result<()> {
        // Serialize with restart/owner stop. Never reconnect to a replacement
        // occurrence between the generation check and the exact native RPC.
        let lane = app_server_user_input::session_input_lock(session)?;
        let _operation = lane.lock().map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
        let current = record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
        anyhow::ensure!(current.runtime_generation == generation, "runtime_generation_changed");
        match exact_interrupt_route(&current) {
            ExactInterruptRoute::Claude => return super::asks_claude::interrupt_exact(session, generation, turn_id),
            ExactInterruptRoute::Acp => return super::asks_dsh::interrupt_exact(session, generation, turn_id),
            ExactInterruptRoute::Native => {}
        }
        let status = app_server_runtime::runtime_manager().status(session)?
            .ok_or_else(|| anyhow::anyhow!("runtime_occurrence_changed"))?;
        anyhow::ensure!(status.connected && status.runtime_generation == generation
            && status.active_turn_id.as_deref() == Some(turn_id), "runtime_occurrence_changed");
        let supported = self.with_client(session, generation, ExternalInputClient::supports_gate_control)?;
        anyhow::ensure!(supported, "exact_interrupt_unsupported");
        self.with_client(session, generation, |client| client.interrupt_keep_gate(turn_id).map(|_| ()))?
    }

    fn call_mcp_tool(&self, session: &str, server: &str, tool: &str, arguments: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let record = record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
        if record.claude_session.is_some() {
            // Claude runtimes have no cutex_job server (delegation await §8).
            anyhow::bail!("the handler's Claude runtime has no {server} MCP server");
        }
        if record.dsh_session.is_some() {
            anyhow::bail!("the handler's DSH runtime has no {server} MCP server");
        }
        let thread_id = record
            .app_server_thread_id()
            .ok_or_else(|| anyhow::anyhow!("the handler has no native thread"))?
            .to_string();
        app_server_user_input::session_manager(session)
            .commands(session)
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .mcp_server_tool_call(&serde_json::json!({
                "threadId": thread_id,
                "server": server,
                "tool": tool,
                "arguments": arguments,
            }))
            .map_err(|error| anyhow::anyhow!("mcpServer/tool/call: {error}"))
    }

    fn read_turn(&self, session: &str, turn_id: &str) -> Option<serde_json::Value> {
        let thread_id = record(session)?.app_server_thread_id()?.to_string();
        let thread = app_server_user_input::session_manager(session)
            .commands(session)
            .ok()?
            .thread_read(&cutex::app_server::commands::ThreadReadParams { thread_id, include_turns: true })
            .ok()?;
        thread
            .pointer("/thread/turns")?
            .as_array()?
            .iter()
            .find(|turn| turn.get("id").and_then(serde_json::Value::as_str) == Some(turn_id))
            .cloned()
    }

    fn respond_question(
        &self,
        session: &str,
        generation: u64,
        response_id: &str,
        request_id: &serde_json::Value,
        result: &serde_json::Value,
    ) -> RespondOutcome {
        use cutex::management::v2::server_requests::{self, ServerResponseClaimDecision};
        let message = json!({"id": request_id, "result": result});
        let claim = json!({"expectedRuntimeGeneration": generation, "body": {"responseId": response_id, "native": {"message": message}}});
        match server_requests::claim_response(session, generation, response_id, &claim, &message) {
            Ok(ServerResponseClaimDecision::Submit) => {}
            Ok(ServerResponseClaimDecision::Deduplicated) => return RespondOutcome::Submitted,
            Ok(ServerResponseClaimDecision::InProgress) => return RespondOutcome::NotSent("in progress".into()),
            Ok(_) => return RespondOutcome::AlreadyResolved,
            Err(error) => return RespondOutcome::NotSent(format!("{error:#}")),
        }
        let handle = match app_server_user_input::session_manager(session).handle_for_generation(session, generation) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = server_requests::release_definitely_failed_response(session, generation, response_id);
                return RespondOutcome::NotSent(format!("{error:?}"));
            }
        };
        use cutex::app_server::client::AppServerClientError as E;
        match handle.respond_raw(message) {
            Ok(()) => {
                let _ = server_requests::mark_response_submitted(session, generation, response_id);
                RespondOutcome::Submitted
            }
            Err(E::Transport(error)) | Err(E::Disconnected(error)) => {
                let _ = server_requests::mark_response_indeterminate(session, generation, response_id);
                RespondOutcome::Unknown(error)
            }
            Err(E::Timeout { method }) => {
                let _ = server_requests::mark_response_indeterminate(session, generation, response_id);
                RespondOutcome::Unknown(format!("timeout {method}"))
            }
            Err(error) => {
                let _ = server_requests::release_definitely_failed_response(session, generation, response_id);
                RespondOutcome::NotSent(error.to_string())
            }
        }
    }

    /// Auto-wake (§8.4, owner-configured policy): the same online path the
    /// owner's `cutex/runtime/online` uses, in the background (a launch can
    /// take seconds). The ask stays `blocked(runtimeDown)` until online.
    fn wake(&self, session: &str) -> anyhow::Result<()> {
        let session = session.to_string();
        std::thread::Builder::new()
            .name("cutex-asks-wake".into())
            .spawn(move || {
                let admitted = (|| -> anyhow::Result<bool> {
                    let lane = app_server_user_input::session_input_lock(&session)?;
                    let _operation = lane.lock().map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
                    if cutex::asks::pause_admission::is_held(&AskStore::open_default()?, &session)? { return Ok(false); }
                // No owner principal: an agent launched from an explicit stock
                // runtime review needs the owner to bring it online (refused).
                if let Err(error) = super::management_context::automatic_online_management_v2_session(&session) {
                    eprintln!("asks: auto-wake of {session} failed: {} ({})", error.message, error.code);
                    if !error.retryable {
                        // Only the owner can help (r208): tell the requesters
                        // and the owner instead of waiting silently.
                        cutex::asks::wake::record_owner_action_default(&session, &error.code, &error.message);
                    }
                }
                    Ok(true)
                })();
                if let Err(error) = admitted { eprintln!("asks: auto-wake admission for {session} unavailable: {error:#}"); }
            })?;
        Ok(())
    }

    /// Reviewed relaunch (owner approval 2026-10-14): the owner's start of an
    /// explicit-launch agent on the owner's latest reviewed launch, for
    /// `requester`'s delegation, in the background. The provider rechecks
    /// the review under its locks; a refusal is reported like a failed wake.
    fn relaunch_reviewed(&self, session: &str, requester: &str, ask_id: &str) -> anyhow::Result<()> {
        let session = session.to_string();
        let requester = requester.to_string();
        let ask_id = ask_id.to_string();
        std::thread::Builder::new()
            .name("cutex-asks-relaunch".into())
            .spawn(move || {
                let admitted = (|| -> anyhow::Result<()> {
                    let lane = app_server_user_input::session_input_lock(&session)?;
                    let _operation = lane.lock().map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
                    if cutex::asks::pause_admission::is_held(&AskStore::open_default()?, &session)? { return Ok(()); }
                match super::management_context::relaunch_reviewed_management_v2_session(&session, &requester) {
                    Ok(result) => match cutex::asks::wake::relaunch_reviewed_action(&result) {
                        Some(reviewed) => {
                            eprintln!(
                                "asks: reviewed relaunch of {session} for {requester}: online (action {}, owner review {reviewed})",
                                result["actionId"]
                            );
                            cutex::asks::wake::record_reviewed_relaunch_default(&session, &requester, &ask_id, reviewed);
                        }
                        // The owner's own start was first: no relaunch ran,
                        // so no relaunch notice.
                        None => eprintln!(
                            "asks: reviewed relaunch of {session} for {requester} not needed: already online (action {})",
                            result["actionId"]
                        ),
                    },
                    Err(error) => {
                        eprintln!("asks: reviewed relaunch of {session} for {requester} failed: {} ({})", error.message, error.code);
                        if !error.retryable {
                            cutex::asks::wake::record_owner_action_default(&session, &error.code, &error.message);
                        }
                    }
                }
                    Ok(())
                })();
                if let Err(error) = admitted { eprintln!("asks: reviewed relaunch admission for {session} unavailable: {error:#}"); }
            })?;
        Ok(())
    }

    /// Reconnect to a live runtime this process has no connection to (r208):
    /// in the background, at most one per session every
    /// [`RECONNECT_RETRY`]; never launches.
    fn reconnect(&self, session: &str) -> anyhow::Result<()> {
        {
            let mut last = self.reconnects.lock().map_err(|_| anyhow::anyhow!("reconnect lock poisoned"))?;
            if last.get(session).is_some_and(|at| at.elapsed() < RECONNECT_RETRY) {
                return Ok(());
            }
            last.insert(session.to_string(), std::time::Instant::now());
        }
        let session = session.to_string();
        std::thread::Builder::new()
            .name("cutex-asks-reconnect".into())
            .spawn(move || match reconnect_live_runtime(&session) {
                Ok(generation) => super::asks_runtime::refresh_session_presence(&session, Some(generation)),
                Err(error) => eprintln!("asks: reconnect of {session} failed: {error:#}"),
            })?;
        Ok(())
    }

    fn steer_note(&self, session: &str, generation: u64, submission: &Submission) -> SubmitOutcome {
        if let Some(outcome)=pause_admission_outcome(session){return outcome;}
        if record(session).is_some_and(|record| record.claude_session.is_some()) {
            if cutex::asks::returns::is_next_step_event(&submission.message_id) {
                return super::asks_claude::submit(session, generation, submission);
            }
            return SubmitOutcome::Unsupported("Claude next-step admission requires a delegation result".into());
        }
        if record(session).is_some_and(|record| record.dsh_session.is_some()) {
            return SubmitOutcome::Unsupported("DSH has no next-step admission".into());
        }
        let mut size = (None, None);
        let result = self.with_client_policy(session, generation, |client| {
            let mut envelope = envelope(client, submission)?;
            envelope.message.delivery = Delivery::Soon;
            envelope.semantic_sha256 = envelope.digest();
            size = (Some(model_item_bytes(&envelope)), canonical_limit(client));
            client.submit(&envelope)
        }, discard_soon_result);
        match result {
            Err(error) => SubmitOutcome::Unsupported(format!("runtime unavailable: {error:#}")),
            Ok(Ok(_)) => SubmitOutcome::Started { turn_id: String::new() },
            Ok(Err(error)) => {
                let message = format!("{error:#}");
                if message.contains("does not support soon") {
                    SubmitOutcome::Unsupported(message)
                } else {
                    submit_error_outcome(&message, size)
                }
            }
        }
    }

    fn takes_next_step(&self, session: &str, generation: u64) -> bool {
        let Some(record) = record(session) else {
            return false;
        };
        if record.claude_session.is_some() {
            return super::asks_claude::adapter_capabilities(session, generation)
                .is_some_and(|caps| super::asks_claude::offers_next_step(&caps));
        }
        if record.dsh_session.is_some() {
            return false;
        }
        self.with_client(session, generation, ExternalInputClient::supports_soon)
            .unwrap_or(false)
    }

    fn durable_next_step(&self, session: &str) -> bool {
        record(session).is_some_and(|record| record.claude_session.is_some())
    }

    fn resume_gate(&self, session: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!cutex::asks::pause_admission::is_held(&AskStore::open_default()?,session)?,"agent_paused");
        let record = record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
        if record.claude_session.is_some() {
            return super::asks_claude::resume(session);
        }
        // DSH: ACP cancel closes no input gate.
        if record.dsh_session.is_some() {
            return Ok(());
        }
        self.with_client(session, record.runtime_generation, |client| client.resume_gate().map(|_| ()))?
    }
}

/// The driver: scheduler ticks, Management v2 notices, owner-queue flushes
/// the scheduler unblocked, and periodic pruning.
pub(crate) fn spawn_scheduler() -> anyhow::Result<()> {
    // Do the one-shot split before this process starts admission threads.
    // The persisted cutoff is reused at restart; an explicit mismatch fails.
    let now=Utc::now();
    let requested=std::env::var("CUTEX_ASKS_ARCHIVE_BEFORE").ok()
        .map(|value|cutex::asks::archive::activation_cutoff(now,Some(&value))).transpose()?;
    let archived=AskStore::open_default()?.archive_before(requested,now)?;
    if archived.rows!=0 {eprintln!("asks: archived {} records / {} bytes ({} delegations)",archived.rows,archived.bytes,archived.asks);}
    cutex::peers::worker::spawn()?;
    super::model_overload_retry::spawn_worker()?;
    super::claude_quota_continue::spawn_worker()?;
    cutex::runtime::kimi::account_tokens::spawn_keeper()?;
    std::thread::Builder::new().name("cutex-admission".into()).spawn(|| {
        let mut published = std::collections::HashMap::new();
        let mut ridealongs = std::collections::BTreeSet::<String>::new();
        let mut busy_wait = cutex::agent_bus::delivery::wake_busy_wait();
        let mut deadlines = std::collections::HashMap::<String, chrono::DateTime<Utc>>::new();
        loop {
            cutex::management::v2::admission::wait_for_changes(TICK);
            cutex::management::v2::admission_wait::wake_expired();
            // Poll only while somebody observes admission or runtime work needs it.
            // Direct list/bootstrap and source changes always read fresh state.
            let observed = cutex::management::v2::repository::management_v2_repository()
                .map(|events| events.has_live_subscribers()).unwrap_or(true);
            let runtime_work = || cutex::management::v2::user_input::user_input_repository()
                .and_then(|queue| queue.needs_admission_poll()).unwrap_or(true);
            if observed || runtime_work() {
                let _ = cutex::management::v2::agent_bus_state::agent_bus_message_repository()
                    .and_then(|bus| bus.refresh_admission_if_changed());
            }
            let configured_wait = cutex::agent_bus::delivery::wake_busy_wait();
            if configured_wait != busy_wait {
                for session in &ridealongs { cutex::management::v2::admission::changed(session); }
                busy_wait = configured_wait;
            }
            let now = Utc::now();
            for (session, _) in deadlines.iter().filter(|(_, at)| **at <= now) {
                cutex::management::v2::admission::changed(session);
            }
            deadlines.retain(|_, at| *at > now);
            for session in cutex::management::v2::admission::take_changes() {
                let result = (|| -> anyhow::Result<()> {
                    let queue = cutex::management::v2::user_input::user_input_repository()?;
                    let (_, _, _, admission) = queue.list_admission(&session)?;
                    if !admission.items.iter().any(|item| item.kind == "owner" && item.state == "waiting") {
                        cutex::management::v2::admission_wait::clear(&session);
                    }
                    if admission.next_step.is_empty() { ridealongs.remove(&session); }
                    else { ridealongs.insert(session.clone()); }
                    if let Some(deadline) = admission.next_step.iter().filter_map(|item| item.next_step_at.as_deref())
                        .filter_map(cutex::asks::parse_time).filter(|at| *at > Utc::now()).min() {
                        deadlines.insert(session.clone(), deadline);
                    } else { deadlines.remove(&session); }
                    if published.get(&session) != Some(&admission.revision) {
                        cutex::management::v2::repository::management_v2_repository()?.append(cutex::management::v2::model::PendingEvent {
                            cutex_session_id: session.clone(), host_id: cutex::platform::host::current_host_name(),
                            source: cutex::management::v2::model::EventSource::Cutex, schema: None,
                            correlation: Default::default(), native: None,
                            cutex: Some(cutex::management::v2::model::CutexMessage {
                                method: "cutex/userInput/queueReordered".into(),
                                params: serde_json::json!({ "admission": { "revision": admission.revision } }),
                            }),
                        })?;
                        published.insert(session.clone(), admission.revision);
                    }
                    app_server_user_input::flush_queued_if_idle(&session)?;
                    Ok(())
                })();
                if let Err(error) = result { eprintln!("admission: {session}: {error:#}"); }
            }
        }
    })?;
    std::thread::Builder::new()
        .name("cutex-asks".into())
        .spawn(|| {
            let runtime = ManagementAskRuntime::default();
            let mut last_prune = std::time::Instant::now();
            let mut last_sweep: Option<std::time::Instant> = None;
            let mut last_receipt_prune: Option<std::time::Instant> = None;
            let mut last_archive=std::time::Instant::now();
            let mut ticks: u64 = 0;
            loop {
                // Agent → owner messages made in the Agent Bus process.
                cutex::management::v2::owner_notify::publish_default();
                if let Err(error) = drive_once(&runtime) {
                    eprintln!("asks: scheduler warning: {error:#}");
                }
                if ticks % 5 == 0 {
                    if let Err(error) = AskStore::open_default().and_then(|store| refresh_presence(&store)) {
                        eprintln!("asks: presence warning: {error:#}");
                    }
                }
                ticks += 1;
                if last_archive.elapsed()>=Duration::from_secs(300) {
                    if let Err(error)=AskStore::open_default().and_then(|store|store.archive_before(None,Utc::now())) {
                        eprintln!("asks: incremental permanent archive warning: {error:#}");
                    }
                    last_archive=std::time::Instant::now();
                }
                if last_prune.elapsed() > Duration::from_secs(3600) {
                    // Housekeeping only: closed asks leave through the
                    // retention sweep, which archives them first.
                    if let Ok(store) = AskStore::open_default() {
                        let _ = store.prune_with(None, Utc::now());
                    }
                    last_prune = std::time::Instant::now();
                }
                let settings = cutex::config::documents::current();
                let interval = Duration::from_secs(u64::from(settings.sweep_interval_hours) * 3600);
                let due = last_sweep.is_none_or(|at: std::time::Instant| at.elapsed() >= interval)
                    || (ticks % 600 == 0
                        && cutex::documents::DocumentStore::open_default()
                            .is_ok_and(|store| cutex::documents::sweep::nearly_full(&store, &settings))
                        && last_sweep.is_some_and(|at| at.elapsed() >= Duration::from_secs(600)));
                if due {
                    match cutex::documents::sweep::run_default(&settings) {
                        Ok(report) if !report.errors.is_empty() => eprintln!("documents: sweep warnings: {:?}", report.errors),
                        Ok(_) => {}
                        Err(error) => eprintln!("documents: sweep failed: {error:#}"),
                    }
                    last_sweep = Some(std::time::Instant::now());
                }
                // Session store: prune explicit-launch receipts nothing
                // needs (once at start, then every 6 h; idempotent).
                if last_receipt_prune.is_none_or(|at: std::time::Instant| {
                    at.elapsed() >= cutex::session::receipt_retention::PRUNE_INTERVAL
                }) {
                    match cutex::session::receipt_retention::prune_default_store() {
                        Ok(report) if report.changed() => eprintln!(
                            "session store: pruned {} explicit-launch receipts ({} of removed agents), {} kept; tombstones +{} -{}",
                            report.pruned,
                            report.pruned_orphaned,
                            report.kept,
                            report.tombstones_added,
                            report.tombstones_expired + report.tombstones_superseded,
                        ),
                        Ok(_) => {}
                        Err(error) => eprintln!("session store: receipt pruning failed: {error:#}"),
                    }
                    last_receipt_prune = Some(std::time::Instant::now());
                }
                std::thread::sleep(TICK);
            }
        })?;
    Ok(())
}

fn drive_once(runtime: &ManagementAskRuntime) -> anyhow::Result<()> {
    let store = AskStore::open_default()?;
    let world = cutex::asks::scheduler::HostWorld::load();
    let report = cutex::asks::scheduler::tick(&world, runtime, &store, Utc::now())?;
    cutex::management::v2::asks::publish_pending_notices(&store);
    for session in report.released { cutex::management::v2::admission::changed(&session); }
    Ok(())
}

/// Runtime events of a bound thread (management process): attempt
/// activity and answers, questions raised in an ask's turn, and native
/// resolution of those questions. Errors are logged, never fatal.
pub(crate) fn on_runtime_event(cutex_session_id: &str, runtime_generation: u64, event: &cutex::app_server::client::AppServerEvent) {
    use cutex::app_server::client::AppServerEvent;
    if let AppServerEvent::Notification(notification) = event {
        // Artifact auto capture: turn windows + scan on a member turn end
        // (handed to its worker; nothing else on this path).
        cutex::artifacts::capture::on_runtime_event(cutex_session_id, runtime_generation, &notification.method, notification.params.as_ref());
    }
    let Ok(store) = AskStore::open_default() else {
        return;
    };
    let now = Utc::now();
    let result = match event {
        AppServerEvent::Notification(notification) if notification.method == "serverRequest/resolved" => {
            let request_id = notification
                .params
                .as_ref()
                .and_then(|params| params.get("requestId"))
                .cloned()
                .unwrap_or_default();
            match store.peek() {
                Ok(Some(state)) if state.questions.values().any(|q| q.native.request_id == request_id) => store
                    .with_state(|state| cutex::asks::questions::native_resolved(state, cutex_session_id, &request_id, now)),
                _ => Ok(()),
            }
        }
        AppServerEvent::Notification(notification) => match &notification.params {
            Some(params) =>  {
                if notification.method == "turn/started" {
                    note_turn_started(&store, cutex_session_id, params);
                }
                // approval-wait: a tool call holding for the owner in this
                // turn stops holding (Codex never cancels it on interrupt).
                if notification.method == "turn/completed" {
                    cutex::management::v2::approvals::on_turn_completed(cutex_session_id, params);
                    note_turn_completed(&store, cutex_session_id, now);
                }
                cutex::agent_bus::wake_log::observe(cutex_session_id, &notification.method, params);
                // A Claude delegation that failed on its exhausted usage limit
                // goes back to the queue (held until the reset) when the
                // agent's quota auto-continue is on.
                let requeue_on_quota = || {
                    record(cutex_session_id).is_some_and(|record| record.claude_session.is_some())
                        && cutex::management::v2::quota_continue::enabled_default(cutex_session_id)
                };
                cutex::asks::observe::observe_with(&store, cutex_session_id, &notification.method, params, now, &requeue_on_quota)
            }
            None => Ok(()),
        },
        AppServerEvent::ServerRequest(request)
            if cutex::asks::questions::QUESTION_METHODS.contains(&request.method.as_str()) =>
        {
            let params = request.params.clone().unwrap_or_default();
            match store.peek() {
                Ok(Some(state)) if state.leases.contains_key(cutex_session_id) => store
                    .with_state(|state| {
                        cutex::asks::questions::record_question(state, cutex_session_id, runtime_generation, &request.id, &params, now)
                    })
                    .map(|_| ()),
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    };
    if let Err(error) = result {
        eprintln!("asks: runtime event for {cutex_session_id}: {error:#}");
    }
}

/// One session's presence inputs observed outside the ask-store lock.
#[derive(Clone)]
struct ObservedPresence {
    entry: cutex::asks::store::Presence,
    runtime_generation: u64,
    /// The durable record still holds a settled runtime binding whose process
    /// is alive (no launch claim). Presence evidence only, never authority.
    binding_live: bool,
}

fn observe_presence(
    record: &cutex::session::model::CutexSessionRecord,
    users: Option<&cutex::management::v2::user_input::UserInputRepository>,
) -> ObservedPresence {
    let session = &record.cutex_session_id;
    // The record at hand decides the manager: `session_manager` would reload
    // the whole session store once per agent (195 agents × 27 MB per refresh
    // on the Linux host, 2026-10-09).
    let manager = if record.claude_session.is_some() {
        super::claude_runtime::manager()
    } else {
        app_server_runtime::runtime_manager()
    };
    let status = manager.status(session).ok().flatten();
    let online = status
        .as_ref()
        .is_some_and(|status| status.connected && status.runtime_generation == record.runtime_generation);
    let owner_queue = users
        .and_then(|repository| repository.list(session).ok())
        .map(|(_, items)| items.len())
        .unwrap_or(0);
    ObservedPresence {
        entry: cutex::asks::store::Presence {
            online,
            busy: status.as_ref().is_some_and(|status| status.active_turn_id.is_some()),
            paused: false,
            owner_queue,
            asks_unsupported: false,
            lifecycle_generation: None,
            runtime_generation: Some(record.runtime_generation),
            turn_id: status.as_ref().and_then(|status| status.active_turn_id.clone()),
            agent_pause: None,
        },
        runtime_generation: record.runtime_generation,
        binding_live: runtime_binding_live(record),
    }
}

/// A settled durable runtime binding whose process exists and did not start
/// after the binding (PID reuse). Cheap: one `/proc` (or OS) lookup.
fn runtime_binding_live(record: &cutex::session::model::CutexSessionRecord) -> bool {
    cutex::asks::wake::runtime_binding_live(record)
}

/// Merge one observation with the stored entry: keep a lifecycle-proven
/// generation online while this process has no connection of its own and
/// the durable binding of that generation stays live.
fn merge_presence(
    observed: ObservedPresence,
    previous: Option<&cutex::asks::store::Presence>,
    proven_generation: Option<u64>,
    paused: bool,
) -> cutex::asks::store::Presence {
    let mut entry = observed.entry;
    entry.runtime_generation = Some(observed.runtime_generation);
    entry.paused = paused;
    entry.agent_pause = previous.and_then(|previous| previous.agent_pause.clone());
    let proven = proven_generation
        .or_else(|| previous.and_then(|previous| previous.lifecycle_generation))
        .filter(|generation| *generation == observed.runtime_generation && observed.binding_live);
    if let Some(generation) = proven {
        entry.lifecycle_generation = Some(generation);
        entry.online = true;
    }
    entry
}

/// Refresh `agent_list` presence for every agent-enabled session.
fn refresh_presence(store: &AskStore) -> anyhow::Result<()> {
    let users = cutex::management::v2::user_input::user_input_repository().ok();
    // Copy only the records it needs out of the cached store (not the whole
    // store), and ask the runtime managers after the cache lock is released.
    let records: Vec<cutex::session::model::CutexSessionRecord> =
        cutex::session::store::with_cutex_session_store(|sessions| {
            sessions
                .sessions
                .values()
                .filter(|record| record.agent_enabled && !cutex::agent_management::agent_execution_blocked(&record))
                .cloned()
                .collect()
        })?;
    let observed: Vec<(String, ObservedPresence)> = records
        .iter()
        .map(|record| (record.cutex_session_id.clone(), observe_presence(record, users)))
        .collect();
    refresh_observed_presence(store, &observed)
}

fn refresh_observed_presence(store: &AskStore, observed: &[(String, ObservedPresence)]) -> anyhow::Result<()> {
    use cutex::asks::{pause_admission, project_control};
    let merged = |state: &cutex::asks::store::AskState, projects: &project_control::ProjectControlState| {
        let mut presence = std::collections::BTreeMap::new();
        for (session, observed) in observed {
            let paused = state.pauses.contains_key(session)
                || state.presence.get(session).is_some_and(|p| p.agent_pause.is_some())
                || project_control::is_paused_in(projects, session);
            let entry = merge_presence(observed.clone(), state.presence.get(session), None, paused);
            presence.insert(session.clone(), entry);
        }
        // Reconciliation retains sticky reasons even when no runtime record is
        // currently observed. Include those entries in the comparison as well.
        for control in projects.projects.values().filter(|p| p.pause_id.is_some()) {
            for session in &control.members {
                let paused = state.pauses.contains_key(session)
                    || project_control::is_paused_in(projects, session)
                    || presence.get(session).is_some_and(|p| p.agent_pause.is_some());
                presence.entry(session.clone()).or_default().paused = paused;
            }
        }
        presence
    };
    let _ = pause_admission::try_with_authority(|snapshot| {
        let changed = store.inspect(|state| {
            let projects = project_control::project_snapshot(state, snapshot);
            let presence = merged(state, &projects);
            projects != state.project_control || presence != state.presence
                || presence.values().any(|p| !p.busy && p.agent_pause.as_ref().is_some_and(|pause|
                    pause.phase == cutex::asks::store::AgentPausePhase::Pausing))
        })?;
        if !changed { return Ok(()); }
        // Keep the same authority guard; re-read inside the final transaction
        // because presence observers may have changed AskState in the meantime.
        pause_admission::with_state_in(store, snapshot, |state, _| {
        state.presence = merged(state, &state.project_control);
        pause_admission::reconcile_in(state, snapshot);
        let sessions: Vec<_> = state.presence.iter()
            .filter(|(_, presence)| presence.agent_pause.is_some())
            .map(|(session, _)| session.clone()).collect();
        for session in sessions {
            let busy = state.presence.get(&session).is_some_and(|presence| presence.busy);
            cutex::asks::agent_pause::settle(state, &session, busy, Utc::now());
        }
        })
    })?;
    Ok(())
}

/// Presence learns a new turn at once (not at the next refresh): agent_tell
/// reads the caller's turn for the wake loop guard and `senderTurnId`.
fn note_turn_started(store: &AskStore, session: &str, params: &serde_json::Value) {
    let Some(turn) = params.pointer("/turn/id").or_else(|| params.get("turnId")).and_then(serde_json::Value::as_str) else {
        return;
    };
    let result = store.with_state(|state| {
        let presence = state.presence.entry(session.to_string()).or_default();
        presence.busy = true;
        presence.turn_id = Some(turn.to_string());
    });
    if let Err(error) = result {
        eprintln!("asks: presence of {session} not updated: {error:#}");
    }
}

fn note_turn_completed(store: &AskStore, session: &str, now: chrono::DateTime<Utc>) {
    let result = store.with_state(|state| {
        if let Some(presence) = state.presence.get_mut(session) {
            presence.busy = false;
            presence.turn_id = None;
        }
        cutex::asks::agent_pause::settle(state, session, false, now);
    });
    if let Err(error) = result {
        eprintln!("agent pause: terminal turn of {session} not settled: {error:#}");
    }
}

/// Refresh one session's presence right after a lifecycle action brought it
/// online (`member_runtime` online/restart, `agent_create`, replace, rotate),
/// instead of waiting for the next periodic refresh. `proven_generation` is
/// the generation the action validated as ready.
pub(crate) fn refresh_session_presence(session: &str, proven_generation: Option<u64>) {
    let result = (|| -> anyhow::Result<()> {
        let store = AskStore::open_default()?;
        let Some(record) = cutex::session::store::with_cutex_session_store(|sessions| sessions.sessions.get(session).cloned())? else {
            return Ok(());
        };
        let record = &record;
        if !record.agent_enabled || cutex::agent_management::agent_execution_blocked(&record) {
            return Ok(());
        }
        let users = cutex::management::v2::user_input::user_input_repository().ok();
        let observed = observe_presence(record, users);
        refresh_observed_session_presence(&store, session, observed, proven_generation)
    })();
    if let Err(error) = result {
        eprintln!("asks: presence refresh for {session} failed: {error:#}");
    }
}

fn refresh_observed_session_presence(store: &AskStore, session: &str, observed: ObservedPresence, proven_generation: Option<u64>) -> anyhow::Result<()> {
    use cutex::asks::{pause_admission, project_control};
    let merged = |state: &cutex::asks::store::AskState, projects: &project_control::ProjectControlState| {
        let paused = state.pauses.contains_key(session)
            || state.presence.get(session).is_some_and(|p|p.agent_pause.is_some())
            || project_control::is_paused_in(projects, session);
        merge_presence(observed.clone(), state.presence.get(session), proven_generation, paused)
    };
    let _ = pause_admission::try_with_authority(|snapshot| {
        let changed = store.inspect(|state| {
            let projects = project_control::project_snapshot(state, snapshot);
            projects != state.project_control || state.presence.get(session) != Some(&merged(state, &projects))
        })?;
        if !changed { return Ok(()); }
        pause_admission::with_state_in(store, snapshot, |state, _| {
            let entry = merged(state, &state.project_control);
            state.presence.insert(session.to_string(), entry);
            pause_admission::reconcile_in(state, snapshot);
        })
    })?;
    Ok(())
}

#[cfg(test)]
mod presence_tests {
    use super::*;
    use cutex::asks::store::Presence;

    #[test]
    fn exact_interrupt_reenters_existing_session_lane_before_binding_check() {
        let _home=crate::cli_app::test_home::IsolatedTestHome::new_private("exact-stop-lane").unwrap();
        let session="cutex.exact-stop-missing-original";
        let lane=app_server_user_input::session_input_lock(session).unwrap();
        let held=lane.lock().unwrap();
        let before=lane.snapshot().unwrap();
        // The production adapter must reenter this same capability, then fail
        // before any transport lookup/effect when the original binding is gone.
        let error=ManagementAskRuntime::default().interrupt_exact(session,7,"old-turn").unwrap_err();
        assert!(error.to_string().contains("no such session"));
        assert_eq!(lane.snapshot().unwrap(),before);
        drop(held);
        assert!(lane.snapshot().unwrap().active.is_none());
    }

    fn observed(online: bool, generation: u64, binding_live: bool) -> ObservedPresence {
        ObservedPresence {
            entry: Presence { online, ..Presence::default() },
            runtime_generation: generation,
            binding_live,
        }
    }

    #[test]
    fn lifecycle_proven_generation_is_online_immediately_and_survives_periodic_refresh() {
        // Agent Bus process: action validated generation 3 ready, its own
        // manager may or may not hold the connection.
        let marked = merge_presence(observed(false, 3, true), None, Some(3), false);
        assert!(marked.online);
        assert_eq!(marked.lifecycle_generation, Some(3));
        assert_eq!(marked.status(), "idle");
        // Management process periodic refresh without its own connection.
        let kept = merge_presence(observed(false, 3, true), Some(&marked), None, false);
        assert!(kept.online);
        assert_eq!(kept.lifecycle_generation, Some(3));
        // Binding gone (stopped) or a new generation: no stale online claim.
        assert!(!merge_presence(observed(false, 3, false), Some(&marked), None, false).online);
        let next = merge_presence(observed(false, 4, true), Some(&marked), None, false);
        assert!(!next.online);
        assert_eq!(next.lifecycle_generation, None);
        // Paused flag comes from the store.
        assert_eq!(merge_presence(observed(true, 4, true), None, None, true).status(), "paused");
    }

    #[test]
    fn unproven_offline_session_stays_offline() {
        let entry = merge_presence(observed(false, 1, true), None, None, false);
        assert!(!entry.online);
        assert_eq!(entry.lifecycle_generation, None);
        let connected = merge_presence(observed(true, 1, false), None, None, false);
        assert!(connected.online);
    }

    #[test]
    fn unchanged_presence_refresh_leaves_revision_and_pause_commands_unchanged() {
        let root=std::env::temp_dir().join(format!("cutex-presence-noop-{}",uuid::Uuid::new_v4()));
        let store=AskStore::at(&root);
        let observations=vec![("cutex.noop".into(),observed(true,3,true))];
        refresh_observed_presence(&store,&observations).unwrap();
        let before=store.inspect(|s|(s.revision,s.presence.clone(),s.project_control.clone(),s.commands.clone())).unwrap();
        for _ in 0..4 {
            refresh_observed_presence(&store,&observations).unwrap();
            refresh_observed_session_presence(&store,"cutex.noop",observations[0].1.clone(),None).unwrap();
        }
        assert_eq!(store.inspect(|s|(s.revision,s.presence.clone(),s.project_control.clone(),s.commands.clone())).unwrap(),before);
        let _=std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore="isolated synthetic copy-count/timing probe: run this filter alone with --test-threads=1 --nocapture"]
    fn measure_unchanged_presence_refresh_whole_state_copies() {
        let root=std::env::temp_dir().join(format!("cutex-presence-copies-{}",uuid::Uuid::new_v4()));
        let store=AskStore::at(&root);
        let observations=vec![("cutex.noop".into(),observed(true,3,true))];
        refresh_observed_presence(&store,&observations).unwrap();
        let copies=cutex::asks::store::whole_state_copy_count();
        let start=std::time::Instant::now();
        for _ in 0..32 {
            refresh_observed_presence(&store,&observations).unwrap();
            refresh_observed_session_presence(&store,"cutex.noop",observations[0].1.clone(),None).unwrap();
        }
        let elapsed=start.elapsed();
        let copied=cutex::asks::store::whole_state_copy_count()-copies;
        eprintln!("presence_noop refreshes=64 whole_state_copies={copied} elapsed_us={}",elapsed.as_micros());
        assert_eq!(copied,0);
        let _=std::fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_observation_still_settles_an_idle_individual_pause() {
        let root=std::env::temp_dir().join(format!("cutex-presence-settle-{}",uuid::Uuid::new_v4()));
        let store=AskStore::at(&root);
        let observations=vec![("cutex.noop".into(),observed(true,3,true))];
        refresh_observed_presence(&store,&observations).unwrap();
        store.with_state(|s| {
            s.presence.get_mut("cutex.noop").unwrap().busy=true;
            cutex::asks::agent_pause::pause(s,"cutex.noop","owner",None,Utc::now());
            s.presence.get_mut("cutex.noop").unwrap().busy=false;
        }).unwrap();
        refresh_observed_presence(&store,&observations).unwrap();
        assert_eq!(store.inspect(|s|s.presence["cutex.noop"].agent_pause.as_ref().unwrap().phase.clone()).unwrap(),cutex::asks::store::AgentPausePhase::Paused);
        let _=std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;
    use cutex::app_server::external_input::exclusive_envelope_allowed;

    fn binding() -> cutex::launch::stock::ExternalInputBinding {
        serde_json::from_value(json!({
            "version": 1, "ownerId": "owner", "threadId": "thread", "runtimeGeneration": 3
        }))
        .unwrap()
    }

    fn ask(view: bool) -> Submission {
        Submission {
            message_id: "ask-1.1".into(),
            source_agent: Some("cutex.lead".into()),
            event_type: "ASK",
            text: "sum 1..10".into(),
            asks: vec!["ask-1".into()],
            attempt: Some(1),
            view: view.then(|| {
                json!({
                    "schema": "cutex.delegation-ask.v1",
                    "data": {"askId": "ask-1", "attempt": 1, "gated": false, "title": "sum",
                             "requester": {"kind": "agent", "cutexSessionId": "cutex.lead", "name": "Lead"}},
                })
            }),
        }
    }

    #[test]
    fn delegation_ask_envelope_is_v2_with_view_and_exclusive_admissible() {
        let envelope = build_envelope(&binding(), true, &ask(true)).unwrap();
        assert_eq!(envelope.version, 2);
        assert!(envelope.view.is_some());
        assert_eq!(envelope.message.delivery, Delivery::AfterTurn);
        exclusive_envelope_allowed(&envelope, true).unwrap();
        // message_status rebuilds the same key.
        let again = build_envelope(&binding(), true, &ask(true)).unwrap();
        assert_eq!(again.semantic_sha256, envelope.semantic_sha256);
    }

    /// r217h2: a results event for a busy requester is `soon`; message_status
    /// rebuilds the same key from its id, and steer_note's re-digest of it is
    /// the same key (no double admission under two keys).
    #[test]
    fn next_step_results_event_is_soon_with_one_key() {
        let mut event = ask(true);
        event.message_id = cutex::asks::new_id(cutex::asks::returns::NEXT_STEP_EVENT_PREFIX);
        event.event_type = "ANSWER";
        let soon = build_envelope(&binding(), true, &event).unwrap();
        assert_eq!(soon.message.delivery, Delivery::Soon);
        assert_eq!(soon.digest(), soon.semantic_sha256);
        assert_eq!(build_envelope(&binding(), true, &event).unwrap().semantic_sha256, soon.semantic_sha256);
        event.message_id = cutex::asks::new_id("cutex-ask-event");
        assert_eq!(build_envelope(&binding(), true, &event).unwrap().message.delivery, Delivery::AfterTurn);
        assert!(!cutex::asks::returns::is_next_step_event("cutex-ask-soonish-x"));
        assert!(!cutex::asks::returns::is_next_step_event("cutex-ask-reminder-x"));
    }

    /// The receiver's size-policy rejection (JSON-RPC -32603, nothing
    /// recorded) is final and keeps the connection; any other submit error
    /// stays an unknown outcome (reconcile, connection dropped by the cache policy). The
    /// runtime-wide "blocks restored history" error is not a size verdict on
    /// this message.
    #[test]
    fn size_policy_rejection_is_final_other_submit_errors_are_uncertain() {
        let live = "native ingress RPC outcome: reconcile the same key; never infer absence from a transport error: app-server request failed with -32603: invalid external input: receiver canonical byte policy exceeded";
        let outcome = submit_error_outcome(live, (Some(11_501), Some(10_000)));
        assert!(!discard_submit_result::<()>(&Err(anyhow::anyhow!(live))));
        assert!(!discard_soon_result::<()>(&Err(anyhow::anyhow!(live))));
        assert_eq!(outcome, SubmitOutcome::TooLarge { item_bytes: Some(11_501), limit: Some(10_000) });
        let blocked = "app-server request failed with -32603: receiver canonical byte policy blocks restored history; adjust trusted launch policy and restart";
        assert!(discard_submit_result::<()>(&Err(anyhow::anyhow!(blocked))));
        assert!(discard_soon_result::<()>(&Err(anyhow::anyhow!("broken pipe"))));
        assert!(!discard_soon_result::<()>(&Err(anyhow::anyhow!("does not support soon"))));
        assert!(matches!(submit_error_outcome(blocked, (None, None)), SubmitOutcome::Uncertain(_)));
        assert!(matches!(submit_error_outcome("broken pipe", (None, None)), SubmitOutcome::Uncertain(_)));
    }

    /// The size estimate follows what the receiver measures: the text is
    /// JSON-escaped twice (a newline costs 3 bytes), CJK is not escaped, and
    /// the wrapper adds a few hundred bytes. A text under 10,000 bytes can
    /// still exceed the default 10,000-byte policy.
    #[test]
    fn model_item_size_counts_double_escaping() {
        assert_eq!(
            binding().canonical_byte_limit,
            cutex::launch::stock::CanonicalBytePolicy::default(),
            "no canonicalByteLimit in the binding means the 10,000-byte default"
        );
        let mut submission = ask(false);
        submission.text = "line\n".repeat(1_900);
        assert_eq!(submission.text.len(), 9_500);
        let envelope = build_envelope(&binding(), false, &submission).unwrap();
        let bytes = model_item_bytes(&envelope);
        assert!(bytes >= 9_500 + 1_900 * 2, "{bytes}");
        assert!(bytes > 10_000);
        submission.text = "中".repeat(3_000);
        let envelope = build_envelope(&binding(), false, &submission).unwrap();
        let bytes = model_item_bytes(&envelope);
        assert!((9_000..9_500).contains(&bytes), "{bytes}");
    }

    #[test]
    fn runtime_without_v2_gets_the_ask_as_v1_without_view() {
        let envelope = build_envelope(&binding(), false, &ask(true)).unwrap();
        assert_eq!(envelope.version, 1);
        assert!(envelope.view.is_none());
        exclusive_envelope_allowed(&envelope, false).unwrap();
        let plain = build_envelope(&binding(), false, &ask(false)).unwrap();
        assert_eq!(plain.semantic_sha256, envelope.semantic_sha256);
        assert_eq!(build_envelope(&binding(), true, &ask(false)).unwrap().version, 1);
    }
}
