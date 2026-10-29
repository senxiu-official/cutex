use super::*;
use crate::app_server::external_input::{Envelope, HoldReason, Message, ProcessingStatus, Source};
use crate::asks::{
    model::RevisionLevel,
    store::{tests as fixtures, AskState, AskStore},
};
use serde_json::json;

pub(crate) fn input() -> ReceiptReadInput {
    let mut envelope = Envelope {
        version: 1,
        owner_id: fixtures::TARGET.into(),
        thread_id: "thread".into(),
        runtime_generation: 7,
        message: Message {
            id: "tell".into(),
            source: Source {
                kind: SourceKind::Agent,
                id: fixtures::REQUESTER.into(),
            },
            event_type: "agent_message".into(),
            delivery: Delivery::AfterTurn,
            text: "synthetic revision".into(),
        },
        view: None,
        semantic_sha256: String::new(),
    };
    envelope.semantic_sha256 = envelope.digest();
    let attempt = claude_attempt_id("thread", "actual");
    let status = Status {
        message_id: "tell".into(),
        semantic_sha256: envelope.semantic_sha256.clone(),
        delivery_state: DeliveryState::Pending,
        receipt: None,
        processing: ProcessingStatus {
            state: ProcessingState::Held,
            attempt_id: Some(attempt.clone()),
            reason: Some(HoldReason::RequestUncertain),
        },
    };
    let evidence = Evidence {
        schema_version: 1,
        runtime: "claude".into(),
        recipient_id: fixtures::TARGET.into(),
        runtime_generation: 7,
        observing_generation: Some(9),
        thread_id: "thread".into(),
        message_id: "tell".into(),
        semantic_sha256: envelope.semantic_sha256.clone(),
        local_semantic_sha256: "b".repeat(64),
        attempt_id: attempt,
        processing_state: "held".into(),
        actual_turn_id: "actual".into(),
        terminal_status: "interrupted".into(),
        observed_at: "2026-10-27T08:00:00Z".parse().unwrap(),
        observation_sha256: "a".repeat(64),
    };
    let snapshot=serde_json::from_value(json!({"messageId":"tell","fromCutexSessionId":fixtures::REQUESTER,
        "toCutexSessionId":fixtures::TARGET,"deliveryMode":"after_turn","content":"synthetic revision",
        "state":"inbox_accepted","semanticSha256":envelope.semantic_sha256,"externalInput":envelope,
        "externalInputLastObserved":status,"localInboxReceipt":{"threadId":"thread","messageId":"tell",
            "semanticSha256":"b".repeat(64),"durable":true,"receiptScope":"local-inbox","turnId":"reserved"}})).unwrap();
    ReceiptReadInput {
        snapshot,
        terminal: Some(evidence),
        observer: Some(CurrentObserver {
            cutex_session_id: fixtures::TARGET.into(),
            thread_id: "thread".into(),
            runtime_generation: 9,
        }),
        status_unavailable: false,
    }
}

fn revision(level: RevisionLevel) -> RevisionRecord {
    serde_json::from_value(
        json!({"id":"v2","text":"synthetic revision","messageId":"tell",
        "sentAt":"2026-10-27T07:00:00Z","level":level,"ackText":"retained detail"}),
    )
    .unwrap()
}

fn context(input: ReceiptReadInput) -> ReceiptReadContext {
    BTreeMap::from([("tell".into(), input)])
}

#[test]
fn native_attempt_correlation_matches_the_pinned_adapter_uuid5_contract() {
    assert_eq!(
        claude_attempt_id("thread", "actual"),
        "8a871a8e-7d89-50f9-a7fe-9381ec8e57e8"
    );
    assert_ne!(
        claude_attempt_id("thread", "reserved"),
        claude_attempt_id("thread", "actual")
    );
}

#[test]
fn output_observed_and_all_exact_terminal_outcomes_remain_uncertain_not_delivered() {
    let mut input = input();
    input.terminal.as_mut().unwrap().processing_state = "output_observed".into();
    input
        .snapshot
        .external_input_last_observed
        .as_mut()
        .unwrap()
        .processing
        .state = ProcessingState::OutputObserved;
    input
        .snapshot
        .external_input_last_observed
        .as_mut()
        .unwrap()
        .processing
        .reason = None;
    for outcome in ["interrupted", "failed", "completed"] {
        input.terminal.as_mut().unwrap().terminal_status = outcome.into();
        let view = project_bus_read(&input, RevisionLevel::Sent);
        assert_eq!(view["state"], "uncertain");
        assert_eq!(view["confirmedLevel"], "sent");
        assert!(input.snapshot.external_input_receipt.is_none());
    }
}

fn a4(input: &ReceiptReadInput) -> Receipt {
    let envelope = input.snapshot.external_input.as_ref().unwrap();
    let mut receipt = Receipt {
        schema: "codex.external-input-receipt.v1".into(),
        receipt_id: String::new(),
        owner_id: envelope.owner_id.clone(),
        thread_id: envelope.thread_id.clone(),
        message_id: "tell".into(),
        response_item_id: "tell".into(),
        semantic_sha256: envelope.semantic_sha256.clone(),
        turn_id: "actual".into(),
        ordinal: 1,
    };
    receipt.receipt_id = receipt.digest_id();
    receipt
}

#[test]
fn exact_historical_input_is_uncertain_without_rewriting_submission_or_observer() {
    let input = input();
    let before = serde_json::to_vec(&(&input.snapshot, &input.terminal)).unwrap();
    let view = project_bus_read(&input, RevisionLevel::Sent);
    assert_eq!(view["state"], "uncertain");
    assert_eq!(view["confirmedLevel"], "sent");
    assert_eq!(view["submissionGeneration"], 7);
    assert_eq!(view["observingGeneration"], 9);
    assert_eq!(
        view["terminalEvidence"]["occurrence"]["runtimeGeneration"],
        7
    );
    assert_eq!(view["terminalEvidence"]["occurrence"]["turnId"], "actual");
    assert_eq!(view["terminalEvidence"]["source"], "exactTurnRead");
    assert!(view.get("turnId").is_none());
    assert_eq!(
        serde_json::to_vec(&(&input.snapshot, &input.terminal)).unwrap(),
        before
    );
}

#[test]
fn legacy_same_generation_observer_is_supported_without_ttl_heuristics() {
    let mut input = input();
    input.terminal.as_mut().unwrap().observing_generation = None;
    input.observer.as_mut().unwrap().runtime_generation = 7;
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Delivered)["state"],
        "uncertain"
    );
}

#[test]
fn bus_and_native_envelope_digests_have_distinct_domains() {
    let mut input = input();
    let native_digest = input
        .snapshot
        .external_input
        .as_ref()
        .unwrap()
        .semantic_sha256
        .clone();
    input.snapshot.semantic_sha256 = Some("c".repeat(64));
    assert_ne!(
        input.snapshot.semantic_sha256.as_ref().unwrap(),
        &native_digest
    );
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Sent)["state"],
        "uncertain"
    );
    input.snapshot.external_input_receipt = Some(a4(&input));
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Sent)["state"],
        "inContext"
    );
}

#[test]
fn mismatched_provenance_never_borrows_current_turn_or_generation() {
    let mut cases = Vec::new();
    macro_rules! changed {
        ($body:expr) => {{
            let mut v = input();
            $body(&mut v);
            cases.push(v);
        }};
    }
    changed!(|v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().runtime_generation = 6);
    changed!(|v: &mut ReceiptReadInput| v.observer.as_mut().unwrap().runtime_generation = 10);
    changed!(
        |v: &mut ReceiptReadInput| v.observer.as_mut().unwrap().thread_id = "different".into()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.observer.as_mut().unwrap().cutex_session_id =
            "different".into()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().message_id = "different".into()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().semantic_sha256 = "c".repeat(64)
    );
    changed!(
        |v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().local_semantic_sha256 =
            "c".repeat(64)
    );
    changed!(
        |v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().attempt_id =
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().terminal_status = "running".into()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().actual_turn_id = String::new()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.terminal.as_mut().unwrap().actual_turn_id =
            "different-turn".into()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.snapshot.external_input.as_mut().unwrap().message.text =
            "changed".into()
    );
    changed!(
        |v: &mut ReceiptReadInput| v.snapshot.local_inbox_receipt.as_mut().unwrap()["durable"] =
            json!(false)
    );
    changed!(|v: &mut ReceiptReadInput| v.observer = None);
    changed!(|v: &mut ReceiptReadInput| v.terminal = None);
    for case in cases {
        assert_eq!(
            project_bus_read(&case, RevisionLevel::Sent)["state"],
            "unknown"
        );
    }
}

#[test]
fn native_unknown_stays_unknown_and_failed_read_recovers_with_retained_evidence() {
    let mut input = input();
    input.status_unavailable = true;
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Sent)["state"],
        "uncertain"
    );
    input.status_unavailable = false;
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Sent)["state"],
        "uncertain"
    );
    input
        .snapshot
        .external_input_last_observed
        .as_mut()
        .unwrap()
        .delivery_state = DeliveryState::Unknown;
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Sent)["state"],
        "unknown"
    );
    input.status_unavailable = true;
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Sent)["state"],
        "unknown"
    );
}

#[test]
fn original_a4_and_explicit_ack_win_over_stale_or_unavailable_observation() {
    let mut input = input();
    let receipt = a4(&input);
    input.snapshot.external_input_receipt = Some(receipt.clone());
    input.observer = None;
    input.terminal = None;
    input.status_unavailable = true;
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Sent)["state"],
        "inContext"
    );
    assert_eq!(input.snapshot.external_input_receipt, Some(receipt));
    assert_eq!(
        project_bus_read(&input, RevisionLevel::Acknowledged)["state"],
        "acknowledged"
    );
}

#[test]
fn production_view_entry_points_keep_full_original_revision_fields() {
    let mut state = AskState::default();
    let now = chrono::Utc::now();
    let (ask, _) = state
        .create_ask(fixtures::new_ask("read-projection", "synthetic"), now)
        .unwrap();
    let mut ask = ask;
    ask.revisions.push(revision(RevisionLevel::Delivered));
    state.asks.insert(ask.ask_id.clone(), ask.clone());
    let context = context(input());
    let before = serde_json::to_vec(&state).unwrap();
    let summary = crate::asks::view::summary_with_receipts(&state, &ask, &context);
    let owner = crate::asks::view::owner_view_with_receipts(&state, &ask, &context);
    let requester = crate::asks::view::requester_view_with_receipts(&state, &ask, &context);
    for value in [&summary, &owner, &requester] {
        assert_eq!(
            value["revisions"][0]["receiptProjection"]["state"],
            "uncertain"
        );
        assert_eq!(value["revisions"][0]["level"], "delivered");
    }
    let mut full = owner["revisions"][0].clone();
    full.as_object_mut().unwrap().remove("receiptProjection");
    assert_eq!(full, serde_json::to_value(&ask.revisions[0]).unwrap());
    assert_eq!(full["text"], "synthetic revision");
    assert_eq!(full["ackText"], "retained detail");
    assert_eq!(serde_json::to_vec(&state).unwrap(), before);
}

#[test]
fn context_link_is_exact_and_ack_does_not_depend_on_a_bus_lookup() {
    let mut context = context(input());
    context.get_mut("tell").unwrap().snapshot.message_id = "unrelated".into();
    assert_eq!(
        revision_projection(&revision(RevisionLevel::Delivered), &context)["state"],
        "unknown"
    );
    assert_eq!(
        revision_projection(&revision(RevisionLevel::Acknowledged), &BTreeMap::new())["state"],
        "acknowledged"
    );
}

#[test]
fn response_decorator_runs_while_store_lock_is_busy_without_writes() {
    let (store, _) = fixtures::temp_store("read-projection-locked");
    let id = store
        .with_state(|state| {
            let (ask, _) = state
                .create_ask(
                    fixtures::new_ask("read-locked", "synthetic"),
                    chrono::Utc::now(),
                )
                .unwrap();
            state
                .asks
                .get_mut(&ask.ask_id)
                .unwrap()
                .revisions
                .push(revision(RevisionLevel::Sent));
            ask.ask_id
        })
        .unwrap();
    let state = store.snapshot().unwrap();
    let ask = state.asks[&id].clone();
    let original = serde_json::to_value(&ask).unwrap();
    let context = context(input());
    let generation = crate::management::v2::hot_store::generation(
        &store
            .dir()
            .join(crate::management::v2::hot_store::DATABASE_FILE),
        "asks",
    )
    .unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.dir().join("asks-v1.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&file).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(crate::asks::view::with_receipt_projections(
            original, &ask, &context,
        ))
        .unwrap()
    });
    let response = rx.recv_timeout(std::time::Duration::from_secs(2));
    drop(file);
    worker.join().unwrap();
    assert_eq!(
        response.unwrap()["revisions"][0]["receiptProjection"]["state"],
        "uncertain"
    );
    assert_eq!(
        crate::management::v2::hot_store::generation(
            &store
                .dir()
                .join(crate::management::v2::hot_store::DATABASE_FILE),
            "asks"
        )
        .unwrap(),
        generation
    );
    assert_eq!(
        serde_json::to_value(store.snapshot().unwrap()).unwrap(),
        serde_json::to_value(state).unwrap()
    );
}
