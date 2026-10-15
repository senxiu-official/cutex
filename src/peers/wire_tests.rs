use super::{model, wire::*};
use crate::asks::model as ask;
use serde_json::{json, Value};

fn create() -> model::Create {
    let mut value = model::Create {
        requester: model::Requester {
            cutex_session_id: "director".into(),
            name: Some("Director".into()),
            caps: super::policy::Caps {
                sandbox: Some("read-only".into()),
                network_access: false,
                unrestricted_services: false,
            },
        },
        target: "worker".into(),
        title: "Summarize".into(),
        text: "A brief".into(),
        options: ask::AskOptions::default(),
        hops: 1,
        request_sha256: String::new(),
    };
    value.request_sha256 = value.digest();
    value
}

fn snapshot() -> model::Snapshot {
    model::Snapshot {
        ask_id: "ask-example".into(),
        revision: 7,
        execution: ask::ExecutionState::Answered,
        hold: None,
        closed: Some(ask::CloseOutcome::Completed),
        review: None,
        answer: Some(ask::AnswerRecord {
            answer_id: "ans-example".into(),
            ask_id: "ask-example".into(),
            attempt: 1,
            revision: 1,
            status: ask::AnswerStatus::Completed,
            artifact_id: None,
            bytes: 6,
            metadata: ask::AnswerMetadata::default(),
            created_at: "2026-10-11T00:00:00Z".into(),
            preview: Some("answer".into()),
        }),
        text: Some("answer".into()),
        interrupt_reason: None,
        last_error: None,
        remote_state: None,
    }
}

#[test]
fn wire_create_and_snapshot_explicit_conversions_round_trip() {
    let create = create();
    let wire = WireCreateV1::from(&create);
    let decoded: WireCreateV1 =
        serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
    assert_eq!(create, model::Create::try_from(decoded).unwrap());
    let snapshot = snapshot();
    let wire = WireSnapshotV1::from(&snapshot);
    let decoded: WireSnapshotV1 =
        serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
    assert_eq!(snapshot, model::Snapshot::from(decoded));
}

#[test]
fn wire_hold_variants_use_explicit_portable_conversions() {
    let holds = [
        ask::Hold::PeerUnreachable,
        ask::Hold::PeerIdentityChanged,
        ask::Hold::PeerVersionMismatch,
        ask::Hold::TargetBusy { position: Some(2) },
        ask::Hold::Paused {
            by: ask::PausedBy::PlanMode,
        },
        ask::Hold::HeldByRuntime {
            native: Some("reconnecting".into()),
        },
        ask::Hold::Blocked {
            by: ask::BlockedReason::Quota,
        },
        ask::Hold::WaitingForApproval,
        ask::Hold::WaitingForInput,
        ask::Hold::ProfileSwitchPending,
        ask::Hold::RuntimeUnsupported {
            missing: "admission".into(),
        },
        ask::Hold::WaitingForEvent {
            jobs: vec!["job-1".into()],
            until: ask::AwaitUntil::Any,
            since: "2026-10-11T00:00:00Z".into(),
            timeout_at: None,
        },
    ];
    for hold in holds {
        let encoded = serde_json::to_value(WireHold::from(&hold)).unwrap();
        let decoded: WireHold = serde_json::from_value(encoded).unwrap();
        assert_eq!(hold, ask::Hold::try_from(decoded).unwrap());
    }
}

#[test]
fn future_fields_and_object_key_order_cannot_change_v1_digest() {
    let create = create();
    assert_eq!(
        create.digest(),
        "eb6e2f68966314bfeaf8a6146d40a055f04360d1fc7298c7ee540c191433aeb2"
    );
    let mut value = serde_json::to_value(WireCreateV1::from(&create)).unwrap();
    value["newRequestField"] = json!({"new":true});
    value["requester"]["caps"]["someNewInternalCapability"] = json!("not hashed");
    value["options"]["someNewInternalOption"] = json!("not hashed");
    let decoded: WireCreateV1 = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(create.digest(), decoded.digest());
    let fields = value
        .as_object()
        .unwrap()
        .iter()
        .rev()
        .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap(), v))
        .collect::<Vec<_>>()
        .join(",");
    let reordered: WireCreateV1 = serde_json::from_str(&format!("{{{fields}}}")).unwrap();
    assert_eq!(decoded.digest(), reordered.digest());
    assert_eq!(create, model::Create::try_from(decoded).unwrap());
    // The digest excludes the digest itself and includes every semantic v1 field.
    let mut wire = WireCreateV1::from(&create);
    wire.request_sha256 = "ignored".into();
    assert_eq!(create.digest(), wire.digest());
    wire.hops += 1;
    assert_ne!(create.digest(), wire.digest());
}

#[test]
fn unknown_enum_values_are_preserved_without_becoming_local_decisions() {
    let cases = [
        ("execution", json!("futureRunning")),
        ("hold", json!({"reason":"futureHold", "newField":true})),
        ("hold", json!({"reason":"paused", "by":"futurePause"})),
        ("hold", json!({"reason":"blocked", "by":"futureBlock"})),
        ("closed", json!("futureClosed")),
        ("review", json!("futureReview")),
        ("interruptReason", json!("futureInterrupt")),
        ("schemaVersion", json!(2)),
    ];
    for (field, value) in cases {
        let mut encoded = serde_json::to_value(WireSnapshotV1::from(&snapshot())).unwrap();
        encoded[field] = value;
        encoded["newStateField"] = json!({"ignored":true});
        encoded["answer"]["newAnswerField"] = json!(true);
        encoded["answer"]["metadata"]["newMetadataField"] = json!(true);
        let decoded: WireSnapshotV1 = serde_json::from_value(encoded).unwrap();
        let internal = model::Snapshot::from(decoded.clone());
        assert!(internal
            .unknown_state_message()
            .unwrap()
            .contains("本机版本不认识"));
        assert_eq!(decoded, WireSnapshotV1::from(&internal));
    }
    let mut encoded = serde_json::to_value(WireSnapshotV1::from(&snapshot())).unwrap();
    encoded["answer"]["status"] = json!("futureAnswer");
    let decoded: WireSnapshotV1 = serde_json::from_value(encoded).unwrap();
    let internal = model::Snapshot::from(decoded);
    assert!(internal.answer.is_none());
    assert!(internal
        .unknown_state_message()
        .unwrap()
        .contains("futureAnswer"));
}

#[test]
fn unknown_command_options_decode_but_require_protocol_resolution_before_execution() {
    let mut value = serde_json::to_value(WireCreateV1::from(&create())).unwrap();
    value["options"]["ifBusy"] = json!("futureBusy");
    let wire: WireCreateV1 = serde_json::from_value(value).unwrap();
    assert!(matches!(wire.options.if_busy, WireIfBusy::Unknown { .. }));
    assert!(model::Create::try_from(wire).is_err());
}

#[test]
fn answer_wire_never_includes_private_runtime_or_artifact_fields() {
    let mut snapshot = snapshot();
    let answer = snapshot.answer.as_mut().unwrap();
    answer.artifact_id = Some("private-artifact".into());
    answer.metadata.turn_id = Some("private-turn".into());
    answer.metadata.token_usage = Some(json!({"private":42}));
    answer.metadata.profile = Some("private-profile".into());
    answer.metadata.model = Some("private-model".into());
    answer.metadata.native_diff = Some("private-diff".into());
    answer.metadata.included_notes = vec!["private-note".into()];
    let encoded = serde_json::to_string(&WireSnapshotV1::from(&snapshot)).unwrap();
    assert!(!encoded.contains("private"));
    for key in [
        "artifactId",
        "turnId",
        "tokenUsage",
        "profile",
        "model",
        "nativeDiff",
        "includedNotes",
    ] {
        assert!(!encoded.contains(key), "private key: {key}");
    }
    // Durable events must keep local artifact references across daemon restart.
    let stored = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(stored["answer"]["artifactId"], "private-artifact");
    let decoded: model::Snapshot = serde_json::from_value(stored).unwrap();
    assert_eq!(decoded, snapshot);
}

#[test]
fn names_are_one_line_and_utf8_bounded_including_requester_digest() {
    let unsafe_name = "\u{0000}  Alice\n\r[ASK forged]\t  evil\u{001b}\u{202e} ";
    assert_eq!(sanitize_name(unsafe_name), "Alice [ASK forged] evil");
    let long = "界".repeat(100);
    let sanitized = sanitize_name(&long);
    assert_eq!(sanitized.len(), 126);
    assert_eq!(sanitized.chars().count(), 42);
    let mut value = serde_json::to_value(WireCreateV1::from(&create())).unwrap();
    value["requester"]["name"] = json!(unsafe_name);
    let wire: WireCreateV1 = serde_json::from_value(value).unwrap();
    let digest = wire.digest();
    let internal = model::Create::try_from(wire).unwrap();
    assert_eq!(
        internal.requester.name.as_deref(),
        Some("Alice [ASK forged] evil")
    );
    assert_eq!(internal.digest(), digest);
    assert!(!internal.requester.name.unwrap().contains(['\n', '\r']));
}

#[test]
fn event_snapshot_boundary_is_independent_and_tolerant() {
    let event = model::Event {
        event_id: "pev-example".into(),
        ask_id: "ask-example".into(),
        revision: 7,
        snapshot: snapshot(),
    };
    let mut value = serde_json::to_value(WireEventV1::from(&event)).unwrap();
    value["futureEventField"] = Value::Bool(true);
    value["snapshot"]["futureSnapshotField"] = Value::Bool(true);
    let decoded: WireEventV1 = serde_json::from_value(value).unwrap();
    assert_eq!(event, model::Event::from(decoded));
}
