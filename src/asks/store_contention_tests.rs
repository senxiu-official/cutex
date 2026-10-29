//! Opt-in retained-history contention probe; no live store or network access.
use super::*;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[test]
fn shared_peek_reads_hot_only_store_after_legacy_directory_loss() {
    let root = std::env::temp_dir().join(format!("cutex-hot-only-{}", uuid::Uuid::new_v4().simple()));
    let store = AskStore::at_runtime(&root);
    // A genuinely absent store remains absent, as with peek().
    assert!(store.peek_shared().unwrap().is_none());
    assert!(!store.dir().exists());
    store.with_state(|state| state.next_seq = 27).unwrap();
    // SQLite lives in management-v2, separately from the legacy asks directory.
    std::fs::remove_file(store.dir().join("asks-v1.lock")).unwrap();
    std::fs::remove_dir(store.dir()).unwrap();
    let copies = super::whole_state_copies_on_this_thread();
    assert_eq!(store.peek_shared().unwrap().unwrap().next_seq, 27);
    assert!(store.dir().is_dir());
    assert_eq!(super::whole_state_copies_on_this_thread(), copies);
    std::fs::remove_dir_all(root).unwrap();
}

fn fixture(label: &str, bytes: usize) -> (AskStore, PathBuf) {
    let (store, dir) = tests::temp_store(label);
    let mut state = AskState::default();
    let (mut record, _) = state.create_ask(tests::new_ask("seed", "x"), Utc::now()).unwrap();
    state.asks.clear();
    state.actions.clear();
    state.notices.clear();
    record.body = tests::body(&"x".repeat(bytes / 256));
    for n in 0..256 {
        record.ask_id = format!("fixture-{n}");
        state.asks.insert(record.ask_id.clone(), record.clone());
    }
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(store.state_path(), serde_json::to_vec(&state).unwrap()).unwrap();
    store.shared().unwrap(); // migrate once before measuring
    (store, dir)
}

fn cold_wave(store: &AskStore, readers: usize) -> usize {
    *ASK_STATE_CACHE.lock().unwrap() = None;
    let barrier = Arc::new(Barrier::new(readers));
    let snapshots = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..readers).map(|_| {
            let barrier = barrier.clone();
            scope.spawn(move || { barrier.wait(); store.shared().unwrap() })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    });
    snapshots.iter().map(|s| Arc::as_ptr(s) as usize)
        .collect::<std::collections::BTreeSet<_>>().len()
}

#[test]
#[ignore = "isolated process required: deliberately clears the global ask cache"]
fn concurrent_cold_readers_share_one_decoded_snapshot() {
    let (store, dir) = fixture("single-flight", 2 * 1024 * 1024);
    let decoded = cold_wave(&store, 12);
    let old = store.shared().unwrap();
    store.with_state(|state| state.next_seq += 1).unwrap();
    let new = store.shared().unwrap();
    assert_eq!(new.next_seq, old.next_seq + 1);
    assert!(!Arc::ptr_eq(&old, &new), "a committed generation must replace the cache");
    // The previous Arc remains a complete immutable snapshot across a write.
    assert_eq!(old.asks.len(), new.asks.len());
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(decoded, 1, "same durable generation must be decoded only once");
}

#[cfg(windows)]
fn process_cpu_ms() -> (f64, f64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    unsafe {
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        assert_ne!(GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user), 0);
        let ms = |t: FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 10_000.0;
        (ms(user), ms(kernel))
    }
}
#[cfg(not(windows))]
fn process_cpu_ms() -> (f64, f64) { (0., 0.) }

#[test]
#[ignore = "20MB synthetic retained asks, concurrent readers and writers; run alone with --nocapture"]
fn retained_history_contention_probe() {
    let (store, dir) = fixture("contention", 20 * 1024 * 1024);
    let before = crate::management::v2::control_timing::snapshot();
    let cpu0 = process_cpu_ms();
    let started = Instant::now();
    let mut decodes = 0;
    let mut writes = Vec::new();
    for _ in 0..6 {
        // Simulate an independent process committing a tiny change. Unlike
        // with_state, it cannot publish into this process's AskState cache.
        let generation = store.store_generation().unwrap().unwrap();
        crate::management::v2::hot_store::commit(&store.database_path(), "asks", generation,
            vec![crate::management::v2::hot_store::RowChange::Put {
                collection: "state".into(), key: "revision".into(),
                value: serde_json::to_vec(&(generation + 1)).unwrap(),
            }]).unwrap();
        let barrier = Arc::new(Barrier::new(13));
        let snapshots = std::thread::scope(|scope| {
            let readers: Vec<_> = (0..12).map(|_| {
                let barrier = barrier.clone(); let store = &store;
                scope.spawn(move || { barrier.wait(); store.shared().unwrap() })
            }).collect();
            let writer = scope.spawn(|| {
                barrier.wait(); std::thread::sleep(Duration::from_millis(2));
                let t = Instant::now();
                store.with_state(|s| s.next_seq += 1).unwrap(); t.elapsed().as_secs_f64() * 1000.
            });
            let snapshots = readers.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>();
            writes.push(writer.join().unwrap()); snapshots
        });
        decodes += snapshots.iter().map(|s| Arc::as_ptr(s) as usize)
            .collect::<std::collections::BTreeSet<_>>().len();
    }
    let cpu1 = process_cpu_ms();
    let after = crate::management::v2::control_timing::snapshot();
    let sum = |v: &Value, path: &str| v["paths"][path]["stages"]["lock_wait"]["sumNs"].as_u64().unwrap_or(0);
    println!("ASK_CONTENTION {}", json!({"readers":12,"rounds":6,"retainedTextBytes":20*1024*1024,
        "wallMs":started.elapsed().as_secs_f64()*1000., "userCpuMs":cpu1.0-cpu0.0,"kernelCpuMs":cpu1.1-cpu0.1,
        "distinctReaderSnapshots":decodes,"writerElapsedMs":writes,
        "readerLockWaitMs":sum(&after,"asks.store_read").saturating_sub(sum(&before,"asks.store_read")) as f64/1e6,
        "writerLockWaitMs":sum(&after,"asks.store_write").saturating_sub(sum(&before,"asks.store_write")) as f64/1e6}));
    let copies = super::whole_state_copies_on_this_thread();
    let t = Instant::now();
    for _ in 0..24 { assert!(!store.peek().unwrap().unwrap().leases.contains_key("idle")); }
    let copied_ms = t.elapsed().as_secs_f64() * 1000.;
    let copied = super::whole_state_copies_on_this_thread() - copies;
    let t = Instant::now();
    for _ in 0..24 { assert!(!store.inspect(|s| s.leases.contains_key("idle")).unwrap()); }
    println!("ASK_LOOKUP {}", json!({"iterations":24,"peekMs":copied_ms,
        "inspectMs":t.elapsed().as_secs_f64()*1000.,"peekWholeCopies":copied,
        "inspectWholeCopies":super::whole_state_copies_on_this_thread()-copies-copied}));
    std::fs::remove_dir_all(dir).unwrap();
}
