// Isolated HTTP stream admission on an ephemeral loopback listener.
// Synthetic runtime callbacks; real workspace watches, half-close and capacity limits.
use cutex::catalog::{CatalogEndpoint, CatalogError};
use cutex::management::server::{ManagementNativeForwardError, ManagementRequestContext};
use cutex::session::model::{CutexSessionRecord, CutexSessionStore};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};

const CODEX: &str = "cutex.10000000-0000-4000-8000-000000000001";
static WORKSPACE: OnceLock<String> = OnceLock::new();
static SIDE_WAITERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SIDE_RELEASE: (Mutex<bool>, std::sync::Condvar) = (Mutex::new(false), std::sync::Condvar::new());
fn thread_id(id: &str) -> String {
    id.trim_start_matches("cutex.").to_string()
}
#[derive(Clone)]
struct Fixture {
    turns: usize,
    online: bool,
}
static FIXTURES: OnceLock<Mutex<HashMap<String, Fixture>>> = OnceLock::new();
fn fixtures() -> std::sync::MutexGuard<'static, HashMap<String, Fixture>> {
    FIXTURES.get_or_init(Default::default).lock().unwrap()
}

fn native(id: &str, method: &str, params: Value) -> Value {
    let count = fixtures()[id].turns;
    let turn = |n: usize| json!({"id":format!("turn-{n}"), "status":"completed", "items":[], "itemsView":"notLoaded"});
    match method {
        "thread/read" => {
            json!({"thread":{"id":thread_id(id), "historyMode":"paginated", "turns":[]}})
        }
        "thread/turns/list" => {
            let offset = params["cursor"]
                .as_str()
                .map(|c| c.parse::<usize>().unwrap())
                .unwrap_or(0);
            let limit = (params["limit"].as_u64().unwrap() as usize).min(100);
            let data: Vec<_> = (0..count)
                .rev()
                .skip(offset)
                .take(limit)
                .map(|n| {
                    let mut value = turn(n);
                    if params["itemsView"] == "full" {
                        value["itemsView"] = json!("full");
                    }
                    value
                })
                .collect();
            json!({"nextCursor":(offset + data.len() < count).then(|| (offset + data.len()).to_string()), "data":data})
        }
        "thread/items/list" => {
            json!({"data":[{"turnId":params["turnId"], "item":{"id":"item-1", "type":"commandExecution", "aggregatedOutput":"fixture-output".repeat(400)}}], "nextCursor":null})
        }
        other => panic!("unexpected native method {other}"),
    }
}

struct Offline(String);
impl CatalogEndpoint for Offline {
    fn request(&mut self, method: &str, params: Value) -> Result<Value, CatalogError> {
        Ok(native(&self.0, method, params))
    }
    fn notify(&mut self, _: &str, _: Option<Value>) -> Result<(), CatalogError> {
        panic!("history must be read only")
    }
}

fn context() -> ManagementRequestContext {
    ManagementRequestContext {
        explicit_launch_action: |_, _| unreachable!(),
        adopt_saved_native: |_, _| unreachable!(),
        review_agent_archive: |_, _| unreachable!(),
        execute_agent_archive: |_, _| unreachable!(),
        owner_archive_action: |_, _| unreachable!(),
        durable_agent_candidates: |_| unreachable!(),
        import_durable_agent: |_, _| unreachable!(),
        load_registry: || Ok(Default::default()),
        load_runtime_status: |id| {
            Ok(fixtures().get(id).filter(|f| f.online).map(|_| {
                cutex::app_server::manager::AppServerManagedRuntimeStatus {
                    cutex_session_id: id.into(),
                    thread_id: thread_id(id),
                    runtime_generation: 1,
                    connected: true,
                    active_turn_id: None,
                    active_turn_observed_at: None,
                    thread_status: Some(json!({"type":"idle"})),
                    thread_status_observed_at: None,
                    thread_settings: None,
                    thread_settings_source: None,
                    thread_settings_complete: false,
                    thread_settings_observed_at: None,
                    runtime_workspace_roots: None,
                    instruction_sources: None,
                    resume_snapshot_observed_at: None,
                    event_method_counts: Default::default(),
                    initialized_user_agent: None,
                    last_event_at: None,
                    last_error: None,
                }
            }))
        },
        forward_native_request: |id, request| {
            if !fixtures()[id].online {
                return Err(ManagementNativeForwardError::BeforeForward(
                    "fixture offline".into(),
                ));
            }
            Ok(
                json!({"id":request["id"], "result":native(id, request["method"].as_str().unwrap(), request["params"].clone())}),
            )
        },
        respond_native_server_request: |_, _, _| unreachable!(),
        handle_user_input: |_| unreachable!(),
        flush_user_input_queue: |_, _, _| unreachable!(),
        steer_user_input_queue: |_, _, _| unreachable!(),
        interrupt_active_turn: |_, _| unreachable!(),
        load_bootstrap_state: |_, _| {
            Ok(json!({"userInputQueue":[], "pendingServerRequests":[], "agentBusMessages":[]}))
        },
        load_latest_plan: |_| Value::Null,
        load_latest_token_usage: |_| Value::Null,
        revert_session_history: |id, turn, _| {
            let before: usize = turn.strip_prefix("turn-").unwrap().parse().unwrap();
            let mut fixtures = fixtures();
            let fixture = fixtures.get_mut(id).unwrap();
            let removed: Vec<_> = (before..fixture.turns)
                .map(|n| format!("turn-{n}"))
                .collect();
            fixture.turns = before;
            Ok(
                json!({"threadId":thread_id(id), "removedTurnIds":removed, "removedTurns":removed.len(),
                "revertedBeforeTurnId":turn, "actionId":"synthetic-revert", "warnings":["synthetic fixture receipt"]}),
            )
        },
        delete_session: |_, _| unreachable!(),
        fork_session: |_, _, _| unreachable!(),
        side_chat: |_, action, params| {
            assert_eq!(action, "get");
            assert_eq!(params["waitMs"], 25000);
            assert_eq!(params["afterRevision"], 1);
            let release = SIDE_RELEASE.0.lock().unwrap();
            SIDE_WAITERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (release, timeout) = SIDE_RELEASE.1.wait_timeout_while(
                release, Duration::from_secs(25), |released| !*released).unwrap();
            assert!(*release && !timeout.timed_out(), "private side-chat waiter was not released");
            Ok(json!({"chat":{"revision":2}}))
        },
        open_offline_history: |id| {
            Ok((
                Box::new(Offline(id.into())),
                cutex::config::paths::config_dir()?,
            ))
        },
        mutate_session: |_, _, _| unreachable!(),
        online_session: |_, _, _| unreachable!(),
        bind_project_authority: |_| unreachable!(),
        import_legacy_director_ownership: |_| unreachable!(),
        reconcile_agent_reservation: |_| unreachable!(),
        abandon_agent_create: |_| unreachable!(),
        list_management_projects: |_| unreachable!(),
        read_management_project: |_, id| {
            Ok(serde_json::from_value(json!({
            "project_id": id, "authority_epoch": 1, "access_role": "human_management",
            "operator_grant_revision": 1, "agent_operators": [], "active_agents": [], "retired_agents": [],
            "presentation": {"display_name": "stream fixture", "badge_label": "test", "color": "blue", "revision": 1, "stored": false},
            "workspace_root": WORKSPACE.get().unwrap()
        })).unwrap())
        },
        update_management_project_presentation: |_, _| unreachable!(),
        execute_management_operator_action: |_, _| unreachable!(),
        execute_management_project_mutation: |_, _| unreachable!(),
        create_human_agent: |_, _| unreachable!(),
        create_human_session: |_, _| unreachable!(),
    }
}

use cutex::management::listeners::{ConnectionInfo, ConnectionSlot, StoppableListener};
use cutex::management::v2::control_capacity::{snapshot, STREAM_LIMIT};
use std::net::{Shutdown, SocketAddr};
use std::time::{Duration, Instant};

fn serve(mut stream: TcpStream, info: ConnectionInfo, mut slot: ConnectionSlot) {
    let request = cutex::http::server::read_simple_http_request(&mut stream).unwrap();
    cutex::management::server::handle_admitted_management_request(
        &mut stream,
        request,
        Some("fixture-root"),
        Some("fixture-root"),
        context(),
        Some(info.peer),
        Some(&mut slot),
    )
    .unwrap();
}

fn connect(addr: SocketAddr, path: &str) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: fixture\r\nAuthorization: Bearer fixture-root\r\nConnection: close\r\n\r\n").unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    (stream, String::from_utf8(head).unwrap())
}
fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let (mut stream, head) = connect(addr, path);
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).unwrap();
    (status, serde_json::from_str(&body).unwrap())
}
fn open_watch(addr: SocketAddr, path: &str) -> TcpStream {
    let (mut stream, head) = connect(addr, path);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("text/event-stream"));
    let mut ready = Vec::new();
    while !ready.ends_with(b"\n\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        ready.push(byte[0]);
    }
    assert!(String::from_utf8(ready).unwrap().contains("event: ready"));
    stream
}
fn await_streams(count: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while snapshot()["streamsActive"] != count {
        assert!(
            Instant::now() < deadline,
            "permit not returned promptly: {}",
            snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(snapshot()["heavyReadsActive"], 0);
    assert_eq!(snapshot()["heavyReadsWaiting"], 0);
}

#[test]
fn watches_do_not_starve_reads_and_stream_permits_return_on_disconnect() {
    let started = Instant::now();
    let parent = std::path::PathBuf::from(
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private home required"),
    );
    assert!(parent.join(".cutex-test-private-home").is_file());
    let home = parent.join("stream-http");
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        home.join(".cutex-test-private-home"),
        "isolated stream HTTP regression",
    )
    .unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("CUTEX_TEST_PRIVATE_HOME", &home);
    WORKSPACE.set(workspace.display().to_string()).unwrap();
    let mut store = CutexSessionStore::default();
    let mut record = CutexSessionRecord::new(
        CODEX.into(),
        Some(thread_id(CODEX)),
        cutex::platform::host::current_host_name(),
        workspace.display().to_string(),
        None,
    )
    .unwrap();
    record.runtime_generation = 1;
    record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
    store.sessions.insert(CODEX.into(), record);
    fixtures().insert(
        CODEX.into(),
        Fixture {
            turns: 0,
            online: true,
        },
    );
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(![24270, 24260].contains(&addr.port()));
    let server = StoppableListener::spawn(listener, false, serve).unwrap();
    let session_watch = format!("/v2/sessions/{CODEX}/fs/watch");
    let project_watch = "/v2/projects/stream-project/fs/watch";
    let bootstrap = format!("/v2/sessions/{CODEX}/bootstrap");
    let side_wait = format!("/v2/sessions/{CODEX}/side-chats/side-fixture?afterRevision=1&waitMs=25000");
    // Real HTTP admission and route dispatch, with a controlled 25s-capable
    // backend wait. Release both only after the unrelated list GET completes.
    std::thread::scope(|scope| {
        let first = scope.spawn(|| get(addr, &side_wait));
        let second = scope.spawn(|| get(addr, &side_wait));
        let deadline = Instant::now() + Duration::from_secs(5);
        while SIDE_WAITERS.load(std::sync::atomic::Ordering::SeqCst) != 2 {
            assert!(Instant::now() < deadline, "two wait routes did not enter their handlers");
            std::thread::sleep(Duration::from_millis(5));
        }
        let during = snapshot();
        let started = Instant::now();
        let list = get(addr, "/v2/sessions");
        let latency = started.elapsed();
        *SIDE_RELEASE.0.lock().unwrap() = true;
        SIDE_RELEASE.1.notify_all();
        assert_eq!(first.join().unwrap().0, 200);
        assert_eq!(second.join().unwrap().0, 200);
        assert_eq!(during["streamsActive"], 2);
        assert_eq!(during["heavyReadsActive"], 0);
        assert_eq!(list.0, 200, "{}", list.1);
        println!("two waitMs=25000 side chats + sessions GET: {latency:?}");
    });
    await_streams(0);
    // The original starvation trigger: TWO session watches, not one of each.
    let mut streams = vec![
        open_watch(addr, &session_watch),
        open_watch(addr, &session_watch),
    ];
    await_streams(2);
    let reads_started = Instant::now();
    std::thread::scope(|scope| {
        let sessions = scope.spawn(|| get(addr, "/v2/sessions"));
        let boot = scope.spawn(|| get(addr, &bootstrap));
        let (status, body) = sessions.join().unwrap();
        assert_eq!(status, 200, "{body}");
        let (status, body) = boot.join().unwrap();
        assert_eq!(status, 200, "{body}");
    });
    println!(
        "two session watches + concurrent sessions/bootstrap: {:?}",
        reads_started.elapsed()
    );
    await_streams(2);
    // Project watch must count too; error returns must not leak a permit.
    streams.push(open_watch(addr, project_watch));
    await_streams(3);
    // HTTP send-half-close is legal: keep receiving new SSE events, not just
    // bytes buffered before FIN. Wait beyond the disconnect polling interval.
    let half_closed = streams.last_mut().unwrap();
    half_closed.shutdown(Shutdown::Write).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(snapshot()["streamsActive"], 3);
    std::fs::write(workspace.join("half-close.txt"), "new event after FIN").unwrap();
    let mut frames = Vec::new();
    loop {
        let mut byte = [0];
        half_closed.read_exact(&mut byte).unwrap();
        frames.push(byte[0]);
        assert!(frames.len() < 64 * 1024);
        if frames.ends_with(b"\n\n") && String::from_utf8_lossy(&frames).contains("half-close.txt") {
            break;
        }
    }
    assert!(String::from_utf8_lossy(&frames).contains("event: changed"));
    println!("send-half-closed project watch received subsequent filesystem change");
    assert_eq!(get(addr, "/v2/sessions/missing/fs/watch").0, 404);
    await_streams(3);
    let close_started = Instant::now();
    let closed = streams.pop().unwrap();
    closed.shutdown(Shutdown::Both).unwrap();
    drop(closed);
    await_streams(2);
    println!(
        "idle project watch disconnect released slot: {:?}",
        close_started.elapsed()
    );
    // Real Stream limit, with all subscriptions served by the real watch loop.
    while streams.len() < STREAM_LIMIT {
        let path = if streams.len() % 2 == 0 {
            project_watch
        } else {
            &session_watch
        };
        streams.push(open_watch(addr, path));
    }
    await_streams(STREAM_LIMIT);
    let (status, refusal) = get(addr, project_watch);
    assert_eq!(status, 503, "{refusal}");
    assert_eq!(refusal["error"]["details"]["reason"], "stream_limit");
    assert_eq!(
        refusal["error"]["details"]["capacity"]["streamsActive"],
        STREAM_LIMIT
    );
    assert_eq!(get(addr, "/v2/sessions").0, 200);
    let (status, refusal) = get(addr, &side_wait);
    assert_eq!(status, 503, "{refusal}");
    assert_eq!(refusal["error"]["details"]["reason"], "stream_limit");
    drop(streams.pop()); // plain peer close, not only explicit shutdown
    await_streams(STREAM_LIMIT - 1);
    streams.push(open_watch(addr, project_watch));
    await_streams(STREAM_LIMIT);
    for stream in streams {
        let _ = stream.shutdown(Shutdown::Both);
    }
    await_streams(0);
    // The same disconnect handling must reach the generic durable/live event
    // stream body (including its cloned socket held by RevocationWatch).
    let (event_stream, head) = connect(addr, "/v2/events/stream");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    await_streams(1);
    event_stream.shutdown(Shutdown::Both).unwrap();
    drop(event_stream);
    await_streams(0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while cutex::management::listeners::open_connections() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(cutex::management::listeners::bulk_connections(), 0);
    server.stop();
    println!(
        "128-stream limit, rejection, replacement and all connection permits returned; total {:?}",
        started.elapsed()
    );
}
