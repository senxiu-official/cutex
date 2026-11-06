use super::*;
use crate::test_support::waste_runtime_scale as scale;

#[test]
fn missing_retained_cursor_never_turns_legacy_fact_into_a_new_append() {
    let root = scale::Root::new("b1-absent-cursor");
    let repository = EventRepository::open(root.path(), current_host_name()).unwrap();
    let original = repository
        .append(PendingEvent {
            cutex_session_id: "cutex.scale".into(),
            host_id: current_host_name(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation {
                management_request_id: Some("legacy".into()),
                ..Default::default()
            },
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/test/fact".into(),
                params: serde_json::json!({}),
            }),
        })
        .unwrap();
    let append = |action: &str| {
        append_owner_event_once_with_repository(
            &repository,
            "cutex.scale",
            "cutex/test/fact",
            action,
            serde_json::json!({}),
            EventCorrelation {
                management_request_id: Some(action.into()),
                ..Default::default()
            },
        )
    };
    append("warmup").unwrap();
    let state = ledger::Ledger::open(root.path()).unwrap();
    assert_eq!(
        state
            .identity("cutex.scale\0cutex/test/fact\0legacy")
            .unwrap(),
        None
    );
    drop(state);
    let before = repository.checkpoint().unwrap();
    let connection = rusqlite::Connection::open(root.path().join(ledger::DATABASE)).unwrap();
    connection
        .execute(
            "UPDATE integration_locations SET cursor='missing-retained-cursor' WHERE identity=?1",
            ["cutex.scale\0cutex/test/fact\0legacy"],
        )
        .unwrap();
    assert!(append("legacy").unwrap_err().to_string().contains("cursor"));
    assert_eq!(repository.checkpoint().unwrap(), before);
    assert_eq!(original.sequence, 1);
}

#[test]
fn interrupted_snapshot_publication_never_exposes_partial_index_or_checkpoint() {
    let root = scale::Root::new("b1-publish");
    let repository = EventRepository::open(root.path(), current_host_name()).unwrap();
    let key = "cutex.scale\0cutex/test/fact\0pending";
    let state = ledger::Ledger::open(root.path()).unwrap();
    state.pending(key).unwrap();
    let original = repository
        .append(PendingEvent {
            cutex_session_id: "cutex.scale".into(),
            host_id: current_host_name(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation {
                management_request_id: Some("pending".into()),
                ..Default::default()
            },
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/test/fact".into(),
                params: serde_json::json!({}),
            }),
        })
        .unwrap();
    let snapshot = repository.integration_read_snapshot().unwrap();
    let mut reader = snapshot.reader(None).unwrap();
    let (events, checkpoint) = reader.batch(8).unwrap().unwrap();
    let connection = rusqlite::Connection::open(root.path().join(ledger::DATABASE)).unwrap();
    connection.execute_batch("CREATE TRIGGER interrupted_publish BEFORE INSERT ON integration_lookup_checkpoint BEGIN SELECT RAISE(ABORT,'interrupted publication'); END;").unwrap();
    assert!(state.index_locations(&events, &checkpoint).is_err());
    assert!(state.location(key).unwrap().is_none());
    assert!(state.lookup_checkpoint().unwrap().is_none());
    assert_eq!(
        state.identity(key).unwrap(),
        Some(ledger::Identity::Pending)
    );
    connection
        .execute_batch("DROP TRIGGER interrupted_publish;")
        .unwrap();
    drop(connection);
    drop(state);
    let before = repository.checkpoint().unwrap();
    let recovered = append_owner_event_once_with_repository(
        &repository,
        "cutex.scale",
        "cutex/test/fact",
        "pending",
        serde_json::json!({}),
        Default::default(),
    )
    .unwrap();
    assert_eq!(original, recovered);
    assert_eq!(before, repository.checkpoint().unwrap());
}

#[test]
fn waste_guard_b1_new_actions_and_replays_read_only_the_suffix() {
    let root = scale::Root::new("b1-guard");
    let host = current_host_name();
    let repository = EventRepository::open(root.path(), host.clone()).unwrap();
    for _ in 0..128 {
        repository
            .append(PendingEvent {
                cutex_session_id: "cutex.noise".into(),
                host_id: host.clone(),
                source: EventSource::Cutex,
                schema: None,
                correlation: Default::default(),
                native: None,
                cutex: Some(CutexMessage {
                    method: "cutex/scale/noise".into(),
                    params: serde_json::json!({}),
                }),
            })
            .unwrap();
    }
    let append = |action: &str| {
        append_owner_event_once_with_repository(
            &repository,
            "cutex.scale",
            "cutex/test/fact",
            action,
            serde_json::json!({}),
            EventCorrelation {
                management_request_id: Some(action.into()),
                ..Default::default()
            },
        )
        .unwrap()
    };
    append("warmup");
    LOOKUP_SCANNED.with(|n| n.set(0));
    for action in ["new-1", "new-2", "new-3"] {
        let first = append(action);
        assert_eq!(first.event_id, append(action).event_id);
    }
    assert!(
        LOOKUP_SCANNED.with(|n| n.get()) <= 6,
        "no full replay after warmup"
    );
    let cold = EventRepository::open(root.path(), host).unwrap();
    let state = ledger::Ledger::open(root.path()).unwrap();
    LOOKUP_SCANNED.with(|n| n.set(0));
    assert!(
        find_indexed_integration_event(&cold, &state, "cutex.scale\0cutex/test/fact\0new-1")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        LOOKUP_SCANNED.with(|n| n.get()),
        0,
        "durable exact cursor survives a process reopen"
    );
}

#[test]
fn cursor_suffix_recovers_an_old_writer_pending_crash_gap() {
    let root = scale::Root::new("b1-gap");
    let host = current_host_name();
    let repository = EventRepository::open(root.path(), host.clone()).unwrap();
    let state = ledger::Ledger::open(root.path()).unwrap();
    state
        .pending("cutex.scale\0cutex/test/fact\0crash")
        .unwrap();
    let original = repository
        .append(PendingEvent {
            cutex_session_id: "cutex.scale".into(),
            host_id: host,
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation {
                management_request_id: Some("crash".into()),
                ..Default::default()
            },
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/test/fact".into(),
                params: serde_json::json!({"original":true}),
            }),
        })
        .unwrap();
    drop(state);
    let before = repository.checkpoint().unwrap();
    let recovered = append_owner_event_once_with_repository(
        &repository,
        "cutex.scale",
        "cutex/test/fact",
        "crash",
        serde_json::json!({"original":false}),
        Default::default(),
    )
    .unwrap();
    assert_eq!(original, recovered);
    assert_eq!(before, repository.checkpoint().unwrap());
    assert_eq!(
        ledger::Ledger::open(root.path())
            .unwrap()
            .identity("cutex.scale\0cutex/test/fact\0crash")
            .unwrap(),
        Some(ledger::Identity::Committed)
    );
}

#[test]
fn corrupted_advisory_cursor_cannot_return_another_action_or_authorize_reappend() {
    let root = scale::Root::new("b1-cache");
    let repository = EventRepository::open(root.path(), current_host_name()).unwrap();
    let append = |action: &str| {
        append_owner_event_once_with_repository(
            &repository,
            "cutex.scale",
            "cutex/test/fact",
            action,
            serde_json::json!({}),
            EventCorrelation {
                management_request_id: Some(action.into()),
                ..Default::default()
            },
        )
    };
    let one = append("one").unwrap();
    let two = append("two").unwrap();
    let checkpoint = repository.checkpoint().unwrap();
    let connection = rusqlite::Connection::open(root.path().join(ledger::DATABASE)).unwrap();
    connection
        .execute(
            "UPDATE integration_locations SET cursor=?1,sequence=?2 WHERE identity=?3",
            rusqlite::params![
                two.cursor,
                i64::try_from(two.sequence).unwrap(),
                "cutex.scale\0cutex/test/fact\0one"
            ],
        )
        .unwrap();
    assert!(append("one")
        .unwrap_err()
        .to_string()
        .contains("identity mismatch"));
    assert_eq!(checkpoint, repository.checkpoint().unwrap());
    // An explicit cache rebuild loses no authoritative identity evidence.
    connection.execute_batch("BEGIN; DELETE FROM integration_locations; DELETE FROM integration_lookup_checkpoint; COMMIT;").unwrap();
    assert_eq!(one, append("one").unwrap());
    assert_eq!(checkpoint, repository.checkpoint().unwrap());
}

#[test]
#[ignore = "257MB retained event fixture; scripts/perf/waste-runtime.ps1"]
fn waste_scale_b1_integration_cursor() {
    let root = scale::Root::new("b1");
    let host = current_host_name();
    let repository = EventRepository::open(root.path(), host.clone()).unwrap();
    let padding = "x".repeat(128 * 1024);
    let count = 2048;
    for _ in 0..count {
        repository
            .append(PendingEvent {
                cutex_session_id: "cutex.noise".into(),
                host_id: host.clone(),
                source: EventSource::Cutex,
                schema: None,
                correlation: Default::default(),
                native: None,
                cutex: Some(CutexMessage {
                    method: "cutex/scale/noise".into(),
                    params: serde_json::json!({"padding":padding}),
                }),
            })
            .unwrap();
    }
    let bytes: u64 = fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("events.jsonl"))
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert!(bytes >= 257_000_000);
    // One cold recovery is allowed. Measure independent new actions and replay
    // after the location index has covered the retained pre-upgrade history.
    let append = |action: &str| {
        append_owner_event_once_with_repository(
            &repository,
            "cutex.scale",
            "cutex/scale/action",
            action,
            serde_json::json!({}),
            EventCorrelation {
                management_request_id: Some(action.into()),
                ..Default::default()
            },
        )
        .unwrap()
    };
    // Model activation from r270: authority already exists, lookup tables do
    // not. Include their first creation in the cold lock measurement.
    drop(ledger::Ledger::open(root.path()).unwrap());
    rusqlite::Connection::open(root.path().join(ledger::DATABASE))
        .unwrap()
        .execute_batch(
            "DROP TABLE integration_locations; DROP TABLE integration_lookup_checkpoint;",
        )
        .unwrap();
    LOOKUP_MAX_HOLD_NS.with(|n| n.set(0));
    let cold_started = std::time::Instant::now();
    append("warmup");
    let cold_elapsed = cold_started.elapsed();
    let cold_max = LOOKUP_MAX_HOLD_NS.with(|n| n.get());
    scale::emit(
        "B1-cold",
        serde_json::json!({"retained_bytes":bytes,"events":count}),
        &[cold_elapsed],
        serde_json::json!({"max_global_lock_hold_ns":cold_max}),
    );
    assert!(
        cold_max < 50_000_000,
        "cold dedupe lock section exceeded 50ms: {cold_max}ns"
    );
    LOOKUP_SCANNED.with(|n| n.set(0));
    let mut samples = Vec::new();
    for i in 0..10 {
        let action = format!("action-{i}");
        let start = std::time::Instant::now();
        let first = append(&action);
        assert_eq!(first.event_id, append(&action).event_id);
        samples.push(start.elapsed());
    }
    let scanned = LOOKUP_SCANNED.with(|n| n.get());
    scale::emit(
        "B1",
        serde_json::json!({"retained_bytes":bytes,"events":count}),
        &samples,
        serde_json::json!({"event_rows_scanned":scanned}),
    );
    if !scale::before() {
        assert!(
            scanned <= 30,
            "steady integration lookups must not replay retained history: {scanned}"
        );
    }
}
