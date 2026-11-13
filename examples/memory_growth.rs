// Reuse the production HTTP/listener fixture and synthetic native callbacks.
// Run this example with a marked private HOME, on Linux:
// cargo run --locked --example memory_growth
// MEM_ROOTS=96 MEM_FILES=6000 MEM_SOAK_SECS=300 MEM_OUTPUT=/own/path.jsonl
// No runtimes are launched. Git indices contain synthetic tracked paths.
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
fn git_index(root: &std::path::Path, files: usize) {
    use std::process::{Command, Stdio};
    assert!(Command::new("git").args(["init", "-q"]).arg(root).status().unwrap().success());
    let mut hash = Command::new("git").args(["-C"]).arg(root)
        .args(["hash-object", "-w", "--stdin"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
    assert!(hash.wait().unwrap().success());
    let mut child = Command::new("git").arg("-C").arg(root).args(["update-index", "--index-info"])
        .stdin(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for index in 0..files {
        writeln!(input, "100644 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\tfile-{index:06}-{}.txt", "x".repeat(180)).unwrap();
    }
    drop(input);
    assert!(child.wait().unwrap().success());
}
fn event(session: &str, index: usize) -> cutex::management::v2::model::PendingEvent {
    use cutex::management::v2::model::*;
    PendingEvent {
        cutex_session_id: session.into(), host_id:cutex::platform::host::current_host_name(), source:EventSource::AppServer,
        schema:Some(AppServerSchema { protocol:"test-native".into(),major_version:1,version:"1".into(),
            sha256:"test".into(),channel:AppServerSchemaChannel::Stable,capabilities:json!({}),extensions:vec![] }),
        correlation:EventCorrelation { runtime_generation:Some(1),thread_id:Some(thread_id(session)),
            turn_id:Some(format!("turn-{index}")),item_id:Some(format!("item-{index}")),..Default::default() },
        native:Some(NativeMessage { kind:NativeMessageKind::Notification,
            message:json!({"method":"item/completed","params":{"threadId":thread_id(session),"turnId":format!("turn-{index}"),
                "item":{"id":format!("item-{index}"),"type":"agentMessage","text":"x".repeat(setting("MEM_EVENT_KIB", 2) * 1024),"phase":"final_answer"}}}) }),cutex:None,
    }
}

fn main() {
    let parent = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private home required"));
    assert!(parent.join(".cutex-test-private-home").is_file());
    assert_eq!(std::path::PathBuf::from(std::env::var_os("HOME").unwrap()), parent);
    let home = parent.join(format!("memory-growth-{}", uuid::Uuid::new_v4()));
    let workspace = home.join("workspaces");
    std::fs::create_dir_all(&workspace).unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(home.join(".cutex-test-private-home"), "private memory fixture").unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("CUTEX_TEST_PRIVATE_HOME", &home);
    WORKSPACE.set(workspace.display().to_string()).unwrap();
    let roots = setting("MEM_ROOTS", 96);
    let files = setting("MEM_FILES", 6000);
    let mut store = CutexSessionStore::default();
    for index in 0..roots {
        let dir = workspace.join(format!("root-{index}"));
        std::fs::create_dir_all(&dir).unwrap();
        git_index(&dir, files);
        let id = format!("cutex.10000000-0000-4000-8000-{index:012}");
        let mut record = CutexSessionRecord::new(id.clone(), Some(thread_id(&id)),
            cutex::platform::host::current_host_name(), dir.display().to_string(), None).unwrap();
        record.runtime_generation = 1;
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        store.sessions.insert(id.clone(), record);
        fixtures().insert(id, Fixture { turns:0, online:true });
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    drop(store);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(![24270,24260,25370].contains(&addr.port()));
    let server = StoppableListener::spawn(listener, false, serve).unwrap();
    let start = Instant::now();
    let output_path = std::env::var_os("MEM_OUTPUT").map(std::path::PathBuf::from).unwrap_or(home.join("samples.jsonl"));
    let mut output = std::fs::File::create(&output_path).unwrap();
    println!("MEM_CONFIG {}", json!({"roots":roots,"filesPerRoot":files,"home":home,"address":addr.to_string(),"output":output_path}));
    sample("start", 0, start, &mut output);
    for index in 0..roots {
        let id = format!("cutex.10000000-0000-4000-8000-{index:012}");
        let (status, _) = get(addr, &format!("/v2/sessions/{id}/fs/search?q=needle&limit=1"));
        assert_eq!(status,200);
        sample("root-search", index+1, start, &mut output);
    }
    let repository = cutex::management::v2::repository::management_v2_repository().unwrap();
    // Repeated disconnected/slow subscribers plus durable events across agents.
    for cycle in 0..setting("MEM_CYCLES", 5) {
        let mut clients = Vec::new();
        for _ in 0..16 {
            let (client, head) = connect(addr, "/v2/events/stream?limit=1");
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            clients.push(client);
        }
        let count = setting("MEM_EVENTS", 1200);
        for index in 0..count {
            let id = format!("cutex.10000000-0000-4000-8000-{:012}", index % roots);
            repository.append(event(&id, cycle * count + index)).unwrap();
        }
        sample("subscribers-filled", cycle, start, &mut output);
        for client in clients { let _ = client.shutdown(Shutdown::Both); }
        std::thread::sleep(Duration::from_secs(1));
        repository.has_live_subscribers();
        sample("subscribers-closed", cycle, start, &mut output);
    }
    for second in 0..setting("MEM_SOAK_SECS", 300) {
        std::thread::sleep(Duration::from_secs(1));
        if second % 10 == 9 { sample("idle", second+1, start, &mut output); }
    }
    sample("end", 0, start, &mut output);
    server.stop();
    // Keep synthetic inputs/samples for reproduction; no live user files used.
}
