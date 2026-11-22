use super::*;
use std::sync::{Barrier, atomic::AtomicUsize};

fn key(root: &str, owner: bool) -> Key { Key::new(Path::new(root), owner) }
fn eventually(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !condition() { assert!(Instant::now() < until, "workers did not reach the expected state"); std::thread::sleep(Duration::from_millis(2)); }
}
#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);
impl Gate {
    fn wait(&self) {
        let guard = self.0.lock().unwrap();
        let (_guard, timed) = self.1.wait_timeout_while(guard, Duration::from_secs(5), |v| !*v).unwrap();
        assert!(!timed.timed_out(), "test gate timed out");
    }
    fn release(&self) { *self.0.lock().unwrap() = true; self.1.notify_all(); }
}

#[test]
fn git_status_same_root_burst_shares_one_scan_and_one_result() {
    let coordinator = Arc::new(Coordinator::new());
    let start = Arc::new(Barrier::new(9));
    let gate = Arc::new(Gate::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..8).map(|_| {
        let (c, start, gate, calls) = (coordinator.clone(), start.clone(), gate.clone(), calls.clone());
        std::thread::spawn(move || { start.wait(); c.get(key("same-root", true), || {
            calls.fetch_add(1, Ordering::SeqCst); gate.wait(); Ok(serde_json::json!({"files":["same"]}))
        }).unwrap() })
    }).collect();
    start.wait();
    eventually(|| coordinator.coalesced.load(Ordering::Relaxed) == 7);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    gate.release();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(results.iter().all(|v| Arc::ptr_eq(v, &results[0])));
    let again = coordinator.get(key("same-root", true), || panic!("short reuse must not rescan")).unwrap();
    assert!(Arc::ptr_eq(&again, &results[0]));
    assert_eq!(coordinator.diagnostics()["scansActive"], 0);
}

#[test]
fn git_status_distinct_roots_are_bounded_and_do_not_borrow_history_capacity() {
    use crate::management::v2::control_capacity::{admit, WorkClass};
    let coordinator = Arc::new(Coordinator::new());
    let gate = Arc::new(Gate::default());
    let history: Vec<_> = (0..2).map(|_| admit(WorkClass::HeavyRead).unwrap()).collect();
    let leaders: Vec<_> = (0..SCANS).map(|i| {
        let (c, gate) = (coordinator.clone(), gate.clone());
        std::thread::spawn(move || c.get(key(&format!("root-{i}"), true), || { gate.wait(); Ok(serde_json::json!({"ok":true})) }))
    }).collect();
    eventually(|| coordinator.diagnostics()["scansActive"] == SCANS);
    drop(history);
    // Conversation reads and control remain admitted while Git is saturated.
    let history: Vec<_> = (0..2).map(|_| admit(WorkClass::HeavyRead).unwrap()).collect();
    let control = admit(WorkClass::Control).unwrap();
    let followers: Vec<_> = (0..QUEUED).map(|i| {
        let c = coordinator.clone();
        std::thread::spawn(move || c.get(key(&format!("queued-{i}"), true), || Ok(serde_json::json!({"ok":true}))))
    }).collect();
    eventually(|| coordinator.diagnostics()["scansWaiting"] == QUEUED);
    let err = coordinator.get(key("over-capacity", true), || panic!("busy must not scan")).unwrap_err();
    assert_eq!((err.status, err.code), (503, "git_status_busy"));
    gate.release();
    for worker in leaders.into_iter().chain(followers) { worker.join().unwrap().unwrap(); }
    drop((history, control));
    assert_eq!(coordinator.diagnostics()["scansActive"], 0);
    assert_eq!(coordinator.diagnostics()["scansWaiting"], 0);
}

#[test]
fn git_status_cache_is_separate_for_owner_local_and_remote_and_invalidates_on_change() {
    let coordinator = Coordinator::new();
    let owner = coordinator.get(key("root", true), || Ok(serde_json::json!({"lineCount":42}))).unwrap();
    let remote = coordinator.get(key("root", false), || Ok(serde_json::json!({"lineCount":null}))).unwrap();
    assert_eq!(owner["lineCount"], 42); assert!(remote["lineCount"].is_null());
    let mut narrowed=key("root",false); narrowed.narrowed=true;
    let narrowed=coordinator.get(narrowed,||Ok(serde_json::json!({"credentialLines":null}))).unwrap();
    assert!(narrowed["credentialLines"].is_null());
    assert!(narrowed.get("lineCount").is_none(),"a narrowed request must run its own filtered scan");
    coordinator.invalidate(Path::new("root"));
    let next = coordinator.get(key("root", true), || Ok(serde_json::json!({"lineCount":43}))).unwrap();
    assert_eq!(next["lineCount"], 43);
}

#[test]
fn git_status_failed_leader_releases_waiters_and_scan_capacity() {
    let coordinator = Arc::new(Coordinator::new());
    let c = coordinator.clone();
    assert!(std::thread::spawn(move || c.get(key("panic-root", true), || panic!("synthetic scan failure"))).join().is_err());
    let result = coordinator.get(key("panic-root", true), || Ok(serde_json::json!({"recovered":true}))).unwrap();
    assert_eq!(result["recovered"], true);
    assert_eq!(coordinator.diagnostics()["scansActive"], 0);
    assert_eq!(coordinator.diagnostics()["scansWaiting"], 0);
}

#[test]
fn git_status_dirty_scan_returns_to_all_waiters_then_coalesces_successor() {
    let coordinator = Arc::new(Coordinator::new());
    let gate = Arc::new(Gate::default());
    let c = coordinator.clone(); let g = gate.clone();
    let owner = std::thread::spawn(move || c.get(key("changing-root", true), || {
        g.wait(); Ok(serde_json::json!({"files":["snapshot"]}))
    }).unwrap());
    eventually(|| coordinator.diagnostics()["scansActive"] == 1);
    let waiters = (0..4).map(|_| {
        let c = coordinator.clone();
        std::thread::spawn(move || c.get(key("changing-root", true), || panic!("one flight")).unwrap())
    }).collect::<Vec<_>>();
    eventually(|| coordinator.coalesced.load(Ordering::Relaxed) == 4);
    coordinator.invalidate(Path::new("changing-root"));
    gate.release();
    let first = owner.join().unwrap();
    assert_eq!(first["possiblyStale"], true);
    for waiter in waiters { assert!(Arc::ptr_eq(&first, &waiter.join().unwrap())); }
    assert_eq!(coordinator.scans.load(Ordering::Relaxed), 1);
    let second = coordinator.get(key("changing-root", true), || Ok(serde_json::json!({"fresh":true}))).unwrap();
    assert_eq!(second["fresh"], true);
    assert!(second.get("possiblyStale").is_none());
    assert_eq!(coordinator.scans.load(Ordering::Relaxed), 2);
    assert!(Arc::ptr_eq(&second, &coordinator.get(key("changing-root", true), || panic!("reuse successor")).unwrap()));
}

#[test]
fn git_status_scan_owner_and_joiner_survive_short_admission_budget() {
    let coordinator = Arc::new(Coordinator::new());
    let c = coordinator.clone();
    let owner = std::thread::spawn(move || c.get(key("slow-root", true), || {
        std::thread::sleep(WAIT + Duration::from_millis(250));
        Ok(serde_json::json!({"snapshot":true}))
    }).unwrap());
    eventually(|| coordinator.diagnostics()["scansActive"] == 1);
    coordinator.invalidate(Path::new("slow-root"));
    let follower = coordinator.get(key("slow-root", true), || panic!("must share running scan")).unwrap();
    let owner = owner.join().unwrap();
    assert!(Arc::ptr_eq(&owner, &follower));
    assert_eq!(follower["snapshot"], true);
    assert_eq!(coordinator.diagnostics()["joinersWaiting"], 0);
    assert_eq!(coordinator.diagnostics()["scansActive"], 0);
}

#[test]
fn git_status_joiners_share_existing_waiter_limit_and_release_it() {
    let coordinator = Arc::new(Coordinator::new());
    let gate = Arc::new(Gate::default());
    let c = coordinator.clone(); let g = gate.clone();
    let owner = std::thread::spawn(move || c.get(key("bounded-joiners", true), || {
        g.wait(); Ok(serde_json::json!({"shared":true}))
    }).unwrap());
    eventually(|| coordinator.diagnostics()["scansActive"] == 1);
    let joiners = (0..QUEUED).map(|_| {
        let c = coordinator.clone();
        std::thread::spawn(move || c.get(key("bounded-joiners", true), || panic!("one scan")).unwrap())
    }).collect::<Vec<_>>();
    eventually(|| coordinator.diagnostics()["joinersWaiting"] == QUEUED);
    let started = Instant::now();
    assert_eq!(coordinator.get(key("bounded-joiners", true), || panic!("full")).unwrap_err().code, "git_status_busy");
    assert!(started.elapsed() < Duration::from_millis(500));
    gate.release();
    let result = owner.join().unwrap();
    for joiner in joiners { assert!(Arc::ptr_eq(&result, &joiner.join().unwrap())); }
    assert_eq!(coordinator.diagnostics()["joinersWaiting"], 0);
}
