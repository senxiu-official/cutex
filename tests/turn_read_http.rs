//! Isolated real HTTP routing with synthetic Codex/Claude native endpoints.
//! Set CUTEX_TURN_READ_SMOKE_PORT=25470 and CUTEX_TURN_READ_EVIDENCE to save a smoke trace.
use cutex::catalog::{CatalogEndpoint, CatalogError};
use cutex::management::server::{ManagementNativeForwardError, ManagementRequestContext};
use cutex::session::model::{
    CutexSessionArchiveState, CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};

const CODEX: &str = "cutex.10000000-0000-4000-8000-000000000001";
const CLAUDE: &str = "cutex.10000000-0000-4000-8000-000000000002";
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
        side_chat: |_, _, _| unreachable!(),
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
        list_management_projects: |_| unreachable!(),
        read_management_project: |_, _| unreachable!(),
        update_management_project_presentation: |_, _| unreachable!(),
        execute_management_operator_action: |_, _| unreachable!(),
        execute_management_project_mutation: |_, _| unreachable!(),
        create_human_agent: |_, _| unreachable!(),
        create_human_session: |_, _| unreachable!(),
    }
}

#[test]
fn synthetic_codex_claude_http_smoke() {
    let parent = std::path::PathBuf::from(
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private home required"),
    );
    assert!(parent.join(".cutex-test-private-home").is_file());
    let home = parent.join(format!("turn-read-http-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".cutex-test-private-home"), "synthetic turn read").unwrap();
    std::env::set_var("CUTEX_TEST_PRIVATE_HOME", &home);
    std::env::set_var("HOME", &home);
    let mut store = CutexSessionStore::default();
    for id in [CODEX, CLAUDE] {
        let mut record = CutexSessionRecord::new(
            id.into(),
            Some(thread_id(id)),
            cutex::platform::host::current_host_name(),
            home.display().to_string(),
            None,
        )
        .unwrap();
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        record.exposed_to_backend = false; // Hidden sessions must still be readable by root.
        if id == CLAUDE {
            record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        }
        store.sessions.insert(id.into(), record);
        fixtures().insert(
            id.into(),
            Fixture {
                turns: 6,
                online: true,
            },
        );
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    let port = std::env::var("CUTEX_TURN_READ_SMOKE_PORT")
        .ok()
        .map(|s| s.parse::<u16>().unwrap())
        .unwrap_or(0);
    assert!(port == 0 || port == 25470);
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let addr = listener.local_addr().unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let server = std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let mut stream = stream.unwrap();
            cutex::management::server::handle_management_request(
                &mut stream,
                Some("fixture-root"),
                Some("fixture-root"),
                context(),
            )
            .unwrap();
        }
    });
    let mut evidence = Vec::new();
    let mut request = |method: &str, path: &str, root: bool, body: Value| -> (u16, Value) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        let payload = if method == "POST" {
            body.to_string()
        } else {
            String::new()
        };
        let bearer = if root {
            "fixture-root"
        } else {
            "fixture-agent"
        };
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}", payload.len()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        let (header, body) = text.split_once("\r\n\r\n").unwrap();
        let status = header.split_whitespace().nth(1).unwrap().parse().unwrap();
        let value: Value = serde_json::from_str(body).unwrap();
        evidence.push(
            json!({"method":method, "path":path, "root":root, "status":status, "response":value}),
        );
        (status, value)
    };
    assert_eq!(
        request(
            "GET",
            &format!("/v2/sessions/{CODEX}/history/turns/turn-0"),
            false,
            Value::Null
        )
        .0,
        401
    );
    assert_eq!(
        request(
            "GET",
            "/v2/sessions/missing/history/turns/t",
            true,
            Value::Null
        )
        .0,
        404
    );
    for id in [CODEX, CLAUDE] {
        let base = format!("/v2/sessions/{id}");
        // A cold page read never scans the thread for ordinals (PERF-10).
        let (status, body) = request("GET", &format!("{base}/history?limit=2"), true, Value::Null);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["page"]["firstOrdinal"], Value::Null);
        assert_eq!(body["page"]["turnCount"], Value::Null);
        // A single-turn read builds the index; pages use it from then on.
        let (status, body) = request(
            "GET",
            &format!("{base}/history/turns/turn-0"),
            true,
            Value::Null,
        );
        assert_eq!(status, 200, "{body}");
        let mut cursor: Option<String> = None;
        for (expected, count) in [(5, json!(6)), (3, Value::Null), (1, Value::Null)] {
            let path = format!(
                "{base}/history?limit=2{}",
                cursor
                    .as_ref()
                    .map(|c| format!("&cursor={c}"))
                    .unwrap_or_default()
            );
            let (status, body) = request("GET", &path, true, Value::Null);
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["page"]["firstOrdinal"], expected);
            assert_eq!(body["page"]["turnCount"], count);
            cursor = body["page"]["nextCursor"].as_str().map(str::to_owned);
        }
        for n in [0, 2, 5] {
            let (status, body) = request(
                "GET",
                &format!("{base}/history/turns/turn-{n}"),
                true,
                Value::Null,
            );
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["ordinal"], n + 1);
            assert_eq!(body["turnCount"], 6);
            assert_eq!(body["source"], "live");
            assert!(body.get("cutexState").is_none());
        }
        let (_, body) = request(
            "GET",
            &format!("{base}/history/turns/turn-0?compact=1"),
            true,
            Value::Null,
        );
        assert_eq!(body["complete"], false);
        assert_eq!(
            body["native"]["result"]["thread"]["turns"][0]["items"][0]["truncated"],
            true
        );
        let (status, body) = request(
            "GET",
            &format!("{base}/bootstrap?turnLimit=2"),
            true,
            Value::Null,
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["historyPage"]["firstOrdinal"], 5, "{body}");
        assert_eq!(body["historyPage"]["turnCount"], 6);
        fixtures().get_mut(id).unwrap().online = false;
        let (_, body) = request(
            "GET",
            &format!("{base}/history/turns/turn-2"),
            true,
            Value::Null,
        );
        assert_eq!(body["source"], "offline");
        assert_eq!(body["ordinal"], 3);
        let (status, body) = request(
            "POST",
            &format!("{base}/cutex/requests"),
            true,
            json!({"requestId":format!("revert-{id}"), "method":"cutex/session/history/revert", "params":{"beforeTurnId":"turn-5"}}),
        );
        assert_eq!(status, 200, "{body}");
        let (status, body) = request(
            "GET",
            &format!("{base}/history/turns/turn-5"),
            true,
            Value::Null,
        );
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "turn_not_found");
        let (_, body) = request(
            "GET",
            &format!("{base}/history/turns/turn-4"),
            true,
            Value::Null,
        );
        assert_eq!(body["turnCount"], 5);
        store.sessions.get_mut(id).unwrap().archive_state = CutexSessionArchiveState::Retired;
        cutex::session::store::save_cutex_session_store(&store).unwrap();
        let (status, body) = request(
            "GET",
            &format!("{base}/history/turns/turn-0"),
            true,
            Value::Null,
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["source"], "offline");
        cutex::management::v2::history::evict_offline_reader(id);
    }
    // A fresh binding with 1500 synthetic turns must not scan during bootstrap.
    store.sessions.get_mut(CODEX).unwrap().archive_state = CutexSessionArchiveState::Active;
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    fixtures().insert(
        CODEX.into(),
        Fixture {
            turns: 1500,
            online: true,
        },
    );
    cutex::management::v2::history::invalidate_session(CODEX);
    let long_base = format!("/v2/sessions/{CODEX}");
    let start = std::time::Instant::now();
    let (status, cold_bootstrap) = request(
        "GET",
        &format!("{long_base}/bootstrap?turnLimit=20"),
        true,
        Value::Null,
    );
    let bootstrap_us = start.elapsed().as_micros();
    assert_eq!(status, 200, "{cold_bootstrap}");
    assert_eq!(cold_bootstrap["historyPage"]["turnCount"], Value::Null);
    let start = std::time::Instant::now();
    let (status, body) = request(
        "GET",
        &format!("{long_base}/history/turns/turn-750"),
        true,
        Value::Null,
    );
    let cold_us = start.elapsed().as_micros();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ordinal"], 751);
    assert_eq!(body["turnCount"], 1500);
    let start = std::time::Instant::now();
    let (status, body) = request(
        "GET",
        &format!("{long_base}/history/turns/turn-750"),
        true,
        Value::Null,
    );
    let hot_us = start.elapsed().as_micros();
    assert_eq!(status, 200, "{body}");
    let (_, warm_bootstrap) = request(
        "GET",
        &format!("{long_base}/bootstrap?turnLimit=20"),
        true,
        Value::Null,
    );
    assert_eq!(warm_bootstrap["historyPage"]["firstOrdinal"], 1481);
    assert_eq!(warm_bootstrap["historyPage"]["turnCount"], 1500);
    stopped.store(true, Ordering::SeqCst);
    drop(TcpStream::connect(addr).unwrap());
    server.join().unwrap();
    if let Ok(path) = std::env::var("CUTEX_TURN_READ_EVIDENCE") {
        std::fs::write(path, serde_json::to_vec_pretty(&json!({"fixture":"synthetic native endpoints; no model calls", "home":home, "address":addr.to_string(),
            "timingsUs":{"coldBootstrap":bootstrap_us,"coldTurnRead":cold_us,"hotTurnRead":hot_us}, "requests":evidence})).unwrap()).unwrap();
    }
    eprintln!(
        "TURN_READ_HTTP_SMOKE requests={} home={} address={addr}",
        evidence.len(),
        home.display()
    );
}
