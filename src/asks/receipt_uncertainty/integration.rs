//! Typed read adapters for the existing bus/revision and native-steer paths.
//! These functions never open stores, query/start a runtime or change receipts.

use serde::{Deserialize, Serialize};

use super::*;
use crate::app_server::commands::InterAgentContextPersistedReceipt;
use crate::app_server::external_input::Envelope;
use crate::management::v2::agent_bus_state::AgentBusMessageSnapshot;

/// Retained per-MESSAGE occurrence evidence. Populate only from an actual
/// message-specific binding; never derive it from an ask's current attempt.
/// This is a proposed evidence payload, not a new business-store namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedReceiptEvidence {
    pub key: MessageKey,
    pub occurrence: TurnOccurrence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalEvidence>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReadEvidence<'a> {
    /// Latest independently verified runtime generation. None is unknown.
    pub runtime_generation: Option<u64>,
    pub trusted: Option<&'a TrustedReceiptEvidence>,
    /// A transient current read failure must not clear retained original data.
    pub status_unavailable: bool,
}

/// The original snapshot/receipt remains owned by the caller. This view adds
/// certainty and the actual context turn, without altering any source field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptReadView<'a> {
    #[serde(flatten)]
    pub projection: ReceiptProjection<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

fn without_native<'a>(
    kind: ReceiptKind,
    level: RevisionLevel,
    reason: ProjectionReason,
) -> ReceiptReadView<'a> {
    ReceiptReadView {
        projection: ReceiptProjection {
            kind,
            confirmed_level: level,
            state: match level {
                RevisionLevel::Acknowledged => ProjectionState::Acknowledged,
                RevisionLevel::InContext => ProjectionState::InContext,
                _ => ProjectionState::Unknown,
            },
            reason: if level >= RevisionLevel::InContext {
                ProjectionReason::ConfirmedProgress
            } else {
                reason
            },
            terminal_evidence: None,
        },
        turn_id: None,
    }
}

/// Native steer adapter. `status` is a previously validated response for this
/// envelope, not a generic turn status. Missing occurrence proof stays unknown.
/// Existing native callers already have these typed values; no JSON heuristics.
pub fn native_steer_read<'a>(
    envelope: &Envelope,
    a4: Option<&Receipt>,
    status: Option<&'a Status>,
    confirmed_level: RevisionLevel,
    evidence: ReadEvidence<'a>,
) -> ReceiptReadView<'a> {
    native_read(
        ReceiptKind::Steer,
        envelope,
        a4,
        status,
        confirmed_level,
        evidence,
    )
}

fn native_read<'a>(
    kind: ReceiptKind,
    envelope: &Envelope,
    a4: Option<&Receipt>,
    status: Option<&'a Status>,
    confirmed_level: RevisionLevel,
    evidence: ReadEvidence<'a>,
) -> ReceiptReadView<'a> {
    if envelope.validate().is_err() {
        return without_native(kind, confirmed_level, ProjectionReason::IdentityMismatch);
    }
    let binding = ExternalInputBinding {
        version: 1,
        owner_id: envelope.owner_id.clone(),
        thread_id: envelope.thread_id.clone(),
        runtime_generation: envelope.runtime_generation,
        canonical_byte_limit: Default::default(),
    };
    let key = envelope.key();
    // A validated A4 already present in the last successful status is retained
    // evidence too. A transient failure on the next read must not erase it.
    let retained_status_a4 = status
        .filter(|status| {
            Response {
                version: 1,
                owner_id: binding.owner_id.clone(),
                thread_id: binding.thread_id.clone(),
                runtime_generation: binding.runtime_generation,
                statuses: vec![(*status).clone()],
            }
            .validate(&binding, std::slice::from_ref(&key))
            .is_ok()
        })
        .and_then(|status| status.receipt.as_ref());
    let retained_a4 = a4.or(retained_status_a4);
    let trusted = evidence.trusted.filter(|proof| {
        proof.key == key
            && proof.occurrence.cutex_session_id == envelope.owner_id
            && proof.occurrence.thread_id == envelope.thread_id
            && proof.occurrence.runtime_generation == envelope.runtime_generation
    });
    let occurrence = trusted.map(|proof| &proof.occurrence);
    let terminal = trusted.and_then(|proof| proof.terminal.as_ref());
    let status_read = if evidence.status_unavailable {
        StatusRead::Unavailable
    } else {
        status.map_or(StatusRead::NotObserved, |status| StatusRead::Available {
            runtime_generation: envelope.runtime_generation,
            status,
        })
    };
    let subject = ReceiptSubject {
        kind,
        key: &key,
        binding: &binding,
        occurrence,
        confirmed_level,
        a4: retained_a4,
    };
    let projection = project(
        &subject,
        &Observations {
            runtime_generation: evidence.runtime_generation,
            status: status_read,
            terminal,
        },
    );
    let turn_id = if projection.state == ProjectionState::InContext {
        retained_a4
            .filter(|receipt| receipt.validate(&binding, &key).is_ok())
            .map(|receipt| receipt.turn_id.clone())
    } else {
        None
    };
    ReceiptReadView {
        projection,
        turn_id,
    }
}

/// Actual bus snapshot adapter for ordinary tell and delegation revisions.
/// Do not serialize/deserialise an arbitrary diagnostic object into this API.
/// The canonical snapshot must come from the authenticated repository read.
pub fn bus_tell_read<'a>(
    snapshot: &'a AgentBusMessageSnapshot,
    confirmed_level: RevisionLevel,
    evidence: ReadEvidence<'a>,
) -> ReceiptReadView<'a> {
    let legacy_a4 = original_a4(snapshot);
    let confirmed_level = if legacy_a4.is_some() {
        confirmed_level.max(RevisionLevel::InContext)
    } else {
        confirmed_level
    };
    if let Some(envelope) = &snapshot.external_input {
        if envelope.message.id != snapshot.message_id
            || envelope.owner_id != snapshot.to_cutex_session_id
        {
            return without_native(
                ReceiptKind::Tell,
                confirmed_level,
                ProjectionReason::IdentityMismatch,
            );
        }
        // The bus message digest and native envelope digest cover different
        // canonical data. native_read validates the frozen envelope itself.
        let mut read = native_read(
            ReceiptKind::Tell,
            envelope,
            snapshot.external_input_receipt.as_ref(),
            snapshot.external_input_last_observed.as_ref(),
            confirmed_level,
            evidence,
        );
        if read.projection.state == ProjectionState::InContext && read.turn_id.is_none() {
            read.turn_id = legacy_a4.map(|receipt| receipt.turn_id.clone());
        }
        return read;
    }
    // Historical committed legacy A4/context insertion is original business
    // evidence. It is not a native terminal-turn heuristic or fabricated A4.
    let level = if snapshot.state == "delivered" {
        confirmed_level.max(RevisionLevel::InContext)
    } else {
        confirmed_level
    };
    let mut view = without_native(ReceiptKind::Tell, level, ProjectionReason::AwaitingEvidence);
    view.turn_id = legacy_a4.map(|receipt| receipt.turn_id.clone());
    view
}

fn original_a4(snapshot: &AgentBusMessageSnapshot) -> Option<&InterAgentContextPersistedReceipt> {
    snapshot.a4_receipt.as_ref().filter(|receipt| {
        !receipt.receipt_id.is_empty()
            && !receipt.turn_id.is_empty()
            && !receipt.response_item_id.is_empty()
            && receipt.rollout_ordinal > 0
            && receipt
                .message_id
                .as_ref()
                .is_none_or(|id| id == &snapshot.message_id)
            && receipt.semantic_sha256.as_ref().is_none_or(|digest| {
                snapshot.semantic_sha256.as_ref().or_else(|| {
                    snapshot
                        .external_input
                        .as_ref()
                        .map(|envelope| &envelope.semantic_sha256)
                }) == Some(digest)
            })
            && receipt.thread_id.as_ref().is_none_or(|thread| {
                snapshot
                    .external_input
                    .as_ref()
                    .is_none_or(|envelope| thread == &envelope.thread_id)
            })
    })
}

/// Compatibility reader used by the existing SchedulerWorld::message_level
/// call path. Only a canonical, typed snapshot may establish positive progress.
/// No processing/terminal state string can synthesize InContext or Delivered.
pub fn snapshot_level(snapshot: &Value) -> (RevisionLevel, Option<String>) {
    let Ok(snapshot) = serde_json::from_value::<AgentBusMessageSnapshot>(snapshot.clone()) else {
        return (RevisionLevel::Sent, None);
    };
    let read = bus_tell_read(&snapshot, RevisionLevel::Sent, ReadEvidence::default());
    (read.projection.confirmed_level, read.turn_id)
}

/// Construct a revision's existing summary fields and the additive read view.
/// Parent view wiring can call this with one request-scoped evidence read, so
/// renderers do not poll native runtimes or acquire per-revision write locks.
pub fn revision_summary(
    revision: &RevisionRecord,
    snapshot: Option<&AgentBusMessageSnapshot>,
    evidence: ReadEvidence<'_>,
) -> Value {
    let view = match snapshot
        .filter(|snapshot| revision.message_id.as_deref() == Some(snapshot.message_id.as_str()))
    {
        Some(snapshot) => bus_tell_read(snapshot, revision.level, evidence),
        None => without_native(
            ReceiptKind::Tell,
            revision.level,
            ProjectionReason::AwaitingEvidence,
        ),
    };
    serde_json::json!({
        "id": revision.id, "level": revision.level, "supersedes": revision.supersedes,
        "sentAt": revision.sent_at, "acknowledgedAt": revision.acknowledged_at,
        "receiptProjection": view,
    })
}

#[cfg(test)]
mod tests;
