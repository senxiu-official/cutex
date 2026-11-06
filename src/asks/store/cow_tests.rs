//! r273 stab: copy-on-write AskState. Owned snapshots and write drafts share
//! every collection the write does not touch; one changed ask copies only
//! that record. Before r273 each copy of host-a's hot store was ~44 MB.
use super::*;
use crate::asks::shared::Shared;

fn seeded(label: &str) -> (AskStore, PathBuf, Vec<String>) {
    let (store, dir) = tests::temp_store(label);
    let ids = store.with_state(|state| {
        let mut ids = Vec::new();
        for n in 0..8 {
            let (ask, _) = state.create_ask(tests::new_ask(&format!("cow-{n}"), "body"), Utc::now()).unwrap();
            ids.push(ask.ask_id);
        }
        ids
    }).unwrap();
    (store, dir, ids)
}

// The process-wide parsed-state cache holds one store, and parallel tests
// replace it; pointer identity is therefore asserted on AskState clones (the
// exact operations snapshot()/with_state perform on the cached Arc).
#[test]
fn clones_share_storage_and_a_change_copies_only_that_record() {
    let (store, dir, ids) = seeded("cow-clone");
    let committed = store.snapshot().unwrap();
    let copy = committed.clone();
    assert!(Shared::ptr_eq(&committed.asks, &copy.asks));
    assert!(Shared::ptr_eq(&committed.actions, &copy.actions));
    assert!(Shared::ptr_eq(&committed.answers, &copy.answers));
    assert!(Shared::ptr_eq(&committed.artifacts, &copy.artifacts));
    let mut draft = committed.clone();
    let (changed, untouched) = (&ids[0], &ids[1]);
    draft.asks.get_mut(changed).unwrap().title = "renamed".into();
    assert_ne!(committed.asks[changed].title, "renamed", "the committed state never sees a draft change");
    assert!(!Shared::ptr_eq(&committed.asks, &draft.asks));
    assert!(!Shared::ptr_eq(&committed.asks[changed], &draft.asks[changed]));
    assert!(Shared::ptr_eq(&committed.asks[untouched], &draft.asks[untouched]), "untouched record is not copied");
    assert!(Shared::ptr_eq(&committed.answers, &draft.answers), "untouched collection is not copied");
    // Row diff and change detection see exactly the one record.
    assert!(!persisted_state_equal(&committed, &draft).unwrap());
    let changes = ask_state_changes(&committed, &draft).unwrap();
    assert!(matches!(changes.as_slice(), [crate::management::v2::hot_store::RowChange::Put { collection, key, .. }]
        if collection == "asks" && key == changed), "{changes:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_write_persists_exactly_and_old_snapshots_stay_isolated() {
    let (store, dir, ids) = seeded("cow-write");
    let before = store.snapshot().unwrap();
    let changed = &ids[0];
    let title = before.asks[changed].title.clone();
    store.with_state(|state| state.asks.get_mut(changed).unwrap().title = "renamed".into()).unwrap();
    let after = store.snapshot().unwrap();
    assert_eq!(after.asks[changed].title, "renamed");
    assert_eq!(before.asks[changed].title, title, "a snapshot taken before the write never observes it");
    // Persisted rows equal the published state (fresh decode, no cache).
    let rows = crate::management::v2::hot_store::verified_rows(&store.database_path(), "asks").unwrap().unwrap();
    assert_eq!(ask_state_from_rows(&rows.rows).unwrap(), after);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unchanged_write_is_detected_without_comparing_shared_records() {
    let (store, dir, ids) = seeded("cow-noop");
    let revision = store.snapshot().unwrap().revision;
    // Touching (but not changing) one record still finds no change.
    store.with_state(|state| { let title = state.asks[&ids[0]].title.clone(); state.asks.get_mut(&ids[0]).unwrap().title = title; }).unwrap();
    store.with_state(|_| ()).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, revision, "no commit for an unchanged state");
    let before = store.snapshot().unwrap();
    let mut after = before.clone();
    after.wait_cancel.insert("s".into(), 1);
    let changes = ask_state_changes(&before, &after).unwrap();
    assert_eq!(changes.len(), 1, "only the changed row: {changes:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn history_decodes_are_bounded() {
    let first = history_decode_permit();
    let second = history_decode_permit();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || { let _third = history_decode_permit(); tx.send(()).unwrap(); });
    assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(), "a third decode waits");
    drop(first);
    rx.recv_timeout(std::time::Duration::from_secs(5)).expect("released permit admits the waiter");
    waiter.join().unwrap();
    drop(second);
}

#[test]
fn merged_history_matches_a_full_decode_and_follows_every_write() {
    let (store, dir) = tests::temp_store("cow-history");
    let old = crate::asks::parse_time("2026-10-01T00:00:00Z").unwrap();
    let cutoff = crate::asks::parse_time("2026-10-30T00:00:00Z").unwrap();
    let cold = store.with_state(|state| {
        let (ask, _) = state.create_ask(tests::new_ask("cow-cold", "history"), old).unwrap();
        state.update_ask(&ask.ask_id, old, |ask| {
            ask.closed = Some(CloseOutcome::Abandoned);
            ask.execution = ExecutionState::Cancelled;
            ask.closed_at = Some(now_rfc3339(old));
        });
        state.notices.clear();
        ask.ask_id
    }).unwrap();
    assert_eq!(store.archive_before(Some(cutoff), cutoff).unwrap().asks, 1);
    let hot = store.with_state(|state| state.create_ask(tests::new_ask("cow-hot", "now"), Utc::now()).unwrap().0.ask_id).unwrap();
    let full = || ask_state_from_rows(&crate::asks::archive::history_rows(&store.database_path()).unwrap()).unwrap();
    for _ in 0..2 {
        // The second round is served from the merged-history cache.
        let history = store.history_snapshot().unwrap();
        assert!(history.asks.contains_key(&cold) && history.asks.contains_key(&hot));
        assert_eq!(history, full());
        assert_eq!(store.readonly_history_snapshot().unwrap(), full());
    }
    // A write commits a new generation: the cache never serves stale history.
    store.with_state(|state| state.asks.get_mut(&hot).unwrap().title = "after".into()).unwrap();
    assert_eq!(store.history_snapshot().unwrap().asks[&hot].title, "after");
    assert_eq!(store.readonly_history_snapshot().unwrap().asks[&hot].title, "after");
    assert_eq!(store.history_snapshot().unwrap(), full());
    // Promotion back to hot moves rows between hot and cold.
    store.promote_history(&cold).unwrap();
    assert!(store.snapshot().unwrap().asks.contains_key(&cold));
    assert_eq!(store.history_snapshot().unwrap(), full());
    assert_eq!(store.readonly_history_snapshot().unwrap(), full());
    std::fs::remove_dir_all(dir).unwrap();
}
