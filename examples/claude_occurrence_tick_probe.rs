//! Private synthetic controller tick/allocation probe. No real profiles or tools.
use cutex::{
    agent_bus::{claude_occurrence, store::AgentBusState},
    session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore},
};
use serde_json::{json, Value};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    net::TcpListener,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
thread_local! {static COUNT:Cell<bool>=const{Cell::new(false)};}
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
struct Counter;
fn count(size: usize) {
    let _ = COUNT.try_with(|enabled| {
        if enabled.get() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size, Ordering::Relaxed);
        }
    });
}
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            count(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            count(l.size());
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let p = unsafe { System.realloc(p, l, n) };
        if !p.is_null() {
            count(n);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static ALLOCATOR: Counter = Counter;
fn measure(label: &str, state: &Arc<Mutex<AgentBusState>>, queries: &AtomicUsize) -> Value {
    for _ in 0..10 {
        claude_occurrence::reconcile(state).unwrap();
    }
    let cloned = cutex::session::store::whole_store_copy_count();
    let native = queries.load(Ordering::Relaxed);
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    COUNT.with(|flag| flag.set(true));
    let started = Instant::now();
    for _ in 0..100 {
        claude_occurrence::reconcile(state).unwrap();
    }
    let elapsed = started.elapsed();
    COUNT.with(|flag| flag.set(false));
    let calls = queries.load(Ordering::Relaxed) - native;
    assert_eq!(calls, 0, "steady ticks must not probe native");
    let authority = cutex::agent_management::archive_authority_snapshot_shared().unwrap();
    let shared = Arc::ptr_eq(
        &authority,
        &cutex::agent_management::archive_authority_snapshot_shared().unwrap(),
    );
    assert!(shared);
    json!({"label":label,"ticks":100,"meanTickUs":elapsed.as_secs_f64()*10_000.0,"callingThreadAllocationsPerTick":ALLOCS.load(Ordering::Relaxed) as f64/100.0,
  "callingThreadRequestedBytesPerTick":BYTES.load(Ordering::Relaxed) as f64/100.0,"ownedWholeStoreClones":cutex::session::store::whole_store_copy_count()-cloned,"archiveSnapshotArcReused":shared,"nativeStatusQueriesInMeasuredTicks":calls})
}
fn main() {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("private HOME"));
    assert_eq!(
        Some(home.as_os_str()),
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").as_deref()
    );
    assert!(home.join(".cutex-test-private-home").is_file());
    // Materialize a private roster so the measured authority has a durable cache key.
    cutex::agent_management::AgentManagementStore::open_default().unwrap().import_now().unwrap();
    let root = home.join("tick-probe");
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let queries = Arc::new(AtomicUsize::new(0));
    let server_queries = queries.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            stream.set_nonblocking(false).unwrap();
            let queries = server_queries.clone();
            std::thread::spawn(move || {
                let Ok(mut socket) = tungstenite::accept(stream) else {
                    return;
                };
                while let Ok(message) = socket.read() {
                    let Ok(text) = message.to_text() else {
                        continue;
                    };
                    let Ok(request) = serde_json::from_str::<Value>(text) else {
                        continue;
                    };
                    if request.get("id").is_none() {
                        continue;
                    }
                    let result = match request["method"].as_str().unwrap() {
                        "initialize" => json!({}),
                        "claude/capabilities" => {
                            json!({"implementation":"cute-claude-app-server","runtimeId":"probe-adapter","backend":"claude-subscription-sdk","persistentRuntime":true,"ensureThread":"claude/thread/ensure"})
                        }
                        "claude/runtime/status" => {
                            queries.fetch_add(1, Ordering::Relaxed);
                            let thread = request["params"]["threadId"].as_str().unwrap();
                            json!({"threadId":thread,"connectionId":thread.replace("thread-","probe-"),"ownerTaskAlive":true,"state":"waiting"})
                        }
                        other => panic!("unexpected native method {other}"),
                    };
                    if socket
                        .send(tungstenite::Message::Text(
                            json!({"id":request["id"],"result":result})
                                .to_string()
                                .into(),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    let mut store = CutexSessionStore::default();
    for i in 0..4096 {
        let mut record = CutexSessionRecord::new(
            format!("cutex.probe-{i}"),
            None,
            cutex::platform::host::current_host_name(),
            root.to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        record.runtime_generation = 1;
        record.agent_enabled = true;
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::Persistent;
        record.claude_session=Some(serde_json::from_value(json!({"deployment":{"python":"unused","adapter_source":"unused","claude_cli":"unused","claude_home":"unused","tui_binary":"unused"},"thread_id":format!("thread-{i}"),"state_dir":root.join(i.to_string()),"model":"sonnet","instructions":"","connection_id":format!("probe-{i}"),"allow_write":false,"allow_exec":false})).unwrap());
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    let state = Arc::new(Mutex::new(AgentBusState::default()));
    let offline = measure("4096 offline", &state, &queries);
    let mut store = cutex::session::store::load_cutex_session_store().unwrap();
    for i in 0..32 {
        let record = store.sessions.get_mut(&format!("cutex.probe-{i}")).unwrap();
        record.runtime_pid = Some(std::process::id());
        record.current_runtime_agent_id = Some(format!("claude.probe-{i}"));
        let session = record.claude_session.as_ref().unwrap();
        std::fs::create_dir_all(&session.state_dir).unwrap();
        std::fs::write(session.state_dir.join("server.json"),json!({"pid":std::process::id(),"runtimeId":"probe-adapter","endpoint":format!("ws://127.0.0.1:{port}")}).to_string()).unwrap();
        std::fs::write(session.state_dir.join("rpc-token"), b"private-probe").unwrap();
        std::fs::write(session.state_dir.join("cutex-mcp-binding.json"),json!({"threadId":session.thread_id,"generation":1,"runtimeAgentId":record.current_runtime_agent_id}).to_string()).unwrap();
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    claude_occurrence::readopt_startup(&state).unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    while state.lock().unwrap().agents.len() != 32 {
        assert!(Instant::now() < deadline, "adoption deadline");
        std::thread::sleep(Duration::from_millis(20));
        claude_occurrence::reconcile(&state).unwrap();
    }
    let active = measure(
        "4096 records /32 current Claude occurrences",
        &state,
        &queries,
    );
    let report = json!({"synthetic":true,"rows":4096,"active":32,"measurement":"hot debug-build public controller ticks; calling-thread allocator counts; fixture setup excluded","offline":offline,"activeTicks":active});
    let encoded = serde_json::to_string_pretty(&report).unwrap();
    if let Some(output) = std::env::args_os().nth(1) {
        std::fs::write(output, &encoded).unwrap();
    }
    println!("{encoded}");
}
