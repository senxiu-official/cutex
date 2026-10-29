//! Actual asks OS-lock contention, using the production preflight resolver.
use super::*;
use std::time::{Duration, Instant};
use std::sync::mpsc;
use fs2::FileExt;

fn locked(path: std::path::PathBuf, delay: Option<Duration>) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (ready, entered) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        file.lock_exclusive().unwrap();
        ready.send(()).unwrap();
        if let Some(delay) = delay { std::thread::sleep(delay); }
        else { wait.recv_timeout(Duration::from_secs(20)).unwrap(); }
        // File drop releases the real platform lock.
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    (release, worker)
}

#[test]
fn held_asks_lock_before_after_preflight_and_exact_retry() {
    super::tests::context("preflight-os-lock", |ctx| {
        let before = serde_json::to_value(ctx.store.snapshot().unwrap()).unwrap();
        let copies = crate::asks::store::whole_state_copies_on_this_thread();
        assert!(ctx.store.lookup_snapshot_with_timeout(&[], Duration::ZERO).unwrap().is_some());
        assert_eq!(crate::asks::store::whole_state_copies_on_this_thread(), copies + 1);
        let lock = ctx.store.dir().join("asks-v1.lock");
        let args = json!({"target":"Kimi","text":"private preflight regression","action_id":"prelock-once"});
        // This is the original preflight implementation path, still used by
        // the business handler. It must wait for the held writer lock.
        let (_release, worker) = locked(lock.clone(), Some(Duration::from_millis(1500)));
        let at = Instant::now();
        let old = normalize(ctx, "agent_delegate", args.clone()).unwrap();
        let baseline = at.elapsed();
        worker.join().unwrap();
        assert!(baseline >= Duration::from_millis(1200), "{baseline:?}");
        println!("original blocking resolver under 1500ms OS lock: {baseline:?}");

        for tool in ["agent_delegate", "agent_create"] {
            let expected = normalize(ctx, tool, args.clone()).unwrap();
            let (_release, worker) = locked(lock.clone(), Some(Duration::from_millis(1500)));
            let at = Instant::now();
            let result = handle(ctx, json!({"tool":tool,"arguments":args}).to_string().as_bytes());
            let elapsed = at.elapsed();
            worker.join().unwrap();
            assert_eq!(result["ok"], true, "{result}");
            assert_eq!(result["arguments"], expected, "canonical digest inputs unchanged");
            assert!(elapsed >= Duration::from_millis(1200) && elapsed < Duration::from_secs(4), "{elapsed:?}");
        }

        let (release, worker) = locked(lock, None);
        for tool in ["agent_delegate", "agent_create"] {
            // Cover hot-only and resource-bearing cold-aware lookup paths.
            for resource in [false, true] {
                let mut arguments = args.clone();
                if resource { arguments["follow_up"] = json!("delegation-abcdef"); }
                let at = Instant::now();
                let result = handle(ctx, json!({"tool":tool,"arguments":arguments}).to_string().as_bytes());
                let elapsed = at.elapsed();
                assert_eq!(result["code"], "id_resolution_busy", "{result}");
                assert_eq!(result["status"], "no_write");
                assert_eq!(result["retryable"], true);
                assert_eq!(result["action_id"], "prelock-once");
                assert!(result["detail"].as_str().unwrap().contains("Earlier calls"));
                assert!(elapsed >= PREFLIGHT_LOOKUP_LOCK_TIMEOUT && elapsed < Duration::from_secs(4), "{elapsed:?}");
                println!("{tool} preflight resource={resource} no_write while OS lock held: {elapsed:?}");
            }
        }
        release.send(()).unwrap(); worker.join().unwrap();
        assert_eq!(serde_json::to_value(ctx.store.snapshot().unwrap()).unwrap(), before);
        let resolved = handle(ctx, json!({"tool":"agent_delegate","arguments":args}).to_string().as_bytes());
        assert_eq!(resolved["ok"], true, "{resolved}");
        assert_eq!(resolved["arguments"], old, "canonical identity/digest inputs unchanged");
        let body = json!({"tool":"agent_delegate","action_id":"prelock-once","arguments":resolved["arguments"]});
        let first = super::super::tools::handle(ctx, body.to_string().as_bytes());
        assert_eq!(first["ok"], true, "{first}");
        // A busy retry is no-write for THIS invocation, not evidence that the
        // already committed action has no effects. Keep its original key.
        let committed = serde_json::to_value(ctx.store.snapshot().unwrap()).unwrap();
        let (release, worker) = locked(ctx.store.dir().join("asks-v1.lock"), None);
        let busy_retry = handle(ctx, json!({"tool":"agent_delegate","arguments":resolved["arguments"]}).to_string().as_bytes());
        assert_eq!(busy_retry["status"], "no_write");
        assert_eq!(busy_retry["action_id"], "prelock-once");
        release.send(()).unwrap(); worker.join().unwrap();
        assert_eq!(serde_json::to_value(ctx.store.snapshot().unwrap()).unwrap(), committed);
        let second = super::super::tools::handle(ctx, body.to_string().as_bytes());
        assert_eq!(second["delegation"], first["delegation"]);
        let mut conflict = body;
        conflict["arguments"]["text"] = json!("changed payload");
        assert_eq!(super::super::tools::handle(ctx, conflict.to_string().as_bytes())["code"], "idempotency_conflict");
        // Delegation idempotency lives on requester-scoped AskRecord, not
        // the generic action map used by other tools. Include archived rows.
        let after = ctx.store.history_snapshot().unwrap();
        assert_eq!(after.asks.len(), 1);
        assert_eq!(after.asks.values().filter(|ask|
            ask.requester.session() == Some(ctx.caller) && ask.action_id == "prelock-once"
        ).count(), 1);
    });
}

// This guard owns only the process returned by this test's spawn. In particular,
// failure/unwind never searches process names or accepts an external PID.
struct PreflightLockChild {
    child: std::process::Child,
    reaped: bool,
}

impl PreflightLockChild {
    fn terminate_and_reap(&mut self) {
        assert!(self.child.try_wait().unwrap().is_none(), "lock holder exited before termination");
        self.child.kill().expect("terminate our own lock holder");
        let status = self.child.wait().expect("reap our own lock holder");
        self.reaped = true;
        assert!(!status.success(), "holder must not exit normally");
    }
}

impl Drop for PreflightLockChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
#[ignore = "child-only OS lock fixture; run through preflight_recovers_after_lock_holder_process_exit"]
fn preflight_lock_holder_child() {
    let root = std::path::PathBuf::from(
        std::env::var_os("CUTEX_PREFLIGHT_LOCK_CHILD_ROOT").expect("private parent required"),
    );
    assert!(root.join("private-preflight-process").is_file());
    let file = std::fs::OpenOptions::new().read(true).write(true)
        .open(root.join("asks-v1.lock")).unwrap();
    file.lock_exclusive().unwrap();
    // Ready means the exclusive OS lock is held, not merely that spawn ran.
    std::fs::write(root.join("lock-holder-ready"), b"locked").unwrap();
    // No release command or normal exit. The parent must terminate this process
    // while it owns the lock. Watchdog only bounds a broken parent/test runner.
    std::thread::sleep(Duration::from_secs(30));
    panic!("parent did not terminate its lock holder within the watchdog");
}

#[test]
fn preflight_recovers_after_lock_holder_process_exit() {
    // Require the same explicit marked private HOME used by the integration
    // runner; do not mutate process-global environment while tests may run.
    let private_home = std::path::PathBuf::from(
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("marked private test HOME required"),
    );
    assert!(private_home.join(".cutex-test-private-home").is_file());
    assert_eq!(std::env::var_os("HOME"), Some(private_home.clone().into_os_string()));
    super::tests::context("preflight-process-exit", |ctx| {
        let before = serde_json::to_value(ctx.store.snapshot().unwrap()).unwrap();
        let args = json!({"target":"Kimi","text":"private process-exit regression","action_id":"prelock-process-once"});
        let canonical = normalize(ctx, "agent_delegate", args.clone()).unwrap();
        let root = ctx.store.dir();
        std::fs::write(root.join("private-preflight-process"), b"test-owned fixture").unwrap();
        let ready = root.join("lock-holder-ready");
        assert!(!ready.exists(), "fixture must be fresh");
        let mut holder = PreflightLockChild {
            child: std::process::Command::new(std::env::current_exe().unwrap())
                .args(["asks::short_ids::preflight_tests::preflight_lock_holder_child",
                    "--exact", "--ignored", "--test-threads=1", "--nocapture"])
                .env("CUTEX_PREFLIGHT_LOCK_CHILD_ROOT", root)
                .env("HOME", &private_home)
                .env("CUTEX_TEST_PRIVATE_HOME", &private_home)
                .env("CUTEX_TEST_FORBID_REAL_HOME", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn().expect("spawn our own OS lock holder"),
            reaped: false,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(holder.child.try_wait().unwrap().is_none(), "holder failed before ready");
            assert!(Instant::now() < deadline, "holder readiness timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        for tool in ["agent_delegate", "agent_create"] {
            assert!(holder.child.try_wait().unwrap().is_none());
            let at = Instant::now();
            let result = handle(ctx, json!({"tool":tool,"arguments":args}).to_string().as_bytes());
            assert_eq!(result["status"], "no_write", "{result}");
            assert_eq!(result["code"], "id_resolution_busy", "{result}");
            assert_eq!(result["action_id"], "prelock-process-once");
            assert_eq!(result["retryable"], true);
            assert!(at.elapsed() >= PREFLIGHT_LOOKUP_LOCK_TIMEOUT && at.elapsed() < Duration::from_secs(4), "preflight exceeded bounded wait");
            assert!(holder.child.try_wait().unwrap().is_none(), "refusal must precede child exit");
        }
        holder.terminate_and_reap();
        // Prove OS release using a nonblocking acquisition before any blocking
        // snapshot/business path. No test unlocks the child's file descriptor.
        let recovered = ctx.store.lookup_snapshot_with_timeout(&[], Duration::ZERO).unwrap().expect("exited child left asks lock held");
        assert_eq!(serde_json::to_value(recovered).unwrap(), before);
        let resolved = handle(ctx, json!({"tool":"agent_delegate","arguments":args}).to_string().as_bytes());
        assert_eq!(resolved["ok"], true, "{resolved}");
        assert_eq!(resolved["arguments"], canonical);
        let body = json!({"tool":"agent_delegate","action_id":"prelock-process-once","arguments":resolved["arguments"]});
        let first = super::super::tools::handle(ctx, body.to_string().as_bytes());
        assert_eq!(first["ok"], true, "{first}");
        let replay = super::super::tools::handle(ctx, body.to_string().as_bytes());
        assert_eq!(replay["ok"], true, "{replay}");
        assert_eq!(replay["delegation"], first["delegation"]);
        let mut conflict = body;
        conflict["arguments"]["text"] = json!("changed payload");
        assert_eq!(super::super::tools::handle(ctx, conflict.to_string().as_bytes())["code"], "idempotency_conflict");
        let after = ctx.store.history_snapshot().unwrap();
        assert_eq!(after.asks.len(), 1);
        assert_eq!(after.asks.values().filter(|ask|
            ask.requester.session() == Some(ctx.caller) && ask.action_id == "prelock-process-once"
        ).count(), 1);
    });
}
