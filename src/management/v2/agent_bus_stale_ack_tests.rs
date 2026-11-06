// Included in settlement_state::tests to reuse the isolated repository fixture.
fn stale_fixture(f: &Fixture, id: &str, state: ProcessingState, native_attempt: bool) -> Evidence {
    let mut evidence = f.seed(id, "failed", false);
    f.repo
        .mutate_message(id, |store| {
            let status = store
                .messages
                .get_mut(id)
                .unwrap()
                .snapshot
                .external_input_last_observed
                .as_mut()
                .unwrap();
            status.processing.state = state;
            status.processing.reason = None;
            if !native_attempt {
                status.processing.attempt_id = None;
            }
            Ok(())
        })
        .unwrap();
    evidence.observed_at = f.now + chrono::Duration::seconds(300);
    evidence.processing_state = match f
        .stored(id)
        .snapshot
        .external_input_last_observed
        .unwrap()
        .processing
        .state
    {
        ProcessingState::Pending => "pending",
        ProcessingState::Claimed => "claimed",
        _ => "held",
    }
    .into();
    let mut proof = f
        .repo
        .stale_ack_candidate(id, evidence.observed_at)
        .unwrap()
        .unwrap();
    proof.later_completed_turn_id = Some("later-completed".into());
    evidence.stale_ack = Some(proof);
    evidence.terminal_status = "inProgress".into();
    evidence
}

fn make_claude_task(f: &Fixture, id: &str, evidence: &mut Evidence) {
    use crate::agent_bus::claude_task as task;
    let notice = serde_json::from_value(json!({
        "noticeId":"notice", "taskId":"background", "taskType":"local_agent", "status":"completed",
        "description":"review", "summary":"done"
    }))
    .unwrap();
    let request = task::send_request(TO, &notice).unwrap();
    f.repo
        .mutate_message(id, |store| {
            store.version = store.version.max(6);
            let stored = store.messages.get_mut(id).unwrap();
            let message = stored.canonical_envelope.as_mut().unwrap();
            message.from = task::SOURCE_ID.into();
            message.from_cutex_session_id = None;
            message.control_type = request.control_type.clone();
            message.control_payload = request.control_payload.clone();
            message.external_message_id = request.external_message_id.clone();
            message.content = request.content.clone();
            stored.snapshot.content = message.content.clone();
            stored.snapshot.from_cutex_session_id = None;
            stored.from_runtime_agent_id = None;
            let (_, text) = task::model_text(message)?;
            let view = task::view(message, &notice)?;
            let envelope = stored.snapshot.external_input.as_mut().unwrap();
            envelope.version = 2;
            envelope.message.source = Source {
                kind: SourceKind::Service,
                id: task::SOURCE_ID.into(),
            };
            envelope.message.event_type = task::EVENT_TYPE.into();
            envelope.message.text = text;
            envelope.view = Some(view);
            envelope.semantic_sha256 = envelope.digest();
            evidence.semantic_sha256 = envelope.semantic_sha256.clone();
            stored
                .snapshot
                .external_input_last_observed
                .as_mut()
                .unwrap()
                .semantic_sha256 = envelope.semantic_sha256.clone();
            Ok(())
        })
        .unwrap();
}

// Usage-limit and server-error entry turns both leave this pending/held,
// request_uncertain state. The canonical notice stays after_turn even when the
// native ingress froze Soon; neither representation may be rewritten on close.
fn failed_claude_task_delivery_fixture(f: &Fixture, id: &str, delivery: Delivery) -> Evidence {
    use sha2::{Digest, Sha256};

    let mut evidence = f.seed(id, "failed", false);
    make_claude_task(f, id, &mut evidence);
    f.repo
        .mutate_message(id, |store| {
            let stored = store.messages.get_mut(id).unwrap();
            let envelope = stored.snapshot.external_input.as_mut().unwrap();
            envelope.message.delivery = delivery.clone();
            envelope.semantic_sha256 = envelope.digest();
            evidence.semantic_sha256 = envelope.semantic_sha256.clone();
            let mut metadata = json!({"nativeExternalInput": true});
            if delivery == Delivery::Soon {
                metadata["deliveryMode"] = json!("soon");
            }
            let payload = json!({
                "source": envelope.message.source,
                "eventType": envelope.message.event_type,
                "text": envelope.message.text,
                "metadata": metadata,
                "view": envelope.view,
            });
            evidence.local_semantic_sha256 =
                format!("{:x}", Sha256::digest(serde_json::to_vec(&payload)?));
            let native = stored
                .snapshot
                .external_input_last_observed
                .as_mut()
                .unwrap();
            native.semantic_sha256 = envelope.semantic_sha256.clone();
            // Keep seed's Held + RequestUncertain, rather than replacing it
            // with Claimed or treating a failed request as a context receipt.
            let local = stored.snapshot.local_inbox_receipt.as_mut().unwrap();
            local["semanticSha256"] = json!(evidence.local_semantic_sha256);
            local["state"] = json!("failed");
            local["turnId"] = json!(evidence.actual_turn_id);
            local["contextTurnId"] = Value::Null;
            local["contextPersisted"] = json!(false);
            Ok(())
        })
        .unwrap();
    evidence.observed_at = f.now + chrono::Duration::seconds(300);
    // Build independently of stale_candidate so the pre-fix failure names the
    // Claude Task provenance/delivery gate, not fixture construction.
    evidence.stale_ack = Some(StaleAckEvidence {
        queued_at: f.now,
        accepted_at: f.now,
        min_wait_secs: 300,
        later_completed_turn_id: Some("later-completed".into()),
        native_attempt_id: Some(evidence.attempt_id.clone()),
    });
    evidence
}

#[test]
fn stale_failed_claude_task_soon_and_after_turn_accept_later_completed_proof_without_rewriting() {
    for delivery in [Delivery::Soon, Delivery::AfterTurn] {
        let f = Fixture::new();
        let id = "failed-task-delivery";
        let evidence = failed_claude_task_delivery_fixture(&f, id, delivery.clone());
        let original = f.stored(id);
        let envelope = original.snapshot.external_input.as_ref().unwrap();
        assert_eq!(envelope.message.delivery, delivery);
        assert_eq!(original.snapshot.delivery_mode, "after_turn");
        assert_eq!(
            original.canonical_envelope.as_ref().unwrap().delivery_mode,
            crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
        );
        assert_eq!(
            envelope.view.as_ref().unwrap().data["deliveryMode"],
            "after_turn"
        );
        assert_eq!(envelope.semantic_sha256, envelope.digest());
        let native = original
            .snapshot
            .external_input_last_observed
            .as_ref()
            .unwrap();
        assert_eq!(native.semantic_sha256, envelope.semantic_sha256);
        assert_eq!(native.delivery_state, DeliveryState::Pending);
        assert_eq!(native.processing.state, ProcessingState::Held);
        assert_eq!(native.processing.reason, Some(HoldReason::RequestUncertain));
        assert!(native.receipt.is_none());
        let local = original.snapshot.local_inbox_receipt.as_ref().unwrap();
        assert_eq!(local["state"], "failed");
        assert_eq!(local["turnId"], evidence.actual_turn_id);
        assert_eq!(local["semanticSha256"], evidence.local_semantic_sha256);
        assert!(envelope.validate().is_ok());
        assert!(evidence.validate_shape().is_ok());
        assert!(
            claude_task(&original),
            "genuine frozen {delivery:?} Claude Task"
        );
        let candidate = f
            .repo
            .stale_ack_candidate(id, evidence.observed_at)
            .unwrap();
        assert!(
            candidate.is_some(),
            "frozen {delivery:?} must remain a stale candidate"
        );
        let mut candidate = candidate.unwrap();
        candidate.later_completed_turn_id = Some("later-completed".into());
        assert_eq!(Some(candidate), evidence.stale_ack);
        assert!(evidence_bound(&original, &evidence));
        f.repo
            .observe_settlement(id, Some(evidence.clone()), evidence.observed_at)
            .unwrap();
        let request = f.request(id, "failed-task-delivery-close");
        assert!(f
            .repo
            .settlement_close_candidate(&request, evidence.observed_at)
            .unwrap()
            .is_ok());
        assert!(
            f.repo
                .close_settlement(&request, "owner", &evidence, evidence.observed_at)
                .unwrap()
                .unwrap()
                .changed
        );
        let closed = f.stored(id);
        assert_eq!(closed.snapshot, original.snapshot);
        assert_eq!(
            serde_json::to_value(&closed.canonical_envelope).unwrap(),
            serde_json::to_value(&original.canonical_envelope).unwrap()
        );
        assert!(closed.snapshot.a4_receipt.is_none());
        assert!(closed.snapshot.external_input_receipt.is_none());
    }
}

#[test]
fn stale_failed_claude_task_soon_still_rejects_passive_source_and_digest_conflicts() {
    let f = Fixture::new();
    let evidence = failed_claude_task_delivery_fixture(&f, "bad-task-delivery", Delivery::Soon);
    let original = f.stored("bad-task-delivery");
    for change in [
        "passive",
        "source-kind",
        "source-id",
        "digest",
        "native-digest",
        "local-digest",
    ] {
        let mut row = original.clone();
        let envelope = row.snapshot.external_input.as_mut().unwrap();
        match change {
            "passive" => envelope.message.delivery = Delivery::Passive,
            "source-kind" => envelope.message.source.kind = SourceKind::Agent,
            "source-id" => envelope.message.source.id = "forged-service".into(),
            "digest" => envelope.semantic_sha256 = "d".repeat(64),
            "native-digest" => {
                row.snapshot
                    .external_input_last_observed
                    .as_mut()
                    .unwrap()
                    .semantic_sha256 = "d".repeat(64)
            }
            _ => {
                row.snapshot.local_inbox_receipt.as_mut().unwrap()["semanticSha256"] =
                    json!("d".repeat(64))
            }
        }
        if matches!(change, "passive" | "source-kind" | "source-id") {
            let envelope = row.snapshot.external_input.as_mut().unwrap();
            envelope.semantic_sha256 = envelope.digest();
            row.snapshot
                .external_input_last_observed
                .as_mut()
                .unwrap()
                .semantic_sha256 = envelope.semantic_sha256.clone();
        }
        if matches!(change, "passive" | "source-kind" | "source-id" | "digest") {
            assert!(!claude_task(&row), "{change}");
        }
        if change != "local-digest" {
            assert!(
                stale_candidate(&row, evidence.observed_at).is_none(),
                "{change}"
            );
        }
        assert!(!evidence_bound(&row, &evidence), "{change}");
    }
}

#[test]
fn stale_age_uses_acceptance_time_at_exact_boundary_and_context_is_coherent() {
    let f = Fixture::new();
    f.seed("age", "failed", false);
    assert!(f
        .repo
        .stale_ack_candidate("age", f.now + chrono::Duration::milliseconds(299999))
        .unwrap()
        .is_none());
    let (snapshot, proof) = f
        .repo
        .stale_ack_candidate_context("age", f.now + chrono::Duration::seconds(300))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.message_id, "age");
    assert_eq!(proof.queued_at, f.now);
    assert_eq!(proof.accepted_at, f.now);
    assert_eq!(proof.min_wait_secs, 300);
    assert!(proof.later_completed_turn_id.is_none());
    assert!(proof.native_attempt_id.is_some());
    f.repo
        .mutate_message("age", |store| {
            store.messages.get_mut("age").unwrap().updated_at =
                (f.now + chrono::Duration::seconds(900)).to_rfc3339();
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.repo
            .stale_ack_candidate("age", f.now + chrono::Duration::seconds(300))
            .unwrap()
            .unwrap()
            .queued_at,
        f.now
    );
    for queued in [None, Some("invalid".into())] {
        let mut row = f.stored("age");
        row.queued_at = queued;
        assert!(stale_candidate(&row, f.now + chrono::Duration::seconds(900)).is_none());
    }
}

#[test]
fn stale_shape_and_durable_binding_reject_forged_age_attempt_and_status() {
    let f = Fixture::new();
    let evidence = stale_fixture(&f, "shape", ProcessingState::Claimed, true);
    assert!(evidence.validate_shape().is_ok());
    assert!(evidence_bound(&f.stored("shape"), &evidence));
    for change in [
        "wait",
        "young",
        "same",
        "blank",
        "native",
        "unknown-without-proof",
        "invented",
    ] {
        let mut bad = evidence.clone();
        match change {
            "wait" => bad.stale_ack.as_mut().unwrap().min_wait_secs = 299,
            "young" => {
                bad.stale_ack.as_mut().unwrap().queued_at += chrono::Duration::milliseconds(1)
            }
            "same" => {
                bad.stale_ack.as_mut().unwrap().later_completed_turn_id =
                    Some(bad.actual_turn_id.clone())
            }
            "blank" => bad.stale_ack.as_mut().unwrap().later_completed_turn_id = Some(" ".into()),
            "native" => {
                bad.stale_ack.as_mut().unwrap().native_attempt_id =
                    Some(uuid::Uuid::new_v4().to_string())
            }
            "unknown-without-proof" => {
                bad.terminal_status = "unknown".into();
                bad.stale_ack.as_mut().unwrap().later_completed_turn_id = None;
            }
            _ => bad.terminal_status = "stale".into(),
        }
        assert!(bad.validate_shape().is_err(), "{change}");
    }
    let mut forged = evidence.clone();
    forged.stale_ack.as_mut().unwrap().queued_at -= chrono::Duration::seconds(1);
    assert!(forged.validate_shape().is_ok());
    assert!(!evidence_bound(&f.stored("shape"), &forged));
    let mut legacy = evidence.clone();
    legacy.stale_ack = None;
    assert!(legacy.validate_shape().is_err());
    legacy.processing_state = "held".into();
    legacy.terminal_status = "failed".into();
    let bytes = serde_json::to_value(&legacy).unwrap();
    assert!(bytes.get("staleAck").is_none());
    assert_eq!(serde_json::from_value::<Evidence>(bytes).unwrap(), legacy);
}

#[test]
fn stale_unclaimed_pending_unknown_requires_later_proof_and_exact_native_absence() {
    let f = Fixture::new();
    let mut evidence = stale_fixture(&f, "pending", ProcessingState::Pending, false);
    evidence.terminal_status = "unknown".into();
    assert!(evidence.validate_shape().is_ok());
    assert!(evidence_bound(&f.stored("pending"), &evidence));
    assert!(f
        .repo
        .observe_settlement("pending", Some(evidence.clone()), evidence.observed_at)
        .unwrap());
    let request = f.request("pending", "pending-close");
    assert!(
        f.repo
            .close_settlement(&request, "owner", &evidence, evidence.observed_at)
            .unwrap()
            .unwrap()
            .changed
    );
    assert_eq!(f.stored("pending").snapshot.state, "inbox_accepted");
    let mut bad = evidence.clone();
    bad.stale_ack.as_mut().unwrap().later_completed_turn_id = None;
    assert!(bad.validate_shape().is_err());
    let mut row = f.stored("pending");
    row.settlement = None;
    row.snapshot
        .external_input_last_observed
        .as_mut()
        .unwrap()
        .processing
        .attempt_id = Some(evidence.attempt_id.clone());
    assert!(!evidence_bound(&row, &evidence));
}

#[test]
fn stale_candidate_rejects_receipts_missing_local_native_and_protected_controls() {
    let f = Fixture::new();
    let evidence = stale_fixture(&f, "receipts", ProcessingState::Claimed, true);
    let original = f.stored("receipts");
    for change in [
        "local",
        "native",
        "delivered",
        "digest",
        "passive",
        "control",
        "external",
        "owner",
        "source",
    ] {
        let mut row = original.clone();
        match change {
            "local" => row.snapshot.local_inbox_receipt = None,
            "native" => row.snapshot.external_input_last_observed = None,
            "delivered" => {
                row.meta
                    .get_or_insert_with(AgentBusMessageMeta::default)
                    .delivered_at = Some(f.now.to_rfc3339())
            }
            "digest" => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .semantic_sha256 = "d".repeat(64)
            }
            "passive" => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .message
                    .delivery = Delivery::Passive
            }
            "control" => {
                row.canonical_envelope.as_mut().unwrap().control_type =
                    Some("cutex.delegation.v1".into())
            }
            "external" => {
                row.canonical_envelope.as_mut().unwrap().external_action_id = Some("action".into())
            }
            "owner" => {
                row.canonical_envelope.as_mut().unwrap().sender_kind =
                    crate::agent_bus::model::AgentMessageKind::Owner
            }
            _ => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .message
                    .source
                    .kind = SourceKind::Service
            }
        }
        assert!(
            stale_candidate(&row, evidence.observed_at).is_none(),
            "{change}"
        );
    }
    f.repo
        .record_context_injected(TO, "receipts", "actual")
        .unwrap();
    assert!(f
        .repo
        .stale_ack_candidate("receipts", evidence.observed_at)
        .unwrap()
        .is_none());
    assert!(!evidence_bound(&f.stored("receipts"), &evidence));
}

#[test]
fn stale_claude_task_requires_full_canonical_frozen_provenance_and_stale_proof() {
    let f = Fixture::new();
    let mut evidence = stale_fixture(&f, "task", ProcessingState::Claimed, true);
    make_claude_task(&f, "task", &mut evidence);
    let original = f.stored("task");
    assert!(claude_task(&original));
    assert!(f
        .repo
        .stale_ack_candidate("task", evidence.observed_at)
        .unwrap()
        .is_some());
    assert!(evidence_bound(&original, &evidence));
    let mut legacy = evidence.clone();
    legacy.stale_ack = None;
    legacy.processing_state = "held".into();
    legacy.terminal_status = "failed".into();
    assert!(!evidence_bound(&original, &legacy));
    for change in [
        "text",
        "view",
        "source",
        "type",
        "sender",
        "target",
        "control",
        "notice",
        "external-id",
        "action",
        "summary",
        "digest",
        "sender-session",
    ] {
        let mut row = original.clone();
        match change {
            "text" => row
                .snapshot
                .external_input
                .as_mut()
                .unwrap()
                .message
                .text
                .push('!'),
            "view" => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .view
                    .as_mut()
                    .unwrap()
                    .data["status"] = json!("failed")
            }
            "source" => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .message
                    .source
                    .kind = SourceKind::Agent
            }
            "type" => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .message
                    .event_type = "agent_message".into()
            }
            "sender" => row.canonical_envelope.as_mut().unwrap().from = "other-service".into(),
            "target" => {
                row.canonical_envelope.as_mut().unwrap().to_cutex_session_id = Some(FROM.into())
            }
            "control" => {
                row.canonical_envelope.as_mut().unwrap().control_type =
                    Some("cutex.other.v1".into())
            }
            "notice" => {
                row.canonical_envelope
                    .as_mut()
                    .unwrap()
                    .control_payload
                    .as_mut()
                    .unwrap()["status"] = json!("bogus")
            }
            "external-id" => {
                row.canonical_envelope.as_mut().unwrap().external_message_id = Some("wrong".into())
            }
            "action" => {
                row.canonical_envelope.as_mut().unwrap().external_action_id = Some("action".into())
            }
            "summary" => row.canonical_envelope.as_mut().unwrap().content.push('!'),
            "digest" => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .semantic_sha256 = "a".repeat(64)
            }
            _ => row.snapshot.from_cutex_session_id = Some(FROM.into()),
        }
        // Rehashing changed frozen data still cannot make false provenance valid.
        if change != "digest" {
            let envelope = row.snapshot.external_input.as_mut().unwrap();
            envelope.semantic_sha256 = envelope.digest();
        }
        assert!(!claude_task(&row), "{change}");
        assert!(
            stale_candidate(&row, evidence.observed_at).is_none(),
            "{change}"
        );
    }
    f.repo
        .observe_settlement("task", Some(evidence.clone()), evidence.observed_at)
        .unwrap();
    let request = f.request("task", "task-close");
    assert!(
        f.repo
            .close_settlement(&request, "owner", &evidence, evidence.observed_at)
            .unwrap()
            .unwrap()
            .changed
    );
    assert_eq!(
        f.stored("task").settlement.unwrap().pending_sessions,
        vec![TO.to_string()]
    );
}

#[test]
fn stale_close_keeps_runtime_fence_idempotence_ttl_outbox_and_late_ack() {
    let f = Fixture::new();
    let evidence = stale_fixture(&f, "close-stale", ProcessingState::Claimed, true);
    f.repo
        .observe_settlement("close-stale", Some(evidence.clone()), evidence.observed_at)
        .unwrap();
    let request = f.request("close-stale", "stale-key");
    let blocked = f
        .repo
        .close_settlement_fenced(&request, "owner", &evidence, evidence.observed_at, &|_| {
            Ok(false)
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(blocked.code, "settlement_runtime_changed");
    assert!(f.stored("close-stale").settlement.is_none());
    assert!(f
        .repo
        .close_settlement(
            &request,
            "owner",
            &evidence,
            evidence.observed_at + chrono::Duration::seconds(181)
        )
        .unwrap()
        .is_err());
    assert!(
        f.repo
            .close_settlement(&request, "owner", &evidence, evidence.observed_at)
            .unwrap()
            .unwrap()
            .changed
    );
    let audit = f.stored("close-stale").settlement.unwrap().audit;
    assert!(
        !f.repo
            .close_settlement_fenced(
                &request,
                "owner",
                &evidence,
                evidence.observed_at,
                &|_| panic!("replay does not query runtime")
            )
            .unwrap()
            .unwrap()
            .changed
    );
    f.repo
        .record_context_injected(TO, "close-stale", "actual")
        .unwrap();
    assert_eq!(f.stored("close-stale").snapshot.state, "delivered");
    assert_eq!(f.stored("close-stale").settlement.unwrap().audit, audit);
    assert!(!f
        .repo
        .observe_settlement("close-stale", None, evidence.observed_at)
        .unwrap());
    let reopened = AgentBusMessageRepository::open(&f.root).unwrap();
    assert!(
        !reopened
            .settlement_replay(&request, "owner")
            .unwrap()
            .unwrap()
            .unwrap()
            .changed
    );
    let mut other = request.clone();
    other.request_id = "different-key".into();
    assert!(reopened
        .close_settlement(&other, "owner", &evidence, evidence.observed_at)
        .unwrap()
        .is_err());
    AgentBusMessageRepository::validate_settlement_observations_read_only(&f.root).unwrap();
    assert!(AgentBusMessageRepository::validate_settlement_rollback_read_only(&f.root).is_err());
}

#[test]
fn stale_sidecar_validates_strict_shape_and_never_rewrites_business_before_close() {
    let f = Fixture::new();
    let evidence = stale_fixture(&f, "sidecar", ProcessingState::Pending, false);
    let database = f.root.join(hot_store::DATABASE_FILE);
    let before = hot_store::verified_rows(&database, "agent_bus")
        .unwrap()
        .unwrap();
    f.repo
        .observe_settlement("sidecar", Some(evidence.clone()), evidence.observed_at)
        .unwrap();
    let after = hot_store::verified_rows(&database, "agent_bus")
        .unwrap()
        .unwrap();
    assert_eq!(before.rows, after.rows);
    assert_eq!(before.generation, after.generation);
    let rows = hot_store::verified_rows(&database, EVIDENCE_STORE)
        .unwrap()
        .unwrap()
        .rows;
    AgentBusMessageRepository::validate_settlement_observation_rows(&rows).unwrap();
    for (field, value) in [
        ("futureField", json!(true)),
        ("minWaitSecs", json!(299)),
        ("nativeAttemptId", json!("wrong")),
    ] {
        let mut bad = rows.clone();
        let bytes = bad.get_mut(&("evidence".into(), "sidecar".into())).unwrap();
        let mut row: Value = serde_json::from_slice(bytes).unwrap();
        row["record"]["evidence"]["staleAck"][field] = value;
        *bytes = serde_json::to_vec(&row).unwrap();
        assert!(AgentBusMessageRepository::validate_settlement_observation_rows(&bad).is_err());
    }
}

#[test]
fn stale_acceptance_receipt_without_inbox_state_is_valid_but_context_receipt_is_not() {
    let f = Fixture::new();
    let mut evidence = stale_fixture(&f, "acceptance", ProcessingState::Pending, false);
    make_claude_task(&f, "acceptance", &mut evidence);
    f.repo
        .mutate_message("acceptance", |store| {
            let local = store
                .messages
                .get_mut("acceptance")
                .unwrap()
                .snapshot
                .local_inbox_receipt
                .as_mut()
                .unwrap();
            local.as_object_mut().unwrap().remove("state");
            local["contextPersisted"] = json!(false);
            local["sourceVerified"] = json!(true);
            local["nativeExternalInput"] = json!(true);
            Ok(())
        })
        .unwrap();
    assert!(f
        .repo
        .stale_ack_candidate("acceptance", evidence.observed_at)
        .unwrap()
        .is_some());
    assert!(evidence_bound(&f.stored("acceptance"), &evidence));
    let mut persisted = f.stored("acceptance");
    persisted.snapshot.local_inbox_receipt.as_mut().unwrap()["contextPersisted"] = json!(true);
    assert!(stale_candidate(&persisted, evidence.observed_at).is_none());
    assert!(!evidence_bound(&persisted, &evidence));
}
fn passive_stale_fixture(f: &Fixture, id: &str) -> Evidence {
    let mut evidence = f.seed(id, "failed", false);
    f.repo
        .mutate_message(id, |store| {
            let stored = store.messages.get_mut(id).unwrap();
            stored.canonical_envelope.as_mut().unwrap().delivery_mode =
                crate::agent_bus::delivery::AgentDeliveryMode::Passive;
            stored.canonical_envelope.as_mut().unwrap().trigger_turn = false;
            stored.snapshot.delivery_mode = "passive".into();
            let envelope = stored.snapshot.external_input.as_mut().unwrap();
            envelope.message.delivery = Delivery::Passive;
            envelope.semantic_sha256 = envelope.digest();
            evidence.semantic_sha256 = envelope.semantic_sha256.clone();
            let native = stored
                .snapshot
                .external_input_last_observed
                .as_mut()
                .unwrap();
            native.semantic_sha256 = envelope.semantic_sha256.clone();
            native.processing = ProcessingStatus {
                state: ProcessingState::None,
                attempt_id: None,
                reason: None,
            };
            let local = stored.snapshot.local_inbox_receipt.as_mut().unwrap();
            local.as_object_mut().unwrap().remove("state");
            local["turnId"] = Value::Null;
            local["contextTurnId"] = Value::Null;
            local["contextPersisted"] = json!(false);
            local["deliveryMode"] = json!("passive");
            Ok(())
        })
        .unwrap();
    evidence.observed_at = f.now + chrono::Duration::seconds(300);
    evidence.processing_state = "none".into();
    evidence.actual_turn_id = "unassigned".into();
    // Independently computed with Python uuid.uuid5(NAMESPACE_URL, ...).
    evidence.attempt_id = "3fbec92c-3ea4-59b9-aaaa-a3435cdada30".into();
    evidence.terminal_status = "unknown".into();
    let mut proof = f
        .repo
        .stale_ack_candidate(id, evidence.observed_at)
        .unwrap()
        .unwrap();
    assert!(proof.native_attempt_id.is_none());
    proof.later_completed_turn_id = Some("later-completed".into());
    evidence.stale_ack = Some(proof);
    evidence
}

#[test]
fn stale_passive_without_native_attempt_or_assigned_turn_closes_followup_only() {
    let f = Fixture::new();
    let evidence = passive_stale_fixture(&f, "passive-close");
    assert_eq!(unassigned_attempt_id("thread"), evidence.attempt_id);
    let original = f.stored("passive-close");
    assert!(
        !ordinary(&original),
        "legacy passive policy stays unchanged"
    );
    assert!(stale_kind(&original));
    assert!(evidence.validate_shape().is_ok());
    assert!(evidence_bound(&original, &evidence));
    assert!(f
        .repo
        .observe_settlement(
            "passive-close",
            Some(evidence.clone()),
            evidence.observed_at
        )
        .unwrap());
    let request = f.request("passive-close", "passive-close-key");
    assert!(f
        .repo
        .settlement_close_candidate(&request, evidence.observed_at)
        .unwrap()
        .is_ok());
    assert!(f
        .repo
        .close_settlement_fenced(&request, "owner", &evidence, evidence.observed_at, &|_| Ok(
            false
        ))
        .unwrap()
        .is_err());
    assert!(
        f.repo
            .close_settlement(&request, "owner", &evidence, evidence.observed_at)
            .unwrap()
            .unwrap()
            .changed
    );
    let closed = f.stored("passive-close");
    assert_eq!(
        serde_json::to_value(&closed.snapshot).unwrap(),
        serde_json::to_value(&original.snapshot).unwrap()
    );
    assert!(closed.snapshot.a4_receipt.is_none());
    assert!(closed.snapshot.external_input_receipt.is_none());
    assert_eq!(
        closed
            .snapshot
            .external_input_last_observed
            .as_ref()
            .unwrap()
            .processing
            .state,
        ProcessingState::None
    );
    assert!(
        !f.repo
            .close_settlement(&request, "owner", &evidence, evidence.observed_at)
            .unwrap()
            .unwrap()
            .changed
    );
    let reopened = AgentBusMessageRepository::open(&f.root).unwrap();
    assert!(
        !reopened
            .settlement_replay(&request, "owner")
            .unwrap()
            .unwrap()
            .unwrap()
            .changed
    );
}

#[test]
fn stale_passive_none_requires_later_proof_unassigned_bookkeeping_and_frozen_passive() {
    let f = Fixture::new();
    let evidence = passive_stale_fixture(&f, "passive-refuse");
    let original = f.stored("passive-refuse");
    for change in [
        "later",
        "native",
        "actual",
        "bookkeeping",
        "legacy",
        "terminal",
    ] {
        let mut bad = evidence.clone();
        match change {
            "later" => bad.stale_ack.as_mut().unwrap().later_completed_turn_id = None,
            "native" => {
                bad.stale_ack.as_mut().unwrap().native_attempt_id = Some(bad.attempt_id.clone())
            }
            "actual" => bad.actual_turn_id = "claimed-turn".into(),
            "bookkeeping" => bad.attempt_id = uuid::Uuid::new_v4().to_string(),
            "legacy" => bad.stale_ack = None,
            _ => bad.terminal_status = "completed".into(),
        }
        assert!(!evidence_bound(&original, &bad), "{change}");
        f.repo
            .observe_settlement("passive-refuse", Some(bad), evidence.observed_at)
            .unwrap();
        assert!(
            f.stored("passive-refuse").observed_settlement.is_none(),
            "{change}"
        );
    }
    for change in [
        "active",
        "canonical-active",
        "snapshot-active",
        "control",
        "sender",
        "source",
    ] {
        let mut row = original.clone();
        match change {
            "active" => {
                let envelope = row.snapshot.external_input.as_mut().unwrap();
                envelope.message.delivery = Delivery::AfterTurn;
                envelope.semantic_sha256 = envelope.digest();
                row.snapshot
                    .external_input_last_observed
                    .as_mut()
                    .unwrap()
                    .semantic_sha256 = envelope.semantic_sha256.clone();
            }
            "canonical-active" => {
                row.canonical_envelope.as_mut().unwrap().delivery_mode =
                    crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
            }
            "snapshot-active" => row.snapshot.delivery_mode = "after_turn".into(),
            "control" => {
                row.canonical_envelope.as_mut().unwrap().control_type =
                    Some("cutex.delegation.v1".into())
            }
            "sender" => {
                row.canonical_envelope
                    .as_mut()
                    .unwrap()
                    .from_cutex_session_id = Some(TO.into())
            }
            _ => {
                row.snapshot
                    .external_input
                    .as_mut()
                    .unwrap()
                    .message
                    .source
                    .id = "forged".into()
            }
        }
        assert!(!evidence_bound(&row, &evidence), "{change}");
    }
    assert!(f
        .repo
        .stale_ack_candidate("passive-refuse", f.now + chrono::Duration::seconds(299))
        .unwrap()
        .is_none());
}

#[test]
fn stale_public_evidence_view_carries_proof_and_legacy_omits_it() {
    let f = Fixture::new();
    f.seed("legacy-view", "failed", false);
    let legacy = f
        .repo
        .settlement_view_by_id("legacy-view", f.now)
        .unwrap()
        .unwrap();
    let legacy_json = serde_json::to_value(&legacy).unwrap();
    assert!(legacy_json["evidence"].get("staleAck").is_none());
    assert!(serde_json::from_value::<SettlementView>(legacy_json)
        .unwrap()
        .evidence
        .stale_ack
        .is_none());

    let evidence = passive_stale_fixture(&f, "stale-view");
    let (_, events) = super::super::capture_events(|| {
        f.repo
            .observe_settlement("stale-view", Some(evidence.clone()), evidence.observed_at)
            .unwrap()
    });
    assert_eq!(events.len(), 2);
    for (_, method, params) in events {
        assert_eq!(
            params["settlement"]["evidence"]["terminalStatus"],
            "unknown"
        );
        assert_eq!(
            params["settlement"]["evidence"]["staleAck"],
            serde_json::to_value(&evidence.stale_ack).unwrap()
        );
        super::super::super::contract_validation::validate_cutex_event_message(
            &json!({"method": method, "params": params}),
        )
        .unwrap();
    }
    let view = f
        .repo
        .settlement_view_by_id("stale-view", evidence.observed_at)
        .unwrap()
        .unwrap();
    assert_eq!(view.evidence.stale_ack, evidence.stale_ack);
    let request = f.request("stale-view", "view-close");
    let closed = f
        .repo
        .close_settlement(&request, "owner", &evidence, evidence.observed_at)
        .unwrap()
        .unwrap();
    assert_eq!(closed.settlement.evidence.stale_ack, evidence.stale_ack);
}

#[test]
fn stale_wait_starts_at_acceptance_even_when_enqueue_was_days_earlier() {
    let f = Fixture::new();
    f.seed("long-queued", "failed", false);
    f.repo
        .mutate_message("long-queued", |store| {
            let stored = store.messages.get_mut("long-queued").unwrap();
            stored.queued_at = Some((f.now - chrono::Duration::days(3)).to_rfc3339());
            stored.inbox_accepted_at = Some(f.now.to_rfc3339());
            Ok(())
        })
        .unwrap();
    assert!(f
        .repo
        .stale_ack_candidate("long-queued", f.now)
        .unwrap()
        .is_none());
    assert!(f
        .repo
        .stale_ack_candidate(
            "long-queued",
            f.now + chrono::Duration::milliseconds(299999)
        )
        .unwrap()
        .is_none());
    let proof = f
        .repo
        .stale_ack_candidate("long-queued", f.now + chrono::Duration::seconds(300))
        .unwrap()
        .unwrap();
    assert_eq!(proof.queued_at, f.now - chrono::Duration::days(3));
    assert_eq!(proof.accepted_at, f.now);
    let mut row = f.stored("long-queued");
    row.inbox_accepted_at = Some((f.now - chrono::Duration::days(4)).to_rfc3339());
    assert!(stale_candidate(&row, f.now + chrono::Duration::seconds(900)).is_none());
    row.inbox_accepted_at = Some("invalid".into());
    assert!(stale_candidate(&row, f.now + chrono::Duration::seconds(900)).is_none());
}

#[test]
fn stale_accepted_at_is_required_age_checked_and_bound_to_durable_acceptance() {
    let f = Fixture::new();
    let evidence = stale_fixture(&f, "acceptance-binding", ProcessingState::Claimed, true);
    for offset in [-1, 1] {
        let mut bad = evidence.clone();
        bad.stale_ack.as_mut().unwrap().accepted_at += chrono::Duration::milliseconds(offset);
        assert!(bad.validate_shape().is_err());
        assert!(!evidence_bound(&f.stored("acceptance-binding"), &bad));
    }
    let mut raw = serde_json::to_value(&evidence).unwrap();
    raw["staleAck"]
        .as_object_mut()
        .unwrap()
        .remove("acceptedAt");
    assert!(serde_json::from_value::<Evidence>(raw).is_err());
    // Give the forged acceptance enough age and a valid enqueue relationship;
    // shape validation alone cannot replace the exact durable-row binding.
    let mut bad = evidence.clone();
    bad.observed_at += chrono::Duration::seconds(1);
    bad.stale_ack.as_mut().unwrap().accepted_at += chrono::Duration::milliseconds(1);
    assert!(bad.validate_shape().is_ok());
    assert!(!evidence_bound(&f.stored("acceptance-binding"), &bad));
}

#[test]
fn stale_legacy_acceptance_fallback_is_conservative_and_survives_close_timestamp_changes() {
    let f = Fixture::new();
    let mut evidence = f.seed("legacy-acceptance", "failed", false);
    let accepted = f.now + chrono::Duration::seconds(20);
    f.repo
        .mutate_message("legacy-acceptance", |store| {
            let stored = store.messages.get_mut("legacy-acceptance").unwrap();
            stored.inbox_accepted_at = None;
            stored.updated_at = accepted.to_rfc3339();
            Ok(())
        })
        .unwrap();
    assert!(f
        .repo
        .stale_ack_candidate("legacy-acceptance", f.now + chrono::Duration::seconds(300))
        .unwrap()
        .is_none());
    evidence.observed_at = accepted + chrono::Duration::seconds(300);
    evidence.stale_ack = f
        .repo
        .stale_ack_candidate("legacy-acceptance", evidence.observed_at)
        .unwrap();
    assert_eq!(evidence.stale_ack.as_ref().unwrap().accepted_at, accepted);
    assert!(evidence_bound(&f.stored("legacy-acceptance"), &evidence));
    f.repo
        .observe_settlement(
            "legacy-acceptance",
            Some(evidence.clone()),
            evidence.observed_at,
        )
        .unwrap();
    let request = f.request("legacy-acceptance", "legacy-acceptance-close");
    assert!(
        f.repo
            .close_settlement(&request, "owner", &evidence, evidence.observed_at)
            .unwrap()
            .unwrap()
            .changed
    );
    let closed = f.stored("legacy-acceptance");
    assert_ne!(accepted_at(&closed), Some(accepted));
    validate_settlement_record(&closed, closed.settlement.as_ref().unwrap()).unwrap();
    let reopened = AgentBusMessageRepository::open(&f.root).unwrap();
    assert!(
        !reopened
            .settlement_replay(&request, "owner")
            .unwrap()
            .unwrap()
            .unwrap()
            .changed
    );
    f.repo
        .record_context_injected(TO, "legacy-acceptance", "actual")
        .unwrap();
    let acknowledged = f.stored("legacy-acceptance");
    validate_settlement_record(&acknowledged, acknowledged.settlement.as_ref().unwrap()).unwrap();
}
