//! Isolated regression/benchmark fixtures; never use a live store.
use super::*;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[test]
#[ignore = "child process invoked by reader_writer_process_probe"]
fn reader_writer_process_child() {
    let Some(dir) = std::env::var_os("ASK_READER_PROBE_DIR") else { panic!("private parent required") };
    let dir = PathBuf::from(dir);
    assert!(dir.join("private-reader-probe").is_file());
    let store = AskStore::at(&dir);
    std::fs::write(dir.join("writer-ready"), b"ready").unwrap();
    while !dir.join("start").exists() { std::thread::sleep(Duration::from_millis(2)); }
    let mut samples = Vec::new();
    for _ in 0..12 {
        let t = Instant::now();
        store.with_state(|s| s.next_seq += 1).unwrap();
        samples.push(t.elapsed().as_secs_f64()*1000.);
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(dir.join("writer.json"), serde_json::to_vec(&samples).unwrap()).unwrap();
}

#[test]
#[ignore = "48 readers and a real separate writer process; run exact and alone"]
fn reader_writer_process_probe() {
    let (mut store, dir) = tests::temp_store("reader-safe-probe");
    store.cache_generation = true;
    store.with_state(|s| {
        let (mut ask, _) = s.create_ask(tests::new_ask("seed", "fixture"), Utc::now()).unwrap();
        s.asks.clear();
        ask.body = tests::body(&"x".repeat(32*1024));
        for n in 0..256 { ask.ask_id = format!("probe-{n}"); s.asks.insert(ask.ask_id.clone(), ask.clone()); }
    }).unwrap();
    std::fs::write(dir.join("private-reader-probe"), b"owned synthetic test").unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "asks::store::reader_safety_tests::reader_writer_process_child", "--ignored", "--nocapture"])
        .env("ASK_READER_PROBE_DIR", &dir).spawn().unwrap();
    let deadline = Instant::now()+Duration::from_secs(20);
    while !dir.join("writer-ready").exists() {
        assert!(Instant::now()<deadline && child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    let barrier = Arc::new(Barrier::new(49));
    let wall = Instant::now();
    let reads = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..48).map(|_| {
            let barrier = barrier.clone(); let store = &store;
            scope.spawn(move || {
                barrier.wait(); let end = Instant::now()+Duration::from_secs(5); let mut n=0;
                while Instant::now()<end {
                    assert_eq!(store.project_snapshot(|s| s.asks.len()).unwrap(),256);
                    n+=1; std::thread::sleep(Duration::from_millis(2));
                }
                n
            })
        }).collect();
        barrier.wait(); std::fs::write(dir.join("start"),b"go").unwrap();
        handles.into_iter().map(|h| h.join().unwrap()).sum::<u64>()
    });
    assert!(child.wait().unwrap().success());
    let mut samples: Vec<f64> = serde_json::from_slice(&std::fs::read(dir.join("writer.json")).unwrap()).unwrap();
    samples.sort_by(f64::total_cmp);
    println!("READER_WRITER_PROBE {}", json!({"readers":48,"readerSeconds":5,"writerProcess":true,"writes":samples.len(),"reads":reads,"p50Ms":samples[samples.len()/2],"p95Ms":samples[(samples.len()*95/100).min(samples.len()-1)],"maxMs":samples.last(),"wallSeconds":wall.elapsed().as_secs_f64(),"fixture":dir}));
    crate::management::v2::hot_store::clear_cached_generation();
}

#[test]
fn locked_callback_excludes_writer_until_derived_sync_finishes() {
    let (store, _) = tests::temp_store("locked-callback");
    store.with_state(|s| { s.create_ask(tests::new_ask("first", "first"), Utc::now()).unwrap(); }).unwrap();
    let (captured_tx, captured_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (writer_tx, writer_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let store = &store;
        let reader = scope.spawn(move || store.inspect(|old| {
            captured_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            // Exercise a durable owner projection using the captured AskState.
            store.queue_repository().unwrap().sync_asks(&AskState::default(), old).unwrap();
        }).unwrap());
        captured_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let writer = scope.spawn(|| store.with_state(|s| {
            writer_tx.send(()).unwrap();
            s.create_ask(tests::new_ask("second", "second"), Utc::now()).unwrap().0.ask_id
        }).unwrap());
        let competing = store.lock_file().unwrap();
        let callback_holds_lock = fs2::FileExt::try_lock_exclusive(&competing).is_err();
        drop(competing);
        let entered_early = writer_rx.recv_timeout(Duration::from_millis(150)).is_ok();
        release_tx.send(()).unwrap(); reader.join().unwrap();
        let id = writer.join().unwrap();
        assert!(callback_holds_lock, "inspect must retain its OS lock through derived writes");
        assert!(!entered_early, "writer entered while stale derived-write callback held ask lock");
        let (_, _, _, admission) = store.queue_repository().unwrap().list_admission(tests::TARGET).unwrap();
        assert!(admission.items.iter().any(|item| item.id == id));
    });
}

#[test]
fn locked_list_waits_for_owner_sync_then_sql_commit() {
    let (store, _) = tests::temp_store("sync-before-commit");
    store.with_state(|s| s.next_seq = 1).unwrap();
    let lock = store.lock_file().unwrap();
    lock.lock_exclusive().unwrap();
    let (before, generation) = store.read_state_shared().unwrap();
    let mut after = (*before).clone();
    let ask = after.create_ask(tests::new_ask("between", "between"), Utc::now()).unwrap().0;
    store.queue_repository().unwrap().sync_asks(&before, &after).unwrap();
    // Owner JSON already represents the new ask; SQLite still represents G.
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let store = &store;
        let reader = scope.spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(store.queue_repository().unwrap().list_admission(tests::TARGET)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let early = result_rx.recv_timeout(Duration::from_millis(150)).ok();
        after.revision += 1;
        crate::management::v2::hot_store::commit(&store.database_path(), "asks", generation, ask_state_changes(&before, &after).unwrap()).unwrap();
        drop(lock);
        reader.join().unwrap();
        assert!(early.is_none(), "old asks read overwrote owner state before SQL publication");
        let (_, _, _, admission) = result_rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert!(admission.items.iter().any(|item| item.id == ask.ask_id));
    });
}

#[test]
fn pure_callback_does_not_block_writer_or_regress_cached_generation() {
    let (store, _) = tests::temp_store("pure-progress");
    store.with_state(|s| s.next_seq = 1).unwrap();
    let (captured_tx, captured_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let store = &store;
        let reader = scope.spawn(move || store.project_snapshot(|old| {
            captured_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            old.next_seq
        }).unwrap());
        captured_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        store.with_state(|s| s.next_seq = 2).unwrap();
        let generation = store.store_generation().unwrap().unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(reader.join().unwrap(), 1, "old immutable snapshot stays coherent");
        assert_eq!(store.project_snapshot(|s| s.next_seq).unwrap(), 2);
        let cache = ASK_STATE_CACHE.lock().unwrap();
        let (_, cached_generation, state, _) = cache.as_ref().unwrap();
        assert_eq!(*cached_generation, generation);
        assert_eq!(state.next_seq, 2);
    });
}

#[test]
fn sqlite_generation_and_rows_share_snapshot_across_external_commit() {
    use crate::management::v2::hot_store as hot;
    let (store, _) = tests::temp_store("coherent-sql");
    store.with_state(|s| s.next_seq = 41).unwrap();
    let generation = store.store_generation().unwrap().unwrap();
    let database = store.database_path();
    let external = database.clone();
    hot::READ_SNAPSHOT_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
        // A second SQLite connection commits between metadata and row SELECTs.
        hot::commit(&external, "asks", generation, vec![hot::RowChange::Put {
            collection: "state".into(), key: "next_seq".into(), value: serde_json::to_vec(&42u64).unwrap(),
        }]).unwrap();
    })));
    let loaded = hot::load_or_migrate(&database, "asks", &store.state_path(), |_| unreachable!(), |_| Ok(())).unwrap();
    assert_eq!(loaded.generation, generation);
    assert_eq!(ask_state_from_rows(&loaded.rows).unwrap().next_seq, 41);
    let loaded = hot::load_or_migrate(&database, "asks", &store.state_path(), |_| unreachable!(), |_| Ok(())).unwrap();
    assert_eq!(loaded.generation, generation + 1);
    assert_eq!(ask_state_from_rows(&loaded.rows).unwrap().next_seq, 42);
}

#[test]
fn pure_projection_keeps_absent_store_absent() {
    let (store, dir) = tests::temp_store("pure-absent");
    assert_eq!(store.project_snapshot(|s| s.asks.len()).unwrap(), 0);
    assert!(!dir.exists());
}

#[test]
#[ignore = "isolated process: deliberately invalidates process-wide parsed cache"]
fn older_decode_cannot_replace_newer_published_generation() {
    use crate::management::v2::hot_store as hot;
    let (store, _) = tests::temp_store("late-cache-publication");
    store.with_state(|s| s.next_seq = 70).unwrap();
    let (before, generation) = store.read_state_shared().unwrap();
    let mut newer = (*before).clone(); newer.next_seq = 71;
    let database = store.database_path();
    let external = database.clone();
    *ASK_STATE_CACHE.lock().unwrap() = None;
    hot::READ_SNAPSHOT_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
        let next = hot::commit(&external, "asks", generation, vec![hot::RowChange::Put {
            collection: "state".into(), key: "next_seq".into(), value: serde_json::to_vec(&71u64).unwrap(),
        }]).unwrap();
        // Force the same newer committed publication a local writer installs,
        // while the reader still owns its earlier SQLite read transaction.
        *ASK_STATE_CACHE.lock().unwrap() = Some((external, next, Arc::new(newer), None));
    })));
    assert_eq!(store.project_snapshot(|s| s.next_seq).unwrap(), 70);
    {
        let cache = ASK_STATE_CACHE.lock().unwrap();
        let (path, current, state, _) = cache.as_ref().unwrap();
        assert_eq!(path, &database);
        assert_eq!(*current, generation+1);
        assert_eq!(state.next_seq, 71);
    }
    assert_eq!(store.project_snapshot(|s| s.next_seq).unwrap(), 71);
    store.with_state(|s| s.next_seq += 1).unwrap();
    assert_eq!(store.project_snapshot(|s| s.next_seq).unwrap(), 72);
}
