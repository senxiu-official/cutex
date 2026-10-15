//! Explicit root-Human recovery for one frozen native message. Never a model tool.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::external_input::{
    DeliveryState, Envelope, ExternalInputClient, MessageKey, ProcessingState, Retry, RetryResponse,
    Status,
};
use crate::launch::stock::ExternalInputBinding;

pub const REPEAT_WARNING: &str = "Retry may repeat model requests and tool effects. This permission applies only to this message and expected attempt.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReview {
    pub envelope: Envelope,
    pub binding: ExternalInputBinding,
    pub status: Status,
    pub durable_sha256: String,
    pub authority_sha256: String,
    pub warning: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReceipt {
    pub action_id: String,
    pub retry_id: String,
    pub review: RecoveryReview,
    /// None is a prepared/uncertain permission, not success or permission to
    /// choose a different retry ID. Reconcile this exact action only.
    pub result: Option<RetryResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryRequest {
    Status {
        action_id: String,
    },
    Review {
        cutex_session_id: String,
        message_id: String,
    },
    Confirm {
        action_id: String,
        retry_id: String,
        review: RecoveryReview,
        confirm_repeat: bool,
    },
}

fn digest(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn durable_digest(record: &crate::session::model::CutexSessionRecord) -> anyhow::Result<String> {
    // Registration heartbeats update these observations without changing the
    // durable specification revision or current occurrence. Do not turn an
    // ordinary heartbeat into stale Human confirmation.
    let mut specification = record.clone();
    specification.last_seen_at = None;
    specification.updated_at.clear();
    digest(&specification)
}
fn action_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
        "bounded nonempty action/retry ID required"
    );
    Ok(())
}

/// Invoked only after the existing root Management token check. Taking a typed
/// Human principal keeps this separate from Agent/MCP semantic operations.
pub fn handle(
    _human: &crate::management::control_plane::HumanManagementPrincipal,
    request: &RecoveryRequest,
) -> anyhow::Result<serde_json::Value> {
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let (owner, message) = match request {
        RecoveryRequest::Status { action_id: id } => {
            action_id(id)?;
            return Ok(serde_json::to_value(repository.recovery_action(id)?)?);
        }
        RecoveryRequest::Review {
            cutex_session_id,
            message_id,
        } => (cutex_session_id.as_str(), message_id.as_str()),
        RecoveryRequest::Confirm {
            action_id: id,
            retry_id,
            review,
            confirm_repeat,
        } => {
            action_id(id)?;
            action_id(retry_id)?;
            ensure!(
                *confirm_repeat && review.warning == REPEAT_WARNING,
                "explicit repeat warning confirmation required"
            );
            if let Some(old) = repository.recovery_action(id)? {
                ensure!(
                    old.review == *review && old.retry_id == *retry_id,
                    "recovery action semantic conflict"
                );
                if old.result.is_some() {
                    return Ok(serde_json::to_value(old)?);
                }
            }
            (
                review.envelope.owner_id.as_str(),
                review.envelope.message.id.as_str(),
            )
        }
    };
    let management = crate::agent_management::AgentManagementStore::open_default()?;
    let _mutation = management
        .try_lock_delivery_mutations()?
        .context("lifecycle transition in progress; review current owner later")?;
    let roster = management.snapshot()?;
    let id = crate::role_revision::CutexSessionId::new(owner.to_string())
        .map_err(|_| anyhow::anyhow!("invalid durable owner"))?;
    ensure!(
        roster
            .agents
            .get(&id)
            .is_none_or(|a| a.retired_at.is_none()),
        "permanently retired recipient"
    );
    let path = crate::session::store::cutex_sessions_path()?;
    crate::session::store::with_locked_session_rows(&path, |rows| {
        let record = rows.session(owner).context("durable owner absent")?;
        let generation = record.runtime_generation;
        let client = ExternalInputClient::connect(&path, owner, generation)?;
        let (frozen, observed) = match repository.snapshot_by_message_id(message)? {
            Some(snapshot) => {
                let canonical = repository.canonical_message(message)?;
                super::bus_bridge::validate_external_recovery_target(&canonical, owner, &roster)?;
                let frozen = snapshot.external_input.context("native envelope absent; no implicit projection during recovery")?;
                frozen.validate()?;
                ensure!(frozen.owner_id == owner && frozen.thread_id == client.binding().thread_id, "frozen recovery target conflict");
                let observed = client.status(&[frozen.key()])?.statuses.remove(0);
                (frozen, observed)
            }
            // A delegation-results event (asks) delivered to this owner: its
            // envelope is rebuilt from the asks event record exactly as the
            // scheduler submitted it.
            None => {
                let delivery = ask_event_deliveries(owner, Some(message))?
                    .into_iter()
                    .next()
                    .context("message absent")?;
                let mut resolved = resolve_ask_events(&client, std::slice::from_ref(&delivery))?;
                let (frozen, observed) = resolved.pop().context("message absent")?;
                frozen.validate()?;
                ensure!(frozen.owner_id == owner && frozen.thread_id == client.binding().thread_id, "frozen recovery target conflict");
                (frozen, observed)
            }
        };
        // r219: the authority digest covers the Agent Management roster
        // only (r218 also hashed the removed seat snapshot), so a review
        // prepared by r218 no longer matches: it is stale and must be
        // prepared again before a retry is confirmed.
        let fresh = RecoveryReview {
            envelope: frozen, binding: client.binding().clone(), status: observed,
            durable_sha256: durable_digest(record)?, authority_sha256: digest(&roster)?, warning: REPEAT_WARNING.into(),
        };
        match request {
            RecoveryRequest::Status {..} => unreachable!("read-only receipt lookup returned above"),
            RecoveryRequest::Review {..} => Ok(serde_json::to_value(fresh)?),
            RecoveryRequest::Confirm {action_id, retry_id, review, ..} => {
                let previous = repository.recovery_action(action_id)?;
                if previous.is_none() {
                    ensure!(fresh == *review, "stale recovery review; no retry issued");
                    ensure!(fresh.status.processing.state == ProcessingState::Held, "only an explicitly held message may be retried");
                } else {
                    // Reconcile an uncertain exact permission, never grant a
                    // second permission based on changed observations.
                    ensure!(fresh.envelope == review.envelope && fresh.binding == review.binding && fresh.durable_sha256 == review.durable_sha256 && fresh.authority_sha256 == review.authority_sha256, "uncertain retry occurrence/authority changed; explicit recovery review required");
                }
                let mut receipt = RecoveryReceipt {action_id: action_id.clone(), retry_id: retry_id.clone(), review: review.clone(), result: None};
                repository.save_recovery_action(&receipt)?;
                let retry = Retry {message_id: review.envelope.message.id.clone(), semantic_sha256: review.envelope.semantic_sha256.clone(), expected_attempt_id: review.status.processing.attempt_id.clone(), retry_id: retry_id.clone()};
                receipt.result = Some(client.retry(&retry)?);
                client.fence()?;
                repository.save_recovery_action(&receipt)?;
                Ok(serde_json::to_value(receipt)?)
            }
        }
    })
}

/// Keep undelivered work and explicitly held messages in the default view.
fn retain_relevant_external_input_snapshot(snapshot: &Value) -> bool {
    // An ask event the runtime does not know (released, never written) is
    // not actionable.
    if snapshot.get("kind").and_then(Value::as_str) == Some("askEvent")
        && snapshot.get("state").and_then(Value::as_str) == Some("unknown")
    {
        return false;
    }
    snapshot.get("state").and_then(Value::as_str) != Some("delivered")
        || snapshot
            .pointer("/externalInputLastObserved/processing/state")
            .and_then(Value::as_str)
            == Some("held")
}

/// A delegation-results event (asks ANSWER / QUESTION) delivered to its
/// requester as native ExternalInput. The asks scheduler, not the Agent Bus,
/// owns these, so they are absent from the bus message repository.
#[derive(Clone, Debug)]
struct AskEventDelivery {
    event_id: String,
    asks: Vec<String>,
    turn_id: Option<String>,
    created_at: String,
    submission: crate::asks::scheduler::Submission,
}

/// The owner's ask events with a stored event text, optionally one native
/// message id. Read-only (asks store).
fn ask_event_deliveries(owner: &str, message_id: Option<&str>) -> anyhow::Result<Vec<AskEventDelivery>> {
    let store = crate::asks::store::AskStore::open_default()?;
    let Some(state) = store.peek()? else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for event in state.events.values().filter(|event| {
        event.cutex_session_id == owner
            && message_id.is_none_or(|id| id == event.native_message_id)
    }) {
        let Some(text) = event
            .event_artifact
            .as_deref()
            .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
            .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
        else {
            continue;
        };
        found.push(ask_event_delivery(&state, event, text));
    }
    Ok(found)
}

/// Rebuild the submission the scheduler made for `event` (asks/returns.rs
/// reconcile: same text, event type rule and view).
fn ask_event_delivery(
    state: &crate::asks::store::AskState,
    event: &crate::asks::model::EventOutboxRecord,
    text: String,
) -> AskEventDelivery {
    let mut asks = Vec::new();
    for delivery in event.delivery_ids.iter().filter_map(|id| state.deliveries.get(id)) {
        if !asks.contains(&delivery.ask_id) {
            asks.push(delivery.ask_id.clone());
        }
    }
    let event_type = if text.contains("## Answer") {
        crate::asks::render::ANSWER_EVENT
    } else {
        crate::asks::render::QUESTION_EVENT
    };
    AskEventDelivery {
        event_id: event.event_id.clone(),
        asks: asks.clone(),
        turn_id: event.turn_id.clone(),
        created_at: event.created_at.clone(),
        submission: crate::asks::scheduler::Submission {
            message_id: event.native_message_id.clone(),
            source_agent: None,
            event_type,
            text,
            asks,
            attempt: None,
            view: event.view.clone(),
        },
    }
}

/// Envelopes an ask event may have been submitted as: first the one the
/// scheduler builds for this runtime now, then (with a view) the other
/// version, in case the runtime's v2 support changed since delivery.
fn ask_event_envelopes(
    submission: &crate::asks::scheduler::Submission,
    binding: &crate::launch::stock::ExternalInputBinding,
    structured_view: bool,
) -> anyhow::Result<Vec<Envelope>> {
    let primary = submission.envelope(binding, structured_view)?;
    let mut envelopes = vec![primary];
    if submission.view.is_some() {
        if let Ok(other) = submission.envelope(binding, !structured_view) {
            if other.semantic_sha256 != envelopes[0].semantic_sha256 {
                envelopes.push(other);
            }
        }
    }
    Ok(envelopes)
}

/// Pick, per ask event, the envelope the runtime knows (else the primary one)
/// with its native status. Status calls carry at most 100 keys.
fn resolve_ask_events(
    client: &ExternalInputClient,
    deliveries: &[AskEventDelivery],
) -> anyhow::Result<Vec<(Envelope, Status)>> {
    let structured = client.supports_structured_view();
    let candidates = deliveries
        .iter()
        .map(|delivery| ask_event_envelopes(&delivery.submission, client.binding(), structured))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let keys: Vec<MessageKey> = candidates.iter().flatten().map(Envelope::key).collect();
    let mut statuses = std::collections::HashMap::new();
    for chunk in keys.chunks(100) {
        for status in client.status(chunk)?.statuses {
            statuses.insert((status.message_id.clone(), status.semantic_sha256.clone()), status);
        }
    }
    Ok(pick_known_envelopes(candidates, &statuses))
}

fn pick_known_envelopes(
    candidates: Vec<Vec<Envelope>>,
    statuses: &std::collections::HashMap<(String, String), Status>,
) -> Vec<(Envelope, Status)> {
    let lookup = |envelope: &Envelope| {
        statuses
            .get(&(envelope.message.id.clone(), envelope.semantic_sha256.clone()))
            .cloned()
    };
    candidates
        .into_iter()
        .filter_map(|envelopes| {
            let known = envelopes.iter().position(|envelope| {
                lookup(envelope).is_some_and(|status| status.delivery_state != DeliveryState::Unknown)
            });
            let envelope = envelopes.into_iter().nth(known.unwrap_or(0))?;
            let status = lookup(&envelope)?;
            Some((envelope, status))
        })
        .collect()
}

/// List view of one ask event (no event text).
fn ask_event_snapshot(delivery: &AskEventDelivery, status: &Status) -> Value {
    let state = if status.processing.state == ProcessingState::Held || status.receipt.is_some() {
        "delivered"
    } else {
        match status.delivery_state {
            DeliveryState::Pending => "pending",
            DeliveryState::Unknown => "unknown",
            _ => "delivered",
        }
    };
    serde_json::json!({
        "kind": "askEvent",
        "messageId": delivery.submission.message_id,
        "eventId": delivery.event_id,
        "asks": delivery.asks,
        "eventType": delivery.submission.event_type,
        "turnId": delivery.turn_id,
        "createdAt": delivery.created_at,
        "state": state,
        "sender": {"id": crate::asks::scheduler::CUTEX_ASKS_SOURCE, "name": null, "runtime": null},
        "senderKind": "service",
        "externalInputLastObserved": status,
    })
}

/// The owner's ask events as list rows, checked against the live runtime.
/// No ask events: the runtime is not contacted.
fn ask_event_snapshots(cutex_session_id: &str) -> anyhow::Result<Vec<Value>> {
    let deliveries = ask_event_deliveries(cutex_session_id, None)?;
    if deliveries.is_empty() {
        return Ok(Vec::new());
    }
    let path = crate::session::store::cutex_sessions_path()?;
    let record = crate::session::store::with_cutex_session_store_at(&path, |store| {
        store.sessions.get(cutex_session_id).cloned()
    })?
    .context("durable owner absent")?;
    if record.claude_session.is_some() {
        // Claude runtimes keep ask events in their own inbox, not ExternalInput.
        return Ok(Vec::new());
    }
    let client = ExternalInputClient::connect(&path, cutex_session_id, record.runtime_generation)?;
    let resolved = resolve_ask_events(&client, &deliveries)?;
    Ok(deliveries
        .iter()
        .zip(resolved.iter())
        .map(|(delivery, (_, status))| ask_event_snapshot(delivery, status))
        .collect())
}

/// Read-only discovery for the Human CLI. Agent Bus messages come from the
/// message repository; delegation-results events (asks) delivered to the
/// owner are checked against the live runtime (`askEventsUnavailable` when it
/// cannot be reached).
pub fn list(cutex_session_id: &str, all: bool) -> anyhow::Result<serde_json::Value> {
    let owner = crate::role_revision::CutexSessionId::new(cutex_session_id)
        .map_err(|_| anyhow::anyhow!("invalid durable owner"))?;
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let snapshots = repository
        .snapshot(owner.as_str())?
        .into_iter()
        .filter(|snapshot| snapshot.get("externalInput").is_some())
        .collect::<Vec<_>>();
    let (ask_events, ask_events_unavailable) = match ask_event_snapshots(owner.as_str()) {
        Ok(rows) => (rows, None),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    let mut snapshots = snapshots;
    snapshots.extend(ask_events);
    let total = snapshots.len();
    let candidates = if all {
        snapshots
    } else {
        snapshots
            .into_iter()
            .filter(|snapshot| retain_relevant_external_input_snapshot(snapshot))
            .collect::<Vec<_>>()
    };
    let mut listing = serde_json::json!({
        "cutexSessionId": owner.as_str(),
        "all": all,
        "total": total,
        "shown": candidates.len(),
        "candidates": candidates,
    });
    if let Some(error) = ask_events_unavailable {
        listing["askEventsUnavailable"] = Value::String(error);
    }
    Ok(listing)
}

/// Native refusals of a `thread/fork` whose source thread still has an
/// unfinished ExternalInput obligation (pending, claimed or held message).
/// Natives up to r52.1 refuse such forks; later natives fork anyway.
pub fn is_fork_blocked_by_external_input(native_message: &str) -> bool {
    native_message.contains("unfinished external input migration")
        || native_message.contains("cannot migrate unfinished or uncertain ExternalInput history")
}

/// Short, body-free label of one ExternalInput snapshot for a fork refusal.
fn fork_blocking_item(snapshot: &Value) -> Value {
    let processing = snapshot.pointer("/externalInputLastObserved/processing");
    let state = snapshot.get("state").and_then(Value::as_str).unwrap_or("unknown");
    let processing_state = processing.and_then(|p| p.get("state")).and_then(Value::as_str);
    let mut item = serde_json::json!({
        "messageId": snapshot.get("messageId").cloned().unwrap_or(Value::Null),
        "state": state,
        "processing": processing_state,
        "held": processing_state == Some("held"),
    });
    if let Some(reason) = processing.and_then(|p| p.get("reason")).filter(|r| !r.is_null()) {
        item["holdReason"] = reason.clone();
    }
    for field in ["kind", "eventId", "asks", "sender", "senderKind", "deliveryKind", "queuedAt"] {
        if let Some(value) = snapshot.get(field).filter(|v| !v.is_null()) {
            item[field] = value.clone();
        }
    }
    item
}

/// Build the details of a fork refused for unfinished ExternalInput: the
/// asks in the runtime and the owner's undelivered / held messages (ids,
/// states, sender; never message bodies). `blockedBy` is `held` when a message
/// is held (it needs an owner retry), else `busy` (wait for the agent).
pub fn fork_blocked_details(cutex_session_id: &str) -> Value {
    let blocking_asks = match crate::asks::store::AskStore::open_default()
        .and_then(|store| store.peek())
    {
        Ok(Some(state)) => crate::asks::lifecycle::blocking_asks(&state, cutex_session_id),
        _ => Vec::new(),
    };
    let candidates = list(cutex_session_id, false)
        .ok()
        .and_then(|listing| listing.get("candidates").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    fork_blocked_details_from(blocking_asks, &candidates)
}

fn fork_blocked_details_from(blocking_asks: Vec<Value>, candidates: &[Value]) -> Value {
    let items: Vec<Value> = candidates.iter().map(fork_blocking_item).collect();
    let held = items.iter().any(|item| item["held"] == Value::Bool(true));
    serde_json::json!({
        "blockedBy": if held { "held" } else { "busy" },
        "blockingAsks": blocking_asks,
        "externalInput": items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fork_blocked_details_name_messages_without_bodies() {
        assert!(is_fork_blocked_by_external_input(
            "invalid external input: unfinished external input migration"
        ));
        assert!(is_fork_blocked_by_external_input(
            "cannot migrate unfinished or uncertain ExternalInput history"
        ));
        assert!(!is_fork_blocked_by_external_input("model is unavailable"));
        let held = serde_json::json!({
            "messageId": "m-held",
            "state": "delivered",
            "content": "secret body",
            "body": "secret body",
            "sender": {"id": "cutex.a", "name": "Alice", "runtime": null},
            "senderKind": "agent",
            "externalInputLastObserved": {"processing": {"state": "held", "attemptId": "x", "reason": "request_uncertain"}}
        });
        let pending = serde_json::json!({"messageId": "m-pending", "state": "pending", "content": "other body"});
        let details = fork_blocked_details_from(
            vec![serde_json::json!({"askId": "ask-1", "title": "t"})],
            &[held, pending.clone()],
        );
        assert_eq!(details["blockedBy"], "held");
        assert_eq!(details["blockingAsks"][0]["askId"], "ask-1");
        assert_eq!(details["externalInput"][0]["messageId"], "m-held");
        assert_eq!(details["externalInput"][0]["holdReason"], "request_uncertain");
        assert_eq!(details["externalInput"][0]["sender"]["name"], "Alice");
        assert_eq!(details["externalInput"][1]["state"], "pending");
        assert_eq!(details["externalInput"][1]["held"], false);
        assert!(!details.to_string().contains("body"));
        let busy = fork_blocked_details_from(Vec::new(), &[pending]);
        assert_eq!(busy["blockedBy"], "busy");
    }

    fn event_record(view: bool) -> crate::asks::model::EventOutboxRecord {
        serde_json::from_value(serde_json::json!({
            "eventId": "aev-1", "cutexSessionId": "owner", "deliveryIds": ["d-1", "d-2"],
            "nativeMessageId": "cutex-ask-event-1", "eventArtifact": "art-1", "turnId": "turn-1",
            "acknowledged": true, "createdAt": "2026-10-13T13:00:00Z",
            "view": if view { serde_json::json!({
                "schema": "cutex.delegation-ask.v1",
                "data": {"askId": "ask-1", "attempt": 1, "gated": false, "title": "sum",
                         "requester": {"kind": "agent", "cutexSessionId": "cutex.lead", "name": "Lead"}},
            }) } else { Value::Null },
        }))
        .unwrap()
    }
    fn binding() -> ExternalInputBinding {
        serde_json::from_value(serde_json::json!({
            "version": 1, "ownerId": "owner", "threadId": "thread", "runtimeGeneration": 3
        }))
        .unwrap()
    }
    fn status_of(envelope: &Envelope, value: Value) -> Status {
        let mut status = serde_json::json!({
            "messageId": envelope.message.id, "semanticSha256": envelope.semantic_sha256,
            "deliveryState": "context_persisted", "receipt": null,
            "processing": {"state": "held", "attemptId": null, "reason": "interrupted"},
        });
        for (key, field) in value.as_object().unwrap() {
            status[key] = field.clone();
        }
        serde_json::from_value(status).unwrap()
    }

    #[test]
    fn ask_event_envelope_matches_the_scheduler_submission() {
        let mut state = crate::asks::store::AskState::default();
        for (id, ask) in [("d-1", "ask-ac46"), ("d-2", "ask-2d79")] {
            state.deliveries.insert(id.into(), serde_json::from_value(serde_json::json!({
                "deliveryId": id, "kind": "answer", "askId": ask, "refId": "a", "requesterCutexSessionId": "owner",
                "state": "acknowledged", "createdAt": "t", "updatedAt": "t",
            })).unwrap());
        }
        let delivery = ask_event_delivery(&state, &event_record(true), "[delegation results]\n## Answer x".into());
        assert_eq!(delivery.asks, vec!["ask-ac46".to_string(), "ask-2d79".to_string()]);
        assert_eq!(delivery.submission.event_type, crate::asks::render::ANSWER_EVENT);
        let scheduler = crate::asks::scheduler::Submission {
            message_id: "cutex-ask-event-1".into(), source_agent: None, event_type: "ANSWER",
            text: "[delegation results]\n## Answer x".into(), asks: vec![], attempt: None,
            view: event_record(true).view,
        };
        let envelopes = ask_event_envelopes(&delivery.submission, &binding(), true).unwrap();
        assert_eq!(envelopes[0], scheduler.envelope(&binding(), true).unwrap());
        assert_eq!(envelopes[0].message.source.id, crate::asks::scheduler::CUTEX_ASKS_SOURCE);
        // With a view the v1 form is a second candidate (runtime v2 support changed).
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[1].version, 1);
        let plain = ask_event_delivery(&state, &event_record(false), "[delegation results]\nquestion".into());
        assert_eq!(plain.submission.event_type, crate::asks::render::QUESTION_EVENT);
        assert_eq!(ask_event_envelopes(&plain.submission, &binding(), true).unwrap().len(), 1);
    }

    #[test]
    fn ask_event_resolution_prefers_the_envelope_the_runtime_knows() {
        let delivery = ask_event_delivery(&Default::default(), &event_record(true), "text".into());
        let envelopes = ask_event_envelopes(&delivery.submission, &binding(), true).unwrap();
        let mut statuses = std::collections::HashMap::new();
        for (index, envelope) in envelopes.iter().enumerate() {
            let state = if index == 1 { "context_persisted" } else { "unknown" };
            statuses.insert(
                (envelope.message.id.clone(), envelope.semantic_sha256.clone()),
                status_of(envelope, serde_json::json!({"deliveryState": state})),
            );
        }
        let picked = pick_known_envelopes(vec![envelopes.clone()], &statuses);
        assert_eq!(picked[0].0, envelopes[1]);
        // None known: the primary one, with its (unknown) status.
        statuses.remove(&(envelopes[1].message.id.clone(), envelopes[1].semantic_sha256.clone()));
        let picked = pick_known_envelopes(vec![envelopes.clone()], &statuses);
        assert_eq!(picked[0].0, envelopes[0]);
    }

    #[test]
    fn held_ask_events_are_listed_without_their_text_and_block_forks() {
        let delivery = ask_event_delivery(&Default::default(), &event_record(false), "secret result text".into());
        let envelope = &ask_event_envelopes(&delivery.submission, &binding(), false).unwrap()[0];
        let held = ask_event_snapshot(&delivery, &status_of(envelope, serde_json::json!({})));
        assert_eq!(held["kind"], "askEvent");
        assert_eq!(held["messageId"], "cutex-ask-event-1");
        assert_eq!(held["state"], "delivered");
        assert_eq!(held["externalInputLastObserved"]["processing"]["state"], "held");
        assert!(!held.to_string().contains("secret result text"));
        assert!(retain_relevant_external_input_snapshot(&held));
        let done = ask_event_snapshot(&delivery, &status_of(envelope, serde_json::json!({
            "processing": {"state": "output_observed", "attemptId": null, "reason": null}})));
        assert!(!retain_relevant_external_input_snapshot(&done));
        let unknown = ask_event_snapshot(&delivery, &status_of(envelope, serde_json::json!({
            "deliveryState": "unknown", "processing": {"state": "none", "attemptId": null, "reason": null}})));
        assert_eq!(unknown["state"], "unknown");
        assert!(!retain_relevant_external_input_snapshot(&unknown));
        let details = fork_blocked_details_from(Vec::new(), &[held]);
        assert_eq!(details["blockedBy"], "held");
        assert_eq!(details["externalInput"][0]["kind"], "askEvent");
        assert_eq!(details["externalInput"][0]["eventId"], "aev-1");
        assert_eq!(details["externalInput"][0]["holdReason"], "interrupted");
    }
    #[test]
    fn recovery_digest_ignores_only_heartbeat_observations() {
        let mut record = crate::session::model::CutexSessionRecord::new(
            "cutex.11111111-1111-4111-8111-111111111111".into(),
            None,
            "private".into(),
            "/private".into(),
            None,
        )
        .unwrap();
        let original = durable_digest(&record).unwrap();
        record.last_seen_at = Some("later".into());
        record.updated_at = "later".into();
        assert_eq!(original, durable_digest(&record).unwrap());
        record.runtime_generation += 1;
        assert_ne!(original, durable_digest(&record).unwrap());
        record.runtime_generation -= 1;
        record.profile = Some("different-config".into());
        assert_ne!(original, durable_digest(&record).unwrap());
    }

    #[test]
    fn external_input_list_keeps_pending_or_held_messages_only_by_default() {
        let delivered = serde_json::json!({
            "state": "delivered",
            "externalInputLastObserved": {"processing": {"state": "output_observed"}}
        });
        let pending = serde_json::json!({"state": "pending"});
        let held = serde_json::json!({
            "state": "delivered",
            "externalInputLastObserved": {"processing": {"state": "held"}}
        });

        assert!(!retain_relevant_external_input_snapshot(&delivered));
        assert!(retain_relevant_external_input_snapshot(&pending));
        assert!(retain_relevant_external_input_snapshot(&held));
    }

    #[test]
    fn recovery_requests_reject_caller_authority_and_control_fields() {
        assert!(serde_json::from_value::<RecoveryRequest>(serde_json::json!({"operation":"review","cutex_session_id":"owner","message_id":"message","caller":"root"})).is_err());
        assert!(serde_json::from_value::<RecoveryRequest>(
            serde_json::json!({"operation":"retry_all"})
        )
        .is_err());
        assert!(serde_json::from_value::<RecoveryRequest>(
            serde_json::json!({"operation":"status","action_id":"a"})
        )
        .is_ok());
    }
}
