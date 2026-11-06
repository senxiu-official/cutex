use super::*;
use std::sync::mpsc;

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
