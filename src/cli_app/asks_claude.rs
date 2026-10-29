//! Asks (委托) on Claude targets (asks design §11, §12.1 P1(d)).
//!
//! The claude-adapter runs every input as its own turn (`fifo-separate-turns`)
//! and allocates the turn id when it enqueues, so the receipt of
//! `claude/input/submit` is the attempt ↔ turn mapping. Exclusive admission
//! is `admission: "exclusive_turn"` on that call (adapter input capability
//! `admissions: ["exclusive_turn"]`): the adapter refuses with the same
//! `externalInputError: "busy"` shape as cute-codex N1 instead of queueing
//! behind other work, and a known message id returns its receipt and never
//! queues a second turn. Without the capability (an older adapter) asks stay
//! held `runtimeUnsupported`.
//! r8 keeps this submit/status path: the complete asks/attempt metadata cannot
//! be reconstructed from a results view that may deliberately omit rows.
//! Queue gate controls use the native protocol when advertised.
//! Busy result events keep this metadata path with `admission: next_step`;
//! only the advertised durable result capability enables it. Acceptance and
//! consumption are separate, and unknown outcomes retain the original id.
//!
//! Requester stop uses the bound r8 external-input interrupt, preserving the
//! previous queue gate. Older adapters without that capability are refused.
//! Owner control keeps its separate stop/resume behavior.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use cutex::app_server::client::{AppServerClientError, AppServerHandle};
use cutex::asks::scheduler::{NativeMessageStatus, Submission, SubmitOutcome, TargetStatus, EXCLUSIVE_TURN};
use cutex::session::model::CutexSessionRecord;

use super::app_server_user_input;
use super::claude_runtime;

/// The adapter's exclusive admission value (input capability `admissions`).
pub(crate) const ADMISSION: &str = "exclusive_turn";
/// Source id of events cutex itself submits (owner asks, ANSWER events).
const CUTEX_SOURCE: &str = "cutex-asks";

/// Capability per session and runtime generation (an adapter restart is a
/// new generation).
fn capability_cache() -> &'static Mutex<HashMap<String, (u64, Value)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (u64, Value)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn handle(session: &str, generation: u64) -> anyhow::Result<AppServerHandle> {
    claude_runtime::manager()
        .handle_for_generation(session, generation)
        .map_err(|error| anyhow::anyhow!("Claude runtime unavailable: {error:?}"))
}

/// Whether `capabilities` (the `claude/capabilities` result) offers
/// exclusive admission.
pub(crate) fn offers_exclusive(capabilities: &Value) -> bool {
    capabilities
        .pointer("/input/admissions")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(ADMISSION)))
}

fn supports_exclusive(session: &str, generation: u64) -> bool {
    adapter_capabilities(session, generation).is_some_and(|capabilities| offers_exclusive(&capabilities))
}

/// The adapter's `claude/capabilities` result, cached per runtime generation.
pub(crate) fn adapter_capabilities(session: &str, generation: u64) -> Option<Value> {
    if let Some((known, value)) = capability_cache().lock().ok().and_then(|cache| cache.get(session).cloned()) {
        if known == generation {
            return Some(value);
        }
    }
    let capabilities = handle(session, generation)
        .and_then(|handle| Ok(handle.request("claude/capabilities", json!({}))?))
        .ok()?;
    cutex::runtime::claude::note_live_capabilities(session, generation, &capabilities);
    if let Ok(mut cache) = capability_cache().lock() {
        cache.insert(session.to_string(), (generation, capabilities.clone()));
    }
    Some(capabilities)
}

/// claude-r5 (adapter 0.4.0): `claude/input/submit` accepts a top-level
/// `view`, passive notes, and projects agent messages as timeline items.
pub(crate) fn offers_agent_messages_v2(capabilities: &Value) -> bool {
    capabilities.pointer("/input/views").and_then(Value::as_bool) == Some(true)
        && capabilities
            .pointer("/input/deliveryModes")
            .and_then(Value::as_array)
            .is_some_and(|modes| modes.iter().any(|mode| mode.as_str() == Some("passive")))
}

pub(crate) fn offers_next_step(capabilities: &Value) -> bool {
    offers_agent_messages_v2(capabilities)
        && capabilities.pointer("/input/durableNextStepResults") == Some(&json!(true))
        && capabilities.pointer("/input/deliveryModes").and_then(Value::as_array)
            .is_some_and(|modes| modes.iter().any(|mode| mode == "soon"))
}

pub(crate) fn target_status(session: &str, record: &CutexSessionRecord) -> TargetStatus {
    let Ok(Some(status)) = claude_runtime::manager().status(session) else {
        return TargetStatus { runtime_generation: record.runtime_generation, ..TargetStatus::default() };
    };
    let online = status.connected && status.runtime_generation == record.runtime_generation;
    TargetStatus {
        online,
        runtime_generation: record.runtime_generation,
        active_turn_id: status.active_turn_id,
        admission: None,
        // The adapter refuses with `plan_mode` while Plan mode is on.
        plan_mode: false,
        profile_switch_pending: cutex::management::v2::profile_switch::pending_switch(session).is_some(),
        exclusive_admission: online && supports_exclusive(session, record.runtime_generation),
        // Its usage limit is exhausted until a pending quota continuation
        // (claude-quota-auto-continue): delegations wait `blocked(quota)`.
        blocked: (online && cutex::management::v2::quota_continue::held_default(session))
            .then_some(cutex::asks::model::BlockedReason::Quota),
        reconnectable: false,
        // Claude has no serialized-size policy, only the raw text cap.
        receiver_limit: cutex::app_server::external_input::size::ReceiverLimit::RawText(
            cutex::app_server::external_input::size::TRANSPORT_TEXT_MAX as u64,
        ),
    }
}

/// `claude/input/submit` params for a submission (deterministic: a retry
/// with the same message id must carry the same semantic content).
/// `views`: the adapter takes the `view` param (claude-r5); older adapters
/// refuse unknown fields, so the view is left out for them.
pub(crate) fn submit_params(thread_id: &str, target: &str, submission: &Submission, views: bool) -> Value {
    let source = match &submission.source_agent {
        Some(agent) => json!({"kind": "agent", "id": agent}),
        None => json!({"kind": "service", "id": CUTEX_SOURCE}),
    };
    let mut metadata = json!({
        "cutexSessionId": target,
        "deliveryMode": "after-turn",
        "asks": submission.asks,
    });
    if let Some(attempt) = submission.attempt {
        metadata["attempt"] = json!(attempt);
    }
    let mut params = json!({
        "threadId": thread_id,
        "messageId": submission.message_id,
        "source": source,
        "eventType": submission.event_type,
        "text": submission.text,
        "metadata": metadata,
        "admission": ADMISSION,
    });
    if let Some(view) = submission.view.as_ref().filter(|_| views) {
        params["view"] = view.clone();
    }
    if cutex::asks::returns::is_next_step_event(&submission.message_id) {
        params["admission"] = json!("next_step");
        params["metadata"]["deliveryMode"] = json!("soon");
    }
    params
}

/// Map the submit result.
pub(crate) fn submit_outcome(result: Result<Value, AppServerClientError>) -> SubmitOutcome {
    match result {
        Ok(receipt) => {
            let Some(turn_id) = receipt.get("turnId").and_then(Value::as_str) else {
                return SubmitOutcome::Uncertain("receipt without turnId; reconciling".into());
            };
            let state = receipt.get("state").and_then(Value::as_str).unwrap_or_default();
            // A replay of an input that already ended: reconcile its status.
            if receipt.get("duplicate") == Some(&json!(true)) && !matches!(state, "queued" | "processing") {
                return SubmitOutcome::Uncertain(format!("replayed input is {state}; reconciling"));
            }
            SubmitOutcome::Started { turn_id: turn_id.to_string() }
        }
        Err(AppServerClientError::Rpc(error)) => {
            let data = error.data.as_ref();
            if data.and_then(|data| data.get("externalInputError")).and_then(Value::as_str) == Some("busy") {
                let reason = data.and_then(|data| data.get("reason")).and_then(Value::as_str).unwrap_or("active_turn");
                return SubmitOutcome::Busy { reason: reason.to_string() };
            }
            match error.code {
                // Message id reused with different content: never rerun.
                -32010 => SubmitOutcome::Refused("message_id_conflict".into()),
                -32001 => SubmitOutcome::Busy { reason: "inbox_full".into() },
                // An adapter without exclusive admission rejects the field.
                -32602 if error.message.contains("unsupported input fields") => {
                    SubmitOutcome::Unsupported(EXCLUSIVE_TURN.into())
                }
                -32009 | -32011 => SubmitOutcome::Busy { reason: "gate_closed".into() },
                _ => SubmitOutcome::Busy { reason: format!("runtime_error: {}", error.message) },
            }
        }
        // Not sent.
        Err(
            error @ (AppServerClientError::Connect(_)
            | AppServerClientError::Backpressure(_)
            | AppServerClientError::Shutdown
            | AppServerClientError::InvalidOptions(_)
            | AppServerClientError::InvalidEndpoint(_)
            | AppServerClientError::RequestIdInUse(_)),
        ) => SubmitOutcome::Unsupported(format!("runtime unavailable: {error}")),
        // Sent, outcome unknown: reconcile by message id.
        Err(error) => SubmitOutcome::Uncertain(error.to_string()),
    }
}

pub(crate) fn submit(session: &str, generation: u64, submission: &Submission) -> SubmitOutcome {
    let Some(record) = super::asks_runtime::record(session) else {
        return SubmitOutcome::Unsupported("no such session".into());
    };
    let Some(thread_id) = record.app_server_thread_id().map(str::to_string) else {
        return SubmitOutcome::Unsupported("Claude thread missing".into());
    };
    // Serialize with owner submit / flush / stop / revert.
    let lock = match app_server_user_input::session_input_lock(session) {
        Ok(lock) => lock,
        Err(error) => return SubmitOutcome::Unsupported(format!("{error:#}")),
    };
    let Ok(_guard) = lock.lock() else {
        return SubmitOutcome::Unsupported("session input lock poisoned".into());
    };
    if let Some(outcome)=super::asks_runtime::pause_admission_outcome_in_lane(session){return outcome;}
    let next_step = cutex::asks::returns::is_next_step_event(&submission.message_id);
    if next_step && (submission.source_agent.is_some() || !matches!(submission.event_type, "ANSWER" | "QUESTION")
        || submission.attempt.is_some() || submission.asks.is_empty()) {
        return SubmitOutcome::Refused("next_step_requires_delegation_result".into());
    }
    if !next_step {
        if let Ok(Some(status)) = claude_runtime::manager().status(session) {
            if status.active_turn_id.is_some() {
                return SubmitOutcome::Busy { reason: "active_turn".into() };
            }
        }
    }
    if !supports_exclusive(session, generation) {
        return SubmitOutcome::Unsupported(EXCLUSIVE_TURN.into());
    }
    if next_step && !adapter_capabilities(session, generation).is_some_and(|caps| offers_next_step(&caps)) {
        return SubmitOutcome::Unsupported("durable_next_step_results".into());
    }
    let handle = match handle(session, generation) {
        Ok(handle) => handle,
        Err(error) => return SubmitOutcome::Unsupported(format!("{error:#}")),
    };
    let views = adapter_capabilities(session, generation).is_some_and(|capabilities| offers_agent_messages_v2(&capabilities));
    let params = submit_params(&thread_id, session, submission, views);
    let digest = legacy_digest(&params);
    let result = handle.request("claude/input/submit", params);
    if next_step { next_step_submit_outcome(result, &thread_id, &submission.message_id, &digest) } else { submit_outcome(result) }
}

/// The existing Python inbox digest (sorted UTF-8 JSON), not a new identity.
fn legacy_digest(params: &Value) -> String {
    use sha2::Digest;
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => serde_json::to_value(map.iter().map(|(key, value)| (key, sorted(value)))
                .collect::<std::collections::BTreeMap<_, _>>()).expect("JSON object"),
            Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    let mut payload = json!({});
    for key in ["source", "eventType", "text", "metadata", "view"] {
        if let Some(value) = params.get(key) { payload[key] = value.clone(); }
    }
    format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(&sorted(&payload)).expect("JSON payload")))
}

fn next_step_submit_outcome(result: Result<Value, AppServerClientError>, thread: &str, message: &str, digest: &str) -> SubmitOutcome {
    match result {
        Ok(receipt) => {
            if receipt["threadId"] != thread || receipt["messageId"] != message || receipt["deliveryMode"] != "soon"
                || receipt["semanticSha256"] != digest
                || receipt["durable"] != true || receipt["receiptScope"] != "local-inbox" {
                return SubmitOutcome::Uncertain("next-step acceptance receipt missing or mismatched; reconcile same id".into());
            }
            // Acceptance only. The scheduler waits for contextPersisted before
            // acknowledging, including on a duplicate of an already-read input.
            SubmitOutcome::Started { turn_id: String::new() }
        }
        Err(AppServerClientError::Rpc(error)) if error.data.as_ref()
            .and_then(|data| data.get("externalInputError")).and_then(Value::as_str) != Some("busy")
            && !matches!(error.code, -32602 | -32001 | -32009 | -32011) => {
                SubmitOutcome::Uncertain(format!("next-step RPC outcome unknown ({}); reconcile same id", error.code))
            }
        other => submit_outcome(other),
    }
}

/// Map a `claude/input/status` result.
pub(crate) fn status_outcome(result: Result<Value, AppServerClientError>) -> NativeMessageStatus {
    match result {
        Ok(receipt) => {
            if receipt["deliveryMode"] == "soon" {
                return next_step_status(&receipt);
            }
            let turn_id = receipt.get("turnId").and_then(Value::as_str).map(str::to_string);
            match (receipt.get("state").and_then(Value::as_str).unwrap_or_default(), turn_id) {
                ("queued", _) => NativeMessageStatus::Pending,
                ("processing" | "completed", Some(turn_id)) => NativeMessageStatus::Persisted { turn_id },
                ("interrupted", turn_id) => NativeMessageStatus::Held { reason: "interrupted".into(), turn_id },
                ("failed", turn_id) => NativeMessageStatus::Held { reason: "failed".into(), turn_id },
                ("uncertain", turn_id) => NativeMessageStatus::Held { reason: "request_uncertain".into(), turn_id },
                (other, _) => NativeMessageStatus::Unavailable(format!("unknown input state {other}")),
            }
        }
        // `input not found`: never reached the adapter's inbox.
        Err(AppServerClientError::Rpc(error)) if error.code == -32004 => NativeMessageStatus::Absent,
        Err(error) => NativeMessageStatus::Unavailable(error.to_string()),
    }
}

fn next_step_status(receipt: &Value) -> NativeMessageStatus {
    if receipt["contextPersisted"] == true {
        if let Some(turn) = receipt["contextTurnId"].as_str() {
            return NativeMessageStatus::Persisted { turn_id: turn.into() };
        }
        return NativeMessageStatus::Unavailable("context receipt without consuming turn".into());
    }
    match receipt["state"].as_str() {
        Some("queued" | "processing" | "injecting") => NativeMessageStatus::Pending,
        // Claimed into a busy turn that ended without a context receipt, or
        // run as its own FIFO turn that failed: never replayed (at most once).
        // Held with the turn that took it (the claiming turn, else the
        // reserved one) so the scheduler acknowledges the event and releases
        // the requester's lease, as on the exclusive path. Without a turn the
        // lease would never be released and would block the owner's queue.
        Some("completed" | "failed" | "uncertain" | "interrupted" | "reverted") => NativeMessageStatus::Held {
            reason: "request_uncertain".into(),
            turn_id: receipt["contextTurnId"].as_str().or_else(|| receipt["turnId"].as_str())
                .filter(|turn| !turn.is_empty()).map(str::to_string),
        },
        _ => NativeMessageStatus::Unavailable("unknown next-step receipt state".into()),
    }
}

pub(crate) fn message_status(session: &str, generation: u64, submission: &Submission) -> NativeMessageStatus {
    if cutex::asks::returns::is_owner_note(&submission.message_id) {
        return owner_note_status(session, generation, submission);
    }
    let Some(thread_id) = super::asks_runtime::record(session).and_then(|record| record.app_server_thread_id().map(str::to_string))
    else {
        return NativeMessageStatus::Unavailable("Claude thread missing".into());
    };
    let handle = match handle(session, generation) {
        Ok(handle) => handle,
        Err(error) => return NativeMessageStatus::Unavailable(format!("{error:#}")),
    };
    let result = handle.request(
        "claude/input/status",
        json!({"threadId": thread_id, "messageId": submission.message_id}),
    );
    if cutex::asks::returns::is_next_step_event(&submission.message_id) {
        let digest = legacy_digest(&submit_params(&thread_id, session, submission, true));
        return match result {
            Ok(receipt) if receipt["threadId"] == thread_id && receipt["messageId"] == submission.message_id
                && receipt["deliveryMode"] == "soon" && receipt["semanticSha256"] == digest => next_step_status(&receipt),
            // A restarted runtime with a missing inbox is not permission to
            // generate a new event identity and execute the result twice.
            Ok(_) => NativeMessageStatus::Unavailable("next-step receipt binding mismatch".into()),
            Err(error) => NativeMessageStatus::Unavailable(error.to_string()),
        };
    }
    status_outcome(result)
}

fn owner_note_envelope(session: &str, thread: &str, generation: u64, submission: &Submission)
    -> anyhow::Result<cutex::app_server::external_input::Envelope> {
    anyhow::ensure!(cutex::asks::returns::is_owner_note(&submission.message_id)
        && submission.source_agent.is_none() && submission.attempt.is_none()
        && !submission.asks.is_empty(), "owner note must carry delegation results");
    submission.envelope(&cutex::launch::stock::ExternalInputBinding {
        version: 1, owner_id: session.into(), thread_id: thread.into(), runtime_generation: generation,
        canonical_byte_limit: Default::default(),
    }, true)
}

/// Called with the owner's session lane held. Native passive inputs do not
/// start a turn; the immediately following owner turn consumes their context.
pub(crate) fn stage_owner_results_locked(session: &str, store: &cutex::asks::store::AskStore) -> anyhow::Result<bool> {
    let (thread, generation, handle) = thread_and_handle(session)?;
    if !adapter_capabilities(session, generation).is_some_and(|caps| super::claude_ingress::offers_native(&caps)) {
        return Ok(false);
    }
    cutex::asks::returns::stage_owner_results(store, session, generation,
        cutex::config::documents::current(), chrono::Utc::now(), |submission| {
            let envelope = owner_note_envelope(session, &thread, generation, submission)?;
            super::claude_ingress::submit(&handle, &envelope, generation)?;
            Ok(())
        })
}

fn owner_note_status(session: &str, generation: u64, submission: &Submission) -> NativeMessageStatus {
    let result = (|| -> anyhow::Result<_> {
        let record = super::asks_runtime::record(session).ok_or_else(|| anyhow::anyhow!("session missing"))?;
        let thread = record.app_server_thread_id().ok_or_else(|| anyhow::anyhow!("thread missing"))?;
        let envelope = owner_note_envelope(session, thread, generation, submission)?;
        super::claude_ingress::status(&handle(session, generation)?, &envelope, generation)
    })();
    match result {
        Ok(status) => {
            if let Some(turn_id) = super::claude_ingress::persisted_turn(&status) {
                NativeMessageStatus::Persisted { turn_id: turn_id.into() }
            } else if status.delivery_state == cutex::app_server::external_input::DeliveryState::Pending {
                NativeMessageStatus::Pending
            } else {
                // Never infer absence or replay an owner-bound passive note.
                NativeMessageStatus::Unavailable("owner result context receipt is unconfirmed".into())
            }
        }
        Err(error) => NativeMessageStatus::Unavailable(format!("{error:#}")),
    }
}

fn thread_and_handle(session: &str) -> anyhow::Result<(String, u64, AppServerHandle)> {
    let record = super::asks_runtime::record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let thread_id = record
        .app_server_thread_id()
        .ok_or_else(|| anyhow::anyhow!("Claude thread missing"))?
        .to_string();
    Ok((thread_id, record.runtime_generation, handle(session, record.runtime_generation)?))
}

/// Exact requester stop using the pinned r8 gate-preserving bound endpoint.
/// Capture the original handle/thread while the lifecycle lane is held; never
/// resolve the current binding again after capture and never use legacy resume.
pub(crate) fn interrupt_exact(session: &str, generation: u64, turn_id: &str) -> anyhow::Result<()> {
    let lane = app_server_user_input::session_input_lock(session)?;
    let _operation = lane.lock()?; // Same-thread nesting shares the dispatch lane.
    let record = super::asks_runtime::record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
    anyhow::ensure!(record.runtime_generation == generation, "runtime_generation_changed");
    let thread = record.app_server_thread_id().ok_or_else(|| anyhow::anyhow!("Claude thread missing"))?.to_string();
    let status = claude_runtime::manager().status(session)?.ok_or_else(|| anyhow::anyhow!("runtime_occurrence_changed"))?;
    anyhow::ensure!(status.connected && status.runtime_generation == generation
        && status.active_turn_id.as_deref() == Some(turn_id), "runtime_occurrence_changed");
    let original_handle = handle(session, generation)?;
    let cached = capability_cache().lock().ok().and_then(|cache| cache.get(session).cloned())
        .filter(|(known, _)| *known == generation).map(|(_, caps)| caps);
    let capabilities = match cached {
        Some(caps) => caps,
        None => original_handle.request("claude/capabilities", json!({}))?,
    };
    interrupt_exact_bound(session, &thread, generation, turn_id, &capabilities,
        |method, params| Ok(original_handle.request(method, params)?))
}

fn interrupt_exact_bound(session: &str, thread: &str, generation: u64, turn_id: &str,
    capabilities: &Value, request: impl FnOnce(&str, Value) -> anyhow::Result<Value>) -> anyhow::Result<()> {
    anyhow::ensure!(super::claude_ingress::offers_controls(capabilities),
        "exact_interrupt_unsupported: adapter lacks bound gate-preserving controls");
    let mut params = super::claude_ingress::binding_params(session, thread, generation);
    params["turnId"] = json!(turn_id);
    // external_input.py preserves the previous halted gate within this single
    // bound RPC. No legacy turn/interrupt or separate queue/resume is allowed.
    request("thread/externalInput/interrupt", params)?;
    Ok(())
}

/// Legacy owner/control compatibility path. StopAttempt uses interrupt_exact.
pub(crate) fn interrupt(session: &str, turn_id: &str) -> anyhow::Result<()> {
    let (thread_id, generation, handle) = thread_and_handle(session)?;
    if adapter_capabilities(session, generation).is_some_and(|caps| super::claude_ingress::offers_controls(&caps)) {
        let mut params = super::claude_ingress::binding_params(session, &thread_id, generation);
        params["turnId"] = json!(turn_id);
        handle.request("thread/externalInput/interrupt", params)?;
        return Ok(());
    }
    handle.request("turn/interrupt", json!({"threadId": thread_id, "turnId": turn_id}))?;
    handle.request("claude/queue/resume", json!({"threadId": thread_id, "acknowledgeUncertainEffects": true}))?;
    Ok(())
}

/// Owner resume of admission: reopen the adapter's input queue. The
/// interrupted input itself is never re-run (adapter rule).
pub(crate) fn resume(session: &str) -> anyhow::Result<()> {
    let (thread_id, generation, handle) = thread_and_handle(session)?;
    if adapter_capabilities(session, generation).is_some_and(|caps| super::claude_ingress::offers_controls(&caps)) {
        handle.request("thread/externalInput/resume", super::claude_ingress::binding_params(session, &thread_id, generation))?;
        return Ok(());
    }
    handle.request("claude/queue/resume", json!({"threadId": thread_id, "acknowledgeUncertainEffects": true}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::app_server::protocol::RpcError;

    fn bound_control_capabilities() -> Value {
        json!({"externalInputVersion":1,"externalInputVersions":[1,2],
            "externalInputDeliveries":["after_turn","passive"],"externalInputControlVersion":1})
    }

    #[test]
    fn exact_stop_bound_request_preserves_original_binding_and_gate_without_resume() {
        for was_halted in [false,true] {
            let gate=std::cell::Cell::new(was_halted);
            let calls=std::cell::Cell::new(0);
            interrupt_exact_bound("original-owner","original-thread",17,"original-turn",
                &bound_control_capabilities(),|method,params| {
                    calls.set(calls.get()+1);
                    assert_eq!(method,"thread/externalInput/interrupt");
                    assert_eq!(params,json!({"version":1,"ownerId":"original-owner","threadId":"original-thread",
                        "runtimeGeneration":17,"turnId":"original-turn"}));
                    // Mock the verified adapter endpoint: interrupt leaves its
                    // previous gate intact. A separate resume would fail here.
                    assert_eq!(gate.get(),was_halted);
                    Ok(json!({}))
                }).unwrap();
            assert_eq!(calls.get(),1);
            assert_eq!(gate.get(),was_halted);
        }
    }

    #[test]
    fn exact_stop_old_adapter_is_unsupported_without_any_request() {
        let mut caps=bound_control_capabilities();caps.as_object_mut().unwrap().remove("externalInputControlVersion");
        let error=interrupt_exact_bound("owner","thread",17,"turn",&caps,|_,_|panic!("legacy fallback RPC")).unwrap_err();
        assert!(error.to_string().starts_with("exact_interrupt_unsupported"));
    }

    #[test]
    fn exact_stop_native_binding_refusal_or_response_loss_never_falls_back() {
        let calls=std::cell::Cell::new(0);
        let error=interrupt_exact_bound("original-owner","original-thread",17,"reused-turn",
            &bound_control_capabilities(),|method,params| {
                calls.set(calls.get()+1);
                assert_eq!(method,"thread/externalInput/interrupt");
                assert_eq!(params["runtimeGeneration"],17);
                anyhow::bail!("external input owner/runtime binding mismatch")
            }).unwrap_err();
        assert!(error.to_string().contains("binding mismatch"));
        assert_eq!(calls.get(),1);
    }

    fn rpc(code: i64, message: &str, data: Option<Value>) -> Result<Value, AppServerClientError> {
        Err(AppServerClientError::Rpc(RpcError { code, message: message.into(), data }))
    }

    #[test]
    fn owner_note_reconnect_preserves_native_key_and_passive_provenance() {
        let submission = Submission {
            message_id: "cutex-ask-owner-identity".into(), source_agent: None, event_type: "ANSWER",
            text: "delegation result".into(), asks: vec!["ask-1".into()], attempt: None,
            view: Some(json!({"schema":"cutex.delegation-results.v1","data":{"items":[]}})),
        };
        let old = owner_note_envelope("owner", "thread", 1, &submission).unwrap();
        let current = owner_note_envelope("owner", "thread", 2, &submission).unwrap();
        assert_eq!(old.key(), current.key(), "transport generation is not semantic input identity");
        assert_eq!(old.message.delivery, cutex::app_server::external_input::Delivery::Passive);
        assert_eq!(current.runtime_generation, 2);
        assert_eq!(current.message.source.id, "cutex-asks");
        assert_eq!(current.view, old.view);
    }

    #[test]
    fn submit_params_carry_ask_ids_and_admission() {
        let submission = Submission {
            message_id: "m1".into(),
            source_agent: Some("cutex.requester".into()),
            event_type: "ASK",
            text: "brief".into(),
            asks: vec!["ask-1".into()],
            attempt: Some(2),
            view: Some(json!({"schema": "cutex.delegation-ask.v1", "data": {"askId": "ask-1"}})),
        };
        let params = submit_params("thread-1", "cutex.target", &submission, true);
        assert_eq!(params["view"]["schema"], "cutex.delegation-ask.v1");
        assert_eq!(params["admission"], "exclusive_turn");
        assert_eq!(params["source"], json!({"kind": "agent", "id": "cutex.requester"}));
        assert_eq!(params["metadata"]["asks"], json!(["ask-1"]));
        assert_eq!(params["metadata"]["attempt"], 2);
        let event = Submission { source_agent: None, event_type: "ANSWER", attempt: None, ..submission };
        let params = submit_params("thread-1", "cutex.target", &event, false);
        assert!(params.get("view").is_none(), "older adapters never get a view");
        assert!(offers_agent_messages_v2(&json!({"input": {"views": true, "deliveryModes": ["after-turn", "passive"]}})));
        assert!(!offers_agent_messages_v2(&json!({"input": {"admissions": ["exclusive_turn"]}})));
        assert_eq!(params["source"], json!({"kind": "service", "id": "cutex-asks"}));
        assert!(params["metadata"].get("attempt").is_none());
        assert!(offers_exclusive(&json!({"input": {"admissions": ["exclusive_turn"]}})));
        assert!(!offers_exclusive(&json!({"input": {"version": 1}})));
    }

    #[test]
    fn submit_and_status_results_map_to_scheduler_outcomes() {
        let receipt = json!({"turnId": "t1", "state": "queued", "duplicate": false});
        assert_eq!(submit_outcome(Ok(receipt)), SubmitOutcome::Started { turn_id: "t1".into() });
        let replay = json!({"turnId": "t1", "state": "completed", "duplicate": true});
        assert!(matches!(submit_outcome(Ok(replay)), SubmitOutcome::Uncertain(_)));
        let busy = rpc(-32600, "busy", Some(json!({"externalInputError": "busy", "reason": "plan_mode"})));
        assert_eq!(submit_outcome(busy), SubmitOutcome::Busy { reason: "plan_mode".into() });
        assert_eq!(
            submit_outcome(rpc(-32602, "unsupported input fields (delivery modes are not implemented)", None)),
            SubmitOutcome::Unsupported(EXCLUSIVE_TURN.into())
        );
        assert!(matches!(submit_outcome(rpc(-32010, "reused", None)), SubmitOutcome::Refused(_)));
        assert!(matches!(
            submit_outcome(Err(AppServerClientError::Timeout { method: "claude/input/submit".into() })),
            SubmitOutcome::Uncertain(_)
        ));
        assert!(matches!(submit_outcome(Err(AppServerClientError::Shutdown)), SubmitOutcome::Unsupported(_)));

        assert_eq!(status_outcome(Ok(json!({"turnId": "t1", "state": "queued"}))), NativeMessageStatus::Pending);
        assert_eq!(
            status_outcome(Ok(json!({"turnId": "t1", "state": "processing"}))),
            NativeMessageStatus::Persisted { turn_id: "t1".into() }
        );
        assert_eq!(
            status_outcome(Ok(json!({"turnId": "t1", "state": "interrupted"}))),
            NativeMessageStatus::Held { reason: "interrupted".into(), turn_id: Some("t1".into()) }
        );
        assert_eq!(status_outcome(rpc(-32004, "input not found", None)), NativeMessageStatus::Absent);
    }

    #[test]
    fn next_step_results_preserve_metadata_and_require_context_confirmation() {
        let submission = Submission { message_id: "cutex-ask-soon-one".into(), source_agent: None,
            event_type: "ANSWER", text: "result".into(), asks: vec!["ask-1".into()], attempt: None,
            view: Some(json!({"schema":"cutex.delegation-results.v1", "data":{"items":[]}})) };
        let params = submit_params("thread", "owner", &submission, true);
        assert_eq!(params["admission"], "next_step");
        assert_eq!(params["metadata"]["asks"], json!(["ask-1"]));
        assert_eq!(params["source"], json!({"kind":"service", "id":"cutex-asks"}));
        assert_eq!(params["metadata"]["deliveryMode"], "soon");
        assert!(!offers_next_step(&json!({"input":{"deliveryModes":["after-turn","passive"]}})));
        assert!(offers_next_step(&json!({"input":{"views":true,"deliveryModes":["passive","soon"],"durableNextStepResults":true}})));
        assert_eq!(legacy_digest(&params), "648b2da0f04b82c425c82a2f3af502b3f096033237c02b51c80fe58403e46b44");
        for state in ["queued", "processing", "injecting"] {
            assert_eq!(next_step_status(&json!({"state":state,"turnId":"reserved","contextTurnId":"busy","contextPersisted":false})), NativeMessageStatus::Pending);
        }
        // Integration review: a held next-step result names the turn that took
        // it, so its event is acknowledged and the requester's lease released
        // (was `turn_id: None`, which never released the lease).
        assert!(matches!(next_step_status(&json!({"state":"failed","turnId":"reserved","contextTurnId":"busy","contextPersisted":false})),
            NativeMessageStatus::Held { turn_id: Some(turn), .. } if turn == "busy"));
        for state in ["failed", "interrupted", "uncertain", "reverted"] {
            assert!(matches!(next_step_status(&json!({"state":state,"turnId":"reserved","contextTurnId":null,"contextPersisted":false})),
                NativeMessageStatus::Held { turn_id: Some(turn), .. } if turn == "reserved"), "{state}");
        }
        assert_eq!(next_step_status(&json!({"state":"completed","turnId":"reserved","contextTurnId":"busy","contextPersisted":true})),
            NativeMessageStatus::Persisted { turn_id: "busy".into() });
        assert!(matches!(next_step_submit_outcome(rpc(-32603, "internal", None), "thread", "message", "digest"), SubmitOutcome::Uncertain(_)));
        assert!(matches!(next_step_submit_outcome(rpc(-32010, "identity conflict", None), "thread", "message", "digest"), SubmitOutcome::Uncertain(_)));
        assert!(matches!(next_step_submit_outcome(Err(AppServerClientError::Timeout { method: "submit".into() }), "thread", "message", "digest"), SubmitOutcome::Uncertain(_)));
    }
}
