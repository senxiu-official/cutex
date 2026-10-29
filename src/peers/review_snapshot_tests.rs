//! Remote prose stays diagnostic; answer bodies stay in the artifact store.
use super::{asks, model, store::Peer, tests, wire};
use crate::asks::{model::*, store::AskStore, view};
use chrono::Utc;
use serde_json::{json, Value};
use std::path::PathBuf;

fn fixture() -> (PathBuf, AskStore, AskRecord, Peer) {
    let root = std::env::temp_dir().join(format!("xhost-snapshot-review-{}", uuid::Uuid::new_v4()));
    let store = AskStore::at(root.clone());
    let (state, ask, peer) = tests::remote_record();
    store.with_state(|s| *s = state).unwrap();
    (root, store, ask, peer)
}

fn hostile_error() -> String {
    format!(
        "remote-instruction@99\r\n\t\u{1b}[31m\u{061c}\u{200e}\u{200f}\u{202e}\u{2066}{}",
        "界".repeat(400)
    )
}

fn assert_diagnostic(value: &str) {
    assert!(value.len() <= 512, "{} bytes", value.len());
    assert!(!value.chars().any(|c| c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')));
    assert!(!value.contains('\n') && !value.contains('\r'));
}

fn answer_snapshot(
    store: &AskStore,
    ask: &AskRecord,
    revision: u64,
    text: &str,
) -> model::Snapshot {
    let mut snapshot = asks::snapshot(&store.snapshot().unwrap(), ask);
    snapshot.revision = revision;
    snapshot.execution = ExecutionState::Answered;
    snapshot.closed = Some(CloseOutcome::Completed);
    snapshot.text = Some(text.into());
    snapshot.last_error = Some(hostile_error());
    snapshot.answer = Some(AnswerRecord {
        answer_id: "remote-answer-id".into(),
        ask_id: ask.ask_id.clone(),
        attempt: 1,
        revision: 1,
        status: AnswerStatus::Completed,
        artifact_id: None,
        bytes: text.len(),
        metadata: AnswerMetadata {
            last_error: Some(hostile_error()),
            ..Default::default()
        },
        created_at: super::now(),
        preview: Some(text.repeat(2)),
    });
    snapshot
}

fn body() -> String {
    format!(
        "{}REMOTE_FULL_TEXT_ONLY_IN_ARTIFACT\n{}",
        "short preview ".repeat(50),
        "完整正文".repeat(2000)
    )
}

fn assert_metadata_views(store: &AskStore, ask_id: &str) {
    let state = store.snapshot().unwrap();
    let ask = &state.asks[ask_id];
    for value in [
        view::owner_view(&state, ask),
        view::summary(&state, ask),
        view::requester_view(&state, ask),
    ] {
        assert!(!value
            .to_string()
            .contains("REMOTE_FULL_TEXT_ONLY_IN_ARTIFACT"));
        assert!(value["remote"]["remoteState"].get("text").is_none());
        if let Some(late) = value.get("lateAnswer").filter(|v| !v.is_null()) {
            assert!(late.get("text").is_none());
            assert!(late["artifactId"].as_str().is_some());
            assert_diagnostic(late["preview"].as_str().unwrap());
        }
        if let Some(error) = value["remote"]["lastError"].as_str() {
            assert_diagnostic(error);
        }
    }
}

#[test]
fn remote_errors_never_become_delivery_identifiers_or_requester_state() {
    let (_root, store, ask, peer) = fixture();
    let mut snapshot = asks::snapshot(&store.snapshot().unwrap(), &ask);
    snapshot.revision = 2;
    snapshot.execution = ExecutionState::Failed;
    snapshot.last_error = Some(hostile_error());
    asks::apply(&store, &peer, snapshot).unwrap();
    let state = store.snapshot().unwrap();
    let updated = &state.asks[&ask.ask_id];
    let remote = updated.remote.as_ref().unwrap();
    let diagnostic = remote.sync.last_error.as_deref().unwrap();
    assert_diagnostic(diagnostic);
    assert!(
        diagnostic.len() >= 509,
        "must use the requested UTF-8 byte budget"
    );
    let delivery = state.deliveries.values().next().unwrap();
    assert_eq!(state.deliveries.len(), 1);
    assert_eq!(delivery.ref_id, format!("peerState@{}", updated.revision));
    let returned = crate::asks::returns::return_item(&store, &state, delivery).unwrap();
    assert_eq!(
        crate::asks::returns::item_json(&returned)["state"],
        "peerState"
    );
    let persisted = serde_json::to_value(store.snapshot().unwrap()).unwrap();
    let saved = &persisted["asks"][&ask.ask_id]["remote"];
    assert_eq!(saved["sync"]["lastError"], diagnostic);
    assert_eq!(saved["remoteState"]["lastError"], diagnostic);
    assert_metadata_views(&store, &ask.ask_id);
}

#[test]
fn unchanged_reconciliation_and_all_sync_writers_bound_diagnostics() {
    let (_root, store, ask, peer) = fixture();
    let mut snapshot = asks::snapshot(&store.snapshot().unwrap(), &ask);
    snapshot.revision = 3;
    snapshot.execution = ExecutionState::Running;
    asks::apply(&store, &peer, snapshot.clone()).unwrap();
    store
        .with_state(|s| s.asks.get_mut(&ask.ask_id).unwrap().hold = Some(Hold::PeerUnreachable))
        .unwrap();
    snapshot.last_error = Some(hostile_error());
    asks::apply(&store, &peer, snapshot).unwrap();
    let state = store.snapshot().unwrap();
    assert_eq!(state.asks[&ask.ask_id].hold, None);
    assert_diagnostic(
        state.asks[&ask.ask_id]
            .remote
            .as_ref()
            .unwrap()
            .sync
            .last_error
            .as_deref()
            .unwrap(),
    );
    // Transport workers also assign the field directly. Its persistence boundary
    // enforces the same bound even when apply() did not write it.
    store
        .with_state(|s| {
            s.asks
                .get_mut(&ask.ask_id)
                .unwrap()
                .remote
                .as_mut()
                .unwrap()
                .sync
                .last_error = Some(hostile_error());
        })
        .unwrap();
    let saved = serde_json::to_value(store.snapshot().unwrap()).unwrap();
    assert_diagnostic(
        saved["asks"][&ask.ask_id]["remote"]["sync"]["lastError"]
            .as_str()
            .unwrap(),
    );
    let mut raw = state.asks[&ask.ask_id].clone();
    raw.remote.as_mut().unwrap().sync.last_error = Some(hostile_error());
    assert_diagnostic(view::peer_remote(&raw)["lastError"].as_str().unwrap());
}

#[test]
fn ordinary_unknown_and_late_snapshots_never_persist_or_display_answer_bodies() {
    for case in ["ordinary", "unknown", "late"] {
        let (_root, store, ask, peer) = fixture();
        if case == "late" {
            store
                .with_state(|s| {
                    let ask = s.asks.get_mut(&ask.ask_id).unwrap();
                    ask.execution = ExecutionState::Failed;
                    ask.closed = Some(CloseOutcome::Completed);
                    ask.remote.as_mut().unwrap().sync.local_terminal = true;
                })
                .unwrap();
        }
        let text = body();
        let mut snapshot = answer_snapshot(&store, &ask, 7, &text);
        if case == "unknown" {
            let mut wire = serde_json::to_value(wire::WireSnapshotV1::from(&snapshot)).unwrap();
            wire["execution"] = json!("futureRemoteState");
            snapshot = serde_json::from_value::<wire::WireSnapshotV1>(wire)
                .unwrap()
                .into();
        }
        asks::apply(&store, &peer, snapshot).unwrap();
        let state = store.snapshot().unwrap();
        let current = &state.asks[&ask.ask_id];
        let artifact = format!("art-{}", crate::asks::sha256_hex(text.as_bytes()));
        assert_eq!(
            store
                .read_artifact(&artifact, ask.requester.session())
                .unwrap()
                .unwrap()
                .1,
            text.as_bytes()
        );
        let persisted = serde_json::to_string(&store.snapshot().unwrap()).unwrap();
        assert!(
            !persisted.contains("REMOTE_FULL_TEXT_ONLY_IN_ARTIFACT"),
            "{case}"
        );
        let saved: Value = serde_json::from_str(&persisted).unwrap();
        let remote_state = &saved["asks"][&ask.ask_id]["remote"]["remoteState"];
        assert!(remote_state.get("text").is_none());
        assert_diagnostic(remote_state["lastError"].as_str().unwrap());
        assert_diagnostic(
            remote_state["answer"]["metadata"]["lastError"]
                .as_str()
                .unwrap(),
        );
        assert_diagnostic(remote_state["answer"]["preview"].as_str().unwrap());
        assert_metadata_views(&store, &ask.ask_id);
        if case == "late" {
            let late = current
                .remote
                .as_ref()
                .unwrap()
                .late_answer
                .as_ref()
                .unwrap();
            assert_eq!(late.artifact_id.as_deref(), Some(artifact.as_str()));
            assert_eq!(current.execution, ExecutionState::Failed);
            assert!(state.answers.is_empty() && state.deliveries.is_empty());
        } else if case == "ordinary" {
            let answer = state.answers.values().next().unwrap();
            assert_eq!(answer.artifact_id.as_deref(), Some(artifact.as_str()));
            assert_diagnostic(answer.metadata.last_error.as_deref().unwrap());
        } else {
            assert!(matches!(current.hold, Some(Hold::PeerUnknown { .. })));
            assert!(state.answers.is_empty());
        }
    }
}

#[test]
fn legacy_inline_late_answer_recovers_the_existing_artifact_and_rewrites_without_text() {
    let (root, store, ask, _) = fixture();
    let text = body();
    let artifact = store
        .put_artifact(
            text.as_bytes(),
            "text/plain",
            &[ask.requester.session().unwrap().into()],
            Some(&ask.ask_id),
            Utc::now(),
        )
        .unwrap();
    let mut state = serde_json::to_value(store.snapshot().unwrap()).unwrap();
    let legacy = serde_json::to_value(wire::WireSnapshotV1::from(&answer_snapshot(
        &store, &ask, 5, &text,
    )))
    .unwrap();
    let remote = &mut state["asks"][&ask.ask_id]["remote"];
    remote["remoteState"] = legacy.clone();
    remote["lateAnswer"] = legacy;
    remote["sync"]["lastError"] = json!(hostile_error());
    let database = root.join(crate::management::v2::hot_store::DATABASE_FILE);
    for candidate in [
        database.clone(),
        database.with_extension("sqlite3-wal"),
        database.with_extension("sqlite3-shm"),
    ] {
        match std::fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove migrated test database: {error}"),
        }
    }
    std::fs::write(
        root.join("asks-v1.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    let migrated = AskStore::at(&root);
    let loaded = migrated.snapshot().unwrap();
    let late = loaded.asks[&ask.ask_id]
        .remote
        .as_ref()
        .unwrap()
        .late_answer
        .as_ref()
        .unwrap();
    assert_eq!(late.artifact_id.as_deref(), Some(artifact.as_str()));
    assert_eq!(
        migrated.read_artifact(&artifact, None).unwrap().unwrap().1,
        text.as_bytes()
    );
    assert_metadata_views(&migrated, &ask.ask_id);
    migrated
        .with_state(|s| {
            s.asks.get_mut(&ask.ask_id).unwrap().title = "force a persisted revision".into()
        })
        .unwrap();
    let rewritten = serde_json::to_string(&migrated.snapshot().unwrap()).unwrap();
    assert!(!rewritten.contains("REMOTE_FULL_TEXT_ONLY_IN_ARTIFACT"));
    let rewritten: Value = serde_json::from_str(&rewritten).unwrap();
    let late = &rewritten["asks"][&ask.ask_id]["remote"]["lateAnswer"];
    assert!(late.get("text").is_none());
    assert_eq!(late["artifactId"], artifact);
    assert_diagnostic(
        rewritten["asks"][&ask.ask_id]["remote"]["sync"]["lastError"]
            .as_str()
            .unwrap(),
    );
}
