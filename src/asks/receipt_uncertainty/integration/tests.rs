use super::*;
use crate::app_server::external_input::{
    Delivery, Message, ProcessingState, ProcessingStatus, Source, SourceKind,
};
use serde_json::json;

fn envelope() -> Envelope {
    let mut envelope = Envelope {
        version: 1,
        owner_id: "target".into(),
        thread_id: "thread".into(),
        runtime_generation: 9,
        message: Message {
            id: "tell".into(),
            source: Source {
                kind: SourceKind::Agent,
                id: "sender".into(),
            },
            event_type: "message".into(),
            delivery: Delivery::Passive,
            text: "change the brief".into(),
        },
        view: None,
        semantic_sha256: String::new(),
    };
    envelope.semantic_sha256 = envelope.digest();
    envelope.validate().unwrap();
    envelope
}

fn snapshot() -> AgentBusMessageSnapshot {
    let envelope = envelope();
    serde_json::from_value(json!({
        "messageId":"tell", "fromCutexSessionId":"sender", "toCutexSessionId":"target",
        "deliveryMode":"passive", "content":"change the brief", "state":"pending",
        "semanticSha256":envelope.semantic_sha256, "externalInput":envelope,
        "externalInputLastObserved":pending(),
    }))
    .unwrap()
}

fn pending() -> Status {
    Status {
        message_id: "tell".into(),
        semantic_sha256: envelope().semantic_sha256,
        delivery_state: DeliveryState::Pending,
        receipt: None,
        processing: ProcessingStatus {
            state: ProcessingState::Pending,
            attempt_id: None,
            reason: None,
        },
    }
}

fn proof() -> TrustedReceiptEvidence {
    let occurrence = TurnOccurrence {
        cutex_session_id: "target".into(),
        thread_id: "thread".into(),
        runtime_generation: 9,
        turn_id: "turn-2".into(),
        attempt: AttemptIdentity::Delegation {
            ask_id: "ask".into(),
            attempt: 2,
        },
    };
    let key = envelope().key();
    let terminal = terminal_from_event(
        &key,
        &occurrence,
        RuntimeIdentity {
            cutex_session_id: "target",
            runtime_generation: 9,
        },
        "turn/completed",
        &json!({"threadId":"thread","turn":{"id":"turn-2","status":"interrupted"}}),
        "2026-10-27T08:00:00Z".parse().unwrap(),
    )
    .unwrap();
    TrustedReceiptEvidence {
        key,
        occurrence,
        terminal: Some(terminal),
    }
}

fn evidence(proof: &TrustedReceiptEvidence) -> ReadEvidence<'_> {
    ReadEvidence {
        runtime_generation: Some(9),
        trusted: Some(proof),
        status_unavailable: false,
    }
}

fn revision(level: RevisionLevel) -> RevisionRecord {
    serde_json::from_value(
        json!({"id":"v2","text":"change the brief","messageId":"tell",
        "sentAt":"2026-10-27T07:00:00Z","level":level}),
    )
    .unwrap()
}

fn a4() -> Receipt {
    let envelope = envelope();
    let mut a4 = Receipt {
        schema: "codex.external-input-receipt.v1".into(),
        receipt_id: String::new(),
        owner_id: envelope.owner_id,
        thread_id: envelope.thread_id,
        message_id: envelope.message.id.clone(),
        response_item_id: envelope.message.id,
        semantic_sha256: envelope.semantic_sha256,
        turn_id: "turn-2".into(),
        ordinal: 1,
    };
    a4.receipt_id = a4.digest_id();
    a4
}

#[test]
fn real_revision_read_seam_adds_uncertainty_while_retaining_original_level_receipts_and_ack_fields()
{
    let snapshot = snapshot();
    let proof = proof();
    let revision = revision(RevisionLevel::Delivered);
    let before = serde_json::to_vec(&(&snapshot, &proof, &revision)).unwrap();
    let summary =
        crate::asks::revisions::read_summary(&revision, Some(&snapshot), evidence(&proof));
    assert_eq!(summary["level"], "delivered");
    assert_eq!(summary["receiptProjection"]["state"], "uncertain");
    assert_eq!(summary["receiptProjection"]["confirmedLevel"], "delivered");
    assert_eq!(
        summary["receiptProjection"]["terminalEvidence"]["key"]["messageId"],
        "tell"
    );
    assert_eq!(
        serde_json::to_vec(&(&snapshot, &proof, &revision)).unwrap(),
        before
    );
}

#[test]
fn pending_or_generic_terminal_diagnostics_without_occurrence_proof_remain_unknown() {
    let mut snapshot = snapshot();
    for state in ["pending", "interrupted", "completed", "failed", "delivered"] {
        snapshot.state = state.into();
        let read = bus_tell_read(
            &snapshot,
            RevisionLevel::Sent,
            ReadEvidence {
                runtime_generation: Some(9),
                ..Default::default()
            },
        );
        assert_eq!(read.projection.state, ProjectionState::Unknown);
        assert_eq!(read.projection.confirmed_level, RevisionLevel::Sent);
        assert!(read.projection.terminal_evidence.is_none());
    }
}

#[test]
fn wrong_occurrence_key_or_generation_cannot_bind_revision_to_a_different_receipt() {
    let snapshot = snapshot();
    for field in ["message", "digest", "generation", "owner", "thread"] {
        let mut proof = proof();
        match field {
            "message" => proof.key.message_id = "other".into(),
            "digest" => proof.key.semantic_sha256 = "b".repeat(64),
            "generation" => proof.occurrence.runtime_generation = 8,
            "owner" => proof.occurrence.cutex_session_id = "other".into(),
            _ => proof.occurrence.thread_id = "other".into(),
        }
        let summary = crate::asks::revisions::read_summary(
            &revision(RevisionLevel::Sent),
            Some(&snapshot),
            evidence(&proof),
        );
        assert_eq!(summary["receiptProjection"]["state"], "unknown", "{field}");
    }
}

#[test]
fn typed_snapshot_reader_on_existing_scheduler_path_requires_actual_native_a4() {
    let mut snapshot = snapshot();
    assert_eq!(
        crate::asks::revisions::level_of_snapshot(&serde_json::to_value(&snapshot).unwrap()),
        (RevisionLevel::Sent, None)
    );
    snapshot
        .external_input_last_observed
        .as_mut()
        .unwrap()
        .delivery_state = DeliveryState::Unknown;
    assert_eq!(
        crate::asks::revisions::level_of_snapshot(&serde_json::to_value(&snapshot).unwrap()),
        (RevisionLevel::Sent, None)
    );
    snapshot
        .external_input_last_observed
        .as_mut()
        .unwrap()
        .delivery_state = DeliveryState::ContextPersisted;
    assert_eq!(
        crate::asks::revisions::level_of_snapshot(&serde_json::to_value(&snapshot).unwrap()),
        (RevisionLevel::Sent, None)
    );
    snapshot.external_input_receipt = Some(a4());
    let original = serde_json::to_vec(&snapshot).unwrap();
    assert_eq!(
        crate::asks::revisions::level_of_snapshot(&serde_json::to_value(&snapshot).unwrap()),
        (RevisionLevel::InContext, Some("turn-2".into()))
    );
    assert_eq!(serde_json::to_vec(&snapshot).unwrap(), original);
}

#[test]
fn retained_actual_a4_precedes_mismatched_sidecar_and_read_failure() {
    let mut snapshot = snapshot();
    snapshot.external_input_receipt = Some(a4());
    let mut proof = proof();
    proof.key.message_id = "other".into();
    let read = bus_tell_read(
        &snapshot,
        RevisionLevel::Sent,
        ReadEvidence {
            trusted: Some(&proof),
            runtime_generation: Some(10),
            status_unavailable: true,
        },
    );
    assert_eq!(read.projection.state, ProjectionState::InContext);
    assert_eq!(read.turn_id.as_deref(), Some("turn-2"));
}

#[test]
fn native_steer_adapter_uses_same_exact_proof_and_recovers_read_failure_at_same_generation() {
    let envelope = envelope();
    let proof = proof();
    let pending = pending();
    let failed = native_steer_read(
        &envelope,
        None,
        None,
        RevisionLevel::Sent,
        ReadEvidence {
            status_unavailable: true,
            ..evidence(&proof)
        },
    );
    assert_eq!(failed.projection.state, ProjectionState::Uncertain);
    assert_eq!(failed.projection.kind, ReceiptKind::Steer);
    let without_proof = native_steer_read(
        &envelope,
        None,
        Some(&pending),
        RevisionLevel::Sent,
        ReadEvidence {
            runtime_generation: Some(9),
            ..Default::default()
        },
    );
    assert_eq!(without_proof.projection.state, ProjectionState::Unknown);
    let mut recovered = pending;
    recovered.delivery_state = DeliveryState::ContextPersisted;
    recovered.receipt = Some(a4());
    let success = native_steer_read(
        &envelope,
        None,
        Some(&recovered),
        RevisionLevel::Sent,
        evidence(&proof),
    );
    assert_eq!(success.projection.state, ProjectionState::InContext);
    assert_eq!(success.turn_id.as_deref(), Some("turn-2"));
    let next_read_failed = native_steer_read(
        &envelope,
        None,
        Some(&recovered),
        RevisionLevel::Sent,
        ReadEvidence {
            status_unavailable: true,
            ..evidence(&proof)
        },
    );
    assert_eq!(
        next_read_failed.projection.state,
        ProjectionState::InContext
    );
}

#[test]
fn snapshot_a4_for_another_key_cannot_turn_a_delivered_string_into_success() {
    let mut snapshot = snapshot();
    snapshot.state = "delivered".into();
    let mut receipt = a4();
    receipt.message_id = "other".into();
    receipt.response_item_id = "other".into();
    receipt.receipt_id = receipt.digest_id();
    snapshot.external_input_receipt = Some(receipt);
    assert_eq!(
        bus_tell_read(&snapshot, RevisionLevel::Sent, ReadEvidence::default())
            .projection
            .state,
        ProjectionState::Unknown
    );
}

#[test]
fn legacy_original_a4_and_committed_context_are_preserved_without_inventing_a_turn() {
    let mut legacy = snapshot();
    legacy.external_input = None;
    legacy.external_input_last_observed = None;
    legacy.a4_receipt = Some(
        serde_json::from_value(json!({"receiptId":"original-a4","turnId":"old-turn",
        "messageId":"tell", "responseItemId":"item-original", "rolloutOrdinal":3}))
        .unwrap(),
    );
    let original = serde_json::to_vec(&legacy).unwrap();
    let read = bus_tell_read(&legacy, RevisionLevel::Sent, ReadEvidence::default());
    assert_eq!(read.projection.state, ProjectionState::InContext);
    assert_eq!(read.turn_id.as_deref(), Some("old-turn"));
    assert_eq!(serde_json::to_vec(&legacy).unwrap(), original);
    legacy.a4_receipt = None;
    legacy.state = "delivered".into();
    let read = bus_tell_read(&legacy, RevisionLevel::Sent, ReadEvidence::default());
    assert_eq!(read.projection.state, ProjectionState::InContext);
    assert!(read.turn_id.is_none());
}

#[test]
fn original_legacy_a4_is_retained_even_when_snapshot_also_has_native_pending_fields() {
    let mut snapshot = snapshot();
    let proof = proof();
    snapshot.a4_receipt = Some(
        serde_json::from_value(json!({"receiptId":"original-a4","turnId":"original-turn",
        "messageId":"tell","responseItemId":"tell","rolloutOrdinal":1}))
        .unwrap(),
    );
    let original = serde_json::to_vec(&snapshot).unwrap();
    let read = bus_tell_read(&snapshot, RevisionLevel::Sent, evidence(&proof));
    assert_eq!(read.projection.state, ProjectionState::InContext);
    assert_eq!(read.turn_id.as_deref(), Some("original-turn"));
    assert_eq!(serde_json::to_vec(&snapshot).unwrap(), original);
}

#[test]
fn arbitrary_json_labels_are_not_a_canonical_snapshot_or_a4_receipt() {
    for value in [
        json!({"state":"delivered"}),
        json!({"externalInputLastObserved":{"deliveryState":"inContext"}}),
        json!({"state":"pending","a4Receipt":{"turnId":"invented"}}),
    ] {
        assert_eq!(
            crate::asks::revisions::level_of_snapshot(&value),
            (RevisionLevel::Sent, None)
        );
    }
}

#[test]
fn missing_or_wrong_snapshot_link_keeps_unknown_and_ack_takes_precedence() {
    let proof = proof();
    let mut snapshot = snapshot();
    snapshot.message_id = "other".into();
    let revision = revision(RevisionLevel::Sent);
    for snapshot in [None, Some(&snapshot)] {
        let view = crate::asks::revisions::read_summary(&revision, snapshot, evidence(&proof));
        assert_eq!(view["receiptProjection"]["state"], "unknown");
    }
    let ack = super::tests::revision(RevisionLevel::Acknowledged);
    let view = crate::asks::revisions::read_summary(&ack, None, ReadEvidence::default());
    assert_eq!(view["receiptProjection"]["state"], "acknowledged");
    assert_eq!(view["level"], "acknowledged");
}

#[test]
fn changed_current_generation_cannot_reuse_exact_old_terminal_proof() {
    let snapshot = snapshot();
    let proof = proof();
    let read = bus_tell_read(
        &snapshot,
        RevisionLevel::Delivered,
        ReadEvidence {
            runtime_generation: Some(10),
            ..evidence(&proof)
        },
    );
    assert_eq!(read.projection.state, ProjectionState::Unknown);
    assert_eq!(read.projection.confirmed_level, RevisionLevel::Delivered);
}
