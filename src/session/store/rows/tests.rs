use std::fs;
use std::path::Path;
use std::path::PathBuf;

use super::*;
use crate::session::model::NativeStopReceipt;
use crate::session::store::load_cutex_session_store_from_path;
use crate::session::store::save_cutex_session_store_to_path;
use crate::session::store::save_locked_session_store;
use crate::session::store::session_database_path;
use crate::session::store::with_locked_session_store;
use crate::session::store::CutexSessionStoreRevisionConflict;

fn temp_store(label: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("cutex-rows-{label}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("cutex-sessions.json");
    (root, path)
}

fn record(name: &str) -> CutexSessionRecord {
    CutexSessionRecord::new_at(
        format!("cutex.{name}"),
        Some(format!("thread-{name}")),
        "devbox".to_string(),
        format!("/tmp/{name}"),
        None,
        "2026-10-09T00:00:00Z".to_string(),
    )
    .expect("record")
}

fn stop_receipt(generation: u64) -> NativeStopReceipt {
    NativeStopReceipt {
        native_id: Some("native".into()),
        generation,
        binding: None,
    }
}

/// A store with sessions a, b, c at revision 1.
fn seeded(label: &str) -> (PathBuf, PathBuf) {
    let (root, path) = temp_store(label);
    let mut store = CutexSessionStore::default();
    for name in ["a", "b", "c"] {
        store.sessions.insert(format!("cutex.{name}"), record(name));
    }
    save_cutex_session_store_to_path(&path, &store).unwrap();
    assert_eq!(store.store_revision.get(), 1);
    (root, path)
}

/// `(key, rev)` of every session row and `(map, key, rev)` of every receipt.
fn row_revs(path: &Path) -> (Vec<(String, i64)>, Vec<(String, String, i64)>) {
    let connection = rusqlite::Connection::open(session_database_path(path)).unwrap();
    let mut statement = connection
        .prepare("SELECT key, rev FROM sessions ORDER BY key")
        .unwrap();
    let sessions = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut statement = connection
        .prepare("SELECT map, key, rev FROM receipts ORDER BY map, key")
        .unwrap();
    let receipts = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (sessions, receipts)
}

fn revision(path: &Path) -> u64 {
    load_cutex_session_store_from_path(path)
        .unwrap()
        .store_revision
        .get()
}

#[test]
fn a_row_commit_writes_only_touched_rows_in_one_revision() {
    let (root, path) = seeded("touched");
    let full_reads = sqlite::FULL_READS.with(|count| count.get());
    let committed = with_locked_session_rows(&path, |rows| {
        assert_eq!(rows.revision(), 1);
        rows.session_mut("cutex.a").unwrap().note = Some("changed".into());
        // Touched but unchanged: not written.
        rows.session_mut("cutex.b").unwrap();
        rows.insert_session("cutex.d", record("d"));
        rows.remove_session("cutex.c");
        rows.insert_receipt::<NativeStopReceipts>("cutex.a", stop_receipt(4));
        assert!(rows.is_dirty());
        // The view shows the uncommitted changes, the committed store not.
        assert_eq!(rows.session("cutex.a").unwrap().note.as_deref(), Some("changed"));
        assert!(rows.session("cutex.c").is_none());
        assert!(rows.committed().sessions.contains_key("cutex.c"));
        assert_eq!(rows.sessions().count(), 3);
        assert_eq!(rows.receipt::<NativeStopReceipts>("cutex.a"), Some(&stop_receipt(4)));
        let next = rows.commit()?;
        assert!(!rows.is_dirty());
        assert_eq!(rows.revision(), next);
        assert!(!rows.committed().sessions.contains_key("cutex.c"));
        Ok(next)
    })
    .unwrap();
    assert_eq!(committed, 2);
    let (sessions, receipts) = row_revs(&path);
    assert_eq!(
        sessions,
        vec![
            ("cutex.a".to_string(), 2),
            ("cutex.b".to_string(), 1),
            ("cutex.d".to_string(), 2)
        ]
    );
    assert_eq!(
        receipts,
        vec![("native_stop".to_string(), "cutex.a".to_string(), 2)],
        "the receipt is in the same commit as its session change"
    );
    // The process cache was patched, not re-read.
    let store = sqlite::shared(&path).unwrap();
    assert_eq!(store.store_revision.get(), 2);
    assert_eq!(store.sessions["cutex.a"].note.as_deref(), Some("changed"));
    assert!(store.sessions.contains_key("cutex.d") && !store.sessions.contains_key("cutex.c"));
    assert_eq!(store.native_stop_receipts["cutex.a"], stop_receipt(4));
    assert_eq!(sqlite::FULL_READS.with(|count| count.get()), full_reads);
    // And a fresh process reads the same.
    sqlite::forget_process_state(&session_database_path(&path));
    assert_eq!(
        serde_json::to_value(&*sqlite::shared(&path).unwrap()).unwrap(),
        serde_json::to_value(&*store).unwrap()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_commit_that_changes_nothing_writes_nothing() {
    let (root, path) = seeded("unchanged");
    with_locked_session_rows(&path, |rows| {
        rows.session_mut("cutex.a").unwrap();
        let same = rows.session("cutex.b").unwrap().clone();
        rows.insert_session("cutex.b", same);
        assert_eq!(rows.commit()?, 1);
        // Nothing touched at all.
        assert_eq!(rows.commit()?, 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(revision(&path), 1);
    assert_eq!(update_session_row(&path, "cutex.a", |_| Ok(())).unwrap(), Some(()));
    assert_eq!(revision(&path), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn several_commits_in_one_closure_and_uncommitted_changes_are_dropped() {
    let (root, path) = seeded("several");
    with_locked_session_rows(&path, |rows| {
        rows.session_mut("cutex.a").unwrap().note = Some("one".into());
        assert_eq!(rows.commit()?, 2);
        rows.session_mut("cutex.a").unwrap().note = Some("two".into());
        assert_eq!(rows.commit()?, 3);
        rows.session_mut("cutex.b").unwrap().note = Some("never".into());
        Ok(())
    })
    .unwrap();
    let store = load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(store.store_revision.get(), 3);
    assert_eq!(store.sessions["cutex.a"].note.as_deref(), Some("two"));
    assert_eq!(store.sessions["cutex.b"].note, None);
    // A failing closure writes nothing either.
    let error = with_locked_session_rows(&path, |rows| -> anyhow::Result<()> {
        rows.remove_session("cutex.a");
        anyhow::bail!("validation failed")
    })
    .unwrap_err();
    assert!(error.to_string().contains("validation failed"));
    assert!(load_cutex_session_store_from_path(&path)
        .unwrap()
        .sessions
        .contains_key("cutex.a"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_revision_moved_behind_the_lock_is_a_conflict() {
    let (root, path) = seeded("conflict");
    let error = with_locked_session_rows(&path, |rows| {
        // A writer that does not take the store lock (hand repair) commits.
        let connection = rusqlite::Connection::open(session_database_path(&path)).unwrap();
        connection
            .execute("UPDATE meta SET value = '7' WHERE key = 'store_revision'", [])
            .unwrap();
        drop(connection);
        rows.session_mut("cutex.a").unwrap().note = Some("lost?".into());
        rows.commit()
    })
    .unwrap_err();
    assert_eq!(
        *error
            .downcast_ref::<CutexSessionStoreRevisionConflict>()
            .unwrap(),
        CutexSessionStoreRevisionConflict {
            expected: 1,
            actual: 7
        }
    );
    let store = load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(store.store_revision.get(), 7);
    assert_eq!(store.sessions["cutex.a"].note, None);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn compare_and_swap_rejects_a_changed_row() {
    let (root, path) = seeded("cas");
    let before = load_session_row(&path, "cutex.a").unwrap().unwrap().1;
    let mut first = before.clone();
    first.note = Some("first".into());
    assert_eq!(
        compare_and_swap_session_row(&path, "cutex.a", Some(&before), Some(first.clone()))
            .unwrap(),
        2
    );
    let mut stale = before.clone();
    stale.note = Some("stale".into());
    let error =
        compare_and_swap_session_row(&path, "cutex.a", Some(&before), Some(stale)).unwrap_err();
    assert_eq!(
        error.downcast_ref::<SessionRowConflict>().unwrap().key,
        "cutex.a"
    );
    // Create only if absent; delete only if unchanged.
    assert!(compare_and_swap_session_row(&path, "cutex.a", None, Some(record("x"))).is_err());
    compare_and_swap_session_row(&path, "cutex.new", None, Some(record("new"))).unwrap();
    compare_and_swap_session_row(&path, "cutex.a", Some(&first), None).unwrap();
    let store = load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(store.store_revision.get(), 4);
    assert!(!store.sessions.contains_key("cutex.a"));
    assert!(store.sessions.contains_key("cutex.new"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn single_rows_load_by_key_or_id_without_creating_a_store() {
    let (root, path) = temp_store("load");
    assert!(load_session_row(&path, "cutex.a").unwrap().is_none());
    assert!(!session_database_path(&path).exists(), "a read creates nothing");
    let mut store = CutexSessionStore::default();
    store.sessions.insert("cutex.a".into(), record("a"));
    // A key that differs from the record id (older stores have some).
    store.sessions.insert("legacy-key".into(), record("b"));
    save_cutex_session_store_to_path(&path, &store).unwrap();
    let (key, loaded) = load_session_row(&path, "cutex.a").unwrap().unwrap();
    assert_eq!((key.as_str(), &loaded), ("cutex.a", &store.sessions["cutex.a"]));
    let (key, loaded) = load_session_row(&path, "cutex.b").unwrap().unwrap();
    assert_eq!((key.as_str(), loaded.cutex_session_id.as_str()), ("legacy-key", "cutex.b"));
    assert!(load_session_row(&path, "cutex.zzz").unwrap().is_none());
    assert_eq!(update_session_row(&path, "cutex.zzz", |_| Ok(())).unwrap(), None);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn receipt_rows_are_written_and_removed_alone() {
    let (root, path) = seeded("receipts");
    update_receipt_row::<NativeStopReceipts>(&path, "cutex.a", Some(stop_receipt(1))).unwrap();
    update_receipt_row::<NativeStopReceipts>(&path, "cutex.b", Some(stop_receipt(2))).unwrap();
    let (sessions, receipts) = row_revs(&path);
    assert!(sessions.iter().all(|(_, rev)| *rev == 1), "sessions untouched");
    assert_eq!(receipts.len(), 2);
    update_receipt_row::<NativeStopReceipts>(&path, "cutex.a", None).unwrap();
    with_locked_session_rows(&path, |rows| {
        assert_eq!(rows.receipt::<NativeStopReceipts>("cutex.a"), None);
        rows.remove_receipt::<NativeStopReceipts>("cutex.b");
        assert_eq!(rows.receipt::<NativeStopReceipts>("cutex.b"), None);
        rows.commit()
    })
    .unwrap();
    let store = load_cutex_session_store_from_path(&path).unwrap();
    assert!(store.native_stop_receipts.is_empty());
    assert_eq!(store.store_revision.get(), 5);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_sessions_copy_round_trips_only_its_differences() {
    let (root, path) = seeded("copy");
    update_receipt_row::<NativeStopReceipts>(&path, "cutex.a", Some(stop_receipt(1))).unwrap();
    with_locked_session_rows(&path, |rows| {
        let mut copy = rows.sessions_copy();
        assert_eq!(copy.store_revision.get(), rows.revision());
        assert!(copy.native_stop_receipts.is_empty(), "no receipts copied");
        copy.sessions.get_mut("cutex.a").unwrap().note = Some("x".into());
        copy.sessions.remove("cutex.b");
        copy.sessions.insert("cutex.e".into(), record("e"));
        rows.replace_sessions(&copy)?;
        let mut with_receipt = rows.sessions_copy();
        with_receipt
            .native_stop_receipts
            .insert("cutex.e".into(), stop_receipt(9));
        assert!(rows.replace_sessions(&with_receipt).is_err());
        rows.commit()
    })
    .unwrap();
    let (sessions, _) = row_revs(&path);
    assert_eq!(
        sessions,
        vec![
            ("cutex.a".to_string(), 3),
            ("cutex.c".to_string(), 1),
            ("cutex.e".to_string(), 3)
        ]
    );
    let store = load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(store.native_stop_receipts.len(), 1, "receipts kept");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn row_writers_and_whole_store_writers_share_the_store_lock() {
    let (root, path) = seeded("shared-lock");
    // A whole-store writer that nests nothing still sees the row commit
    // made before it took the lock, and vice versa.
    update_session_row(&path, "cutex.a", |record| {
        record.note = Some("row".into());
        Ok(())
    })
    .unwrap();
    with_locked_session_store(&path, |store| {
        assert_eq!(store.sessions["cutex.a"].note.as_deref(), Some("row"));
        store.sessions.get_mut("cutex.b").unwrap().note = Some("whole".into());
        save_locked_session_store(&path, store)
    })
    .unwrap();
    with_locked_session_rows(&path, |rows| {
        assert_eq!(rows.revision(), 3);
        assert_eq!(rows.session("cutex.b").unwrap().note.as_deref(), Some("whole"));
        Ok(())
    })
    .unwrap();
    fs::remove_dir_all(root).unwrap();
}

// --- Several processes -----------------------------------------------------

const CHILD_ENV: &str = "CUTEX_SESSION_ROWS_TEST_CHILD";
const ROUNDS: usize = 24;
const SHARED: &str = "cutex.shared";

fn spawn_child(path: &Path, id: usize) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session::store::rows::tests::child_mixed_writer",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, path)
        .env("CUTEX_SESSION_ROWS_TEST_ID", id.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

/// One writer's rounds. Every round increments the shared counter
/// (`runtime_generation` of `cutex.shared`, a read-modify-write that loses
/// updates unless writers serialise) and adds a record of its own, rotating
/// through: a row-level update, a row-level view with a receipt, a
/// whole-store update under the lock, and a lock-free whole-store
/// compare-and-swap that retries on conflict. Returns the store revisions
/// its commits produced, in order.
fn mixed_rounds(path: &Path, id: usize) -> Vec<u64> {
    let mut revisions = Vec::new();
    for n in 0..ROUNDS {
        let key = format!("cutex.p{id}-{n}");
        match n % 4 {
            0 => {
                update_session_row(path, SHARED, |record| {
                    record.runtime_generation += 1;
                    Ok(())
                })
                .unwrap()
                .unwrap();
                revisions.push(revision(path));
                revisions.push(
                    compare_and_swap_session_row(path, &key, None, Some(record(&key))).unwrap(),
                );
            }
            1 => revisions.push(
                with_locked_session_rows(path, |rows| {
                    rows.session_mut(SHARED).unwrap().runtime_generation += 1;
                    rows.insert_session(key.clone(), record(&key));
                    rows.insert_receipt::<NativeStopReceipts>(key.clone(), stop_receipt(n as u64));
                    rows.commit()
                })
                .unwrap(),
            ),
            2 => revisions.push(
                with_locked_session_store(path, |store| {
                    store.sessions.get_mut(SHARED).unwrap().runtime_generation += 1;
                    store.sessions.insert(key.clone(), record(&key));
                    save_locked_session_store(path, store)?;
                    Ok(store.store_revision.get())
                })
                .unwrap(),
            ),
            _ => loop {
                let mut store = load_cutex_session_store_from_path(path).unwrap();
                store.sessions.get_mut(SHARED).unwrap().runtime_generation += 1;
                store.sessions.insert(key.clone(), record(&key));
                match save_cutex_session_store_to_path(path, &store) {
                    Ok(()) => {
                        revisions.push(store.store_revision.get());
                        break;
                    }
                    Err(error)
                        if error
                            .downcast_ref::<CutexSessionStoreRevisionConflict>()
                            .is_some() => {}
                    Err(error) => panic!("{error:#}"),
                }
            },
        }
    }
    revisions
}

#[test]
#[ignore = "child process of mixed_row_and_whole_store_writers_in_several_processes_lose_nothing"]
fn child_mixed_writer() {
    let Some(path) = std::env::var_os(CHILD_ENV).map(PathBuf::from) else {
        return;
    };
    let id: usize = std::env::var("CUTEX_SESSION_ROWS_TEST_ID")
        .unwrap()
        .parse()
        .unwrap();
    let revisions = mixed_rounds(&path, id);
    assert!(
        revisions.windows(2).all(|pair| pair[0] < pair[1]),
        "revisions seen by one writer only grow: {revisions:?}"
    );
    println!("CHILD_DONE {id}");
}

#[test]
fn mixed_row_and_whole_store_writers_in_several_processes_lose_nothing() {
    let (root, path) = temp_store("processes");
    let mut store = CutexSessionStore::default();
    store.sessions.insert(SHARED.into(), record("shared"));
    save_cutex_session_store_to_path(&path, &store).unwrap();
    let start = store.store_revision.get();
    let generation = store.sessions[SHARED].runtime_generation;
    let children = (0..4).map(|id| spawn_child(&path, id)).collect::<Vec<_>>();
    let threads = (10..12)
        .map(|id| {
            let path = path.clone();
            std::thread::spawn(move || {
                let revisions = mixed_rounds(&path, id);
                assert!(revisions.windows(2).all(|pair| pair[0] < pair[1]));
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("CHILD_DONE"),
            "child failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let writers = 4 + 2;
    let store = load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(
        store.sessions[SHARED].runtime_generation,
        generation + (writers * ROUNDS) as u64,
        "no lost counter update"
    );
    assert_eq!(store.sessions.len(), 1 + writers * ROUNDS);
    assert_eq!(store.native_stop_receipts.len(), writers * ROUNDS / 4);
    // Every commit changed something: one revision each (round kind 0
    // commits twice).
    assert_eq!(
        store.store_revision.get(),
        start + (writers * (ROUNDS + ROUNDS / 4)) as u64
    );
    // Row revisions never exceed the store revision.
    let (sessions, receipts) = row_revs(&path);
    let top = store.store_revision.get() as i64;
    assert!(sessions.iter().all(|(_, rev)| *rev <= top));
    assert!(receipts.iter().all(|(_, _, rev)| *rev <= top));
    fs::remove_dir_all(root).unwrap();
}
