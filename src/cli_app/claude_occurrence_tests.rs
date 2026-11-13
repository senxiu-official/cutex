//! Private loopback acceptance of the two observer CLI processes.
use super::*;
use cutex::agent_bus::{
    claude_occurrence::{self, Occurrence, Request},
    store::AgentBusState,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::atomic::{AtomicBool, AtomicUsize},
    thread,
};
const SESSION: &str = "cutex.occurrence-fixture";
const THREAD: &str = "018e0000-0000-7000-8000-0000b0009ae4";
const TEST: &str = "cli_app::claude_runtime::occurrence_tests::two_cli_observers_keep_native_authority_across_transport_failures";

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(25);
    while !condition() {
        assert!(Instant::now() < deadline, "fixture deadline");
        thread::sleep(Duration::from_millis(15));
    }
}
struct Children(Vec<Child>);
impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}
fn child(root: &Path, role: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env("HOME", root)
        .env_remove("CUTEX_HOME")
        .env("CUTEX_TEST_PRIVATE_HOME", root)
        .env("CUTEX_OCCURRENCE_FIXTURE_ROLE", role)
        .spawn()
        .unwrap()
}
fn handlers() -> cutex::agent_bus::server::AgentBusRequestHandlers {
    cutex::agent_bus::server::AgentBusRequestHandlers {
        reconcile_registration_agent: |_, _| {
            panic!("Claude must not register through generic reconciliation")
        },
        reconcile_agent: |_| panic!("unexpected reconcile"),
        redrive_ordinary_messages: |_| Ok(0),
        send_payload_response: |_, _| panic!("no input dispatch"),
        agent_management: |_, _, _| panic!("no management mutation"),
        automatic_managed_online: |_, _| panic!("no autostart"),
        managed_archive: |_, _| panic!("no archive"),
        job_service_completion: |_, _, _| panic!("no jobs"),
        job_service_completion_query: |_, _| panic!("no jobs"),
        management_start_acked: |_, _| {},
    }
}
fn raw(
    bus: u16,
    path: &str,
    token: Option<&str>,
    runtime: &str,
    generation: u64,
    body: Value,
) -> (String, Value) {
    let body = body.to_string();
    let mut socket = TcpStream::connect(("127.0.0.1", bus)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    write!(socket,"POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}X-Cutex-Agent-Id: {runtime}\r\nX-Cutex-Mcp-Thread-Id: {THREAD}\r\nX-Cutex-Mcp-Generation: {generation}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    socket.shutdown(Shutdown::Write).unwrap();
    let mut bytes = String::new();
    socket.read_to_string(&mut bytes).unwrap();
    let (status, body) = bytes.split_once("\r\n\r\n").unwrap();
    (
        status.to_string(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}
fn authorized(port: u16, generation: u64, runtime: &str) -> bool {
    let token = cutex::agent_bus::agent_auth::agent_credential("fixture-root", runtime, SESSION);
    raw(
        port,
        cutex::agent_bus::agent_auth::BROWSER_FENCE_PATH,
        Some(&token),
        runtime,
        generation,
        json!({}),
    )
    .0
    .starts_with("HTTP/1.1 200")
}
pub(super) fn fixture_record(root: &Path) -> cutex::session::model::CutexSessionRecord {
    use cutex::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};
    let mut record = CutexSessionRecord::new(
        SESSION.into(),
        None,
        cutex::platform::host::current_host_name(),
        root.to_string_lossy().into_owned(),
        None,
    )
    .unwrap();
    record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
    record.runtime_generation = 1;
    record.runtime_pid = Some(std::process::id());
    record.current_runtime_agent_id = Some("claude.one".into());
    record.agent_enabled = true;
    record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::Persistent;
    record.claude_session=Some(serde_json::from_value(json!({
        "deployment":{"python":"unused","adapter_source":root.join("adapter"),"claude_cli":"unused","claude_home":"unused","tui_binary":"unused"},
        "thread_id":THREAD,"state_dir":root.join("adapter-state"),"model":"sonnet","instructions":"","connection_id":"one","allow_write":false,"allow_exec":false
    })).unwrap());
    record
}
fn save_record(record: &cutex::session::model::CutexSessionRecord) {
    let mut store = cutex::session::store::load_cutex_session_store().unwrap();
    store.sessions.insert(SESSION.into(), record.clone());
    cutex::session::store::save_cutex_session_store(&store).unwrap();
}
fn transition(
    record: &mut cutex::session::model::CutexSessionRecord,
    logical: &Mutex<String>,
    generation: u64,
    connection: &str,
) {
    record.runtime_generation = generation;
    record.runtime_pid = Some(std::process::id());
    record.current_runtime_agent_id = Some(format!("claude.{connection}"));
    record.archive_state = cutex::session::model::CutexSessionArchiveState::Active;
    record.claude_session.as_mut().unwrap().connection_id = Some(connection.into());
    *logical.lock().unwrap() = connection.into();
    save_record(record);
    cutex::runtime::claude::refresh_mcp_binding(SESSION).unwrap();
}
fn offline_hint(port: u16, record: &cutex::session::model::CutexSessionRecord) {
    raw(
        port,
        claude_occurrence::PATH,
        Some("fixture-root"),
        record.current_runtime_agent_id.as_deref().unwrap(),
        record.runtime_generation,
        json!(Request {
            occurrence: Occurrence::from_record(record).unwrap(),
            confirmed_offline: true
        }),
    );
}
fn observer(root: &Path, role: &str) {
    observe(SESSION).unwrap();
    assert!(manager().status(SESSION).unwrap().unwrap().connected);
    let diagnostics = with_state(|state| state.snapshot(Instant::now()));
    assert_eq!(diagnostics["sessions"][0]["state"], "observing");
    assert!(
        diagnostics["sessions"][0]["registrationError"].is_string(),
        "first stream must survive failed/pending registration"
    );
    std::fs::write(root.join(format!("ready-{role}")), b"ready").unwrap();
    for step in 0..20 {
        let command = root.join(format!("command-{role}-{step}"));
        wait_for(|| command.is_file());
        let command = std::fs::read_to_string(command).unwrap();
        let result = match command.as_str() {
            "timeout" => {
                // A new socket really fails, while the authenticated observer
                // remains usable even when its status RPC is also unanswered.
                assert!(cutex::runtime::claude::observation(SESSION).is_err());
                observe(SESSION).unwrap();
                assert!(manager().status(SESSION).unwrap().unwrap().connected);
                let diagnostics = with_state(|state| state.snapshot(Instant::now()));
                assert_eq!(diagnostics["sessions"][0]["state"], "observing");
                assert!(diagnostics["sessions"][0]["probeError"].is_string());
                let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(&cutex::config::store::load_codez_config());
                let record = cutex::session::store::shared_session_row(SESSION).unwrap().unwrap();
                let identity = RuntimeIdentity { generation:record.runtime_generation,pid:record.runtime_pid.unwrap(),
                    connection_id:record.claude_session.as_ref().unwrap().connection_id.clone() };
                DELIVERY_WAKE_TARGETS.lock().unwrap().insert(SESSION.into(),identity);
                let mut healthy=HashSet::from([SESSION.to_owned()]);
                let began=Instant::now();
                deliver_woken(SESSION, &mut healthy, &bus, &mut HashMap::new());
                assert!(healthy.contains(SESSION),"diagnostic timeout disabled immediate delivery wake");
                // Diagnostic retry is five seconds away; delivery did not wait.
                assert!(began.elapsed() < Duration::from_secs(5));
                // This measurement traverses the REAL authenticated gate, not
                // the synthetic lower-level snapshot fixture's no-runtime row.
                assert!(observed_connection::current(&record).unwrap().is_some());
                let checks=observed_connection::delivery_checks_on_thread();
                let clones=cutex::session::model::whole_store_clones_on_this_thread();
                let hot=Instant::now();
                for _ in 0..5 {deliver_messages(SESSION,&bus,&mut HashMap::new()).unwrap();}
                assert_eq!(observed_connection::delivery_checks_on_thread()-checks,5,"one real gate per delivery visit");
                assert_eq!(cutex::session::model::whole_store_clones_on_this_thread()-clones,0);
                eprintln!("verified Claude delivery hot path: 5 real gates, 0 whole-store clones, {} us",hot.elapsed().as_micros());
                "retained"
            }
            "deliver" => {
                observe(SESSION).unwrap();
                let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(&cutex::config::store::load_codez_config());
                deliver_messages(SESSION, &bus, &mut HashMap::new()).unwrap();
                "delivered"
            }
            "replacement-rejected" => {
                assert!(observe(SESSION).is_err(),"replacement needs fresh authentication");
                assert!(manager().status(SESSION).unwrap().is_none_or(|status|!status.connected));
                assert!(observed_connection::snapshot(SESSION).is_none());
                let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(&cutex::config::store::load_codez_config());
                assert!(deliver_messages(SESSION,&bus,&mut HashMap::new()).is_err());
                "rejected"
            }
            "dropped" => {
                wait_for(|| {
                    manager()
                        .status(SESSION)
                        .unwrap()
                        .is_some_and(|status| !status.connected)
                });
                "dropped"
            }
            "recover" => {
                let old = observed_connection::snapshot(SESSION);
                observe(SESSION).unwrap();
                if let Some(old) = old { detach_observed_proof(SESSION,&old).unwrap(); }
                assert!(manager().status(SESSION).unwrap().unwrap().connected);
                "recovered"
            }
            "teardown" => {
                manager().disconnect(SESSION).unwrap();
                "torn-down"
            }
            "offline" => {
                let error = observe(SESSION).unwrap_err();
                assert!(
                    error
                        .downcast_ref::<cutex::runtime::claude::ClaudeObservationOffline>()
                        .is_some(),
                    "Bus failure must not replace the native offline classification: {error:#}"
                );
                assert!(manager()
                    .status(SESSION)
                    .unwrap()
                    .is_none_or(|status| !status.connected));
                "offline"
            }
            "native-offline" => {
                let record = cutex::session::store::shared_session_row(SESSION).unwrap().unwrap();
                let identity = RuntimeIdentity { generation:record.runtime_generation,pid:record.runtime_pid.unwrap(),
                    connection_id:record.claude_session.as_ref().unwrap().connection_id.clone() };
                with_state(|state| { state.probe_result(SESSION,&identity,None,Instant::now()-Duration::from_secs(10)); });
                let error = observe(SESSION).unwrap_err();
                assert!(error.downcast_ref::<cutex::runtime::claude::ClaudeObservationOffline>().is_some(),"{error:#}");
                assert!(manager().status(SESSION).unwrap().is_none_or(|status|!status.connected));
                assert!(observed_connection::snapshot(SESSION).is_none());
                "offline"
            }
            "unowned-offline" => {
                // Deliberately remove only local proof from a real connected
                // manager. Gen/thread alone must not authorize detaching a
                // possibly newer connection after this cold probe returns.
                observed_connection::forget(SESSION);
                let error=observe(SESSION).unwrap_err();
                assert!(error.downcast_ref::<cutex::runtime::claude::ClaudeObservationOffline>().is_some(),"{error:#}");
                assert!(format!("{error:#}").contains("No captured observer proof"));
                assert!(manager().status(SESSION).unwrap().unwrap().connected);
                assert!(observed_connection::snapshot(SESSION).is_none());
                let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(&cutex::config::store::load_codez_config());
                let error=deliver_messages(SESSION,&bus,&mut HashMap::new()).unwrap_err();
                assert!(error.to_string().contains("exact authenticated observer connection"));
                "blocked"
            }
            "recovering" => {
                let record = cutex::session::store::shared_session_row(SESSION).unwrap().unwrap();
                let identity = RuntimeIdentity { generation:record.runtime_generation,pid:record.runtime_pid.unwrap(),
                    connection_id:record.claude_session.as_ref().unwrap().connection_id.clone() };
                with_state(|state| { state.probe_result(SESSION,&identity,None,Instant::now()-Duration::from_secs(10)); });
                let error=observe(SESSION).unwrap_err();
                assert!(error.downcast_ref::<cutex::runtime::claude::ClaudeObservationRecovering>().is_some(),"{error:#}");
                assert!(manager().status(SESSION).unwrap().unwrap().connected);
                assert!(observed_connection::snapshot(SESSION).is_some());
                assert!(observed_connection::current(&record).unwrap().is_none());
                let bus = cutex::agent_bus::client::AgentBusHttpClient::from_config(&cutex::config::store::load_codez_config());
                assert!(deliver_messages(SESSION,&bus,&mut HashMap::new()).is_err());
                "recovering"
            }
            "exit" => {
                std::fs::write(root.join(format!("reply-{role}-{step}")), b"exit").unwrap();
                return;
            }
            _ => panic!("unknown fixture command"),
        };
        std::fs::write(root.join(format!("reply-{role}-{step}")), result).unwrap();
    }
}
fn command(root: &Path, role: &str, step: usize, text: &str) {
    std::fs::write(root.join(format!("command-{role}-{step}")), text).unwrap();
    wait_for(|| root.join(format!("reply-{role}-{step}")).is_file());
}
#[test]
fn two_cli_observers_keep_native_authority_across_transport_failures() {
    let role = std::env::var("CUTEX_OCCURRENCE_FIXTURE_ROLE").ok();
    if role.is_none() {
        let root = PathBuf::from(std::env::var_os("HOME").expect("marked private test HOME"))
            .join(format!("claude-two-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".cutex-test-private-home"), b"isolated fixture").unwrap();
        let mut children = Children(vec![child(&root, "orchestrator")]);
        let status = children.0[0].wait().unwrap();
        let finished = root.join("fixture-passed").is_file();
        drop(children);
        std::fs::remove_dir_all(root).unwrap();
        assert!(status.success() && finished);
        return;
    }
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    if matches!(role.as_deref(), Some("a" | "b" | "c")) {
        observer(&root, role.as_deref().unwrap());
        return;
    }
    let state = Arc::new(Mutex::new(AgentBusState::default()));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let state_server = state.clone();
    let bus_mode = Arc::new(AtomicUsize::new(0));
    let bus_mode_server = bus_mode.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let state = state_server.clone();
            let mode = bus_mode_server.clone();
            thread::spawn(move || {
                stream.set_nonblocking(false).unwrap();
                let mode = mode.load(Ordering::SeqCst);
                if mode != 0 {
                    cutex::http::server::read_simple_http_request(&mut stream).unwrap();
                    cutex::http::server::write_json_response(
                        &mut stream,
                        if mode == 1 { 503 } else { 200 },
                        if mode == 1 { "Unavailable" } else { "OK" },
                        &if mode == 1 {
                            json!({"error":"fixture Bus unavailable"})
                        } else {
                            json!({"outcome":"stale"})
                        },
                    )
                    .unwrap();
                    return;
                }
                if let Err(error) = cutex::agent_bus::server::handle_agent_bus_request(
                    &mut stream,
                    &state,
                    Some("fixture-root"),
                    None,
                    handlers(),
                ) {
                    cutex::http::server::write_http_response(
                        &mut stream,
                        500,
                        "Error",
                        "text/plain",
                        error.to_string().as_bytes(),
                    )
                    .unwrap();
                }
            });
        }
    });
    let mut config = cutex::profiles::model::CodezConfig::default();
    config.agent_bus_port = Some(port);
    config.agent_bus_token = Some("fixture-root".into());
    cutex::config::store::save_codez_config(&config).unwrap();
    let timeout_new = Arc::new(AtomicBool::new(false));
    let drop_sockets = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let logical = Arc::new(Mutex::new("one".to_string()));
    let native_state = Arc::new(AtomicUsize::new(0));
    let accepts = Arc::new(AtomicUsize::new(0));
    let adapter = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let adapter_port = adapter.local_addr().unwrap().port();
    let (timeouts, drops, methods) = (timeout_new.clone(), drop_sockets.clone(), calls.clone());
    let (logical_server, native_server) = (logical.clone(), native_state.clone());
    let accepts_server = accepts.clone();
    thread::spawn(move || {
        for stream in adapter.incoming() {
            let stream = stream.unwrap();
            accepts_server.fetch_add(1, Ordering::SeqCst);
            stream.set_nonblocking(false).unwrap();
            if timeouts.load(Ordering::SeqCst) {
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(700));
                    drop(stream);
                });
                continue;
            }
            let drops = drops.clone();
            let methods = methods.clone();
            let (logical, native) = (logical_server.clone(), native_server.clone());
            let epoch = drops.load(Ordering::SeqCst);
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_millis(80)))
                    .unwrap();
                let Ok(mut socket) = tungstenite::accept(stream) else {
                    return;
                };
                loop {
                    if drops.load(Ordering::SeqCst) != epoch {
                        let _ = socket.get_ref().shutdown(Shutdown::Both);
                        return;
                    }
                    let message = match socket.read() {
                        Ok(message) => message,
                        Err(tungstenite::Error::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue
                        }
                        Err(_) => return,
                    };
                    let Ok(text) = message.to_text() else {
                        continue;
                    };
                    let Ok(request) = serde_json::from_str::<Value>(text) else {
                        continue;
                    };
                    let Some(method) = request["method"].as_str() else {
                        continue;
                    };
                    methods.lock().unwrap().push(method.into());
                    if request.get("id").is_none() {
                        continue;
                    }
                    if method == "claude/runtime/status" && native.load(Ordering::SeqCst) == 4 {
                        continue; // PRIVATE existing-stream status uncertainty
                    }
                    let result = match method {
                        "initialize" => json!({"userAgent":"fixture","capabilities":{}}),
                        "claude/capabilities" => {
                            json!({"implementation":"cute-claude-app-server","runtimeId":"adapter","backend":"claude-subscription-sdk","persistentRuntime":true,"ensureThread":"claude/thread/ensure"})
                        }
                        "claude/runtime/status" => {
                            let state = native.load(Ordering::SeqCst);
                            json!({"threadId":THREAD,"state":match state {1=>"closed",2=>"close_unverified",3=>"reconnecting",_=>"waiting"},
                                "connectionId":logical.lock().unwrap().clone(), "ownerTaskAlive":state == 0 || state == 3,
                                "processExitConfirmed":state == 1, "nativeConnectionGeneration":state + 1})
                        }
                        "claude/tasks/list" => json!({"tasks":[],"completeInventory":true}),
                        "claude/input/submit" => json!({"threadId":THREAD,"messageId":request["params"]["messageId"],
                            "durable":true,"receiptScope":"local-inbox","state":"queued"}),
                        "thread/resume" | "thread/read" => {
                            json!({"thread":{"id":THREAD,"status":{"type":"idle"},"turns":[]}})
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
                        return;
                    }
                }
            });
        }
    });
    let mut record = fixture_record(&root);
    let dir = record.claude_session.as_ref().unwrap().state_dir.clone();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(root.join("adapter/src/cute_claude")).unwrap();
    std::fs::write(
        root.join("adapter/src/cute_claude/protocol.py"),
        b"fixture schema",
    )
    .unwrap();
    std::fs::write(dir.join("server.json"),json!({"endpoint":format!("ws://127.0.0.1:{adapter_port}"),"runtimeId":"adapter","pid":std::process::id()}).to_string()).unwrap();
    std::fs::write(dir.join("rpc-token"), b"fixture-adapter").unwrap();
    save_record(&record);
    cutex::runtime::claude::refresh_mcp_binding(SESSION).unwrap();
    let binding = std::fs::read(dir.join("cutex-mcp-binding.json")).unwrap();
    // Genuine old mtimes use authenticated status rather than remaining stuck.
    let before_birth: std::time::SystemTime =
        (cutex::platform::process::process_started_at(std::process::id()).unwrap()
            - chrono::Duration::seconds(1))
        .into();
    std::fs::File::options()
        .write(true)
        .open(dir.join("server.json"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(before_birth))
        .unwrap();
    let generation = record.runtime_generation;
    // Startup adoption needs no observer, input, binding refresh or generation write.
    claude_occurrence::spawn(&state, |_| Ok(0)).unwrap();
    wait_for(|| authorized(port, 1, "claude.one"));
    assert_eq!(
        binding,
        std::fs::read(dir.join("cutex-mcp-binding.json")).unwrap()
    );
    assert!(calls.lock().unwrap().iter().all(|method| matches!(
        method.as_str(),
        "initialize" | "initialized" | "claude/capabilities" | "claude/runtime/status"
    )));
    let occurrence = Occurrence::from_record(&record).unwrap();
    let hint = json!(Request {
        occurrence: occurrence.clone(),
        confirmed_offline: true
    });
    assert_eq!(
        raw(
            port,
            claude_occurrence::PATH,
            Some("fixture-root"),
            "claude.one",
            1,
            hint.clone()
        )
        .1["outcome"],
        "unchanged"
    );
    assert!(authorized(port, 1, "claude.one"));
    let credential =
        cutex::agent_bus::agent_auth::agent_credential("fixture-root", "claude.one", SESSION);
    assert!(raw(
        port,
        claude_occurrence::PATH,
        Some(&credential),
        "claude.one",
        1,
        hint.clone()
    )
    .0
    .starts_with("HTTP/1.1 403"));
    let unconfigured = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let unconfigured_port = unconfigured.local_addr().unwrap().port();
    let unconfigured_state = state.clone();
    thread::spawn(move || {
        let (mut stream, _) = unconfigured.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        cutex::agent_bus::server::handle_agent_bus_request(
            &mut stream,
            &unconfigured_state,
            None,
            None,
            handlers(),
        )
        .unwrap();
    });
    assert!(raw(
        unconfigured_port,
        claude_occurrence::PATH,
        None,
        "claude.one",
        1,
        hint.clone()
    )
    .0
    .starts_with("HTTP/1.1 403"));
    let mut forged = hint.clone();
    forged["occurrence"]["pid"] = json!(1);
    assert_eq!(
        raw(
            port,
            claude_occurrence::PATH,
            Some("fixture-root"),
            "claude.one",
            1,
            forged
        )
        .1["outcome"],
        "stale"
    );
    assert!(authorized(port, 1, "claude.one"));
    for runtime in ["stock.fixture", "dsh.fixture"] {
        let mut roster = state.lock().unwrap().agents["claude.one"].clone();
        roster.id = runtime.into();
        state.lock().unwrap().agents.insert(runtime.into(), roster);
        assert_eq!(
            raw(
                port,
                "/api/agents/unregister",
                Some("fixture-root"),
                runtime,
                1,
                json!({"id":runtime})
            )
            .1["removed"],
            true
        );
    }
    let legacy = raw(
        port,
        "/api/agents/register",
        Some("fixture-root"),
        "claude.legacy",
        1,
        json!({"id":"claude.legacy","name":"legacy","sessionId":THREAD,"profile":"default","cwd":root,
            "pid":std::process::id(),"groups":[],"registrationClass":"persistent"}),
    );
    assert!(!legacy.0.starts_with("HTTP/1.1 200"));
    assert!(!state.lock().unwrap().agents.contains_key("claude.legacy"));
    bus_mode.store(1, Ordering::SeqCst);
    let mut children = Children(vec![child(&root, "a")]);
    wait_for(|| root.join("ready-a").is_file());
    bus_mode.store(2, Ordering::SeqCst);
    children.0.push(child(&root, "b"));
    wait_for(|| root.join("ready-b").is_file());
    bus_mode.store(0, Ordering::SeqCst);
    // Canonical private bus message, then the actual HTTP poll/native receipt/ACK.
    let message: cutex::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
        "id":"fixture-probe-independent-message","from":"fixture.sender","to":"claude.one",
        "fromCutexSessionId":"cutex.fixture-sender","toCutexSessionId":SESSION,"content":"private unique passive message",
        "deliveryMode":"passive","triggerTurn":false,"createdAtEpochSecs":chrono::Utc::now().timestamp(),"senderKind":"agent"
    })).unwrap();
    let params = cutex::app_server::bus_bridge::inter_agent_params(THREAD,"fixture",SESSION,&message).unwrap();
    cutex::management::v2::agent_bus_state::agent_bus_message_repository().unwrap().record_queued(
        cutex::management::v2::agent_bus_state::AgentBusQueuedMessage { owner_cutex_session_id:SESSION.into(),
            message_id:message.id.clone(),from_cutex_session_id:message.from_cutex_session_id.clone(),to_cutex_session_id:SESSION.into(),
            from_runtime_agent_id:Some(message.from.clone()),to_runtime_agent_id:Some(message.to.clone()),delivery_mode:"passive".into(),
            content:message.content.clone(),queued_at:chrono::Utc::now(),canonical_envelope:message.clone(),
            semantic_sha256:cutex::app_server::bus_bridge::inter_agent_semantic_sha256(&params) }).unwrap();
    state.lock().unwrap().messages.entry("claude.one".into()).or_default().push_back(message.clone());
    thread::sleep(Duration::from_millis(5100)); // the normal same-handle check is due
    timeout_new.store(true, Ordering::SeqCst);
    native_state.store(4, Ordering::SeqCst);
    command(&root, "a", 0, "timeout");
    assert_eq!(calls.lock().unwrap().iter().filter(|m| m.as_str()=="claude/input/submit").count(),1);
    assert_eq!(cutex::management::v2::agent_bus_state::agent_bus_message_repository().unwrap()
        .message_state(&message.id).unwrap().unwrap().0,"inbox_accepted","local receipt must not invent native context consumption");
    assert!(state.lock().unwrap().messages.get("claude.one").is_none_or(|messages| messages.is_empty()));
    // Lost ACK/replayed poll across the other CLI cannot submit a second input.
    state.lock().unwrap().messages.entry("claude.one".into()).or_default().push_back(message);
    command(&root,"b",0,"deliver");
    assert_eq!(calls.lock().unwrap().iter().filter(|m| m.as_str()=="claude/input/submit").count(),1);
    assert!(state.lock().unwrap().messages.get("claude.one").is_none_or(|messages| messages.is_empty()));
    assert!(authorized(port, 1, "claude.one"));
    native_state.store(0, Ordering::SeqCst);
    timeout_new.store(false, Ordering::SeqCst);
    drop_sockets.fetch_add(1, Ordering::SeqCst);
    command(&root, "a", 1, "dropped");
    command(&root, "b", 1, "dropped");
    claude_occurrence::reconcile(&state).unwrap();
    assert!(authorized(port, 1, "claude.one"));
    command(&root, "b", 2, "recover");
    command(&root, "a", 2, "teardown");
    assert!(authorized(port, 1, "claude.one"));
    assert_eq!(
        raw(
            port,
            "/api/agents/unregister",
            Some("fixture-root"),
            "claude.one",
            1,
            json!({"id":"claude.one"})
        )
        .1["removed"],
        false
    );
    assert!(authorized(port, 1, "claude.one"));
    native_state.store(3,Ordering::SeqCst);
    command(&root,"b",3,"recovering");
    native_state.store(0,Ordering::SeqCst);
    command(&root,"b",4,"recover");
    bus_mode.store(1, Ordering::SeqCst);
    native_state.store(1, Ordering::SeqCst);
    command(&root,"b",5,"unowned-offline");
    native_state.store(0,Ordering::SeqCst);
    save_record(&record);
    command(&root,"b",6,"recover");
    native_state.store(1,Ordering::SeqCst);
    command(&root, "b", 7, "native-offline");
    native_state.store(0, Ordering::SeqCst);
    record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
    save_record(&record);
    command(&root, "b", 8, "offline");
    bus_mode.store(0, Ordering::SeqCst);
    command(&root, "a", 3, "exit");
    command(&root, "b", 9, "exit");
    for child in &mut children.0 {
        assert!(child.wait().unwrap().success());
    }
    // Replacement through durable authority fences old tools before the tick.
    record.runtime_generation = 2;
    record.archive_state = cutex::session::model::CutexSessionArchiveState::Active;
    record.current_runtime_agent_id = Some("claude.two".into());
    record.claude_session.as_mut().unwrap().connection_id = Some("two".into());
    *logical.lock().unwrap() = "two".into();
    save_record(&record);
    assert!(!authorized(port, 1, "claude.one"));
    cutex::runtime::claude::refresh_mcp_binding(SESSION).unwrap();
    claude_occurrence::reconcile(&state).unwrap();
    wait_for(|| authorized(port, 2, "claude.two"));
    assert_eq!(
        raw(
            port,
            claude_occurrence::PATH,
            Some("fixture-root"),
            "claude.one",
            1,
            hint
        )
        .1["outcome"],
        "stale"
    );
    assert!(authorized(port, 2, "claude.two"));

    // A storm of offline hints during native transport failure stays coalesced
    // and cannot clear or unregister a live, already adopted occurrence.
    thread::sleep(Duration::from_millis(10_100));
    let before_connections = accepts.load(Ordering::SeqCst);
    timeout_new.store(true, Ordering::SeqCst);
    for _ in 0..20 {
        offline_hint(port, &record);
    }
    thread::sleep(Duration::from_millis(800));
    assert_eq!(accepts.load(Ordering::SeqCst) - before_connections, 1);
    assert!(authorized(port, 2, "claude.two"));
    assert_eq!(
        cutex::session::store::shared_session_row(SESSION)
            .unwrap()
            .unwrap()
            .runtime_pid,
        record.runtime_pid
    );
    timeout_new.store(false, Ordering::SeqCst);

    // An ended owner with unverified cleanup loses roster authority while its
    // durable PID/binding remain reserved. Startup must independently verify it.
    bus_mode.store(2,Ordering::SeqCst);
    children.0.push(child(&root,"c"));
    wait_for(||root.join("ready-c").is_file());
    bus_mode.store(0,Ordering::SeqCst);
    timeout_new.store(true,Ordering::SeqCst);
    transition(&mut record, &logical, 3, "three");
    command(&root,"c",0,"replacement-rejected");
    timeout_new.store(false,Ordering::SeqCst);
    command(&root,"c",1,"recover");
    command(&root,"c",2,"exit");
    assert!(children.0.last_mut().unwrap().wait().unwrap().success());
    claude_occurrence::reconcile(&state).unwrap();
    wait_for(|| authorized(port, 3, "claude.three"));
    thread::sleep(Duration::from_millis(10_100));
    native_state.store(2, Ordering::SeqCst);
    offline_hint(port, &record);
    wait_for(|| !state.lock().unwrap().agents.contains_key("claude.three"));
    assert!(!authorized(port, 3, "claude.three"));
    assert_eq!(
        cutex::session::store::shared_session_row(SESSION)
            .unwrap()
            .unwrap()
            .runtime_pid,
        record.runtime_pid
    );
    thread::sleep(Duration::from_millis(10_100));
    let before_status = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|method| method.as_str() == "claude/runtime/status")
        .count();
    claude_occurrence::readopt_startup(&state).unwrap();
    wait_for(|| {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| method.as_str() == "claude/runtime/status")
            .count()
            > before_status
    });
    assert!(!authorized(port, 3, "claude.three"));

    // A positively closed exact owner settles the durable claim through the
    // same CAS as explicit close, without stopping the still-live adapter.
    transition(&mut record, &logical, 4, "four");
    native_state.store(1, Ordering::SeqCst);
    claude_occurrence::reconcile(&state).unwrap();
    wait_for(|| {
        cutex::session::store::shared_session_row(SESSION)
            .unwrap()
            .unwrap()
            .runtime_pid
            .is_none()
    });
    assert!(cutex::platform::process::process_is_running(
        std::process::id()
    ));
    assert!(!authorized(port, 4, "claude.four"));
    claude_occurrence::readopt_startup(&state).unwrap();
    assert!(!state.lock().unwrap().agents.contains_key("claude.four"));

    native_state.store(0, Ordering::SeqCst);
    transition(&mut record, &logical, 5, "five");
    claude_occurrence::reconcile(&state).unwrap();
    wait_for(|| authorized(port, 5, "claude.five"));
    record.runtime_pid = None;
    record.current_runtime_agent_id = None;
    save_record(&record);
    assert!(!authorized(port, 5, "claude.five"));
    claude_occurrence::reconcile(&state).unwrap();
    assert!(state.lock().unwrap().agents.is_empty());

    transition(&mut record, &logical, 6, "six");
    claude_occurrence::reconcile(&state).unwrap();
    wait_for(|| authorized(port, 6, "claude.six"));
    let mut restored = state.lock().unwrap().agents["claude.six"].clone();
    record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
    save_record(&record);
    assert!(!authorized(port, 6, "claude.six"));
    claude_occurrence::reconcile(&state).unwrap();
    assert!(state.lock().unwrap().agents.is_empty());

    // Restored roster plus a stale PID/endpoint is not proof of adoption.
    transition(&mut record, &logical, 7, "reused");
    restored.id = "claude.reused".into();
    state
        .lock()
        .unwrap()
        .agents
        .insert(restored.id.clone(), restored);
    let absent = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let absent_port = absent.local_addr().unwrap().port();
    drop(absent);
    std::fs::write(dir.join("server.json"), json!({"endpoint":format!("ws://127.0.0.1:{absent_port}"),"runtimeId":"adapter","pid":std::process::id()}).to_string()).unwrap();
    std::fs::File::options()
        .write(true)
        .open(dir.join("server.json"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(before_birth))
        .unwrap();
    claude_occurrence::readopt_startup(&state).unwrap();
    assert!(state.lock().unwrap().agents.is_empty());
    thread::sleep(Duration::from_millis(500));
    assert!(!authorized(port, 7, "claude.reused"));
    assert_eq!(generation, 1);
    assert!(!calls
        .lock()
        .unwrap()
        .iter()
        .any(|method| method.contains("/start") || method.contains("externalInput")));
    std::fs::write(
        root.join("fixture-passed"),
        b"two CLI, timeout, stream loss, delayed teardown, replacement, close",
    )
    .unwrap();
}
