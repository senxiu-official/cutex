use super::*;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

#[test]
#[ignore = "scale fixture: explicit run only"]
fn git_status_churn_scale_delivers_with_ignored_and_tracked_writers() {
    let dir = std::env::temp_dir().join(format!("status-scale-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    let _cleanup = Cleanup(dir.clone());
    for args in [&["init", "-q", "-b", "main"][..], &["config", "user.name", "Fixture"], &["config", "user.email", "fixture@example.invalid"], &["config", "core.autocrlf", "false"]] {
        assert!(run_git(&dir, args, None).unwrap().status.success());
    }
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir(dir.join("target")).unwrap();
    let tracked = 12000; let untracked = 12000;
    for i in 0..tracked + untracked {
        let bucket = dir.join(format!("files/{:03}", i / 200));
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::write(bucket.join(format!("file-{i:06}.txt")), "fixture\n").unwrap();
    }
    // Stage half the directories; the remaining half exercise --untracked-files=all.
    let mut add = vec!["add".to_owned(), ".gitignore".into()];
    add.extend((0..tracked / 200).map(|i| format!("files/{i:03}")));
    assert!(run_git(&dir, &add.iter().map(String::as_str).collect::<Vec<_>>(), None).unwrap().status.success());
    assert!(run_git(&dir, &["commit", "-q", "-m", "fixture"], None).unwrap().status.success());
    let root = super::super::workspace::ResolvedRoot::from_dir(super::super::workspace::Scope { kind: super::super::workspace::ScopeKind::Session, id: "scale".into() }, &dir).unwrap();
    let warm = Instant::now(); let first = status_uncached(&root).unwrap();
    println!("scale fixture tracked={tracked} untracked={untracked} files={} cold_scan_ms={}", first["files"].as_array().unwrap().len(), warm.elapsed().as_millis());
    let receiver = super::super::workspace_fs::subscribe_watch(&root.canonical).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let paths = [dir.join("target/build.out"), dir.join("files/000/file-000000.txt")];
    let writers = paths.into_iter().map(|file| {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::Acquire) {
                std::fs::write(&file, format!("write {n}\n")).unwrap(); n += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    }).collect::<Vec<_>>();
    struct WriterGuard(Arc<AtomicBool>, Vec<std::thread::JoinHandle<()>>);
    impl Drop for WriterGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
            for writer in self.1.drain(..) { let _ = writer.join(); }
        }
    }
    let writers = WriterGuard(stop, writers);
    let mut delivered = 0; let mut busy = 0; let mut stale = 0; let mut latency = Vec::new();
    for _ in 0..4 {
        let started = Instant::now();
        match status_resource(&root) {
            Ok(value) => { delivered += 1; if value["possiblyStale"] == true { stale += 1; } }
            Err(error) => { assert_eq!(error.code, "git_status_busy"); busy += 1; }
        }
        latency.push(started.elapsed().as_millis());
    }
    drop(writers);
    println!("scale simultaneous ignored+tracked 200ms writers: delivered={delivered} busy={busy} stale={stale} requests=4 latency_ms={latency:?}");
    assert_eq!(delivered, 4);
    assert_eq!(busy, 0);
    // Fast hosts can finish all four scans before the debounced watcher
    // publishes a change. Prove the stale contract separately: keep this
    // flight open after its real Git scan, write a distinct tracked file,
    // and wait for that exact path in the real watcher batch. The watcher
    // invalidates status before publishing the batch, so finish must see it.
    let tracked_path = "files/000/file-000001.txt";
    status_capacity::invalidate(&root.canonical);
    let mut scanned = false;
    let overlapped = status_capacity::get(&root.canonical, || {
        let snapshot = status_uncached(&root)?;
        scanned = true;
        std::fs::write(dir.join(tracked_path), "write during reserved scan\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())
                .expect("tracked write was not observed before the watcher deadline");
            let batch = receiver.recv_timeout(remaining)
                .expect("watcher did not publish the tracked write");
            if batch.paths.iter().any(|path| path == tracked_path) { break; }
        }
        Ok(snapshot)
    }).unwrap();
    assert!(scanned, "the deterministic overlap must run a new Git scan");
    assert_eq!(overlapped["possiblyStale"], true,
        "a tracked write observed before flight completion must mark the snapshot stale");
    println!("scale deterministic tracked-write overlap: watcher path observed, possiblyStale=true");
    drop(receiver);
}
