//! Regression cases for peer lifecycle, durable cancellation and partial upgrades.
use super::{asks, model, store, tests, wire, worker};
use crate::asks::{model::*, store::AskStore};
use chrono::Utc;
use serde_json::json;

fn fixture() -> (store::Store, AskStore, AskRecord, store::Peer) {
    let root = std::env::temp_dir().join(format!("xhost-fixes-{}", uuid::Uuid::new_v4()));
    let peers = store::Store::at(root.join("runtime/peers"));
    let asks = AskStore::at(root.join("runtime/asks"));
    let (mut state, mut ask, peer) = tests::remote_record();
    let remote = ask.remote.as_mut().unwrap();
    remote.request.request_sha256 = remote.request.digest();
    remote.sync.pending_ops.push(model::Op {
        op_id: crate::asks::new_id("pop"),
        op: "create".into(),
        outcome: None,
        reason: None,
    });
    state.asks.insert(ask.ask_id.clone(), ask.clone());
    peers
        .insert(peer.clone(), &super::token::generate())
        .unwrap();
    peers
        .settings(&json!({"enabled":true,"outboxMaxAgeHours":1}))
        .unwrap();
    asks.with_state(|s| *s = state).unwrap();
    (peers, asks, ask, peer)
}

fn inbound(asks: &AskStore, mut ask: AskRecord, peer: &store::Peer) -> AskRecord {
    let request = ask.remote.take().unwrap().request;
    ask.requester = Requester::Peer {
        peer_id: peer.peer_id.clone(),
        cutex_session_id: "remote-director".into(),
        name: Some("Director".into()),
        label: "alpha".into(),
    };
    ask.peer_inbound = Some(model::Inbound {
        dead_letters: vec![],
        queued_at: None,
        result_undeliverable: false,
        delivery_stopped: false,
        last_error: None,
        request,
        peer_deliveries: vec![],
        received_ops: vec![],
        next_attempt_at: None,
        failures: 0,
    });
    // Replacing the mirror is a new state for the transactional snapshot hook.
    ask.execution = ExecutionState::Queued;
    ask.revision += 1;
    asks.with_state(|s| {
        s.asks.clear();
        s.asks.insert(ask.ask_id.clone(), ask.clone());
    })
    .unwrap();
    asks.snapshot().unwrap().asks[&ask.ask_id].clone()
}

fn wait_for_change(asks: &AskStore, ask: &AskRecord, action: &str) -> serde_json::Value {
    crate::asks::waits::wait_blocking(
        asks,
        ask.requester.session().unwrap(),
        &crate::asks::waits::WaitRequest {
            asks: vec![ask.ask_id.clone()],
            all: true,
            timeout_ms: 30_000,
            action_id: action.into(),
        },
        &Utc::now,
        &|| panic!("peer lifecycle change must wake delegation_wait immediately"),
    )
    .unwrap()
}

#[test]
fn disabled_deleted_and_unpaired_mirrors_reach_terminal_and_wake_waiters() {
    for (change, reason) in [
        ("peerDisabled", InterruptReason::PeerDisabled),
        ("peersDisabled", InterruptReason::PeersDisabled),
        ("peerRemoved", InterruptReason::PeerRemoved),
        ("removedByPeer", InterruptReason::RemovedByPeer),
    ] {
        let (peers, asks, ask, peer) = fixture();
        match change {
            "peerDisabled" => peers
                .with(|s| {
                    s.peers.get_mut(&peer.peer_id).unwrap().enabled = false;
                    Ok(())
                })
                .unwrap(),
            "peersDisabled" => {
                peers.settings(&json!({"enabled":false})).unwrap();
            }
            "peerRemoved" => {
                peers.remove(&peer.peer_id).unwrap();
            }
            "removedByPeer" => peers
                .with(|s| {
                    s.peers.get_mut(&peer.peer_id).unwrap().status = "removedByPeer".into();
                    Ok(())
                })
                .unwrap(),
            _ => unreachable!(),
        }
        worker::maintain(&peers, &asks).unwrap();
        let state = asks.snapshot().unwrap();
        let updated = &state.asks[&ask.ask_id];
        assert_eq!(updated.execution, ExecutionState::Interrupted, "{change}");
        assert!(!updated.is_open(), "{change}");
        let remote = updated.remote.as_ref().unwrap();
        assert_eq!(remote.sync.interrupt_reason, Some(reason));
        assert!(remote.sync.local_terminal);
        if matches!(change, "peerDisabled" | "peersDisabled") {
            assert_eq!(remote.sync.pending_ops.len(), 1);
            assert_eq!(remote.sync.pending_ops[0].op, "close");
            assert_eq!(remote.sync.pending_ops[0].reason.as_deref(), Some(change));
        } else {
            assert!(remote.sync.pending_ops.is_empty());
        }
        assert_eq!(state.deliveries.len(), 1);
        assert_eq!(
            wait_for_change(&asks, &ask, change)["reason"],
            "stateChanged"
        );
        worker::maintain(&peers, &asks).unwrap();
        assert_eq!(asks.snapshot().unwrap().deliveries.len(), 1);
    }
}

#[test]
fn identity_and_version_mismatch_hold_then_expire_even_without_transport() {
    for (status, hold) in [
        ("identityChanged", Hold::PeerIdentityChanged),
        ("versionMismatch", Hold::PeerVersionMismatch),
    ] {
        let (peers, asks, ask, peer) = fixture();
        peers
            .with(|s| {
                let p = s.peers.get_mut(&peer.peer_id).unwrap();
                p.status = status.into();
                p.next_probe_at = Some((Utc::now() + chrono::Duration::days(1)).to_rfc3339());
                Ok(())
            })
            .unwrap();
        worker::maintain(&peers, &asks).unwrap();
        let current = asks.snapshot().unwrap().asks[&ask.ask_id].clone();
        assert_eq!(current.hold, Some(hold));
        assert!(current.is_open());
        assert_eq!(
            wait_for_change(&asks, &ask, "mismatch-hold")["reason"],
            "stateChanged"
        );
        asks.with_state(|s| {
            s.asks.get_mut(&ask.ask_id).unwrap().created_at =
                (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        })
        .unwrap();
        worker::maintain(&peers, &asks).unwrap();
        let state = asks.snapshot().unwrap();
        let current = &state.asks[&ask.ask_id];
        assert_eq!(current.execution, ExecutionState::Interrupted);
        assert_eq!(
            current.remote.as_ref().unwrap().sync.interrupt_reason,
            Some(InterruptReason::OutcomeUnknown)
        );
        assert!(!current.is_open());
        assert_eq!(
            wait_for_change(&asks, &ask, "mismatch-expired")["reason"],
            "stateChanged"
        );
    }
}

#[test]
fn disabled_inbound_execution_continues_but_aged_results_become_dead_letters() {
    for globally_disabled in [false, true] {
        let (peers, asks, mirror, peer) = fixture();
        let ask = inbound(&asks, mirror, &peer);
        asks.with_state(|s| {
            s.update_ask(&ask.ask_id, Utc::now(), |a| {
                a.execution = ExecutionState::Running;
            });
        })
        .unwrap();
        if globally_disabled {
            peers.settings(&json!({"enabled":false})).unwrap();
        } else {
            peers
                .with(|s| {
                    s.peers.get_mut(&peer.peer_id).unwrap().enabled = false;
                    Ok(())
                })
                .unwrap();
        }
        worker::maintain(&peers, &asks).unwrap();
        let state = asks.snapshot().unwrap();
        let current = &state.asks[&ask.ask_id];
        assert_eq!(current.execution, ExecutionState::Running);
        assert!(!current
            .peer_inbound
            .as_ref()
            .unwrap()
            .peer_deliveries
            .is_empty());
        asks.with_state(|s| {
            s.asks
                .get_mut(&ask.ask_id)
                .unwrap()
                .peer_inbound
                .as_mut()
                .unwrap()
                .queued_at = Some((Utc::now() - chrono::Duration::hours(2)).to_rfc3339());
        })
        .unwrap();
        worker::maintain(&peers, &asks).unwrap();
        let state = asks.snapshot().unwrap();
        let current = &state.asks[&ask.ask_id];
        assert_eq!(current.execution, ExecutionState::Running);
        let inbound = current.peer_inbound.as_ref().unwrap();
        assert!(inbound.peer_deliveries.is_empty());
        assert_eq!(inbound.dead_letters.len(), 1);
        assert_eq!(inbound.dead_letters[0].code, "peer_event_expired");
        assert!(inbound.result_undeliverable && inbound.delivery_stopped);
        asks.with_state(|s| {
            s.update_ask(&ask.ask_id, Utc::now(), |a| {
                a.execution = ExecutionState::Answered
            });
        })
        .unwrap();
        assert!(asks.snapshot().unwrap().asks[&ask.ask_id]
            .peer_inbound
            .as_ref()
            .unwrap()
            .peer_deliveries
            .is_empty());
    }
}

#[test]
fn inbound_delete_and_remote_unpair_stop_execution_and_discard_outbox() {
    for code in ["peerRemoved", "removedByPeer"] {
        let (_peers, asks, mirror, peer) = fixture();
        let ask = inbound(&asks, mirror, &peer);
        worker::removed_with_reason(&asks, &peer.peer_id, code).unwrap();
        let state = asks.snapshot().unwrap();
        let current = &state.asks[&ask.ask_id];
        assert!(!current.is_open());
        let inbound = current.peer_inbound.as_ref().unwrap();
        assert!(inbound.peer_deliveries.is_empty());
        assert!(inbound.delivery_stopped);
        assert_eq!(inbound.last_error.as_deref(), Some(code));
    }
}

#[test]
fn lost_create_response_expiry_stays_terminal_and_durable_close_stops_remote() {
    let (peers, asks, ask, peer) = fixture();
    // The remote accepted the same id, but the initiator has no acknowledgement.
    let remote_store = AskStore::at(
        std::env::temp_dir().join(format!("xhost-lost-create-{}", uuid::Uuid::new_v4())),
    );
    let remote_ask = inbound(&remote_store, ask.clone(), &peer);
    let request = remote_ask.peer_inbound.as_ref().unwrap().request.clone();
    let before = remote_store.snapshot().unwrap();
    asks::receive(&remote_store, &peer, &ask.ask_id, request.clone()).unwrap();
    asks::receive(&remote_store, &peer, &ask.ask_id, request).unwrap();
    assert_eq!(before, remote_store.snapshot().unwrap());
    asks.with_state(|s| {
        s.asks.get_mut(&ask.ask_id).unwrap().created_at =
            (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    })
    .unwrap();
    worker::maintain(&peers, &asks).unwrap();
    let expired = asks.snapshot().unwrap();
    let current = &expired.asks[&ask.ask_id];
    assert_eq!(current.execution, ExecutionState::Failed);
    assert_eq!(expired.deliveries.len(), 1);
    let op = current.remote.as_ref().unwrap().sync.pending_ops[0].clone();
    assert_eq!(op.op, "close");
    assert_eq!(op.outcome.as_deref(), Some("abandoned"));
    assert_eq!(op.reason.as_deref(), Some("expired"));
    let mut snapshot = asks::snapshot(&before, &remote_ask);
    snapshot.revision += 2;
    snapshot.execution = ExecutionState::Running;
    asks::apply(&asks, &peer, snapshot.clone()).unwrap();
    snapshot.revision += 1;
    snapshot.execution = ExecutionState::Answered;
    snapshot.closed = Some(CloseOutcome::Completed);
    snapshot.text = Some("late answer".into());
    snapshot.answer = Some(AnswerRecord {
        answer_id: "ans-late".into(),
        ask_id: ask.ask_id.clone(),
        attempt: 1,
        revision: 1,
        status: AnswerStatus::Completed,
        artifact_id: None,
        bytes: 11,
        metadata: Default::default(),
        created_at: super::now(),
        preview: None,
    });
    asks::apply(&asks, &peer, snapshot).unwrap();
    let after = asks.snapshot().unwrap();
    let current = &after.asks[&ask.ask_id];
    assert_eq!(current.execution, ExecutionState::Failed);
    assert_eq!(current.closed, Some(CloseOutcome::Completed));
    assert!(after.answers.is_empty());
    assert_eq!(after.deliveries.len(), 1);
    let remote = current.remote.as_ref().unwrap();
    let late = serde_json::to_value(remote.late_answer.as_ref().unwrap()).unwrap();
    assert!(late.get("text").is_none());
    assert_eq!(late["preview"], "late answer");
    assert_eq!(
        asks.read_artifact(late["artifactId"].as_str().unwrap(), None)
            .unwrap()
            .unwrap()
            .1,
        b"late answer"
    );
    assert_eq!(remote.sync.pending_ops, vec![op.clone()]);
    assert_eq!(
        wait_for_change(&asks, &ask, "expiry-once")["reason"],
        "stateChanged"
    );
    asks::receive_op(&remote_store, &peer, &ask.ask_id, op.clone()).unwrap();
    asks::receive_op(&remote_store, &peer, &ask.ask_id, op).unwrap();
    let stopped = remote_store.snapshot().unwrap();
    assert!(!stopped.asks[&ask.ask_id].is_open());
    assert_eq!(
        stopped.asks[&ask.ask_id]
            .peer_inbound
            .as_ref()
            .unwrap()
            .received_ops
            .len(),
        1
    );
}

#[test]
fn unknown_remote_state_is_a_visible_hold_and_never_a_local_answer() {
    let (_peers, asks, ask, peer) = fixture();
    let mut value = serde_json::to_value(wire::WireSnapshotV1::from(&asks::snapshot(
        &asks.snapshot().unwrap(),
        &ask,
    )))
    .unwrap();
    value["revision"] = json!(5);
    value["execution"] = json!("futureExecution");
    value["hold"] = json!({"reason":"futureHold", "futureField":true});
    value["extra"] = json!(true);
    let wire: wire::WireSnapshotV1 = serde_json::from_value(value).unwrap();
    let snapshot: model::Snapshot = wire.into();
    asks::apply(&asks, &peer, snapshot.clone()).unwrap();
    asks::apply(&asks, &peer, snapshot.clone()).unwrap();
    let current = asks.snapshot().unwrap();
    assert_eq!(current.asks[&ask.ask_id].execution, ask.execution);
    assert!(matches!(
        current.asks[&ask.ask_id].hold,
        Some(Hold::PeerUnknown { .. })
    ));
    assert!(current.answers.is_empty());
    assert_eq!(current.deliveries.len(), 1);
    assert_eq!(
        wait_for_change(&asks, &ask, "unknown-hold")["reason"],
        "stateChanged"
    );
    let view = crate::asks::view::requester_view(&current, &current.asks[&ask.ask_id]);
    assert!(view.to_string().contains("futureHold"));
}

#[test]
fn local_intermediaries_inherit_maximum_peer_hops_through_parent_chain() {
    let (mut state, mut incoming, peer) = tests::remote_record();
    let mut request = incoming.remote.take().unwrap().request;
    request.hops = 3;
    // Simulate an older stored inbound record that has no additive top-level hops.
    incoming.hops = 0;
    incoming.requester = Requester::Peer {
        peer_id: peer.peer_id,
        cutex_session_id: "remote-director".into(),
        name: None,
        label: "alpha".into(),
    };
    incoming.peer_inbound = Some(model::Inbound {
        dead_letters: vec![],
        queued_at: None,
        result_undeliverable: false,
        delivery_stopped: false,
        last_error: None,
        request,
        peer_deliveries: vec![],
        received_ops: vec![],
        next_attempt_at: None,
        failures: 0,
    });
    state.asks.insert(incoming.ask_id.clone(), incoming.clone());
    let mut local = crate::asks::store::tests::new_ask("local-hop", "delegate to local X");
    local.parent_ask = Some(incoming.ask_id.clone());
    let (local, _) = state.create_ask(local, Utc::now()).unwrap();
    assert_eq!(local.hops, 3);
    let mut child = crate::asks::store::tests::new_ask("local-hop-2", "delegate again locally");
    child.parent_ask = Some(local.ask_id.clone());
    let (child, _) = state.create_ask(child, Utc::now()).unwrap();
    assert_eq!(child.hops, 3);
    assert_eq!(
        state.inherited_hops(Some(&child.ask_id)).saturating_add(1),
        4
    );
    assert!(
        state.inherited_hops(Some(&child.ask_id)).saturating_add(1)
            > super::policy::Settings::default().max_hops
    );
    // Broken or cyclic persisted ancestry cannot hang a daemon.
    state.asks.get_mut(&incoming.ask_id).unwrap().parent_ask = Some(child.ask_id.clone());
    assert_eq!(state.inherited_hops(Some(&child.ask_id)), 3);
}

#[test]
fn semantic_event_rejection_dead_letters_are_bounded_and_never_requeued() {
    let (peers, asks, mirror, peer) = fixture();
    let ask = inbound(&asks, mirror, &peer);
    for code in ["ask_not_found", "ask_gone", "peer_asks_disabled"] {
        asks.with_state(|s| {
            let a = s.asks.get_mut(&ask.ask_id).unwrap();
            let i = a.peer_inbound.as_mut().unwrap();
            i.delivery_stopped = false;
            s.update_ask(&ask.ask_id, Utc::now(), |a| {
                a.execution = ExecutionState::Running;
                a.title = format!("result: {code}");
            });
        })
        .unwrap();
        worker::dead_letter(&peers, &asks, &peer, &ask.ask_id, code).unwrap();
        let state = asks.snapshot().unwrap();
        let i = state.asks[&ask.ask_id].peer_inbound.as_ref().unwrap();
        assert!(i.peer_deliveries.is_empty());
        assert_eq!(i.dead_letters.last().unwrap().code, code);
        assert!(i.result_undeliverable);
    }
    asks.with_state(|s| {
        let i = s
            .asks
            .get_mut(&ask.ask_id)
            .unwrap()
            .peer_inbound
            .as_mut()
            .unwrap();
        let example = i.dead_letters[0].clone();
        for n in 0..105 {
            let mut item = example.clone();
            item.event.event_id = format!("pev-old-{n}");
            i.dead_letters.push(item);
        }
    })
    .unwrap();
    worker::dead_letter(&peers, &asks, &peer, &ask.ask_id, "ask_not_found").unwrap();
    assert_eq!(
        asks.snapshot().unwrap().asks[&ask.ask_id]
            .peer_inbound
            .as_ref()
            .unwrap()
            .dead_letters
            .len(),
        100
    );
}

#[test]
fn coalescing_results_preserves_original_outbox_age() {
    let (_peers, asks, mirror, peer) = fixture();
    let ask = inbound(&asks, mirror, &peer);
    let old = (Utc::now() - chrono::Duration::minutes(59)).to_rfc3339();
    asks.with_state(|s| {
        s.asks
            .get_mut(&ask.ask_id)
            .unwrap()
            .peer_inbound
            .as_mut()
            .unwrap()
            .queued_at = Some(old.clone());
        s.update_ask(&ask.ask_id, Utc::now(), |a| {
            a.execution = ExecutionState::Running
        });
    })
    .unwrap();
    let state = asks.snapshot().unwrap();
    let i = state.asks[&ask.ask_id].peer_inbound.as_ref().unwrap();
    assert_eq!(i.queued_at.as_deref(), Some(old.as_str()));
    assert_eq!(i.peer_deliveries.len(), 1);
    assert_eq!(
        i.peer_deliveries[0].snapshot.execution,
        ExecutionState::Running
    );
}

#[test]
fn default_review_and_local_assumed_caps_ignore_widest_wire_claims() {
    let (_, ask, _) = tests::remote_record();
    let mut value =
        serde_json::to_value(wire::WireCreateV1::from(&ask.remote.unwrap().request)).unwrap();
    value["requester"]["caps"] = json!({
        "sandbox":"danger-full-access", "networkAccess":true, "unrestrictedServices":true,
        "futureAllPermissions":true,
    });
    let wire: wire::WireCreateV1 = serde_json::from_value(value).unwrap();
    let claimed = model::Create::try_from(wire).unwrap().requester.caps;
    let mut policy = super::policy::Policy::default();
    let narrow = super::policy::Caps {
        sandbox: Some("read-only".into()),
        network_access: false,
        unrestricted_services: false,
    };
    assert_eq!(policy.inbound.review, super::policy::Review::Always);
    assert_eq!(policy.inbound.assumed_caps, narrow);
    assert!(policy.needs_review(&narrow));
    assert!(policy.needs_review(&claimed));
    // ifTargetWider consults only the owner's local ceiling. The widest possible
    // peer-provided capabilities cannot replace that ceiling.
    policy.inbound.review = super::policy::Review::IfTargetWider;
    assert!(!policy.needs_review(&narrow));
    assert!(policy.needs_review(&claimed));
    assert_eq!(policy.inbound.assumed_caps, narrow);
    // An explicit local policy change is needed to trust broader target caps.
    policy.inbound.assumed_caps = claimed.clone();
    assert!(!policy.needs_review(&claimed));
    policy.inbound.review = super::policy::Review::Never;
    policy.inbound.assumed_caps = narrow;
    assert!(!policy.needs_review(&claimed));
}

#[test]
fn missing_features_pause_outbox_with_specific_peer_error_until_next_hello() {
    let (peers, asks, ask, peer) = fixture();
    let partial = json!({"instanceId":peer.peer_id,"protocol":1,"features":["asks.v1"]});
    let error = super::pair::validate_hello(&peer, &partial).unwrap_err();
    assert!(!error.retryable);
    assert!(error.is_protocol());
    worker::note_error(&peers, &peer, &error).unwrap();
    let current = peers.get(&peer.peer_id).unwrap();
    assert_eq!(current.status, "versionMismatch");
    assert!(current
        .last_error
        .as_ref()
        .unwrap()
        .contains("asks.snapshot.v1"));
    assert!(current
        .last_error
        .as_ref()
        .unwrap()
        .contains("asks.close.v1"));
    assert!(current
        .last_error
        .as_ref()
        .unwrap()
        .contains("agents.list.v1"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    peers
        .with(|s| {
            let p = s.peers.get_mut(&peer.peer_id).unwrap();
            p.urls = vec![format!("http://{}", listener.local_addr().unwrap())];
            p.next_probe_at = Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
            Ok(())
        })
        .unwrap();
    worker::maintain(&peers, &asks).unwrap();
    worker::send_peer(&peers, &asks, &peer.peer_id).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let state = asks.snapshot().unwrap();
    let a = &state.asks[&ask.ask_id];
    assert!(a.is_open());
    assert_eq!(a.hold, Some(Hold::PeerVersionMismatch));
    assert_eq!(a.remote.as_ref().unwrap().sync.pending_ops[0].op, "create");
}

#[test]
fn close_overtaking_create_persists_cancellation_and_blocks_execution_after_restart() {
    let path = std::env::temp_dir().join(format!(
        "xhost-close-before-create-{}",
        uuid::Uuid::new_v4()
    ));
    let store = AskStore::at(&path);
    let (_, ask, peer) = tests::remote_record();
    let mut request = ask.remote.unwrap().request;
    request.request_sha256 = request.digest();
    let op = model::Op {
        op_id: crate::asks::new_id("pop"),
        op: "close".into(),
        outcome: Some("abandoned".into()),
        reason: Some("expired".into()),
    };
    asks::receive_op(&store, &peer, &ask.ask_id, op.clone()).unwrap();
    let cancelled = store.snapshot().unwrap();
    assert!(cancelled.asks.is_empty());
    assert_eq!(
        cancelled.peer_cancellations.get(&ask.ask_id),
        Some(&(peer.peer_id.clone(), op.clone()))
    );
    asks::receive_op(&store, &peer, &ask.ask_id, op.clone()).unwrap();
    assert_eq!(cancelled, store.snapshot().unwrap());
    drop(store);
    let restarted = AskStore::at(&path);
    let rejected = asks::receive(&restarted, &peer, &ask.ask_id, request.clone()).unwrap_err();
    assert_eq!(rejected.status, 410);
    assert_eq!(rejected.code, "ask_gone");
    assert_eq!(cancelled, restarted.snapshot().unwrap());
    let other = tests::peer();
    assert_eq!(
        asks::receive_op(&restarted, &other, &ask.ask_id, op.clone())
            .unwrap_err()
            .status,
        409
    );
    assert_eq!(
        asks::receive(&restarted, &other, &ask.ask_id, request)
            .unwrap_err()
            .status,
        409
    );
    let mut conflict = op;
    conflict.op_id = crate::asks::new_id("pop");
    assert_eq!(
        asks::receive_op(&restarted, &peer, &ask.ask_id, conflict)
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    assert_eq!(cancelled, restarted.snapshot().unwrap());
}

#[test]
fn retention_keeps_expired_mirror_until_its_remote_close_is_acknowledged() {
    let (mut state, mut ask, _) = tests::remote_record();
    ask.execution = ExecutionState::Failed;
    ask.closed = Some(CloseOutcome::Completed);
    ask.closed_at = Some((Utc::now() - chrono::Duration::days(365)).to_rfc3339());
    let remote = ask.remote.as_mut().unwrap();
    remote.sync.local_terminal = true;
    remote.sync.pending_ops.push(model::Op {
        op_id: crate::asks::new_id("pop"),
        op: "close".into(),
        outcome: Some("abandoned".into()),
        reason: Some("expired".into()),
    });
    state.asks.insert(ask.ask_id.clone(), ask.clone());
    assert!(state.closed_before(30, Utc::now()).is_empty());
    state.prune(Utc::now());
    assert!(state.asks.contains_key(&ask.ask_id));
    state
        .asks
        .get_mut(&ask.ask_id)
        .unwrap()
        .remote
        .as_mut()
        .unwrap()
        .sync
        .pending_ops
        .clear();
    assert_eq!(
        state.closed_before(30, Utc::now()),
        vec![ask.ask_id.clone()]
    );
    state.prune(Utc::now());
    assert!(!state.asks.contains_key(&ask.ask_id));
}

#[test]
fn unchanged_reconciliation_refreshes_contact_and_prevents_false_expiry() {
    let (peers, asks, ask, peer) = fixture();
    let mut snapshot = asks::snapshot(&asks.snapshot().unwrap(), &ask);
    snapshot.revision = 5;
    snapshot.execution = ExecutionState::Running;
    snapshot.hold = None;
    asks::apply(&asks, &peer, snapshot.clone()).unwrap();
    let old = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    asks.with_state(|s| {
        let remote = s
            .asks
            .get_mut(&ask.ask_id)
            .unwrap()
            .remote
            .as_mut()
            .unwrap();
        remote.sync.last_received_at = old.clone();
        remote.sync.pending_ops.clear();
    })
    .unwrap();
    let before = asks.snapshot().unwrap();
    asks::apply(&asks, &peer, snapshot.clone()).unwrap();
    let contacted = asks.snapshot().unwrap();
    let current = &contacted.asks[&ask.ask_id];
    assert_ne!(current.remote.as_ref().unwrap().sync.last_received_at, old);
    assert_eq!(current.revision, before.asks[&ask.ask_id].revision);
    assert_eq!(contacted.deliveries, before.deliveries);
    assert_eq!(contacted.answers, before.answers);
    worker::maintain(&peers, &asks).unwrap();
    let live = asks.snapshot().unwrap();
    assert_eq!(live.asks[&ask.ask_id].execution, ExecutionState::Running);
    assert!(live.asks[&ask.ask_id].is_open());
    assert!(live.asks[&ask.ask_id]
        .remote
        .as_ref()
        .unwrap()
        .sync
        .pending_ops
        .is_empty());

    // A stale revision cannot roll back state or extend the fresh revision's TTL.
    asks.with_state(|s| {
        s.asks
            .get_mut(&ask.ask_id)
            .unwrap()
            .remote
            .as_mut()
            .unwrap()
            .sync
            .last_received_at = old.clone();
    })
    .unwrap();
    snapshot.revision -= 1;
    snapshot.execution = ExecutionState::Queued;
    asks::apply(&asks, &peer, snapshot).unwrap();
    let stale = asks.snapshot().unwrap();
    assert_eq!(stale.asks[&ask.ask_id].execution, ExecutionState::Running);
    assert_eq!(
        stale.asks[&ask.ask_id]
            .remote
            .as_ref()
            .unwrap()
            .sync
            .last_received_at,
        old
    );
}

#[test]
fn unchanged_snapshot_recovers_only_local_peer_holds_without_redelivering_answers() {
    for local_hold in [
        Hold::PeerUnreachable,
        Hold::PeerIdentityChanged,
        Hold::PeerVersionMismatch,
    ] {
        let (_peers, asks, ask, peer) = fixture();
        let mut snapshot = asks::snapshot(&asks.snapshot().unwrap(), &ask);
        snapshot.revision = 5;
        snapshot.execution = ExecutionState::Answered;
        snapshot.review = Some(ReviewState::AwaitingAcceptance);
        snapshot.hold = Some(Hold::WaitingForInput);
        snapshot.text = Some("one answer".into());
        snapshot.answer = Some(AnswerRecord {
            answer_id: "remote-answer".into(),
            ask_id: ask.ask_id.clone(),
            attempt: 1,
            revision: 1,
            status: AnswerStatus::Completed,
            artifact_id: None,
            bytes: 10,
            metadata: Default::default(),
            created_at: super::now(),
            preview: None,
        });
        asks::apply(&asks, &peer, snapshot.clone()).unwrap();
        asks.with_state(|s| {
            let a = s.asks.get_mut(&ask.ask_id).unwrap();
            a.hold = Some(local_hold);
            let remote = a.remote.as_mut().unwrap();
            remote.sync.last_error = Some("peer_unreachable".into());
            remote.sync.failures = 3;
        })
        .unwrap();
        let before = asks.snapshot().unwrap();
        asks::apply(&asks, &peer, snapshot.clone()).unwrap();
        let recovered = asks.snapshot().unwrap();
        let current = &recovered.asks[&ask.ask_id];
        assert_eq!(current.hold, snapshot.hold);
        assert_eq!(current.execution, ExecutionState::Answered);
        assert_eq!(current.review, Some(ReviewState::AwaitingAcceptance));
        assert_eq!(current.answer_id, before.asks[&ask.ask_id].answer_id);
        assert_eq!(current.remote.as_ref().unwrap().sync.state, "synced");
        assert_eq!(current.remote.as_ref().unwrap().sync.last_error, None);
        assert_eq!(current.remote.as_ref().unwrap().sync.failures, 0);
        assert_eq!(recovered.answers, before.answers);
        assert_eq!(recovered.deliveries, before.deliveries);
        assert_eq!(recovered.artifacts, before.artifacts);
        asks::apply(&asks, &peer, snapshot).unwrap();
        let repeated = asks.snapshot().unwrap();
        assert_eq!(repeated.asks[&ask.ask_id].revision, current.revision);
        assert_eq!(repeated.deliveries, recovered.deliveries);
    }
}

#[test]
fn unchanged_snapshot_preserves_nontransport_holds_and_local_terminal_state() {
    for (hold, terminal) in [
        (Hold::WaitingForInput, false),
        (
            Hold::PeerUnknown {
                raw: "future state".into(),
            },
            false,
        ),
        (Hold::PeerVersionMismatch, true),
    ] {
        let (_peers, asks, ask, peer) = fixture();
        let mut snapshot = asks::snapshot(&asks.snapshot().unwrap(), &ask);
        snapshot.revision = 5;
        snapshot.execution = ExecutionState::Running;
        snapshot.hold = None;
        asks::apply(&asks, &peer, snapshot.clone()).unwrap();
        let old = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        asks.with_state(|s| {
            let a = s.asks.get_mut(&ask.ask_id).unwrap();
            a.hold = Some(hold.clone());
            if terminal {
                a.execution = ExecutionState::Interrupted;
                a.closed = Some(CloseOutcome::Stopped);
                a.remote.as_mut().unwrap().sync.local_terminal = true;
            }
            a.remote.as_mut().unwrap().sync.last_received_at = old.clone();
        })
        .unwrap();
        let before = asks.snapshot().unwrap();
        asks::apply(&asks, &peer, snapshot).unwrap();
        let after = asks.snapshot().unwrap();
        let mut current = after.asks[&ask.ask_id].clone();
        assert_ne!(current.remote.as_ref().unwrap().sync.last_received_at, old);
        current.remote.as_mut().unwrap().sync.last_received_at = old;
        assert_eq!(current, before.asks[&ask.ask_id]);
        assert_eq!(after.answers, before.answers);
        assert_eq!(after.deliveries, before.deliveries);
    }
}
