//! Request-scoped, owned inputs for production receipt response views.
//!
//! Parent repository wiring supplies the canonical message and authenticated
//! collector Evidence from one coherent bus read, then independently obtains
//! CurrentObserver OUTSIDE AskStore/bus guards. No IO, stores, clocks, retries,
//! delivery writes or deserialization of arbitrary response JSON occur here.

use std::collections::BTreeMap;

use serde_json::Value;

use super::integration::{bus_tell_read, ReadEvidence, ReceiptReadView};
use super::*;
use crate::app_server::external_input::{Delivery, ProcessingState, SourceKind};
use crate::management::v2::agent_bus_settlement_model::Evidence;
use crate::management::v2::agent_bus_state::AgentBusMessageSnapshot;

/// Provenance comes from a fresh authenticated runtime/session observation,
/// independently of the retained sidecar. Never construct by copying Evidence.
/// No Deserialize implementation: this is not an HTTP request proof format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentObserver {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub runtime_generation: u64,
}

/// Original facts are retained unchanged. `terminal` must originate in the
/// authenticated exact-input/history collector, not SettlementView/can_close,
/// an ask's execution label, roster busy/idle, or caller-provided JSON.
#[derive(Clone, Debug)]
pub struct ReceiptReadInput {
    pub snapshot: AgentBusMessageSnapshot,
    pub terminal: Option<Evidence>,
    pub observer: Option<CurrentObserver>,
    pub status_unavailable: bool,
}

pub type ReceiptReadContext = BTreeMap<String, ReceiptReadInput>;

pub fn unknown_bus_read() -> Value {
    serde_json::to_value(ReceiptReadView {
        projection: ReceiptProjection {
            kind: ReceiptKind::Tell,
            state: ProjectionState::Unknown,
            confirmed_level: RevisionLevel::Sent,
            reason: ProjectionReason::AwaitingEvidence,
            terminal_evidence: None,
        },
        turn_id: None,
    })
    .expect("receipt view serializes")
}

/// Pinned Claude adapter bookkeeping identity, matching the authenticated
/// collector's UUIDv5 NAMESPACE_URL algorithm exactly. This is a correlation
/// check, not authentication and not a delegation attempt-number conversion.
pub fn claude_attempt_id(thread: &str, turn: &str) -> String {
    let mut sha = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    sha.update(uuid::Uuid::NAMESPACE_URL.as_bytes());
    sha.update(format!("cute-claude:external:{thread}:{turn}").as_bytes());
    let digest = sha.finish();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.as_ref()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn observer_matches<'a>(input: &'a ReceiptReadInput) -> Option<&'a CurrentObserver> {
    let envelope = input.snapshot.external_input.as_ref()?;
    input.observer.as_ref().filter(|observer| {
        observer.cutex_session_id == envelope.owner_id && observer.thread_id == envelope.thread_id
    })
}

/// Revalidate the collector's exact identity against the ORIGINAL frozen row.
/// The observing generation is checked separately, never substituted into the
/// envelope or original turn occurrence. Close-policy TTL does not erase proof.
fn terminal_bound(input: &ReceiptReadInput) -> Option<TerminalEvidence> {
    let snapshot = &input.snapshot;
    let envelope = snapshot.external_input.as_ref()?;
    let evidence = input.terminal.as_ref()?;
    let observer = observer_matches(input)?;
    envelope.validate().ok()?;
    evidence.validate_shape().ok()?;
    let local = snapshot.local_inbox_receipt.as_ref()?;
    let status = snapshot.external_input_last_observed.as_ref()?;
    if envelope.message.source.kind != SourceKind::Agent
        || envelope.message.event_type != "agent_message"
        || envelope.message.delivery == Delivery::Passive
        || snapshot.message_id != envelope.message.id
        || snapshot.to_cutex_session_id != envelope.owner_id
        || evidence.message_id != envelope.message.id
        || evidence.recipient_id != envelope.owner_id
        || evidence.thread_id != envelope.thread_id
        || evidence.runtime_generation != envelope.runtime_generation
        || evidence.semantic_sha256 != envelope.semantic_sha256
        || evidence.attempt_id != claude_attempt_id(&evidence.thread_id, &evidence.actual_turn_id)
        || evidence.observer_generation() != observer.runtime_generation
        || local["messageId"] != evidence.message_id
        || local["threadId"] != evidence.thread_id
        || local["semanticSha256"] != evidence.local_semantic_sha256
        || local["durable"] != true
        || local["receiptScope"] != "local-inbox"
        || status.message_id != evidence.message_id
        || status.semantic_sha256 != evidence.semantic_sha256
        || status.processing.attempt_id.as_deref() != Some(evidence.attempt_id.as_str())
        || match evidence.processing_state.as_str() {
            "held" => status.processing.state != ProcessingState::Held,
            "output_observed" => status.processing.state != ProcessingState::OutputObserved,
            _ => true,
        }
    {
        return None;
    }
    // snapshot.semantic_sha256 hashes the bus message. The frozen native
    // envelope has a different digest domain, verified above by validate().
    let occurrence = TurnOccurrence {
        cutex_session_id: evidence.recipient_id.clone(),
        thread_id: evidence.thread_id.clone(),
        runtime_generation: evidence.runtime_generation,
        turn_id: evidence.actual_turn_id.clone(),
        attempt: AttemptIdentity::Native {
            attempt_id: evidence.attempt_id.clone(),
        },
    };
    let key = envelope.key();
    if !valid_key(&key) || !occurrence.valid() {
        return None;
    }
    Some(TerminalEvidence {
        key,
        occurrence,
        outcome: match evidence.terminal_status.as_str() {
            "completed" => TerminalOutcome::Completed,
            "failed" => TerminalOutcome::Failed,
            "interrupted" => TerminalOutcome::Interrupted,
            _ => return None,
        },
        source: TerminalSource::ExactTurnRead,
        observed_at: evidence.observed_at,
    })
}

/// Additive serialized view. Positive original A4/ack always wins, including
/// when observation is unavailable or a newer generation is now connected.
pub fn project_bus_read(input: &ReceiptReadInput, confirmed: RevisionLevel) -> Value {
    let observer = observer_matches(input);
    let base = bus_tell_read(
        &input.snapshot,
        confirmed,
        ReadEvidence {
            runtime_generation: observer.map(|observer| observer.runtime_generation),
            trusted: None,
            status_unavailable: input.status_unavailable,
        },
    );
    if base.projection.confirmed_level >= RevisionLevel::InContext {
        return serde_json::to_value(base).expect("receipt view serializes");
    }
    let Some(terminal) = terminal_bound(input) else {
        return serde_json::to_value(base).expect("receipt view serializes");
    };
    let evidence = input.terminal.as_ref().expect("bound terminal");
    let observer = observer.expect("bound observer");
    let envelope = input
        .snapshot
        .external_input
        .as_ref()
        .expect("bound envelope");
    let binding = ExternalInputBinding {
        version: 1,
        owner_id: envelope.owner_id.clone(),
        thread_id: envelope.thread_id.clone(),
        runtime_generation: envelope.runtime_generation,
        canonical_byte_limit: Default::default(),
    };
    let subject = ReceiptSubject {
        kind: ReceiptKind::Tell,
        key: &terminal.key,
        binding: &binding,
        occurrence: Some(&terminal.occurrence),
        confirmed_level: confirmed,
        a4: input.snapshot.external_input_receipt.as_ref(),
    };
    let observations = Observations {
        runtime_generation: Some(observer.runtime_generation),
        // A failed new read cannot erase an explicit retained unknown/conflict.
        status: if input.status_unavailable
            && !input
                .snapshot
                .external_input_last_observed
                .as_ref()
                .is_some_and(|status| status.delivery_state != DeliveryState::Pending)
        {
            StatusRead::Unavailable
        } else {
            input.snapshot.external_input_last_observed.as_ref().map_or(
                StatusRead::NotObserved,
                |status| StatusRead::Available {
                    runtime_generation: envelope.runtime_generation,
                    status,
                },
            )
        },
        terminal: Some(&terminal),
    };
    let projection = project_history(
        &subject,
        &observations,
        HistoryObserver {
            session: &observer.cutex_session_id,
            thread: &observer.thread_id,
            current_generation: observer.runtime_generation,
            evidence_observing_generation: evidence.observer_generation(),
        },
    );
    let mut value = serde_json::to_value(ReceiptReadView {
        projection,
        turn_id: None,
    })
    .expect("receipt view serializes");
    value["submissionGeneration"] = serde_json::json!(envelope.runtime_generation);
    value["observingGeneration"] = serde_json::json!(evidence.observer_generation());
    value
}

pub fn revision_projection(revision: &RevisionRecord, context: &ReceiptReadContext) -> Value {
    if let Some(input) = revision
        .message_id
        .as_ref()
        .and_then(|id| context.get(id))
        .filter(|input| revision.message_id.as_deref() == Some(input.snapshot.message_id.as_str()))
    {
        project_bus_read(input, revision.level)
    } else {
        super::integration::revision_summary(revision, None, ReadEvidence::default())
            ["receiptProjection"]
            .clone()
    }
}

#[cfg(test)]
pub(crate) mod tests;
