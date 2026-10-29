use super::*;
use crate::app_server::external_input::{HoldReason, ProcessingState, ProcessingStatus};
use serde_json::json;

struct Fixture {
    key: MessageKey,
    binding: ExternalInputBinding,
    occurrence: TurnOccurrence,
    status: Status,
    terminal: TerminalEvidence,
}

impl Fixture {
    fn new() -> Self {
        let key = MessageKey {
            message_id: "tell-1".into(),
            semantic_sha256: "a".repeat(64),
        };
        let binding = serde_json::from_value(json!({
            "version": 1, "ownerId": "target", "threadId": "thread-1", "runtimeGeneration": 7
        }))
        .unwrap();
        let occurrence = TurnOccurrence {
            cutex_session_id: "target".into(),
            thread_id: "thread-1".into(),
            runtime_generation: 7,
            turn_id: "turn-1".into(),
            attempt: AttemptIdentity::Delegation {
                ask_id: "ask-1".into(),
                attempt: 2,
            },
        };
        let status = Status {
            message_id: key.message_id.clone(),
            semantic_sha256: key.semantic_sha256.clone(),
            delivery_state: DeliveryState::Pending,
            receipt: None,
            processing: ProcessingStatus {
                state: ProcessingState::Pending,
                attempt_id: None,
                reason: None,
            },
        };
        let terminal = terminal_from_event(
            &key,
            &occurrence,
            runtime_identity(),
            "turn/completed",
            &json!({
                "threadId": "thread-1", "turn": { "id": "turn-1", "status": "interrupted" }
            }),
            at(),
        )
        .unwrap();
        Self {
            key,
            binding,
            occurrence,
            status,
            terminal,
        }
    }

    fn subject(&self) -> ReceiptSubject<'_> {
        ReceiptSubject {
            kind: ReceiptKind::Tell,
            key: &self.key,
            binding: &self.binding,
            occurrence: Some(&self.occurrence),
            confirmed_level: RevisionLevel::Sent,
            a4: None,
        }
    }

    fn observations(&self) -> Observations<'_> {
        Observations {
            runtime_generation: Some(7),
            status: StatusRead::Available {
                runtime_generation: 7,
                status: &self.status,
            },
            terminal: Some(&self.terminal),
        }
    }

    fn a4(&self) -> Receipt {
        let mut receipt = Receipt {
            schema: "codex.external-input-receipt.v1".into(),
            receipt_id: String::new(),
            owner_id: self.binding.owner_id.clone(),
            thread_id: self.binding.thread_id.clone(),
            message_id: self.key.message_id.clone(),
            semantic_sha256: self.key.semantic_sha256.clone(),
            response_item_id: self.key.message_id.clone(),
            turn_id: self.occurrence.turn_id.clone(),
            ordinal: 1,
        };
        receipt.receipt_id = receipt.digest_id();
        receipt
    }
}

fn at() -> DateTime<Utc> {
    "2026-10-27T07:00:00Z".parse().unwrap()
}

fn runtime_identity() -> RuntimeIdentity<'static> {
    RuntimeIdentity {
        cutex_session_id: "target",
        runtime_generation: 7,
    }
}

#[test]
fn exact_interrupted_tell_and_steer_are_uncertain_without_raising_any_progress() {
    let f = Fixture::new();
    for kind in [ReceiptKind::Tell, ReceiptKind::Steer] {
        for level in [RevisionLevel::Sent, RevisionLevel::Delivered] {
            let subject = ReceiptSubject {
                kind,
                confirmed_level: level,
                ..f.subject()
            };
            let projection = project(&subject, &f.observations());
            assert_eq!(projection.state, ProjectionState::Uncertain);
            assert_eq!(projection.confirmed_level, level);
            assert_eq!(projection.reason, ProjectionReason::TerminalWithoutA4);
            assert_eq!(projection.terminal_evidence, Some(&f.terminal));
        }
    }
}

#[test]
fn terminal_event_without_a4_is_sufficient_even_if_latest_read_failed_or_was_never_made() {
    let f = Fixture::new();
    for read in [StatusRead::Unavailable, StatusRead::NotObserved] {
        let observation = Observations {
            status: read,
            ..f.observations()
        };
        assert_eq!(
            project(&f.subject(), &observation).state,
            ProjectionState::Uncertain
        );
    }
}

#[test]
fn no_terminal_proof_does_not_turn_idle_or_missing_reads_into_uncertainty() {
    let f = Fixture::new();
    let mut observation = Observations {
        terminal: None,
        ..f.observations()
    };
    assert_eq!(
        project(&f.subject(), &observation).state,
        ProjectionState::Waiting
    );
    observation.status = StatusRead::Unavailable;
    let projection = project(&f.subject(), &observation);
    assert_eq!(projection.state, ProjectionState::Unknown);
    assert_eq!(projection.reason, ProjectionReason::ReadUnavailable);
    observation.status = StatusRead::NotObserved;
    assert_eq!(
        project(&f.subject(), &observation).state,
        ProjectionState::Unknown
    );
}

#[test]
fn terminal_identity_is_scoped_to_message_digest_session_thread_generation_turn_and_attempt() {
    let f = Fixture::new();
    let mut cases = Vec::new();
    let mut evidence = f.terminal.clone();
    evidence.key.message_id = "other".into();
    cases.push(evidence);
    let mut evidence = f.terminal.clone();
    evidence.key.semantic_sha256 = "b".repeat(64);
    cases.push(evidence);
    let mut evidence = f.terminal.clone();
    evidence.occurrence.cutex_session_id = "other".into();
    cases.push(evidence);
    let mut evidence = f.terminal.clone();
    evidence.occurrence.thread_id = "other".into();
    cases.push(evidence);
    let mut evidence = f.terminal.clone();
    evidence.occurrence.runtime_generation = 6;
    cases.push(evidence);
    let mut evidence = f.terminal.clone();
    evidence.occurrence.turn_id = "other".into();
    cases.push(evidence);
    let mut evidence = f.terminal.clone();
    evidence.occurrence.attempt = AttemptIdentity::Delegation {
        ask_id: "ask-1".into(),
        attempt: 1,
    };
    cases.push(evidence);
    let mut evidence = f.terminal.clone();
    evidence.occurrence.attempt = AttemptIdentity::Delegation {
        ask_id: "other".into(),
        attempt: 2,
    };
    cases.push(evidence);
    for evidence in cases {
        let observation = Observations {
            terminal: Some(&evidence),
            ..f.observations()
        };
        let projection = project(&f.subject(), &observation);
        assert_eq!(projection.state, ProjectionState::Unknown, "{evidence:?}");
        assert_eq!(projection.confirmed_level, RevisionLevel::Sent);
        assert!(projection.terminal_evidence.is_none());
    }
}

#[test]
fn changed_or_unknown_runtime_generation_cannot_reuse_a_terminal_projection() {
    let f = Fixture::new();
    for generation in [None, Some(6), Some(8)] {
        let observation = Observations {
            runtime_generation: generation,
            ..f.observations()
        };
        let projection = project(&f.subject(), &observation);
        assert_eq!(projection.state, ProjectionState::Unknown);
        assert_eq!(projection.reason, ProjectionReason::GenerationMismatch);
    }
    let observation = Observations {
        status: StatusRead::Available {
            runtime_generation: 6,
            status: &f.status,
        },
        ..f.observations()
    };
    assert_eq!(
        project(&f.subject(), &observation).state,
        ProjectionState::Unknown
    );
}

#[test]
fn occurrence_must_match_original_owner_thread_and_generation_even_when_event_matches_it() {
    for field in ["owner", "thread", "generation"] {
        let mut f = Fixture::new();
        match field {
            "owner" => f.occurrence.cutex_session_id = "other".into(),
            "thread" => f.occurrence.thread_id = "other".into(),
            _ => f.occurrence.runtime_generation = 6,
        }
        f.terminal.occurrence = f.occurrence.clone();
        assert_eq!(
            project(&f.subject(), &f.observations()).state,
            ProjectionState::Unknown
        );
    }
    let f = Fixture::new();
    let subject = ReceiptSubject {
        occurrence: None,
        ..f.subject()
    };
    assert_eq!(
        project(&subject, &f.observations()).state,
        ProjectionState::Unknown
    );
}

#[test]
fn invalid_occurrence_or_message_key_cannot_become_terminal_proof() {
    let mut f = Fixture::new();
    f.occurrence.attempt = AttemptIdentity::Delegation {
        ask_id: "ask-1".into(),
        attempt: 0,
    };
    f.terminal.occurrence = f.occurrence.clone();
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Unknown
    );
    let mut f = Fixture::new();
    f.key.semantic_sha256 = "bad-digest".into();
    f.terminal.key = f.key.clone();
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Unknown
    );
    assert!(terminal_from_event(
        &f.key,
        &f.occurrence,
        runtime_identity(),
        "turn/completed",
        &json!({
            "threadId": "thread-1", "turn": { "id": "turn-1", "status": "interrupted" }
        }),
        at()
    )
    .is_none());
}

#[test]
fn transient_read_failure_recovers_on_same_generation_without_a_sidecar_write() {
    let f = Fixture::new();
    let failed = Observations {
        status: StatusRead::Unavailable,
        terminal: None,
        ..f.observations()
    };
    assert_eq!(
        project(&f.subject(), &failed).state,
        ProjectionState::Unknown
    );
    let recovered = Observations {
        terminal: None,
        ..f.observations()
    };
    assert_eq!(
        project(&f.subject(), &recovered).state,
        ProjectionState::Waiting
    );
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Uncertain
    );
    // A second failure also cannot erase exact terminal evidence.
    assert_eq!(
        project(
            &f.subject(),
            &Observations {
                terminal: Some(&f.terminal),
                ..failed
            }
        )
        .state,
        ProjectionState::Uncertain
    );
}

#[test]
fn explicitly_unknown_or_conflicting_native_state_stays_unknown_despite_terminal_hint() {
    let mut f = Fixture::new();
    for state in [
        DeliveryState::Unknown,
        DeliveryState::Conflict,
        DeliveryState::RetryableError,
    ] {
        f.status.delivery_state = state;
        let projection = project(&f.subject(), &f.observations());
        assert_eq!(projection.state, ProjectionState::Unknown);
        assert_eq!(projection.confirmed_level, RevisionLevel::Sent);
    }
}

#[test]
fn actual_a4_wins_over_terminal_failure_and_later_runtime_generation() {
    let f = Fixture::new();
    let receipt = f.a4();
    let subject = ReceiptSubject {
        a4: Some(&receipt),
        ..f.subject()
    };
    let observation = Observations {
        runtime_generation: Some(8),
        status: StatusRead::Unavailable,
        ..f.observations()
    };
    let projection = project(&subject, &observation);
    assert_eq!(projection.state, ProjectionState::InContext);
    assert_eq!(projection.confirmed_level, RevisionLevel::InContext);
    assert_eq!(projection.reason, ProjectionReason::ActualA4);
    assert!(projection.terminal_evidence.is_none());
}

#[test]
fn late_validated_a4_read_replaces_uncertainty_without_mutating_the_terminal_record() {
    let mut f = Fixture::new();
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Uncertain
    );
    f.status.delivery_state = DeliveryState::ContextPersisted;
    f.status.receipt = Some(f.a4());
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::InContext
    );
    assert_eq!(f.terminal.outcome, TerminalOutcome::Interrupted);
}

#[test]
fn missing_forged_or_other_turn_a4_cannot_create_success() {
    let mut f = Fixture::new();
    f.status.delivery_state = DeliveryState::ContextPersisted;
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Unknown
    );
    let mut receipt = f.a4();
    receipt.receipt_id = "forged".into();
    let subject = ReceiptSubject {
        a4: Some(&receipt),
        ..f.subject()
    };
    assert_eq!(
        project(&subject, &f.observations()).state,
        ProjectionState::Unknown
    );
    receipt.turn_id = "other-turn".into();
    receipt.receipt_id = receipt.digest_id();
    f.status.receipt = Some(receipt);
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Unknown
    );
}

#[test]
fn durable_ack_or_in_context_progress_is_never_downgraded_by_missing_receipts() {
    let f = Fixture::new();
    for (level, state) in [
        (RevisionLevel::InContext, ProjectionState::InContext),
        (RevisionLevel::Acknowledged, ProjectionState::Acknowledged),
    ] {
        let subject = ReceiptSubject {
            confirmed_level: level,
            ..f.subject()
        };
        let observation = Observations {
            runtime_generation: None,
            status: StatusRead::Unavailable,
            ..f.observations()
        };
        let projection = project(&subject, &observation);
        assert_eq!(projection.state, state);
        assert_eq!(projection.confirmed_level, level);
    }
}

#[test]
fn native_attempt_ids_are_checked_exactly_and_never_equal_delegation_attempt_numbers() {
    let mut f = Fixture::new();
    let attempt = "8c3ab77a-20a2-44c7-a2d4-4a361ec5326c";
    f.occurrence.attempt = AttemptIdentity::Native {
        attempt_id: attempt.into(),
    };
    f.terminal.occurrence = f.occurrence.clone();
    f.status.processing = ProcessingStatus {
        state: ProcessingState::Held,
        attempt_id: Some(attempt.into()),
        reason: Some(HoldReason::Interrupted),
    };
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Uncertain
    );
    f.status.processing.attempt_id = Some("ce014b6f-b52b-44ac-a33b-2ae218eb5082".into());
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Unknown
    );
    f.status.processing = ProcessingStatus {
        state: ProcessingState::Pending,
        attempt_id: None,
        reason: None,
    };
    assert_eq!(
        project(&f.subject(), &f.observations()).state,
        ProjectionState::Unknown
    );
}

#[test]
fn mismatched_status_message_or_digest_does_not_borrow_another_receipts_evidence() {
    for field in ["message", "digest"] {
        let mut f = Fixture::new();
        if field == "message" {
            f.status.message_id = "other".into();
        } else {
            f.status.semantic_sha256 = "b".repeat(64);
        }
        assert_eq!(
            project(&f.subject(), &f.observations()).state,
            ProjectionState::Unknown
        );
    }
}

#[test]
fn event_capture_requires_exact_method_thread_turn_and_explicit_terminal_status() {
    let f = Fixture::new();
    for (method, params) in [
        (
            "turn/started",
            json!({"threadId":"thread-1","turn":{"id":"turn-1","status":"interrupted"}}),
        ),
        (
            "turn/completed",
            json!({"threadId":"other","turn":{"id":"turn-1","status":"interrupted"}}),
        ),
        (
            "turn/completed",
            json!({"threadId":"thread-1","turn":{"id":"other","status":"interrupted"}}),
        ),
        (
            "turn/completed",
            json!({"turn":{"id":"turn-1","status":"interrupted"}}),
        ),
        (
            "turn/completed",
            json!({"threadId":"thread-1","turn":{"id":"turn-1"}}),
        ),
        (
            "turn/completed",
            json!({"threadId":"thread-1","turn":{"id":"turn-1","status":"inProgress"}}),
        ),
        (
            "turn/completed",
            json!({"threadId":"thread-1","turn":{"id":"turn-1","status":"future"}}),
        ),
    ] {
        assert!(terminal_from_event(
            &f.key,
            &f.occurrence,
            runtime_identity(),
            method,
            &params,
            at()
        )
        .is_none());
    }
    for status in ["completed", "failed", "interrupted"] {
        let evidence = terminal_from_event(
            &f.key,
            &f.occurrence,
            runtime_identity(),
            "turn/completed",
            &json!({
                "threadId":"thread-1","turn":{"id":"turn-1","status":status}
            }),
            at(),
        )
        .unwrap();
        let observation = Observations {
            terminal: Some(&evidence),
            ..f.observations()
        };
        assert_eq!(
            project(&f.subject(), &observation).state,
            ProjectionState::Uncertain
        );
    }
}

#[test]
fn canonical_attempt_cross_check_fences_generation_turn_ask_and_attempt_and_rejects_refusal() {
    let f = Fixture::new();
    let mut record = AttemptRecord::submitting(2, "native-brief".into(), 7, at().to_rfc3339());
    record.turn_id = Some("turn-1".into());
    for state in [
        AttemptState::Running,
        AttemptState::Answered,
        AttemptState::Failed,
        AttemptState::Interrupted,
        AttemptState::Yielded,
    ] {
        record.state = state;
        assert!(terminal_matches_attempt(&f.terminal, "ask-1", &record));
    }
    record.state = AttemptState::Refused;
    assert!(!terminal_matches_attempt(&f.terminal, "ask-1", &record));
    record.state = AttemptState::Interrupted;
    for field in ["generation", "turn", "attempt", "ask"] {
        let mut stale = record.clone();
        match field {
            "generation" => stale.runtime_generation = 6,
            "turn" => stale.turn_id = Some("other".into()),
            "attempt" => stale.n = 1,
            _ => {}
        }
        assert!(!terminal_matches_attempt(
            &f.terminal,
            if field == "ask" { "other" } else { "ask-1" },
            &stale
        ));
    }
}

#[test]
fn interrupted_durable_attempt_without_actual_terminal_evidence_stays_waiting() {
    let f = Fixture::new();
    let mut record = AttemptRecord::submitting(2, "native-brief".into(), 7, at().to_rfc3339());
    record.turn_id = Some("turn-1".into());
    record.state = AttemptState::Interrupted;
    record.interrupt_reason = Some(super::super::model::InterruptReason::OutcomeUnknown);
    record.ended_at = Some(at().to_rfc3339());
    let observation = Observations {
        terminal: None,
        ..f.observations()
    };
    assert_eq!(
        project(&f.subject(), &observation).state,
        ProjectionState::Waiting
    );
    // The durable label is deliberately not an input to project().
    assert!(record.state.is_terminal());
}

#[test]
fn exact_turn_read_is_scoped_and_requires_an_explicit_terminal_status() {
    let f = Fixture::new();
    let turn = json!({ "id": "turn-1", "status": "interrupted" });
    let terminal = terminal_from_exact_turn_read(
        &f.key,
        &f.occurrence,
        runtime_identity(),
        "thread-1",
        &turn,
        at(),
    )
    .unwrap();
    assert_eq!(terminal.source, TerminalSource::ExactTurnRead);
    assert!(terminal_from_exact_turn_read(
        &f.key,
        &f.occurrence,
        runtime_identity(),
        "other-thread",
        &turn,
        at()
    )
    .is_none());
    assert!(terminal_from_exact_turn_read(
        &f.key,
        &f.occurrence,
        runtime_identity(),
        "thread-1",
        &json!({ "id": "turn-1", "status": "inProgress" }),
        at()
    )
    .is_none());
    assert!(terminal_from_exact_turn_read(
        &f.key,
        &f.occurrence,
        runtime_identity(),
        "thread-1",
        &json!({ "id": "turn-2", "status": "interrupted" }),
        at()
    )
    .is_none());
}

#[test]
fn capture_rejects_a_stale_authenticated_generation_even_when_turn_and_thread_ids_repeat() {
    let f = Fixture::new();
    let params =
        json!({ "threadId": "thread-1", "turn": { "id": "turn-1", "status": "interrupted" } });
    for runtime in [
        RuntimeIdentity {
            cutex_session_id: "target",
            runtime_generation: 6,
        },
        RuntimeIdentity {
            cutex_session_id: "other-target",
            runtime_generation: 7,
        },
    ] {
        assert!(terminal_from_event(
            &f.key,
            &f.occurrence,
            runtime,
            "turn/completed",
            &params,
            at()
        )
        .is_none());
        assert!(terminal_from_exact_turn_read(
            &f.key,
            &f.occurrence,
            runtime,
            "thread-1",
            &params["turn"],
            at()
        )
        .is_none());
    }
}

#[test]
fn revision_adapter_preserves_ack_and_rejects_a_different_message_link() {
    let f = Fixture::new();
    let mut revision = RevisionRecord {
        id: "v2".into(),
        text: "new brief".into(),
        supersedes: None,
        message_id: Some(f.key.message_id.clone()),
        wake: false,
        sent_at: at().to_rfc3339(),
        level: RevisionLevel::Acknowledged,
        delivered_at: None,
        in_context_at: None,
        turn_id: None,
        acknowledged_at: Some(at().to_rfc3339()),
        ack_text: Some("applied".into()),
    };
    assert_eq!(
        project_revision(&revision, &f.subject(), &f.observations())
            .unwrap()
            .state,
        ProjectionState::Acknowledged
    );
    revision.level = RevisionLevel::Sent;
    let before = revision.clone();
    let projection = project_revision(&revision, &f.subject(), &f.observations()).unwrap();
    assert_eq!(projection.state, ProjectionState::Uncertain);
    assert_eq!(projection.confirmed_level, RevisionLevel::Sent);
    assert_eq!(revision, before);
    revision.message_id = Some("other".into());
    assert!(project_revision(&revision, &f.subject(), &f.observations()).is_none());
}

#[test]
fn pure_projection_is_repeatable_and_leaves_all_original_receipts_and_evidence_byte_identical() {
    let f = Fixture::new();
    let before =
        serde_json::to_vec(&(&f.key, &f.binding, &f.occurrence, &f.status, &f.terminal)).unwrap();
    let expected = project(&f.subject(), &f.observations());
    for _ in 0..100 {
        assert_eq!(project(&f.subject(), &f.observations()), expected);
    }
    assert_eq!(
        serde_json::to_vec(&(&f.key, &f.binding, &f.occurrence, &f.status, &f.terminal)).unwrap(),
        before
    );
    let serialized = serde_json::to_value(expected).unwrap();
    assert_eq!(serialized["state"], "uncertain");
    assert_eq!(serialized["confirmedLevel"], "sent");
    assert_eq!(
        serialized["terminalEvidence"]["occurrence"]["runtimeGeneration"],
        7
    );
    assert!(serialized.get("a4Receipt").is_none());
}
