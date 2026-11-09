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
