//! Read-only projection for tell/steer receipts whose exact turn ended without A4.
//!
//! This is deliberately separate from `RevisionLevel`: uncertainty is neither
//! delivery nor permission to retry, settle, delete, release admission or change
//! a receipt. One bound occurrence and one terminal observation make the work
//! bounded; there is no polling, wall-clock timeout or sticky observation cache.
//! The caller retains the original records, including contradictory/late evidence.
//!
//! Trust boundary: occurrences must come from an actual per-message native
//! binding, not the target's currently active turn, an idle/offline observation,
//! a stop request, a generic RPC failure or an ask's terminal execution label.
//! Native status responses must first pass `Response::validate`. Terminal events
//! must arrive on the authenticated connection for the supplied session/generation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{AttemptRecord, AttemptState, RevisionLevel, RevisionRecord};
use crate::app_server::external_input::{DeliveryState, MessageKey, Receipt, Response, Status};
use crate::launch::stock::ExternalInputBinding;

pub mod integration;
pub mod read_context;
pub mod read_views;

fn valid_key(key: &MessageKey) -> bool {
    !key.message_id.is_empty()
        && key.message_id.len() <= 256
        && key.semantic_sha256.len() == 64
        && key
            .semantic_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReceiptKind {
    Tell,
    Steer,
}

/// These identities are different namespaces; never convert one into the other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AttemptIdentity {
    Delegation { ask_id: String, attempt: u32 },
    Native { attempt_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnOccurrence {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub runtime_generation: u64,
    pub turn_id: String,
    pub attempt: AttemptIdentity,
}

impl TurnOccurrence {
    fn valid(&self) -> bool {
        let id = |s: &str| !s.is_empty() && s.len() <= 256;
        id(&self.cutex_session_id)
            && id(&self.thread_id)
            && id(&self.turn_id)
            && match &self.attempt {
                AttemptIdentity::Delegation { ask_id, attempt } => id(ask_id) && *attempt > 0,
                AttemptIdentity::Native { attempt_id } => uuid::Uuid::parse_str(attempt_id)
                    .is_ok_and(|v| !v.is_nil() && v.to_string() == *attempt_id),
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalOutcome {
    Completed,
    Failed,
    Interrupted,
    /// A delegation_await stage ended this exact turn.
    Yielded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalSource {
    RuntimeEvent,
    ExactTurnRead,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEvidence {
    pub key: MessageKey,
    pub occurrence: TurnOccurrence,
    pub outcome: TerminalOutcome,
    pub source: TerminalSource,
    pub observed_at: DateTime<Utc>,
}

/// Identity of the actual authenticated event connection / exact-turn read.
/// Supply it from the runtime manager, independently of the stored occurrence.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeIdentity<'a> {
    pub cutex_session_id: &'a str,
    pub runtime_generation: u64,
}

/// Capture only an actual exact-turn terminal notification. A read of that
/// exact turn can use this same shape after the caller validates its provenance.
/// `key`/`occurrence` must already be bound to this message before this call.
pub fn terminal_from_event(
    key: &MessageKey,
    occurrence: &TurnOccurrence,
    runtime: RuntimeIdentity<'_>,
    method: &str,
    params: &Value,
    observed_at: DateTime<Utc>,
) -> Option<TerminalEvidence> {
    if method != "turn/completed" {
        return None;
    }
    terminal_from_parts(
        key,
        occurrence,
        runtime,
        params.get("threadId")?.as_str()?,
        params.get("turn")?,
        TerminalSource::RuntimeEvent,
        observed_at,
    )
}

fn terminal_from_parts(
    key: &MessageKey,
    occurrence: &TurnOccurrence,
    runtime: RuntimeIdentity<'_>,
    thread_id: &str,
    turn: &Value,
    source: TerminalSource,
    observed_at: DateTime<Utc>,
) -> Option<TerminalEvidence> {
    if !valid_key(key)
        || !occurrence.valid()
        || runtime.cutex_session_id != occurrence.cutex_session_id
        || runtime.runtime_generation != occurrence.runtime_generation
        || thread_id != occurrence.thread_id
        || turn.get("id").and_then(Value::as_str) != Some(occurrence.turn_id.as_str())
    {
        return None;
    }
    let outcome = match turn.get("status").and_then(Value::as_str)? {
        "completed" => TerminalOutcome::Completed,
        "failed" => TerminalOutcome::Failed,
        "interrupted" => TerminalOutcome::Interrupted,
        _ => return None,
    };
    Some(TerminalEvidence {
        key: key.clone(),
        occurrence: occurrence.clone(),
        outcome,
        source,
        observed_at,
    })
}

/// Cross-check actual terminal evidence against the canonical attempt. This
/// deliberately cannot CREATE evidence from `AttemptState::Interrupted`:
/// offline grace, runtime restart and outcomeUnknown also set that state.
/// The event may precede the durable state change, so Running is permitted.
pub fn terminal_matches_attempt(
    terminal: &TerminalEvidence,
    ask_id: &str,
    record: &AttemptRecord,
) -> bool {
    let occurrence = &terminal.occurrence;
    if !occurrence.valid()
        || occurrence.attempt
            != (AttemptIdentity::Delegation {
                ask_id: ask_id.into(),
                attempt: record.n,
            })
        || occurrence.runtime_generation != record.runtime_generation
        || record.turn_id.as_deref() != Some(occurrence.turn_id.as_str())
        || record.state == AttemptState::Refused
    {
        return false;
    }
    true
}

/// The caller must have read this exact turn from the authenticated original
/// binding, not a current-turn/idle summary or an unbounded history search.
pub fn terminal_from_exact_turn_read(
    key: &MessageKey,
    occurrence: &TurnOccurrence,
    runtime: RuntimeIdentity<'_>,
    thread_id: &str,
    turn: &Value,
    observed_at: DateTime<Utc>,
) -> Option<TerminalEvidence> {
    terminal_from_parts(
        key,
        occurrence,
        runtime,
        thread_id,
        turn,
        TerminalSource::ExactTurnRead,
        observed_at,
    )
}

/// A failed/missing read is different from a native `delivery_state=unknown`.
/// Re-evaluate every successful read, even when its generation is unchanged.
#[derive(Clone, Copy, Debug)]
pub enum StatusRead<'a> {
    NotObserved,
    Unavailable,
    Available {
        runtime_generation: u64,
        status: &'a Status,
    },
}

pub struct ReceiptSubject<'a> {
    pub kind: ReceiptKind,
    pub key: &'a MessageKey,
    pub binding: &'a ExternalInputBinding,
    pub occurrence: Option<&'a TurnOccurrence>,
    /// Trusted durable progress for this exact receipt; never inferred here.
    pub confirmed_level: RevisionLevel,
    /// A previously validated A4, retained independently of latest read errors.
    pub a4: Option<&'a Receipt>,
}

pub struct Observations<'a> {
    /// None means generation is unknown, not zero. No current-generation guess.
    pub runtime_generation: Option<u64>,
    pub status: StatusRead<'a>,
    pub terminal: Option<&'a TerminalEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectionState {
    Waiting,
    Unknown,
    Uncertain,
    InContext,
    Acknowledged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectionReason {
    ConfirmedProgress,
    ActualA4,
    AwaitingEvidence,
    ReadUnavailable,
    NativeUnknown,
    ConflictingStatus,
    IdentityMismatch,
    GenerationMismatch,
    TerminalWithoutA4,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptProjection<'a> {
    pub kind: ReceiptKind,
    pub state: ProjectionState,
    pub confirmed_level: RevisionLevel,
    pub reason: ProjectionReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_evidence: Option<&'a TerminalEvidence>,
}

/// Constant-size, side-effect-free view. Exact terminal evidence makes the
/// receipt uncertain immediately; no timer/retry budget can leave it waiting
/// forever. Positive progress wins over uncertainty; uncertainty never raises
/// the confirmed level. In particular, native `pending` does NOT imply A4.
pub fn project<'a>(
    subject: &ReceiptSubject<'_>,
    observations: &Observations<'a>,
) -> ReceiptProjection<'a> {
    project_inner(subject, observations, None)
}

/// An independently fenced observer of an EXACT historical input/turn read.
/// This is deliberately not accepted by the runtime-event projection. Only the
/// typed repository-evidence bridge can supply it; it never rewrites submission
/// generation to the observer's newer generation.
pub(super) struct HistoryObserver<'a> {
    pub session: &'a str,
    pub thread: &'a str,
    pub current_generation: u64,
    pub evidence_observing_generation: u64,
}

pub(super) fn project_history<'a>(
    subject: &ReceiptSubject<'_>,
    observations: &Observations<'a>,
    observer: HistoryObserver<'_>,
) -> ReceiptProjection<'a> {
    project_inner(subject, observations, Some(observer))
}

fn project_inner<'a>(
    subject: &ReceiptSubject<'_>,
    observations: &Observations<'a>,
    history: Option<HistoryObserver<'_>>,
) -> ReceiptProjection<'a> {
    let mut result = ReceiptProjection {
        kind: subject.kind,
        state: ProjectionState::Unknown,
        confirmed_level: subject.confirmed_level,
        reason: ProjectionReason::AwaitingEvidence,
        terminal_evidence: None,
    };
    if subject.confirmed_level >= RevisionLevel::InContext {
        result.state = if subject.confirmed_level == RevisionLevel::Acknowledged {
            ProjectionState::Acknowledged
        } else {
            ProjectionState::InContext
        };
        result.reason = ProjectionReason::ConfirmedProgress;
        return result;
    }
    if !valid_key(subject.key)
        || subject.binding.version != 1
        || subject.binding.owner_id.is_empty()
        || subject.binding.owner_id.len() > 256
        || subject.binding.thread_id.is_empty()
        || subject.binding.thread_id.len() > 256
    {
        result.reason = ProjectionReason::IdentityMismatch;
        return result;
    }
    // A4 belongs to the original occurrence. A later runtime restart/read
    // failure must not erase it. Validation never performs IO.
    let actual_a4 = |receipt: &Receipt| {
        receipt.validate(subject.binding, subject.key).is_ok()
            && subject.occurrence.is_none_or(|o| {
                o.valid()
                    && o.cutex_session_id == subject.binding.owner_id
                    && o.thread_id == subject.binding.thread_id
                    && o.runtime_generation == subject.binding.runtime_generation
                    && receipt.turn_id == o.turn_id
            })
    };
    if let Some(receipt) = subject.a4 {
        if actual_a4(receipt) {
            result.state = ProjectionState::InContext;
            result.confirmed_level = RevisionLevel::InContext;
            result.reason = ProjectionReason::ActualA4;
        } else {
            result.reason = ProjectionReason::IdentityMismatch;
        }
        return result;
    }
    if let StatusRead::Available {
        runtime_generation,
        status,
    } = observations.status
    {
        let response = Response {
            version: 1,
            owner_id: subject.binding.owner_id.clone(),
            thread_id: subject.binding.thread_id.clone(),
            runtime_generation,
            statuses: vec![status.clone()],
        };
        if response
            .validate(subject.binding, std::slice::from_ref(subject.key))
            .is_err()
        {
            result.reason = ProjectionReason::IdentityMismatch;
            return result;
        }
        if status.receipt.as_ref().is_some_and(actual_a4) {
            result.state = ProjectionState::InContext;
            result.confirmed_level = RevisionLevel::InContext;
            result.reason = ProjectionReason::ActualA4;
            return result;
        }
        match status.delivery_state {
            DeliveryState::Unknown => {
                result.reason = ProjectionReason::NativeUnknown;
                return result;
            }
            DeliveryState::Conflict | DeliveryState::RetryableError => {
                result.reason = ProjectionReason::ConflictingStatus;
                return result;
            }
            DeliveryState::ContextPersisted => {
                // A valid A4 for another turn is not this occurrence's proof.
                result.reason = ProjectionReason::IdentityMismatch;
                return result;
            }
            DeliveryState::Pending => {}
        }
        if subject.occurrence.is_none() {
            // Native acceptance alone does not identify which turn/attempt
            // this tell/steer belongs to. Never borrow the active turn.
            result.reason = ProjectionReason::AwaitingEvidence;
            return result;
        }
        if let Some(TurnOccurrence {
            attempt: AttemptIdentity::Native { attempt_id },
            ..
        }) = subject.occurrence
        {
            if status.processing.attempt_id.as_deref() != Some(attempt_id.as_str()) {
                result.reason = ProjectionReason::IdentityMismatch;
                return result;
            }
        }
        result.state = ProjectionState::Waiting;
    } else if matches!(observations.status, StatusRead::Unavailable) {
        result.reason = ProjectionReason::ReadUnavailable;
    }
    let exact_history = history.is_some_and(|observer| {
        observer.session == subject.binding.owner_id
            && observer.thread == subject.binding.thread_id
            && observer.current_generation == observer.evidence_observing_generation
            && observations.runtime_generation == Some(observer.current_generation)
            && observations
                .terminal
                .is_some_and(|terminal| terminal.source == TerminalSource::ExactTurnRead)
    });
    if observations.runtime_generation != Some(subject.binding.runtime_generation) && !exact_history
    {
        result.state = ProjectionState::Unknown;
        result.reason = ProjectionReason::GenerationMismatch;
        return result;
    }
    let Some(terminal) = observations.terminal else {
        return result;
    };
    let Some(occurrence) = subject.occurrence else {
        result.state = ProjectionState::Unknown;
        result.reason = ProjectionReason::IdentityMismatch;
        return result;
    };
    if !occurrence.valid()
        || terminal.key != *subject.key
        || terminal.occurrence != *occurrence
        || occurrence.cutex_session_id != subject.binding.owner_id
        || occurrence.thread_id != subject.binding.thread_id
        || occurrence.runtime_generation != subject.binding.runtime_generation
    {
        result.state = ProjectionState::Unknown;
        result.reason = ProjectionReason::IdentityMismatch;
        return result;
    }
    result.state = ProjectionState::Uncertain;
    result.reason = ProjectionReason::TerminalWithoutA4;
    result.terminal_evidence = Some(terminal);
    result
}

/// A revision adapter checks message linkage before applying the same view.
/// It does not change the revision's level, timestamps, text or receipt.
pub fn project_revision<'a>(
    revision: &RevisionRecord,
    subject: &ReceiptSubject<'_>,
    observations: &Observations<'a>,
) -> Option<ReceiptProjection<'a>> {
    if revision.message_id.as_deref() != Some(subject.key.message_id.as_str()) {
        return None;
    }
    Some(project(
        &ReceiptSubject {
            confirmed_level: revision.level,
            ..*subject
        },
        observations,
    ))
}

#[cfg(test)]
mod tests;
