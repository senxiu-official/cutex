//! r226: stable peer change keys, borrowed statistics and no ask-store notice writes.
use super::{model, routes, store, tests, token};
use crate::asks::{
    model::{AskRecord, CloseOutcome, Requester},
    store::AskStore,
};
use serde_json::json;

#[test]
fn pending_outbox_uses_stable_timestamp_and_observes_real_changes_without_writes() {
    let root = std::env::temp_dir().join(format!("peer-view-review-{}", uuid::Uuid::new_v4()));
    let asks = AskStore::at(root.join("asks"));
    let (mut state, mut ask, peer) = tests::remote_record();
    ask.created_at = "2026-01-01T00:00:00Z".into();
    ask.remote
        .as_mut()
        .unwrap()
        .sync
        .pending_ops
        .push(model::Op {
            op_id: "pop-create".into(),
            op: "create".into(),
            outcome: None,
            reason: None,
        });
    state.asks.insert(ask.ask_id.clone(), ask.clone());
    asks.with_state(|s| *s = state).unwrap();
    let peers = store::State {
        settings: super::policy::Settings {
            enabled: true,
            ..Default::default()
        },
        peers: std::collections::BTreeMap::from([(peer.peer_id.clone(), peer.clone())]),
        ..Default::default()
    };
    let path = asks.dir().join("asks-v1.json");
    let before = std::fs::read(&path).unwrap();
    let stamp = crate::platform::file_stamp::FileStamp::of(&path);
    let view = routes::peer_views(&peers, &asks)
        .unwrap()
        .remove(&peer.peer_id)
        .unwrap();
    assert_eq!(view["outboxPending"], 1);
    assert_eq!(
        crate::asks::parse_time(view["outboxOldestAt"].as_str().unwrap()),
        crate::asks::parse_time(&ask.created_at)
    );
    assert_eq!(view["inflight"]["outbound"], 1);
    let mut later = view.clone();
    later["outboxOldestAgeSecs"] = json!(view["outboxOldestAgeSecs"].as_i64().unwrap() + 60);
    later["outbox"]["oldestAgeSecs"] = later["outboxOldestAgeSecs"].clone();
    assert_eq!(
        routes::peer_change_key(&view),
        routes::peer_change_key(&later)
    );
    later["outboxOldestAt"] = json!("2026-01-02T00:00:00Z");
    assert_ne!(
        routes::peer_change_key(&view),
        routes::peer_change_key(&later)
    );
    for _ in 0..10 {
        routes::peer_views(&peers, &asks).unwrap();
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(crate::platform::file_stamp::FileStamp::of(&path), stamp);
    AskStore::at(asks.dir())
        .with_state(|s| {
            s.asks
                .get_mut(&ask.ask_id)
                .unwrap()
                .remote
                .as_mut()
                .unwrap()
                .sync
                .pending_ops
                .clear()
        })
        .unwrap();
    let after = routes::peer_views(&peers, &asks)
        .unwrap()
        .remove(&peer.peer_id)
        .unwrap();
    assert_eq!(after["outboxPending"], 0);
    assert!(after["outboxOldestAt"].is_null());
    assert_ne!(
        routes::peer_change_key(&view),
        routes::peer_change_key(&after)
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn peer_changed_never_creates_or_mutates_ask_store_notices() {
    use crate::config::instance::{self, Selection};
    struct Restore(Selection, std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            instance::set_test_selection(self.0.clone());
            if self.1.exists() {
                let removed = std::fs::remove_dir_all(&self.1);
                if !std::thread::panicking() {
                    removed.expect("remove private peer-events-review directory");
                }
            }
        }
    }
    let root = crate::config::paths::home_dir()
        .unwrap()
        .join(format!("peer-events-review-{}", uuid::Uuid::new_v4()));
    let _restore = Restore(instance::selected().unwrap(), root.clone());
    instance::set_test_selection(Selection::Custom(root.join(".cutex")));
    let asks = AskStore::open_default().unwrap();
    let repository = crate::management::v2::repository::management_v2_repository().unwrap();
    let checkpoint = repository.checkpoint().unwrap();
    let peer = format!("ci-{}", uuid::Uuid::new_v4().simple());
    routes::changed(&peer, "first");
    assert!(!asks.dir().exists(), "host event must not initialize asks");
    let page = repository
        .page(crate::management::v2::repository::ReplayQuery {
            stream_id: Some(checkpoint.stream_id),
            after: checkpoint.cursor,
            limit: 1000,
            cutex_session_id: Some("cutex.host".into()),
        })
        .unwrap();
    let published: Vec<_> = page
        .events
        .iter()
        .filter(|event| {
            event.cutex.as_ref().is_some_and(|message| {
                message.method == "cutex/peers/changed" && message.params["peerId"] == peer
            })
        })
        .collect();
    assert_eq!(
        published.len(),
        1,
        "exactly one host change event must be published"
    );
    assert!(published[0].sequence > checkpoint.sequence);
    assert_eq!(published[0].cutex_session_id, "cutex.host");
    assert_eq!(
        published[0].cutex.as_ref().unwrap().params["reason"],
        "first"
    );
    asks.with_state(|s| s.next_seq = 1).unwrap();
    let path = asks.dir().join("asks-v1.json");
    let stamp = crate::platform::file_stamp::FileStamp::of(&path);
    let before = std::fs::read(&path).unwrap();
    routes::changed(&peer, "state");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(crate::platform::file_stamp::FileStamp::of(&path), stamp);
    assert!(asks.snapshot().unwrap().notices.is_empty());
}

#[test]
fn absent_peer_authentication_and_view_are_read_only() {
    let root = std::env::temp_dir().join(format!("peer-read-review-{}", uuid::Uuid::new_v4()));
    let peers = store::Store::at(root.join("peers"));
    assert_eq!(
        peers
            .authenticate(&token::generate(), &tests::peer().peer_id)
            .unwrap_err()
            .status,
        401
    );
    let asks = AskStore::at(root.join("asks"));
    assert!(routes::peer_views(&peers.snapshot().unwrap(), &asks)
        .unwrap()
        .is_empty());
    assert!(!root.exists());
}

fn outbound(
    base: &AskRecord,
    id: &str,
    peer: &store::Peer,
    created_at: &str,
    count: usize,
    closed: bool,
) -> AskRecord {
    let mut ask = base.clone();
    ask.ask_id = id.into();
    ask.created_at = created_at.into();
    if closed {
        ask.closed = Some(CloseOutcome::Completed);
    }
    let remote = ask.remote.as_mut().unwrap();
    remote.peer_id = peer.peer_id.clone();
    remote.sync.pending_ops = (0..count)
        .map(|n| model::Op {
            op_id: format!("pop-{id}-{n}"),
            op: "close".into(),
            outcome: Some("abandoned".into()),
            reason: None,
        })
        .collect();
    ask
}

fn inbound(
    base: &AskRecord,
    id: &str,
    peer: &store::Peer,
    queued_at: Option<&str>,
    dead: usize,
    closed: bool,
) -> AskRecord {
    let mut ask = base.clone();
    ask.ask_id = id.into();
    ask.created_at = "2025-01-01T00:00:00Z".into();
    if closed {
        ask.closed = Some(CloseOutcome::Completed);
    }
    let request = ask.remote.take().unwrap().request;
    ask.requester = Requester::Peer {
        peer_id: peer.peer_id.clone(),
        cutex_session_id: "cutex.remote".into(),
        name: None,
        label: peer.label.clone(),
    };
    let event = model::Event {
        event_id: format!("pev-{id}"),
        ask_id: id.into(),
        revision: 1,
        snapshot: model::Snapshot {
            ask_id: id.into(),
            revision: 1,
            execution: ask.execution.clone(),
            hold: None,
            closed: ask.closed.clone(),
            review: None,
            answer: None,
            text: None,
            interrupt_reason: None,
            last_error: None,
            remote_state: None,
        },
    };
    ask.peer_inbound = Some(model::Inbound {
        request,
        peer_deliveries: queued_at.map(|_| vec![event.clone()]).unwrap_or_default(),
        queued_at: queued_at.map(str::to_string),
        dead_letters: (0..dead)
            .map(|n| {
                let mut event = event.clone();
                event.event_id.push_str(&format!("-{n}"));
                model::DeadLetter {
                    event,
                    failed_at: "2026-01-05T00:00:00Z".into(),
                    code: "peer_event_expired".into(),
                }
            })
            .collect(),
        result_undeliverable: dead != 0,
        delivery_stopped: queued_at.is_none(),
        received_ops: vec![],
        next_attempt_at: None,
        failures: 0,
        last_error: None,
    });
    ask
}

#[test]
fn batched_peer_views_keep_each_peers_inbound_outbound_and_dead_letters_separate() {
    let root = std::env::temp_dir().join(format!("peer-batched-view-{}", uuid::Uuid::new_v4()));
    let asks = AskStore::at(root.join("asks"));
    let (mut state, base, first) = tests::remote_record();
    let mut second = tests::peer();
    second.label = "second".into();
    let mut empty = tests::peer();
    empty.label = "empty".into();
    let unrelated = tests::peer();
    let mut local = base.clone();
    local.ask_id = "ask-local".into();
    local.remote = None;
    let records = [
        outbound(
            &base,
            "ask-first-open",
            &first,
            "2026-01-02T00:00:00Z",
            2,
            false,
        ),
        inbound(
            &base,
            "ask-first-inbound",
            &first,
            Some("2026-01-03T00:00:00Z"),
            1,
            false,
        ),
        outbound(
            &base,
            "ask-second-closed",
            &second,
            "2026-01-06T00:00:00Z",
            1,
            true,
        ),
        inbound(
            &base,
            "ask-second-open",
            &second,
            Some("2026-01-01T00:00:00Z"),
            0,
            false,
        ),
        inbound(&base, "ask-second-dead", &second, None, 2, true),
        outbound(
            &base,
            "ask-unlisted",
            &unrelated,
            "2024-01-01T00:00:00Z",
            5,
            false,
        ),
        local,
    ];
    state.asks = records
        .into_iter()
        .map(|ask| (ask.ask_id.clone(), ask))
        .collect();
    // Persist the explicit fixture without queue_snapshots coalescing its outboxes.
    crate::config::atomic::write_private_pretty_json_atomic(
        &asks.dir().join("asks-v1.json"),
        &state,
        "test asks",
    )
    .unwrap();
    let peers = store::State {
        settings: super::policy::Settings {
            enabled: true,
            ..Default::default()
        },
        peers: [first.clone(), second.clone(), empty.clone()]
            .into_iter()
            .map(|peer| (peer.peer_id.clone(), peer))
            .collect(),
        ..Default::default()
    };
    let views = routes::peer_views(&peers, &asks).unwrap();
    assert_eq!(views.len(), 3);
    for (peer, pending, dead, outbound, inbound, oldest) in [
        (&first, 3, 1, 1, 1, Some("2026-01-02T00:00:00Z")),
        (&second, 2, 2, 0, 1, Some("2026-01-01T00:00:00Z")),
        (&empty, 0, 0, 0, 0, None),
    ] {
        let view = &views[&peer.peer_id];
        assert_eq!(view["outboxPending"], pending);
        assert_eq!(view["outbox"]["pending"], pending);
        assert_eq!(view["deadLetterCount"], dead);
        assert_eq!(
            view["inflight"],
            json!({"outbound":outbound,"inbound":inbound})
        );
        assert_eq!(
            view["outboxOldestAt"]
                .as_str()
                .and_then(crate::asks::parse_time),
            oldest.and_then(crate::asks::parse_time)
        );
        assert_eq!(view["outboxOldestAt"], view["outbox"]["oldestAt"]);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn inspect_observes_external_atomic_replacement_after_cache_was_populated() {
    let root = std::env::temp_dir().join(format!("peer-inspect-replace-{}", uuid::Uuid::new_v4()));
    let asks = AskStore::at(root.join("asks"));
    asks.with_state(|state| state.next_seq = 1717).unwrap();
    assert_eq!(asks.inspect(|state| state.next_seq).unwrap(), 1717);
    let path = asks.dir().join("asks-v1.json");
    let mut external: crate::asks::store::AskState =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    external.next_seq = 9182;
    external.revision += 1;
    // No AskStore write path or process-local cache invalidation participates.
    let replacement = asks.dir().join("external-asks.json");
    let replacement_bytes = serde_json::to_vec_pretty(&external).unwrap();
    assert_eq!(
        replacement_bytes.len(),
        std::fs::metadata(&path).unwrap().len() as usize
    );
    std::fs::write(&replacement, replacement_bytes).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    assert_eq!(
        asks.inspect(|state| (state.next_seq, state.revision))
            .unwrap(),
        (9182, external.revision)
    );
    assert_eq!(asks.inspect(|state| state.next_seq).unwrap(), 9182);
    std::fs::remove_dir_all(root).unwrap();
}
