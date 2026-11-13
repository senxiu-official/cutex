// Private cross-process lifecycle memory fixture. See progress/report for env.
include!("../tests/stream_capacity_http.rs");

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

struct CountingAllocator;
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);
fn added(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed) + bytes as u64;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() { added(layout.size()); }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() { added(layout.size()); }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = System.realloc(ptr, layout, size);
        if !next.is_null() {
            LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            added(size);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn sample(phase: &str, step: usize, start: Instant, output: &mut std::fs::File) {
    let heap = LIVE_BYTES.load(Ordering::Relaxed);
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name))
        .and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok());
    let value = json!({"phase":phase,"step":step,"elapsedMs":start.elapsed().as_millis(),
        "pid":std::process::id(),"rustLiveBytes":heap,"rustPeakBytes":PEAK_BYTES.load(Ordering::Relaxed),
        "rssKiB":field("VmRSS:"),"threads":field("Threads:"),
        "fds":std::fs::read_dir("/proc/self/fd").ok().map(|entries| entries.count())});
    writeln!(output, "{value}").unwrap();
    output.flush().unwrap();
    println!("MEM_SAMPLE {value}");
}

fn agent_id(index: usize) -> String { format!("cutex.20000000-0000-4000-8000-{index:012}") }

fn retire_or_delete(count: usize) {
    let path = cutex::session::store::cutex_sessions_path().unwrap();
    for index in 0..count {
        let id = agent_id(index);
        if index % 2 == 0 {
            cutex::session::store::update_session_row(&path, &id, |record| {
                let revision = record.durable_revision();
                let generation = record.runtime_generation;
                cutex::session::archive::commit_retire(record, revision, generation,
                    true, "2026-11-11T10:00:00Z".into())?;
                Ok(())
            }).unwrap();
        } else {
            cutex::session::store::with_locked_session_rows(&path, |rows| {
                rows.remove_session(&id);
                rows.commit()?;
                Ok(())
            }).unwrap();
        }
    }
    println!("LIFECYCLE_WRITER_DONE {count}");
}

fn main() {
    let parent = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private HOME required"));
    assert!(parent.join(".cutex-test-private-home").is_file());
    assert_eq!(std::path::PathBuf::from(std::env::var_os("HOME").unwrap()), parent);
    let count = setting("MEM_AGENTS", 200);
    if std::env::args().any(|arg| arg == "--retire-or-delete") {
        retire_or_delete(count);
        return;
    }
    let home = parent.join(format!("session-lifecycle-{}", uuid::Uuid::new_v4()));
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(home.join(".cutex-test-private-home"), "private lifecycle fixture").unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("CUTEX_TEST_PRIVATE_HOME", &home);
    WORKSPACE.set(workspace.display().to_string()).unwrap();
    let mut store = CutexSessionStore::default();
    for index in 0..count {
        let id = agent_id(index);
        let mut record = CutexSessionRecord::new(id.clone(), Some(thread_id(&id)),
            cutex::platform::host::current_host_name(), workspace.display().to_string(), None).unwrap();
        record.runtime_generation = 1;
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        store.sessions.insert(id.clone(), record);
        fixtures().insert(id, Fixture { turns:1, online:false });
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    drop(store);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(![24270,24260,25370].contains(&addr.port()));
    let server = StoppableListener::spawn(listener, false, serve).unwrap();
    cutex::management::v2::sessions_watch::start_watcher().unwrap();
    let started = Instant::now();
    let output = std::env::var_os("MEM_OUTPUT").map(std::path::PathBuf::from).unwrap_or(home.join("samples.jsonl"));
    let mut output = std::fs::File::create(output).unwrap();
    sample("before-validator", 0, started, &mut output);
    // The first semantic event compiles the process-wide JSON schema validator.
    // Warm that fixed startup cost before attributing per-session retention.
    cutex::management::v2::contract_validation::validate_cutex_event_message(&json!({
        "method":"cutex/sessions/changed", "params":{"added":[],"removed":[],"changed":[],
            "projectsChanged":false,"hostId":cutex::platform::host::current_host_name()}
    })).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    sample("start", 0, started, &mut output);
    let children = setting("MEM_CHILDREN", 512);
    for index in 0..count {
        let id = agent_id(index);
        let root = thread_id(&id);
        for child in 0..children {
            let thread = format!("child-{child}-{}", "x".repeat(256));
            assert!(cutex::management::v2::subagent_lineage::record_edge(&id, &root, &thread, &root));
            cutex::management::v2::subagent_lineage::mark_rejected(&id, &root, &format!("foreign-{child}-{}", "y".repeat(256)));
        }
        let (status, _) = get(addr, &format!("/v2/sessions/{id}/history?limit=1&media=ref"));
        assert_eq!(status, 200);
        if index % 20 == 19 { sample("warm", index+1, started, &mut output); }
    }
    sample("before-retire", count, started, &mut output);
    // A separate process commits canonical retire transitions and row deletes;
    // there is no direct cleanup call in the process holding these caches.
    let status = std::process::Command::new(std::env::current_exe().unwrap()).arg("--retire-or-delete").status().unwrap();
    assert!(status.success());
    sample("writer-finished", count, started, &mut output);
    for second in 1..=setting("MEM_SOAK_SECS", 90) {
        std::thread::sleep(Duration::from_secs(1));
        if second % 5 == 0 { sample("after-retire", second, started, &mut output); }
    }
    sample("end", count, started, &mut output);
    server.stop();
}
