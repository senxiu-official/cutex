use super::*;
use std::sync::mpsc;

#[test]
fn stalled_session_waiters_leave_capacity_for_other_sessions() {
    let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(2, 64)));
    let held = pool.acquire_session(Duration::ZERO, false, Some("stuck".into())).unwrap();
    let follower = std::thread::spawn(move || pool.acquire_session(Duration::from_secs(5), false, Some("stuck".into())).unwrap());
    wait(pool, 1);
    assert_eq!(pool.counts(), (1, 1));
    let healthy = pool.acquire_session(Duration::ZERO, false, Some("healthy".into())).unwrap();
    assert_eq!(pool.counts(), (2, 1));
    drop(healthy);
    assert_eq!(pool.acquire_session(Duration::ZERO, false, Some("stuck".into())).err().unwrap().reason, RefusalReason::HeavyReadWaitTimeout);
    drop(held);
    drop(follower.join().unwrap());
    assert_eq!(pool.counts(), (0, 0));
    assert!(pool.state.lock().unwrap().sessions.is_empty());
}

#[test]
fn blocked_background_session_does_not_reserve_the_other_slot() {
    let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(2, 64)));
    let held = pool.acquire_session(Duration::ZERO, false, Some("stuck".into())).unwrap();
    let follower = std::thread::spawn(move || pool.acquire_session(Duration::from_secs(5), true, Some("stuck".into())).unwrap());
    wait(pool, 1);
    let healthy = pool.acquire_session(Duration::ZERO, false, Some("healthy".into())).unwrap();
    drop(healthy);
    drop(held);
    drop(follower.join().unwrap());
    assert_eq!(pool.counts(), (0, 0));
}

#[test]
fn session_read_identity_matches_dispatch_decoding() {
    let request = |path: &str| SimpleHttpRequest { method: "GET".into(), path: path.into(), headers: Default::default(), body: vec![] };
    for path in ["/v2/sessions/cutex.a/history", "/v2/sessions/cutex%2Ea/history/turns/t", "/v2/sessions/cutex.a/bootstrap?turnLimit=10"] {
        assert_eq!(heavy_session(&request(path)).as_deref(), Some("cutex.a"), "{path}");
    }
    for path in ["/v2/sessions", "/v2/sessions/page", "/v2/sessions/cutex%2Fa/history", "/v2/sessions/%zz/history"] {
        assert!(heavy_session(&request(path)).is_none(), "{path}");
    }
}

#[test]
fn stored_media_blobs_and_files_do_not_share_the_native_reader_cap() {
    let pool = Box::leak(Box::new(HeavyPool::new(2, 64)));
    let request = |path: &str| SimpleHttpRequest { method: "GET".into(), path: path.into(), headers: Default::default(), body: vec![] };
    let held = pool.acquire_session(Duration::ZERO, false, heavy_session(&request("/v2/sessions/a/history"))).unwrap();
    let media = format!("/v2/sessions/a/media/{}", "a".repeat(64));
    let blob = format!("/v2/sessions/a/blobs/{}", "b".repeat(64));
    assert!(super::super::media_refs::parse_path(&media).is_some());
    assert!(super::super::event_blobs::parse_path(&blob).is_some());
    for path in [media.as_str(), blob.as_str(), "/v2/sessions/a/fs/read", "/v2/sessions/a/app-server/requests", "/v2/sessions/a/cutex/requests"] {
        assert!(heavy_session(&request(path)).is_none(), "{path}");
        let independent = pool.acquire_session(Duration::ZERO, false, heavy_session(&request(path))).unwrap();
        drop(independent);
    }
    let mut native = request("/v2/sessions/a/app-server/requests");
    native.method = "POST".into();
    assert_eq!(heavy_session(&native).as_deref(), Some("a"));
    assert!(pool.acquire_session(Duration::ZERO, false, heavy_session(&native)).is_err());
    drop(held);
}

fn wait(pool: &HeavyPool, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while pool.counts().1 != count {
        assert!(Instant::now() < deadline, "queue did not settle");
        std::thread::yield_now();
    }
}

#[test]
fn full_background_backlog_keeps_foreground_active_and_queue_capacity() {
    let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(2, 64)));
    let held = pool.acquire_priority(Duration::ZERO, true).unwrap();
    let mut workers = vec![];
    for n in 0..48 {
        workers.push(std::thread::spawn(move || pool.acquire_priority(Duration::from_secs(10), true).unwrap()));
        wait(pool, n + 1);
    }
    assert_eq!(pool.acquire_priority(Duration::ZERO, true).err().unwrap().reason, RefusalReason::HeavyReadQueueFull);
    // No background completion or retry is needed to open the owner's page.
    let foreground = pool.acquire(Duration::ZERO).unwrap();
    assert_eq!(pool.counts(), (2, 48));
    assert_eq!(pool.state.lock().unwrap().active_background, 1);
    drop(foreground);
    drop(held);
    for worker in workers { drop(worker.join().unwrap()); }
    assert_eq!(pool.counts(), (0, 0));
    assert_eq!(pool.timed_out.load(Ordering::Relaxed), 0);
}

#[test]
fn queued_background_gets_share_before_more_foreground_borrows_it() {
    let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(2, 64)));
    let first = pool.acquire(Duration::ZERO).unwrap();
    let second = pool.acquire(Duration::ZERO).unwrap();
    let (sent, received) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let fg_sent = sent.clone();
    let foreground = std::thread::spawn(move || {
        let _permit = pool.acquire(Duration::from_secs(5)).unwrap();
        fg_sent.send("foreground").unwrap();
    });
    wait(pool, 1);
    let background = std::thread::spawn(move || {
        let _permit = pool.acquire_priority(Duration::from_secs(5), true).unwrap();
        sent.send("background").unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    wait(pool, 2);
    drop(first);
    assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), "background");
    assert!(received.try_recv().is_err());
    drop(second);
    assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), "foreground");
    release.send(()).unwrap();
    background.join().unwrap(); foreground.join().unwrap();
    assert_eq!(pool.counts(), (0, 0));
}

#[test]
fn background_is_fifo_and_recovers_after_a_timeout() {
    let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(2, 64)));
    let held = pool.acquire_priority(Duration::ZERO, true).unwrap();
    assert_eq!(pool.acquire_priority(Duration::ZERO, true).err().unwrap().reason, RefusalReason::HeavyReadWaitTimeout);
    let (sent, received) = mpsc::channel();
    let mut workers = vec![];
    for n in 0..8 {
        let sent = sent.clone();
        workers.push(std::thread::spawn(move || {
            let _permit = pool.acquire_priority(Duration::from_secs(5), true).unwrap();
            sent.send(n).unwrap();
        }));
        wait(pool, n + 1);
    }
    wait(pool, 8);
    drop(held);
    for n in 0..8 {assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), n);}
    for worker in workers {worker.join().unwrap();}
    assert_eq!(pool.counts(), (0, 0));
}

#[test]
fn marker_is_additive_and_only_schedules_heavy_reads() {
    let request = |priority: Option<&str>, method: &str, path: &str| {
        let mut headers = Default::default();
        if let Some(value) = priority {headers = [(READ_PRIORITY_HEADER.into(), value.into())].into_iter().collect();}
        SimpleHttpRequest {method:method.into(),path:path.into(),headers,body:vec![]}
    };
    let bg = admit_request(&request(Some("background"), "GET", "/v2/sessions"), None).unwrap();
    assert_eq!(HEAVY.state.lock().unwrap().active_background, 1);
    for value in [None, Some("foreground"), Some("unknown")] {
        let foreground = admit_request(&request(value, "GET", "/v2/sessions"), None).unwrap();
        assert_eq!(HEAVY.counts().0, 2);
        drop(foreground);
    }
    let control = admit_request(&request(Some("background"), "POST", "/v2/human/agents"), None).unwrap();
    assert_eq!(HEAVY.counts(), (1, 0));
    drop((control, bg));
    assert_eq!(HEAVY.counts(), (0, 0));
}

#[test]
fn both_free_slots_are_used_without_waiting_for_another_completion() {
    let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(2, 64)));
    let first = pool.acquire(Duration::ZERO).unwrap(); let second = pool.acquire(Duration::ZERO).unwrap();
    let (sent, received) = mpsc::channel(); let (release, released) = mpsc::channel();
    let released = std::sync::Arc::new(Mutex::new(released));
    let mut workers = vec![];
    for n in 0..2 {
        let (sent, released) = (sent.clone(), released.clone());
        workers.push(std::thread::spawn(move || {
            let _permit = pool.acquire(Duration::from_secs(5)).unwrap();
            sent.send(n).unwrap(); released.lock().unwrap().recv().unwrap();
        })); wait(pool, n + 1);
    }
    drop((first, second));
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(pool.counts().0, 2);
    release.send(()).unwrap(); release.send(()).unwrap();
    for worker in workers {worker.join().unwrap();}
    assert_eq!(pool.counts(), (0, 0));
}
