use super::*;
use crate::asks::model::{ArtifactRecord, CloseOutcome, ExecutionState};
use crate::asks::store::tests::{body, new_ask, temp_store};

fn stamp(value: &str) -> DateTime<Utc> {
    crate::asks::parse_time(value).unwrap()
}
fn cutoff() -> DateTime<Utc> {
    stamp("2026-10-25T00:00:00Z")
}

fn closed(state: &mut AskState, action: &str) -> String {
    let at = stamp("2026-10-23T00:00:00Z");
    let id = state
        .create_ask(new_ask(action, "completed synthetic brief"), at)
        .unwrap()
        .0
        .ask_id;
    let ask = state.asks.get_mut(&id).unwrap();
    ask.execution = ExecutionState::Cancelled;
    ask.closed = Some(CloseOutcome::Abandoned);
    ask.closed_at = Some(crate::asks::now_rfc3339(at));
    state.notices.clear();
    id
}

#[test]
fn closed_old_only_and_open_ancestry_are_protected() {
    let mut state = AskState::default();
    let old = closed(&mut state, "old");
    let ancestor = closed(&mut state, "ancestor");
    let today = closed(&mut state, "today");
    state.asks.get_mut(&today).unwrap().closed_at = Some(crate::asks::now_rfc3339(cutoff()));
    let mut new = new_ask("active", "active");
    new.parent_ask = Some(ancestor.clone());
    let active = state.create_ask(new, cutoff()).unwrap().0.ask_id;
    state.notices.clear();
    let keys = plan(&state, cutoff()).unwrap();
    assert!(keys.contains(&("asks".into(), old)));
    for id in [ancestor, today, active] {
        assert!(!keys.contains(&("asks".into(), id)));
    }
}

#[test]
fn malformed_cutoff_hold_and_runtime_command_do_not_archive() {
    let mut state = AskState::default();
    let invalid = closed(&mut state, "invalid");
    state.asks.get_mut(&invalid).unwrap().closed_at = Some("not a timestamp".into());
    let command = closed(&mut state, "command");
    state.commands.push(crate::asks::store::QueuedCommand {
        command_id: "command".into(),
        created_at: crate::asks::now_rfc3339(cutoff()),
        command: crate::asks::store::RuntimeCommand::StopAttempt {
            ask_id: command.clone(),
            attempt: 1,
        },
    });
    assert!(plan(&state, cutoff()).unwrap().is_empty());
}

#[test]
fn shared_artifact_stays_hot_without_retaining_closed_reverse_owners() {
    let mut state = AskState::default();
    let old = closed(&mut state, "old");
    let active = state
        .create_ask(new_ask("active", "active"), cutoff())
        .unwrap()
        .0
        .ask_id;
    state.notices.clear();
    let artifact = format!("art-{}", "a".repeat(64));
    for id in [&old, &active] {
        state
            .asks
            .get_mut(id)
            .unwrap()
            .body
            .attachments
            .push(artifact.clone());
    }
    state.artifacts.insert(
        artifact.clone(),
        ArtifactRecord {
            artifact_id: artifact.clone(),
            bytes: 1,
            media_type: "application/octet-stream".into(),
            readers: vec![],
            asks: vec![old.clone(), active],
            created_at: "2026-10-23T00:00:00Z".into(),
        },
    );
    let keys = plan(&state, cutoff()).unwrap();
    assert!(!keys.contains(&("artifacts".into(), artifact)));
    assert!(keys.contains(&("asks".into(), old)));
}

#[test]
fn atomic_split_history_and_restore_preserve_latest_bytes() {
    let (store, dir) = temp_store("archive-roundtrip");
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    store.with_state(|value| *value = state.clone()).unwrap();
    let path = dir.join(crate::management::v2::hot_store::DATABASE_FILE);
    let original = crate::management::v2::hot_store::verified_rows(&path, "asks")
        .unwrap()
        .unwrap();
    let keys = plan(&state, cutoff()).unwrap();
    let report = move_rows(&path, original.generation, &keys, cutoff(), cutoff()).unwrap();
    assert_eq!(report.asks, 1);
    let db = open(&path).unwrap();
    let format: i64 = db
        .query_row(
            "SELECT format_version FROM hot_store_meta WHERE store='asks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        format, 2,
        "r261/r260's format-1-only reader must refuse rather than hide history"
    );
    drop(db);
    let mut all = history_rows(&path).unwrap();
    assert_eq!(
        all.remove(&("asks".into(), id.clone())),
        original.rows.get(&("asks".into(), id.clone())).cloned()
    );
    // A post-activation input/answer is represented by a hot row. It must
    // survive rollback, rather than restoring a pre-activation snapshot.
    let db = open(&path).unwrap();
    db.execute("INSERT INTO hot_store_rows(store,collection,row_key,value_json) VALUES('asks','wait_cancel','new-input',?1)", [b"9".as_slice()]).unwrap();
    drop(db);
    assert!(restore_all(&path, report.generation, false).is_err());
    let restored = restore_all(&path, report.generation, true).unwrap();
    assert_eq!(restored.asks, 1);
    let after = crate::management::v2::hot_store::verified_rows(&path, "asks")
        .unwrap()
        .unwrap();
    assert_eq!(
        after.rows.get(&("asks".into(), id)),
        original
            .rows
            .get(&("asks".into(), state.asks.keys().next().unwrap().clone()))
    );
    assert_eq!(
        after.rows[&("wait_cancel".into(), "new-input".into())],
        b"9"
    );
    assert_eq!(
        restore_all(&path, restored.generation, true).unwrap().rows,
        0
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn conflict_or_digest_damage_is_atomic_and_refuses_rollback() {
    let (store, dir) = temp_store("archive-corruption");
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    store.with_state(|value| *value = state.clone()).unwrap();
    let path = dir.join(crate::management::v2::hot_store::DATABASE_FILE);
    let generation = crate::management::v2::hot_store::generation(&path, "asks")
        .unwrap()
        .unwrap();
    assert!(move_rows(
        &path,
        generation + 1,
        &plan(&state, cutoff()).unwrap(),
        cutoff(),
        cutoff()
    )
    .is_err());
    let report = move_rows(
        &path,
        generation,
        &plan(&state, cutoff()).unwrap(),
        cutoff(),
        cutoff(),
    )
    .unwrap();
    let db = open(&path).unwrap();
    db.execute(
        "UPDATE asks_archive_rows SET sha256='damaged' WHERE collection='asks' AND row_key=?1",
        [&id],
    )
    .unwrap();
    drop(db);
    assert!(history_rows(&path).is_err());
    assert!(restore_all(&path, report.generation, true).is_err());
    let db = open(&path).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM asks_archive_rows", [], |r| r.get(0))
        .unwrap();
    assert!(count > 0);
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM hot_store_rows WHERE store='asks' AND collection='asks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_archive_policy_never_falls_back_to_legacy_cleanup() {
    let (store, dir) = temp_store("archive-missing-marker");
    let mut state = AskState::default();
    closed(&mut state, "old");
    store.with_state(|target| *target = state).unwrap();
    store.archive_before(Some(cutoff()), cutoff()).unwrap();
    let db = open(&dir.join(crate::management::v2::hot_store::DATABASE_FILE)).unwrap();
    db.execute("UPDATE asks_archive_meta SET active=0", [])
        .unwrap();
    assert!(
        store.snapshot().is_err(),
        "inactive marker must reject format 2"
    );
    db.execute(
        "UPDATE asks_archive_meta SET active=1,schema_version=99",
        [],
    )
    .unwrap();
    assert!(store.archive_configured().is_err());
    assert!(
        store.snapshot().is_err(),
        "unknown archive schema must fail closed"
    );
    db.execute("UPDATE asks_archive_meta SET schema_version=1", [])
        .unwrap();
    db.execute("DELETE FROM asks_archive_meta", []).unwrap();
    assert!(store.archive_configured().is_err());
    assert!(store.snapshot().is_err());
    assert!(store
        .prune_with(Some(0), cutoff() + chrono::Duration::days(4000))
        .is_err());
    let retained: i64 = db
        .query_row("SELECT count(*) FROM asks_archive_rows", [], |r| r.get(0))
        .unwrap();
    assert!(retained > 0);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cutoff_is_fixed_and_empty_split_is_not_a_format_change() {
    let (store, dir) = temp_store("archive-policy");
    store
        .with_state(|state| {
            state.wait_cancel.insert("x".into(), 1);
        })
        .unwrap();
    let path = dir.join(crate::management::v2::hot_store::DATABASE_FILE);
    let generation = crate::management::v2::hot_store::generation(&path, "asks")
        .unwrap()
        .unwrap();
    assert_eq!(
        move_rows(&path, generation, &BTreeSet::new(), cutoff(), cutoff())
            .unwrap()
            .rows,
        0
    );
    assert!(
        crate::management::v2::hot_store::verified_rows(&path, "asks")
            .unwrap()
            .is_some()
    );
    assert!(move_rows(
        &path,
        generation,
        &BTreeSet::new(),
        cutoff() + chrono::Duration::days(1),
        cutoff()
    )
    .is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn store_hooks_keep_artifact_bytes_acl_search_and_history() {
    use crate::asks::store::tests::{REQUESTER, TARGET};
    let (store, dir) = temp_store("archive-links");
    let old = stamp("2026-10-23T00:00:00Z");
    let bytes = b"\x00binary\xffretained forever";
    let artifact = store
        .put_artifact(
            bytes,
            "application/octet-stream",
            &[REQUESTER.into()],
            None,
            old,
        )
        .unwrap();
    let mut state = store.snapshot().unwrap();
    let id = closed(&mut state, "archive-links");
    state
        .asks
        .get_mut(&id)
        .unwrap()
        .body
        .attachments
        .push(artifact.clone());
    state
        .artifacts
        .get_mut(&artifact)
        .unwrap()
        .asks
        .push(id.clone());
    store.with_state(|value| *value = state).unwrap();
    let split = store.archive_before(Some(cutoff()), cutoff()).unwrap();
    assert_eq!(split.asks, 1);
    assert!(store.snapshot().unwrap().asks.is_empty());
    let copies = crate::asks::store::whole_state_copies_on_this_thread();
    assert_eq!(store.lookup_ask(&id).unwrap().unwrap().ask_id, id);
    assert_eq!(
        crate::asks::store::whole_state_copies_on_this_thread(),
        copies,
        "single-record projector read must not clone AskState"
    );
    assert!(store
        .lookup_snapshot(&[&id])
        .unwrap()
        .asks
        .contains_key(&id));
    assert_eq!(
        store
            .read_artifact(&artifact, Some(REQUESTER))
            .unwrap()
            .unwrap()
            .1,
        bytes
    );
    assert_eq!(
        store
            .read_artifact(&artifact, Some(TARGET))
            .unwrap()
            .unwrap()
            .1,
        bytes
    );
    assert!(store
        .read_artifact(&artifact, Some("cutex.stranger"))
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .put_artifact(
                bytes,
                "application/octet-stream",
                &["cutex.new-reader".into()],
                None,
                cutoff()
            )
            .unwrap(),
        artifact
    );
    assert!(
        store.snapshot().unwrap().asks.is_empty(),
        "shared artifact reuse must not hydrate closed owners"
    );
    assert_eq!(
        store
            .read_artifact(&artifact, Some(TARGET))
            .unwrap()
            .unwrap()
            .1,
        bytes
    );
    store
        .prune_with(Some(0), cutoff() + chrono::Duration::days(4000))
        .unwrap();
    assert_eq!(
        store.read_artifact(&artifact, None).unwrap().unwrap().1,
        bytes
    );
    let archive = crate::documents::archive::RecordArchive::at(dir.join("legacy-archive"))
        .with_asks(store.clone());
    let filter = crate::documents::archive::ArchiveFilter {
        query: Some("completed synthetic".into()),
        ..Default::default()
    };
    let (entries, more) = archive.list(&filter, 10, 0).unwrap();
    assert!(!more);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].record_id, id);
    assert_eq!(
        archive.get(&id).unwrap().unwrap()["record"]["ask"]["askId"],
        id
    );
    let reply = crate::management::v2::asks::route_json(
        &store,
        "/v2/asks",
        &crate::management::v2::asks::Route::Ask(id.clone()),
    )
    .map_err(|error| error.message)
    .unwrap();
    assert_eq!(reply["ask"]["askId"], id);
    store.restore_archived_for_rollback(true).unwrap();
    assert!(store.archived_identities().unwrap().is_empty());
    assert_eq!(
        store.read_artifact(&artifact, None).unwrap().unwrap().1,
        bytes
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn promote_and_mutate_is_one_lock_and_other_handles_see_new_generation() {
    let (store, dir) = temp_store("archive-promote");
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    store.with_state(|target| *target = state).unwrap();
    store.archive_before(Some(cutoff()), cutoff()).unwrap();
    let old_snapshot = store.shared().unwrap();
    assert!(old_snapshot.asks.is_empty());
    let second = crate::asks::store::AskStore::at(&dir);
    second
        .with_records(&[&id], |state| {
            let ask = state.asks.get_mut(&id).unwrap();
            ask.closed = None;
            ask.closed_at = None;
            ask.execution = ExecutionState::Queued;
        })
        .unwrap();
    assert!(
        old_snapshot.asks.is_empty(),
        "published old Arc must stay immutable"
    );
    assert!(store.shared().unwrap().asks[&id].is_open());
    assert_eq!(
        store
            .archive_before(None, cutoff() + chrono::Duration::days(2))
            .unwrap()
            .rows,
        0
    );
    assert!(store.lookup_snapshot(&[&id]).unwrap().asks[&id].is_open());
    second.restore_archived_for_rollback(true).unwrap();
    assert!(store.shared().unwrap().asks[&id].is_open());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn empty_startup_persists_activation_cutoff_without_later_drift() {
    let (store, dir) = temp_store("archive-empty-startup");
    store.archive_before(Some(cutoff()), cutoff()).unwrap();
    assert!(store.archive_configured().unwrap());
    assert_eq!(
        persisted_cutoff(&dir.join(crate::management::v2::hot_store::DATABASE_FILE)).unwrap(),
        Some(cutoff())
    );
    assert!(store
        .archive_before(None, cutoff() + chrono::Duration::days(100))
        .is_ok());
    assert!(store
        .archive_before(Some(cutoff() + chrono::Duration::days(1)), cutoff())
        .is_err());
    let advanced = cutoff() + chrono::Duration::days(1);
    assert!(store.configure_archive_cutoff(advanced, false).is_err());
    store.configure_archive_cutoff(advanced, true).unwrap();
    assert_eq!(
        persisted_cutoff(&dir.join(crate::management::v2::hot_store::DATABASE_FILE)).unwrap(),
        Some(advanced)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cold_document_references_and_mixed_legacy_months_are_never_expired() {
    use crate::documents::archive::{NewArchiveRecord, RecordArchive};
    use crate::documents::{DocumentSource, DocumentStore, NewDocument};
    let (store, dir) = temp_store("archive-document-retention");
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    store.with_state(|target| *target = state).unwrap();
    let old = stamp("2026-10-23T00:00:00Z");
    let documents = DocumentStore::at(dir.join("documents"));
    let bytes = b"original document bytes".to_vec();
    let doc = documents
        .put(
            NewDocument {
                bytes: bytes.clone(),
                media_type: "text/plain".into(),
                source: DocumentSource {
                    kind: "ask".into(),
                    id: id.clone(),
                    producer: "asks.brief".into(),
                },
                readers: vec![],
                title: "retained".into(),
            },
            1 << 30,
            old,
        )
        .unwrap();
    let archive = RecordArchive::at(dir.join("legacy-archive"));
    archive
        .append(
            NewArchiveRecord {
                record_id: "legacy-message".into(),
                kind: "message".into(),
                title: "old mixed month".into(),
                participants: vec![],
                closed_at: old,
                record: serde_json::json!({"text":"do not delete"}),
                documents: vec![],
            },
            old,
        )
        .unwrap();
    store.archive_before(Some(cutoff()), cutoff()).unwrap();
    let mut settings = crate::config::documents::DocumentSettings::default();
    settings.archive_retention_days = 1;
    let report = crate::documents::sweep::run(
        &crate::documents::sweep::SweepStores {
            asks: &store,
            documents: &documents,
            archive: &archive,
            messages: None,
            owner_notify: None,
        },
        &settings,
        cutoff() + chrono::Duration::days(4000),
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.pruned_archive_months, 0);
    assert_eq!(report.deleted_documents, 0);
    assert_eq!(
        documents.read(&doc.document_id, None).unwrap().unwrap().1,
        bytes
    );
    assert!(archive.get("legacy-message").unwrap().is_some());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn idempotency_lookup_and_old_export_cannot_omit_cold_records() {
    let (store, dir) = temp_store("archive-idempotency");
    let mut state = AskState::default();
    let id = closed(&mut state, "existing-action");
    store.with_state(|target| *target = state).unwrap();
    store.archive_before(Some(cutoff()), cutoff()).unwrap();
    let new = new_ask("existing-action", "completed synthetic brief");
    assert_eq!(
        store
            .archived_by_action(&new.requester, &new.action_id)
            .unwrap()
            .unwrap()
            .ask_id,
        id
    );
    assert!(store
        .verify_legacy_export()
        .unwrap_err()
        .to_string()
        .contains("asks-archive-restore"));
    assert!(store
        .export_legacy_json()
        .unwrap_err()
        .to_string()
        .contains("asks-archive-restore"));
    let (existing, created) = store
        .with_new_ask_state(&new, |state| state.create_ask(new.clone(), cutoff()))
        .unwrap()
        .unwrap();
    assert!(!created);
    assert_eq!(existing.ask_id, id);
    assert_eq!(store.history_snapshot().unwrap().asks.len(), 1);
    store.restore_archived_for_rollback(true).unwrap();
    assert!(store.verify_legacy_export().unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_wait_and_revert_still_find_cold_records() {
    use crate::asks::model::{AttemptRecord, AttemptState};
    use crate::asks::store::tests::{REQUESTER, TARGET};
    let (store, dir) = temp_store("archive-wait-revert");
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    let mut attempt =
        AttemptRecord::submitting(1, "message".into(), 1, "2026-10-23T00:00:00Z".into());
    attempt.turn_id = Some("turn-old".into());
    attempt.state = AttemptState::Answered;
    state.asks.get_mut(&id).unwrap().attempts.push(attempt);
    store.with_state(|target| *target = state).unwrap();
    store.archive_before(Some(cutoff()), cutoff()).unwrap();
    assert!(store.snapshot().unwrap().asks.is_empty());
    let flagged = crate::asks::lifecycle::mark_reverted_in_store(
        &store,
        TARGET,
        &["turn-old".into()],
        cutoff(),
    )
    .unwrap();
    assert_eq!(flagged, vec![id.clone()]);
    assert!(store.history_snapshot().unwrap().asks[&id].target_reverted);
    // Retire the new transport notice, then explicitly cold-move again.
    store
        .with_state(|state| {
            state.notices.clear();
            for delivery in state.deliveries.values_mut() {
                delivery.state = crate::asks::model::DeliveryState::Acknowledged;
                delivery.updated_at = "2026-10-23T00:00:00Z".into();
            }
        })
        .unwrap();
    store.archive_before(None, cutoff()).unwrap();
    let request = crate::asks::waits::WaitRequest {
        asks: vec![id],
        all: false,
        timeout_ms: 100,
        action_id: "wait-old".into(),
    };
    let (wait, cyclic) = crate::asks::waits::begin(&store, REQUESTER, &request, cutoff()).unwrap();
    crate::asks::waits::finish(&store, &wait, "nothingPending", &cyclic, cutoff()).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unanswered_question_and_active_lease_keep_closed_records_hot() {
    use crate::asks::model::*;
    let mut state = AskState::default();
    let question_ask = closed(&mut state, "question");
    let lease_ask = closed(&mut state, "lease");
    state.questions.insert(
        "qst-fixture".into(),
        QuestionRecord {
            question_id: "qst-fixture".into(),
            ask_id: question_ask.clone(),
            attempt: 1,
            runtime_generation: 1,
            native: NativeQuestionRef {
                thread_id: "thread".into(),
                turn_id: "turn".into(),
                item_id: "item".into(),
                request_id: serde_json::json!(1),
            },
            questions: serde_json::json!([]),
            route: QuestionRoute::Requester,
            routed_to_owner_at: None,
            state: QuestionState::Open,
            resolution: None,
            created_at: "2026-10-23T00:00:00Z".into(),
        },
    );
    state.leases.insert(
        "target".into(),
        AdmissionLease {
            cutex_session_id: "target".into(),
            holder: LeaseHolder::Ask {
                ask_id: lease_ask.clone(),
                attempt: 1,
            },
            native_message_id: "message".into(),
            runtime_generation: 1,
            turn_id: Some("turn".into()),
            since: "2026-10-23T00:00:00Z".into(),
        },
    );
    let keys = plan(&state, cutoff()).unwrap();
    assert!(!keys.contains(&("asks".into(), question_ask)));
    assert!(!keys.contains(&("asks".into(), lease_ask)));
}

#[test]
fn authority_fenced_sidecar_roots_are_selective_and_release_when_settled() {
    use crate::asks::store::tests::REQUESTER;
    use crate::director_address::{HandoverReview, Store};
    let (asks, dir) = temp_store("archive-sidecar-roots");
    let roster =
        crate::agent_management::AgentManagementStore::open(dir.join("authority")).unwrap();
    let sidecar = Store::at(dir.join("sidecar"));
    let mut state = AskState::default();
    let ids: Vec<_> = (0..5)
        .map(|n| closed(&mut state, &format!("old-{n}")))
        .collect();
    for id in &ids {
        sidecar
            .stamp_at(&state.asks[id], REQUESTER, "alpha", 1)
            .unwrap();
    }
    asks.with_state(|target| *target = state).unwrap();
    sidecar
        .update(|state| {
            state.asks.get_mut(&ids[0]).unwrap().pending_reconcile = true;
            state.asks.get_mut(&ids[1]).unwrap().owner_hold = true;
            state.asks.get_mut(&ids[2]).unwrap().summary_pending = true;
            state.reviews.insert(
                "pending-review".into(),
                HandoverReview {
                    id: "pending-review".into(),
                    project_id: "alpha".into(),
                    ask_id: ids[3].clone(),
                    from: REQUESTER.into(),
                    to: "next".into(),
                    change_id: "change".into(),
                    revision: 0,
                    state: "pending".into(),
                    reason: "test".into(),
                    provenance_known: true,
                    actor: None,
                    owner_only: false,
                    resolved_to: None,
                },
            );
            Ok(())
        })
        .unwrap();
    let report = asks
        .archive_before_with_authority(&roster, &sidecar, Some(cutoff()), cutoff())
        .unwrap();
    assert_eq!(report.asks, 1);
    assert_eq!(asks.snapshot().unwrap().asks.len(), 4);
    assert!(asks.lookup_ask(&ids[4]).unwrap().is_some());
    // An already-fenced caller must use the guard-aware entry, not recursively
    // enter with_role_action or another mutation fence.
    let guard = roster.lock_mutations().unwrap();
    sidecar
        .update(|state| {
            state.asks.get_mut(&ids[4]).unwrap().summary_pending = true;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        asks.archive_before_guarded(&guard, &sidecar, None, cutoff())
            .unwrap()
            .asks,
        0
    );
    assert_eq!(
        asks.snapshot().unwrap().asks.len(),
        5,
        "an existing cold sidecar obligation must be promoted and protected"
    );
    sidecar
        .update(|state| {
            for item in state.asks.values_mut() {
                item.pending_reconcile = false;
                item.owner_hold = false;
                item.summary_pending = false;
            }
            state.reviews.get_mut("pending-review").unwrap().state = "accepted".into();
            Ok(())
        })
        .unwrap();
    assert_eq!(
        asks.archive_before_guarded(&guard, &sidecar, None, cutoff())
            .unwrap()
            .asks,
        5
    );
    drop(guard);
    assert!(asks.snapshot().unwrap().asks.is_empty());
    assert_eq!(asks.history_snapshot().unwrap().asks.len(), 5);
    assert_eq!(sidecar.snapshot().unwrap().asks.len(), 5);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unsupported_sidecar_is_not_treated_as_an_empty_protection_set() {
    let (asks, dir) = temp_store("archive-sidecar-invalid");
    let roster =
        crate::agent_management::AgentManagementStore::open(dir.join("authority")).unwrap();
    let sidecar = crate::director_address::Store::at(dir.join("sidecar"));
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    asks.with_state(|target| *target = state).unwrap();
    let mut unsupported = crate::director_address::OwnershipState::default();
    unsupported.version = 99;
    crate::config::atomic::write_private_pretty_json_atomic(
        &sidecar.root().join("ownership-v1.json"),
        &unsupported,
        "Unsupported test ownership",
    )
    .unwrap();
    assert!(asks
        .archive_before_with_authority(&roster, &sidecar, Some(cutoff()), cutoff())
        .is_err());
    assert!(asks.snapshot().unwrap().asks.contains_key(&id));
    assert!(!asks.archive_configured().unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cold_pending_review_and_summary_repair_keep_the_business_record() {
    use crate::asks::model::route::{self, Pin};
    use crate::asks::store::tests::REQUESTER;
    use crate::director_address::{HandoverReview, Store};
    let (asks, dir) = temp_store("archive-sidecar-repair");
    let sidecar = Store::at(dir.join("sidecar"));
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    let original = state.asks[&id].clone();
    sidecar.stamp_at(&original, REQUESTER, "alpha", 1).unwrap();
    asks.with_state(|target| *target = state).unwrap();
    asks.archive_before(Some(cutoff()), cutoff()).unwrap();
    sidecar
        .update(|state| {
            state.reviews.insert(
                "cold-review".into(),
                HandoverReview {
                    id: "cold-review".into(),
                    project_id: "alpha".into(),
                    ask_id: id.clone(),
                    from: REQUESTER.into(),
                    to: "next".into(),
                    change_id: "change".into(),
                    revision: 0,
                    state: "pending".into(),
                    reason: "test".into(),
                    provenance_known: true,
                    actor: None,
                    owner_only: false,
                    resolved_to: None,
                },
            );
            Ok(())
        })
        .unwrap();
    let cold = asks.lookup_ask(&id).unwrap().unwrap();
    sidecar
        .decide_review(
            "cold-review",
            0,
            "owner",
            true,
            &cold,
            "alpha",
            "next",
            2,
            true,
            true,
        )
        .unwrap();
    assert!(sidecar.snapshot().unwrap().asks[&id].summary_pending);
    // Failed/unavailable authority does not consume summary_pending.
    assert_eq!(sidecar.repair_summaries_for(&asks, |_| false).unwrap(), 0);
    assert!(sidecar.snapshot().unwrap().asks[&id].summary_pending);
    route::pin(
        &id,
        Pin {
            controller: Ok(Some("next".into())),
            history: vec![REQUESTER.into()],
            questions: None,
        },
    );
    assert_eq!(sidecar.repair_summaries_for(&asks, |_| true).unwrap(), 1);
    assert!(!sidecar.snapshot().unwrap().asks[&id].summary_pending);
    let hot = asks.snapshot().unwrap();
    assert!(hot.asks.contains_key(&id));
    assert_eq!(hot.asks[&id].closed, original.closed);
    assert!(hot
        .deliveries
        .values()
        .any(|delivery| delivery.ask_id == id));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cold_business_reference_is_not_an_uncreated_intent() {
    use crate::asks::store::tests::REQUESTER;
    let (asks, dir) = temp_store("archive-sidecar-intent");
    let sidecar = crate::director_address::Store::at(dir.join("sidecar"));
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    let ask = state.asks[&id].clone();
    sidecar.stamp_at(&ask, REQUESTER, "alpha", 1).unwrap();
    asks.with_state(|target| *target = state).unwrap();
    asks.archive_before(Some(cutoff()), cutoff()).unwrap();
    assert!(asks.snapshot().unwrap().asks.is_empty());
    assert!(!sidecar
        .discard_uncreated_intent(&asks, &ask, "alpha", None, true, false)
        .unwrap());
    assert!(sidecar.snapshot().unwrap().asks.contains_key(&id));
    assert!(asks.lookup_ask(&id).unwrap().is_some());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn archiver_waits_for_authority_before_asks_and_sees_pending_changes() {
    use crate::asks::store::tests::REQUESTER;
    use std::sync::mpsc;
    let (asks, dir) = temp_store("archive-sidecar-race");
    let roster =
        crate::agent_management::AgentManagementStore::open(dir.join("authority")).unwrap();
    let sidecar = crate::director_address::Store::at(dir.join("sidecar"));
    let mut state = AskState::default();
    let id = closed(&mut state, "old");
    sidecar
        .stamp_at(&state.asks[&id], REQUESTER, "alpha", 1)
        .unwrap();
    asks.with_state(|target| *target = state).unwrap();
    let guard = roster.lock_mutations().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (archive_done_tx, archive_done_rx) = mpsc::channel();
    let thread_asks = asks.clone();
    let thread_roster = roster.clone();
    let thread_sidecar = sidecar.clone();
    let archiver = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = thread_asks.archive_before_with_authority(
            &thread_roster,
            &thread_sidecar,
            Some(cutoff()),
            cutoff(),
        );
        archive_done_tx.send(result).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (write_tx, write_rx) = mpsc::channel();
    let writer_asks = asks.clone();
    let writer = std::thread::spawn(move || {
        write_tx
            .send(writer_asks.with_state(|state| {
                state.wait_cancel.insert("probe".into(), 1);
            }))
            .unwrap();
    });
    let write = write_rx.recv_timeout(Duration::from_secs(5));
    if write.is_err() {
        drop(guard);
        archiver.join().unwrap();
        writer.join().unwrap();
        panic!("archiver acquired asks before authority mutation fence");
    }
    write.unwrap().unwrap();
    assert!(
        archive_done_rx.try_recv().is_err(),
        "authority fence must still block archival"
    );
    sidecar
        .update(|state| {
            state.asks.get_mut(&id).unwrap().summary_pending = true;
            Ok(())
        })
        .unwrap();
    drop(guard);
    let report = archive_done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    archiver.join().unwrap();
    writer.join().unwrap();
    assert_eq!(report.asks, 0);
    assert!(asks.snapshot().unwrap().asks.contains_key(&id));
    assert!(sidecar.snapshot().unwrap().asks[&id].summary_pending);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "synthetic EVA-shaped 17.2 MB payload benchmark; run in an isolated process"]
fn benchmark_eva_shaped_split() {
    let (store, dir) = temp_store("archive-eva-shape");
    let mut state = AskState::default();
    // Separate collections reproduce retained serde work, not real owner text.
    for n in 0..881 {
        let id = closed(&mut state, &format!("old-{n}"));
        state.asks.get_mut(&id).unwrap().body = body(&"a".repeat(4_600_000 / 881));
    }
    let ask_overhead: usize = state
        .asks
        .values()
        .map(|ask| serde_json::to_vec(ask).unwrap().len() - ask.body.text.len())
        .sum();
    let brief_bytes = 4_600_000_usize.saturating_sub(ask_overhead) / 881;
    for ask in state.asks.values_mut() {
        ask.body = body(&"a".repeat(brief_bytes));
    }
    let seed = state.asks.values().next().unwrap().clone();
    let mut ids = state.asks.keys().cloned().cycle();
    for n in 0..878 {
        let id = ids.next().unwrap();
        state.answers.insert(
            format!("ans-{n}"),
            crate::asks::model::AnswerRecord {
                answer_id: format!("ans-{n}"),
                ask_id: id,
                attempt: 1,
                revision: 1,
                status: crate::asks::model::AnswerStatus::Completed,
                artifact_id: None,
                bytes: 0,
                metadata: crate::asks::model::AnswerMetadata {
                    native_diff: Some("b".repeat(10_300_000 / 878)),
                    ..Default::default()
                },
                created_at: "2026-10-23T00:00:00Z".into(),
                preview: None,
            },
        );
    }
    for n in 0..8773 {
        let id = format!("art-{n:064x}");
        state.artifacts.insert(
            id.clone(),
            ArtifactRecord {
                artifact_id: id,
                bytes: 1,
                media_type: "application/octet-stream".into(),
                readers: vec![],
                asks: vec![seed.ask_id.clone()],
                created_at: "2026-10-23T00:00:00Z".into(),
            },
        );
    }
    // Keep eight genuinely open/queued old records hot, plus their answers.
    // An age-only filter would incorrectly archive these too.
    for ask in state.asks.values_mut().rev().take(8) {
        ask.closed = None;
        ask.closed_at = None;
        ask.execution = ExecutionState::Queued;
    }
    state.notices.clear();
    store.with_state(|target| *target = state).unwrap();
    let path = dir.join(crate::management::v2::hot_store::DATABASE_FILE);
    let loaded = crate::management::v2::hot_store::verified_rows(&path, "asks")
        .unwrap()
        .unwrap();
    // Match r261's owned-row production rebuild, not the compatibility
    // wrapper which first clones all raw bytes. Clone fixture input before
    // starting the timer; this is serde rebuild, not SQLite fetch or send.
    let owned_before = loaded.rows.clone();
    let start = std::time::Instant::now();
    let state = crate::asks::store::ask_state_from_owned_rows(owned_before).unwrap();
    let before_us = start.elapsed().as_micros();
    let before_bytes: usize = loaded.rows.values().map(Vec::len).sum();
    let collection_bytes: BTreeMap<String, usize> =
        loaded
            .rows
            .iter()
            .fold(BTreeMap::new(), |mut acc, ((kind, _), value)| {
                *acc.entry(kind.clone()).or_default() += value.len();
                acc
            });
    let keys = plan(&state, cutoff()).unwrap();
    let moved = move_rows(&path, loaded.generation, &keys, cutoff(), cutoff()).unwrap();
    let db = open(&path).unwrap();
    let mut hot = Rows::new();
    {
        let mut q = db
            .prepare("SELECT collection,row_key,value_json FROM hot_store_rows WHERE store='asks'")
            .unwrap();
        for r in q
            .query_map([], |r| {
                Ok((
                    (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .unwrap()
        {
            let (k, v) = r.unwrap();
            hot.insert(k, v);
        }
    }
    let owned_after = hot.clone();
    let start = std::time::Instant::now();
    let small = crate::asks::store::ask_state_from_owned_rows(owned_after).unwrap();
    let after_us = start.elapsed().as_micros();
    let after_bytes: usize = hot.values().map(Vec::len).sum();
    println!("ASK_ARCHIVE_BENCH before_bytes={before_bytes} after_bytes={after_bytes} before_rebuild_us={before_us} after_rebuild_us={after_us} moved_rows={} moved_asks={} hot_asks={} collections={collection_bytes:?}",moved.rows,moved.asks,small.asks.len());
    assert_eq!(small.asks.len(), 8);
    assert!(after_bytes * 20 < before_bytes);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
