use super::*;
use crate::asks::{
    model::*,
    store::{AskState, AskStore},
};
use serde_json::json;

pub(super) fn peer() -> store::Peer {
    store::Peer {
        peer_id: format!("ci-{}", uuid::Uuid::new_v4().simple()),
        user_id: store::OWNER_USER_ID.into(),
        label: "beta".into(),
        host_id: "same-host".into(),
        platform: "windows".into(),
        urls: vec!["http://127.0.0.1:26370".into()],
        enabled: true,
        inbound_token_sha256: String::new(),
        policy: Default::default(),
        policy_revision: 1,
        protocol: 1,
        created_at: now(),
        last_seen_at: None,
        last_error: None,
        status: "online".into(),
        next_probe_at: None,
        probe_failures: 0,
        pending_until: None,
        features: pair::FEATURES.iter().map(|s| s.to_string()).collect(),
        negotiated_features: pair::FEATURES.iter().map(|s| s.to_string()).collect(),
        last_successful_url: None,
        allow_loopback: true,
    }
}
#[test]
fn credentials_are_bound_to_enabled_instance_and_never_in_views() {
    let dir = std::env::temp_dir().join(format!("xhost-token-{}", uuid::Uuid::new_v4()));
    let store = store::Store::at(dir.join("runtime/peers"));
    let mut p = peer();
    let inbound = token::generate();
    let outbound = token::generate();
    p.inbound_token_sha256 = token::hash(&inbound);
    store.insert(p.clone(), &outbound).unwrap();
    store.settings(&json!({"enabled":true})).unwrap();
    assert!(store.authenticate(&inbound, &p.peer_id).is_ok());
    assert!(store.authenticate(&outbound, &p.peer_id).is_err());
    assert!(store.authenticate(&inbound, &peer().peer_id).is_err());
    let disk = std::fs::read_to_string(store.dir.join("peers.json")).unwrap();
    assert!(!disk.contains(&inbound));
    assert!(!disk.contains(&outbound));
    assert!(!p.view().to_string().contains(&p.inbound_token_sha256));
    assert_eq!(store.secret(&p.peer_id).unwrap(), outbound);
    store
        .with(|s| {
            s.peers.get_mut(&p.peer_id).unwrap().enabled = false;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store.authenticate(&inbound, &p.peer_id).unwrap_err().status,
        401
    );
    assert!(store.secret_path("../../other").is_err());
}
#[test]
fn review_truth_table_ignores_host_local_paths() {
    use policy::{needs_review, Caps, Review};
    let levels = [
        None,
        Some("read-only"),
        Some("workspace-write"),
        Some("danger-full-access"),
    ];
    for (i, a) in levels.iter().enumerate() {
        for (j, b) in levels.iter().enumerate() {
            for bits in 0..16 {
                let a = Caps {
                    sandbox: a.map(str::to_string),
                    network_access: bits & 1 != 0,
                    unrestricted_services: bits & 2 != 0,
                };
                let b = Caps {
                    sandbox: b.map(str::to_string),
                    network_access: bits & 4 != 0,
                    unrestricted_services: bits & 8 != 0,
                };
                assert_eq!(
                    needs_review(&Review::IfTargetWider, &a, &b),
                    i == 0
                        || j == 0
                        || j > i
                        || b.network_access && !a.network_access
                        || b.unrestricted_services && !a.unrestricted_services
                );
                assert!(needs_review(&Review::Always, &a, &b));
                assert!(!needs_review(&Review::Never, &a, &b));
            }
        }
    }
}
#[test]
fn peer_gate_on_loopback_is_as_strict_as_remote() {
    use std::collections::HashMap;
    for remote in [false, true] {
        for bearer in [None, Some("root"), Some("cxm1.fake")] {
            let mut headers = HashMap::new();
            if let Some(b) = bearer {
                headers.insert("authorization".into(), format!("Bearer {b}"));
            }
            let request = crate::http::server::SimpleHttpRequest {
                method: "GET".into(),
                path: "/peer/v1/agents".into(),
                headers,
                body: vec![],
            };
            assert_eq!(
                crate::management::server::remote_gate(remote, &request, "/peer/v1/agents")
                    .unwrap()
                    .2,
                "peer_credential_required"
            );
        }
        let request = crate::http::server::SimpleHttpRequest {
            method: "GET".into(),
            path: "/peer/v1/agents".into(),
            headers: HashMap::from([(
                "authorization".into(),
                format!("Bearer {}", token::generate()),
            )]),
            body: vec![],
        };
        assert!(
            crate::management::server::remote_gate(remote, &request, "/peer/v1/agents").is_none()
        );
        assert!(crate::management::server::remote_gate(remote, &request, "/v2/asks").is_some());
    }
}
#[test]
fn exports_are_explicit_and_settings_fail_closed() {
    let mut p = peer();
    let mut a = crate::asks::directory::tests::agent("cutex.a", "worker");
    assert!(!policy::visible(&p, &a));
    p.policy = p
        .policy
        .patched(
            &json!({"inbound":{"exports":[{"agent":"cutex.a"}]},"outbound":{"visibility":"all"}}),
        )
        .unwrap();
    assert!(policy::visible(&p, &a));
    a.archived = true;
    assert!(!policy::visible(&p, &a));
    assert!(p
        .policy
        .patched(&json!({"inbound":{"maxOpenAsks":0}}))
        .is_err());
    assert!(p
        .policy
        .patched(&json!({"outbound":{"visibility":"grants"}}))
        .is_err());
    for patch in [
        json!({"outboxMaxAgeHours":0}),
        json!({"maxHops":9}),
        json!({"refreshAgentsSecs":1}),
        json!({"enabled":"yes"}),
        json!({"allowPlainLan":true}),
    ] {
        assert!(policy::Settings::default().patched(&patch).is_err());
    }
}
#[test]
fn enabling_does_not_create_a_stale_ask_backup() {
    let dir = std::env::temp_dir().join(format!("xhost-backup-{}", uuid::Uuid::new_v4()));
    let store = store::Store::at(dir.join("runtime/peers"));
    let path = dir.join("runtime/asks/asks-v1.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"original").unwrap();
    store.settings(&json!({"enabled":true})).unwrap();
    store.settings(&json!({"enabled":false})).unwrap();
    std::fs::write(&path, b"changed").unwrap();
    store.settings(&json!({"enabled":true})).unwrap();
    assert!(!path.with_file_name("asks-v1.json.pre-peers").exists());
}
pub(super) fn remote_record() -> (AskState, AskRecord, store::Peer) {
    let p = peer();
    let new = crate::asks::store::tests::new_ask("xhost", "hello");
    let mut s = AskState::default();
    let (mut a, _) = s.create_ask(new, chrono::Utc::now()).unwrap();
    a.remote = Some(model::Remote {
        remote_state: None,
        late_answer: None,
        peer_id: p.peer_id.clone(),
        label: p.label.clone(),
        target_name: "worker".into(),
        request: model::Create {
            requester: model::Requester {
                cutex_session_id: "director".into(),
                name: None,
                caps: Default::default(),
            },
            target: a.target_cutex_session_id.clone(),
            title: "hello".into(),
            text: "hello".into(),
            options: a.options.clone(),
            hops: 0,
            request_sha256: String::new(),
        },
        sync: model::Sync {
            local_terminal: false,
            reconcile_at: None,
            interrupt_reason: None,
            state: "queued".into(),
            last_remote_revision: 0,
            pending_ops: vec![],
            last_error: None,
            next_attempt_at: None,
            last_received_at: now(),
            failures: 0,
            accepted: false,
        },
    });
    s.asks.insert(a.ask_id.clone(), a.clone());
    (s, a, p)
}
#[test]
fn normalized_peer_diagnostic_noop_preserves_store_revision_and_file_stamp() {
    let (store, dir) = crate::asks::store::tests::temp_store("peer-diagnostic-noop");
    let (mut initial, ask, _) = remote_record();
    initial.asks.get_mut(&ask.ask_id).unwrap().remote.as_mut().unwrap().sync.last_error =
        Some("  temporary\n  failure  ".into());
    store.with_state(|state| *state = initial).unwrap();
    let path = dir.join(crate::management::v2::hot_store::DATABASE_FILE);
    let before = store.snapshot().unwrap();
    let before_generation = crate::management::v2::hot_store::generation(&path, "asks").unwrap();
    assert_eq!(
        before.asks[&ask.ask_id].remote.as_ref().unwrap().sync.last_error.as_deref(),
        Some("temporary failure")
    );
    // Structurally different, but the diagnostic serializer emits the same
    // bytes. Such a mutation must remain a no-op even after local writes use
    // a faster structural change check.
    store.with_state(|state| {
        state.asks.get_mut(&ask.ask_id).unwrap().remote.as_mut().unwrap().sync.last_error =
            Some("\ttemporary   failure\r\n".into());
    }).unwrap();
    assert_eq!(crate::management::v2::hot_store::generation(&path, "asks").unwrap(), before_generation);
    let after = store.snapshot().unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.asks[&ask.ask_id].remote.as_ref().unwrap().sync.last_error.as_deref(),
        Some("temporary failure"),
        "reads must not return the unnormalized mutation from a write-through cache"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn remote_mirrors_do_not_enter_local_queue_even_with_colliding_ids() {
    let (s, a, _) = remote_record();
    assert!(s.waiting_for_target(&a.target_cutex_session_id).is_empty());
    assert_eq!(
        AskState::ask_sessions(&a),
        vec![a.requester.session().unwrap().to_string()]
    );
    let remote = Requester::Peer {
        peer_id: peer().peer_id,
        cutex_session_id: a.requester.session().unwrap().into(),
        name: None,
        label: "alpha".into(),
    };
    assert!(!asks::same_requester(&a.requester, &remote));
    assert!(!asks::same_requester(&Requester::Owner, &remote));
    assert!(remote.session().is_none());
}
#[test]
fn duplicate_and_out_of_order_snapshots_create_one_local_answer_delivery() {
    let (s, a, p) = remote_record();
    let store =
        AskStore::at(std::env::temp_dir().join(format!("xhost-snapshot-{}", uuid::Uuid::new_v4())));
    store.with_state(|v| *v = s.clone()).unwrap();
    let mut snapshot = asks::snapshot(&s, &a);
    snapshot.revision = 5;
    snapshot.execution = ExecutionState::Answered;
    snapshot.closed = Some(CloseOutcome::Completed);
    snapshot.text = Some("XHOST-answer".into());
    snapshot.answer = Some(AnswerRecord {
        answer_id: "untrusted-id".into(),
        ask_id: a.ask_id.clone(),
        attempt: 1,
        revision: 1,
        status: AnswerStatus::Completed,
        artifact_id: None,
        bytes: 0,
        metadata: Default::default(),
        created_at: now(),
        preview: None,
    });
    asks::apply(&store, &p, snapshot.clone()).unwrap();
    asks::apply(&store, &p, snapshot.clone()).unwrap();
    snapshot.revision = 3;
    snapshot.execution = ExecutionState::Running;
    asks::apply(&store, &p, snapshot.clone()).unwrap();
    let state = store.snapshot().unwrap();
    assert_eq!(state.answers.len(), 1);
    assert_eq!(state.deliveries.len(), 1);
    assert_eq!(state.asks[&a.ask_id].execution, ExecutionState::Answered);
    let answer = state.answers.values().next().unwrap();
    assert_ne!(answer.answer_id, "untrusted-id");
    assert_eq!(
        store
            .read_artifact(answer.artifact_id.as_ref().unwrap(), a.requester.session())
            .unwrap()
            .unwrap()
            .1,
        b"XHOST-answer"
    );
    let result = crate::asks::waits::wait_blocking(
        &store,
        a.requester.session().unwrap(),
        &crate::asks::waits::WaitRequest {
            asks: vec![a.ask_id.clone()],
            all: true,
            timeout_ms: 0,
            action_id: "wait-remote-answer".into(),
        },
        &chrono::Utc::now,
        &|| panic!("answer should be immediately available"),
    )
    .unwrap();
    assert!(result.to_string().contains("XHOST-answer"));
    snapshot.revision = 6;
    assert!(asks::apply(&store, &peer(), snapshot).is_err());
}
#[test]
fn incoming_state_change_and_outbox_are_one_store_write() {
    let (mut s, mut a, p) = remote_record();
    let request = a.remote.take().unwrap().request;
    a.requester = Requester::Peer {
        peer_id: p.peer_id,
        cutex_session_id: "director".into(),
        name: None,
        label: "alpha".into(),
    };
    a.peer_inbound = Some(model::Inbound {
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
    s.asks.insert(a.ask_id.clone(), a.clone());
    let store =
        AskStore::at(std::env::temp_dir().join(format!("xhost-outbox-{}", uuid::Uuid::new_v4())));
    store.with_state(|v| *v = s).unwrap();
    store
        .with_state(|s| {
            s.update_ask(&a.ask_id, chrono::Utc::now(), |a| {
                a.execution = ExecutionState::Running
            });
        })
        .unwrap();
    let s = store.snapshot().unwrap();
    let a = &s.asks[&a.ask_id];
    let events = &a.peer_inbound.as_ref().unwrap().peer_deliveries;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].revision, a.revision);
    assert_eq!(events[0].snapshot.execution, ExecutionState::Running);
    assert!(s.deliveries.is_empty());
}

#[test]
fn operation_receipt_and_stop_are_atomic_and_replayable() {
    let (mut s, mut a, p) = remote_record();
    let request = a.remote.take().unwrap().request;
    a.requester = Requester::Peer {
        peer_id: p.peer_id.clone(),
        cutex_session_id: "director".into(),
        name: None,
        label: "alpha".into(),
    };
    a.peer_inbound = Some(model::Inbound {
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
    s.asks.insert(a.ask_id.clone(), a.clone());
    let store =
        AskStore::at(std::env::temp_dir().join(format!("xhost-op-{}", uuid::Uuid::new_v4())));
    store.with_state(|v| *v = s).unwrap();
    let op = model::Op {
        reason: None,
        op_id: "unique-op".into(),
        op: "stop".into(),
        outcome: None,
    };
    asks::receive_op(&store, &p, &a.ask_id, op.clone()).unwrap();
    let first = store.snapshot().unwrap();
    assert_eq!(first.asks[&a.ask_id].execution, ExecutionState::Cancelled);
    assert_eq!(
        first.asks[&a.ask_id]
            .peer_inbound
            .as_ref()
            .unwrap()
            .received_ops,
        vec![op.clone()]
    );
    asks::receive_op(&store, &p, &a.ask_id, op.clone()).unwrap();
    assert_eq!(first, store.snapshot().unwrap());
    let mut conflicting = op;
    conflicting.op = "close".into();
    assert_eq!(
        asks::receive_op(&store, &p, &a.ask_id, conflicting)
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    assert_eq!(first, store.snapshot().unwrap());
}
#[test]
fn peer_review_policy_versions_invalidate_the_approval() {
    use crate::asks::gate::*;
    let (_, ask, _) = remote_record();
    let party = GateParty {
        cutex_session_id: "target".into(),
        name: None,
        caps: crate::asks::caps::Capabilities::new(None, None),
        profile: None,
        approval_policy: None,
    };
    let mut inputs = GateInputs {
        requester: Some(party.clone()),
        target: party,
        project_policy: "peer-review-skipped-v1".into(),
        links_version: "policy-1".into(),
    };
    assert!(!evaluate(&ask_facts(&ask), &inputs).required);
    inputs.project_policy = "peer-review-required-v1".into();
    let store = crate::approvals::ApprovalStore::at(
        std::env::temp_dir()
            .join(format!("xhost-gate-{}", uuid::Uuid::new_v4()))
            .join("approvals.json"),
    );
    let now = chrono::Utc::now();
    let GateVerdict::Review { approval, .. } = check(&store, &ask, &inputs, false, now).unwrap()
    else {
        panic!("always must review")
    };
    store
        .decide(&approval.approval_id, true, None, now)
        .unwrap();
    assert!(matches!(
        check(&store, &ask, &inputs, false, now).unwrap(),
        GateVerdict::Pass(_)
    ));
    inputs.links_version = "policy-2".into();
    assert!(matches!(
        check(&store, &ask, &inputs, false, now).unwrap(),
        GateVerdict::Review { created: true, .. }
    ));
}
#[test]
fn failed_handshake_aborts_remote_and_removes_local_pending_record() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let id = peer().peer_id;
    let token = token::generate();
    let server = std::thread::spawn(move || {
        let mut paths = vec![];
        for n in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut buf = vec![];
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                buf.push(byte[0]);
            }
            let head = String::from_utf8(buf).unwrap();
            let path = head.split_whitespace().nth(1).unwrap().to_string();
            paths.push(path);
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; len];
            stream.read_exact(&mut body).unwrap();
            let value = if n == 2 {
                json!({"removed":true})
            } else {
                json!({"instanceId":id,"hostId":"fake","label":"fake","platform":"test","protocol":if n==0{1}else{99},"features":pair::FEATURES,"tokenForYou":token})
            };
            let body = value.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
        paths
    });
    let store =
        store::Store::at(std::env::temp_dir().join(format!("xhost-pair-{}", uuid::Uuid::new_v4())));
    assert_eq!(
        pair::add(&store, &json!({"url":url,"code":"fake","label":"beta"}))
            .unwrap_err()
            .code,
        "peer_unsupported"
    );
    assert!(store.snapshot().unwrap().peers.is_empty());
    assert_eq!(
        server.join().unwrap(),
        vec!["/peer/v1/pair", "/peer/v1/hello", "/peer/v1/abort"]
    );
}
#[test]
fn expired_pending_credentials_and_duplicate_labels_fail_closed() {
    let store = store::Store::at(
        std::env::temp_dir().join(format!("xhost-pending-{}", uuid::Uuid::new_v4())),
    );
    let mut p = peer();
    let t = token::generate();
    p.inbound_token_sha256 = token::hash(&t);
    p.pending_until = Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
    store.insert(p.clone(), &t).unwrap();
    assert_eq!(store.authenticate(&t, &p.peer_id).unwrap_err().status, 401);
    assert_eq!(store.insert(p.clone(), &t).unwrap_err().code, "peer_exists");
    assert_eq!(store.insert(peer(), &t).unwrap_err().code, "label_taken");
}

#[test]
fn outbox_expiry_and_unknown_outcome_are_durable_without_repeat_delivery() {
    for accepted in [false, true] {
        let (mut s, mut a, mut p) = remote_record();
        let past = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        a.created_at = past.clone();
        let r = a.remote.as_mut().unwrap();
        r.sync.accepted = accepted;
        r.sync.last_received_at = past;
        r.sync.pending_ops = if accepted {
            vec![]
        } else {
            vec![model::Op {
                reason: None,
                op_id: "create".into(),
                op: "create".into(),
                outcome: None,
            }]
        };
        p.last_seen_at = Some(now());
        p.urls = vec!["http://127.0.0.1:0".into()];
        s.asks.insert(a.ask_id.clone(), a.clone());
        let dir = std::env::temp_dir().join(format!("xhost-expire-{}", uuid::Uuid::new_v4()));
        let peers = store::Store::at(dir.join("runtime/peers"));
        peers.insert(p.clone(), &token::generate()).unwrap();
        peers.settings(&json!({"enabled":true})).unwrap();
        let asks = AskStore::at(dir.join("runtime/asks"));
        asks.with_state(|v| *v = s).unwrap();
        worker::tick(&peers, &asks).unwrap();
        let first = asks.snapshot().unwrap();
        let a = &first.asks[&a.ask_id];
        assert_eq!(
            a.execution,
            if accepted {
                ExecutionState::Interrupted
            } else {
                ExecutionState::Failed
            }
        );
        assert_eq!(
            a.remote.as_ref().unwrap().sync.interrupt_reason,
            accepted.then_some(InterruptReason::OutcomeUnknown)
        );
        worker::tick(&peers, &asks).unwrap();
        assert_eq!(
            first.deliveries.len(),
            asks.snapshot().unwrap().deliveries.len()
        );
    }
}
#[test]
fn removal_interrupts_mirrors_and_clears_their_outbox() {
    let (mut s, mut a, p) = remote_record();
    a.remote.as_mut().unwrap().sync.pending_ops.push(model::Op {
        reason: None,
        op_id: "create".into(),
        op: "create".into(),
        outcome: None,
    });
    s.asks.insert(a.ask_id.clone(), a.clone());
    let store =
        AskStore::at(std::env::temp_dir().join(format!("xhost-remove-{}", uuid::Uuid::new_v4())));
    store.with_state(|v| *v = s).unwrap();
    worker::removed(&store, &p.peer_id).unwrap();
    let s = store.snapshot().unwrap();
    let a = &s.asks[&a.ask_id];
    assert_eq!(a.execution, ExecutionState::Interrupted);
    assert_eq!(
        a.remote.as_ref().unwrap().sync.interrupt_reason,
        Some(InterruptReason::PeerRemoved)
    );
    assert!(a.remote.as_ref().unwrap().sync.pending_ops.is_empty());
    assert_eq!(s.deliveries.len(), 1);
}
#[test]
fn peer_schemas_accept_actual_wire_shapes() {
    let (s, mut a, p) = remote_record();
    crate::management::v2::schema_export::assert_valid("peers", "peer", &p.view());
    crate::management::v2::schema_export::assert_valid(
        "peers",
        "snapshot",
        &serde_json::to_value(wire::WireSnapshotV1::from(&asks::snapshot(&s, &a))).unwrap(),
    );
    crate::management::v2::schema_export::assert_valid(
        "peers",
        "settings",
        &serde_json::to_value(policy::Settings::default()).unwrap(),
    );
    a.requester = Requester::Peer {
        peer_id: p.peer_id,
        label: "alpha".into(),
        cutex_session_id: "remote-session".into(),
        name: None,
    };
    crate::management::v2::schema_export::assert_valid(
        "asks",
        "askSummary",
        &crate::asks::view::summary(&s, &a),
    );
}
