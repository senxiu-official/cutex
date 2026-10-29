// Native I/O alone is synthetic. Routing, input locks, delivery reservations,
// durable queue state, event append/group commit and subscription are production.
use super::*;
use cutex::management::v2::model::{
    AppServerSchema, AppServerSchemaChannel, NativeMessage, NativeMessageKind,
};
use cutex::management::v2::repository::{EventRepository, ReplayQuery};
use cutex::management::v2::user_input::{
    UserInputOrigin, UserInputOriginKind, UserInputRepository, UserInputSubmitParams,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    OnceLock,
};
use std::sync::{Arc, Barrier};

static HTTP_SENDS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
// Test-only handler wall time includes RAII trace logging before socket EOF.
static HTTP_HANDLER_SAMPLES: OnceLock<Mutex<Vec<(String,u128)>>> = OnceLock::new();

pub(super) fn http_context() -> cutex::management::server::ManagementRequestContext {
    let mut context = crate::cli_app::management_context::management_request_context();
    context.load_runtime_status = |id| {
        Ok(Some(
            cutex::app_server::manager::AppServerManagedRuntimeStatus {
                cutex_session_id: id.into(),
                thread_id: id.trim_start_matches("cutex.").into(),
                runtime_generation: 1,
                connected: true,
                active_turn_id: Some("busy-turn".into()),
                active_turn_observed_at: None,
                thread_status: Some(json!({"type":"active"})),
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
            },
        ))
    };
    context.handle_user_input = |command| {
        let lock =
            session_input_lock(&command.cutex_session_id).map_err(queue_persistence_error)?;
        let _guard = lock
            .lock()
            .map_err(|_| queue_persistence_error(anyhow::anyhow!("fixture lock poisoned")))?;
        submit_v2_with_repository(
            command,
            user_input_repository().map_err(queue_persistence_error)?,
            |_, _| Some(Some("busy-turn".into())),
            |_, _| panic!("tracked new input must not read summary"),
            |_, _| {
                Ok(
                    json!({"thread":{"turns":[{"id":"busy-turn","status":"inProgress","items":[]}]}}),
                )
            },
            |command, method, _, disposition| {
                assert_eq!(method, "turn/steer");
                assert!(
                    HTTP_SENDS
                        .get_or_init(Default::default)
                        .lock()
                        .unwrap()
                        .insert(command.params.client_user_message_id.clone()),
                    "duplicate native send"
                );
                Ok(UserInputSubmitExecution {
                    join_refusal: None,
                    disposition,
                    app_server_accepted: true,
                    native_request_id: Some(json!(format!(
                        "fixture-native-{}",
                        command.params.client_user_message_id
                    ))),
                    native_method: Some(method.into()),
                    turn_id: Some("busy-turn".into()),
                    queue: None,
                    queue_paused: false,
                })
            },
            |_, _| panic!("busy input must not flush"),
        )
    };
    context
}

// Every exchange uses a freshly allocated loopback port, the real HTTP parser,
// root-proof authentication and V2 routing. It deliberately bypasses listener admission.
fn http_exchange(session: &str, body: Value) -> (u16, Value) {
    http_post(&format!("/v2/sessions/{session}/cutex/requests"), body)
}
fn http_post(path: &str, body: Value) -> (u16, Value) {
    http_post_with_context(path, body, http_context())
}
pub(super) fn http_post_with_context(path: &str, body: Value, context: cutex::management::server::ManagementRequestContext) -> (u16, Value) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            cutex::http::server::set_keep_alive(cutex::http::server::KeepAlive::Off);
            cutex::http::server::set_accept_gzip(false);
            let request = cutex::http::server::read_simple_http_request(&mut stream).unwrap();
            let handler_started=Instant::now();
            cutex::management::v2::server::handle_v2_request(
                &mut stream,
                &request,
                Some("isolated-fixture-root"),
                Some("isolated-fixture-root"),
                context,
            )
            .unwrap();
            let handler_us=handler_started.elapsed().as_micros();
            if let Some(samples)=HTTP_HANDLER_SAMPLES.get() {
                let body:Value=serde_json::from_slice(&request.body).unwrap();
                samples.lock().unwrap().push((body["requestId"].as_str().unwrap_or("").into(),handler_us));
            }
        });
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let payload = body.to_string();
        write!(stream, "POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer isolated-fixture-root\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}", payload.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        server.join().unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (
            head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            serde_json::from_str(body).unwrap(),
        )
    })
}

#[test]
#[ignore = "isolated 6870-file cleanup/control HTTP timing; run explicitly with --nocapture"]
fn old_logs_preview_keeps_http_control_and_submit_responsive() {
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("log-preview-http").unwrap();
    let session = format!("cutex.{}", Uuid::new_v4());
    let mut record = cutex::session::model::CutexSessionRecord::new(
        session.clone(),
        Some(session.trim_start_matches("cutex.").into()),
        current_host_name(),
        home.root().display().to_string(),
        None,
    )
    .unwrap();
    record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
    record.runtime_generation = 1;
    record.exposed_to_backend = false;
    let mut store = cutex::session::model::CutexSessionStore::default();
    store.sessions.insert(session.clone(), record);
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    management_v2_repository()
        .unwrap()
        .materialize_recovery_reset(&[], true)
        .unwrap();
    let root = cutex::config::paths::config_dir().unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(90 * 86400);
    let file_count = std::env::var("CUTEX_LOG_PREVIEW_LOAD_FILES")
        .ok()
        .map(|v| v.parse::<usize>().unwrap())
        .unwrap_or(6870);
    assert!((1..=6870).contains(&file_count));
    for i in 0..file_count {
        let path = root.join(format!(
            "runtime/app-server-journal/group-{}/capture-{i}.jsonl",
            i / 687
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        file.set_len(1_500_000_000 / 6870).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    let control = || {
        let start = Instant::now();
        let response = http_exchange(
            &session,
            json!({"requestId":format!("log-control-{}",Uuid::new_v4()),"method":"cutex/userInput/queue/list","params":{}}),
        );
        assert_eq!(response.0, 200, "{}", response.1);
        start.elapsed().as_micros()
    };
    // Exclude one-time schema/repository initialization from the steady-state idle comparison.
    let _ = control();
    let mut idle: Vec<_> = (0..10).map(|_| control()).collect();
    let running = AtomicBool::new(true);
    let mut busy = Vec::new();
    let mut submits = Vec::new();
    let (preview_ms, preview) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let start = Instant::now();
            let response = http_post("/v2/storage/cleanups/preview", json!({"action":"old-logs"}));
            running.store(false, Ordering::Release);
            assert_eq!(response.0, 200, "{}", response.1);
            (start.elapsed().as_millis(), response.1)
        });
        for n in 0..200 {
            if !running.load(Ordering::Acquire) {
                break;
            }
            busy.push(control());
            if n < 10 {
                let command = command(&session, &format!("log-preview-submit-{n}"));
                let start = Instant::now();
                let response = http_exchange(
                    &session,
                    json!({"requestId":command.management_request_id,"method":"cutex/userInput/submit","params":{"clientUserMessageId":command.params.client_user_message_id,"origin":command.params.origin,"strategy":"auto","input":command.params.input}}),
                );
                assert_eq!(response.0, 200, "{}", response.1);
                submits.push(start.elapsed().as_micros());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        worker.join().unwrap()
    });
    idle.sort_unstable();
    busy.sort_unstable();
    submits.sort_unstable();
    assert!(!busy.is_empty() && !submits.is_empty());
    eprintln!(
        "STORAGE_PREVIEW_HTTP_LOAD_RESULT {}",
        json!({"files":file_count,"logicalBytes":(1_500_000_000u64/6870)*file_count as u64,"previewMs":preview_ms,"complete":preview["complete"],"inspectedItems":preview["inspectedItems"],"returnedItems":preview["items"].as_array().unwrap().len(),"idleControlP95Us":percentile(&idle,95),"busyControlP95Us":percentile(&busy,95),"busyControlMaxUs":busy.last(),"submitP95Us":percentile(&submits,95),"controlSamples":busy.len(),"submitSamples":submits.len()})
    );
    assert!(
        *busy.last().unwrap() < 2_000_000,
        "control request stalled behind a preview"
    );
    assert!(
        *submits.last().unwrap() < 2_000_000,
        "submit stalled behind a preview"
    );
}

// A frozen baseline has no inherent publish_live method. Keep one byte-identical
// fixture on both revisions: Rust selects the candidate's inherent method, or
// this legacy append fallback on the baseline.
#[allow(dead_code)]
trait ProbeLiveFallback {
    fn publish_live(&self, pending:PendingEvent)->bool;
    fn live_delta_stats(&self)->Value;
}
impl ProbeLiveFallback for EventRepository {
    fn publish_live(&self,_pending:PendingEvent)->bool { false }
    fn live_delta_stats(&self)->Value { json!({"inputDeltas":0}) }
}

fn append_http_delta(repository: &EventRepository, session: &str, delta: usize) {
    let pending=PendingEvent {
        cutex_session_id: session.into(), host_id: current_host_name(), source: EventSource::AppServer,
        schema: Some(AppServerSchema { protocol: "codex-app-server".into(), major_version: 2,
            version: "load-fixture".into(), sha256: "0".repeat(64), channel: AppServerSchemaChannel::Experimental,
            capabilities: json!({"features":{"fullTextItemTerminal":true}}), extensions: vec![] }),
        correlation: EventCorrelation { runtime_generation: Some(1),
            thread_id: Some(session.trim_start_matches("cutex.").into()), turn_id: Some("busy-turn".into()), item_id:Some("stream-item".into()), ..Default::default() },
        native: Some(NativeMessage { kind: NativeMessageKind::Notification,
            message: json!({"method":"item/agentMessage/delta", "params":{
                "threadId":session.trim_start_matches("cutex."),"turnId":"busy-turn", "itemId":"stream-item","delta":format!("fixture-{delta}")}}) }),
        cutex: None,
    };
    if !repository.publish_live(pending.clone()) { repository.append(pending).unwrap(); }
}

#[test]
#[ignore = "isolated real V2 HTTP capacity measurement; run explicitly with --nocapture"]
fn ten_streaming_sessions_exercise_outer_http_submit_and_control() {
    const AGENTS: usize = 10;
    const REQUESTS: usize = 10;
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("control-http-load").unwrap();
    let mut store = cutex::session::model::CutexSessionStore::default();
    let sessions: Vec<_> = (0..AGENTS)
        .map(|_| format!("cutex.{}", Uuid::new_v4()))
        .collect();
    for session in &sessions {
        let mut record = cutex::session::model::CutexSessionRecord::new(
            session.clone(),
            Some(session.trim_start_matches("cutex.").into()),
            current_host_name(),
            home.root().display().to_string(),
            None,
        )
        .unwrap();
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        record.runtime_generation = 1;
        record.exposed_to_backend = false;
        store.sessions.insert(session.clone(), record);
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    let events = management_v2_repository().unwrap();
    events.materialize_recovery_reset(&[], true).unwrap();
    // Warm resource/contract caches equally on baseline and candidate.
    assert_eq!(
        http_exchange(
            &sessions[0],
            json!({"requestId":format!("warm-{}",Uuid::new_v4()),
        "method":"cutex/session/get","params":{}})
        )
        .0,
        200
    );
    let stop = AtomicBool::new(false);
    let native_events = AtomicUsize::new(0);
    let samples = Mutex::new(Vec::new());
    let controls = Mutex::new(Vec::new());
    let barrier = Barrier::new(AGENTS * 3 + 1);
    std::thread::scope(|scope| {
        let mut writers = Vec::new();
        let mut workers = Vec::new();
        for session in &sessions {
            let (stop, native_events, barrier) = (&stop, &native_events, &barrier);
            writers.push(scope.spawn(move || {
                barrier.wait();
                let mut delta = 0;
                while !stop.load(Ordering::Relaxed) {
                    append_http_delta(events, session, delta);
                    native_events.fetch_add(1, Ordering::Relaxed);
                    delta += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
            }));
            let samples = &samples;
            workers.push(scope.spawn(move || {
                barrier.wait();
                for n in 0..REQUESTS {
                    let command = command(session, &format!("{session}-{n}"));
                    let body = json!({"requestId":command.management_request_id,
                        "method":"cutex/userInput/submit","params":{
                            "clientUserMessageId":command.params.client_user_message_id,
                            "origin":command.params.origin,"strategy":"auto",
                            "input":command.params.input}});
                    let start = Instant::now();
                    let response = http_exchange(session, body.clone());
                    assert_eq!(response.0, 200, "{}", response.1);
                    samples.lock().unwrap().push(start.elapsed().as_micros());
                    assert_eq!(
                        http_exchange(session, body.clone()),
                        response,
                        "request receipt replay changed"
                    );
                    let mut retry = body;
                    retry["requestId"] = json!(format!("retry-{session}-{n}"));
                    let replay = http_exchange(session, retry);
                    assert_eq!(replay.0, 200, "{}", replay.1);
                    assert_eq!(
                        replay
                            .1
                            .pointer("/cutex/result/disposition")
                            .and_then(Value::as_str),
                        Some("deduplicated")
                    );
                }
            }));
            let controls = &controls;
            workers.push(scope.spawn(move || {
                barrier.wait();
                for n in 0..REQUESTS {
                    let start = Instant::now();
                    let response = http_exchange(
                        session,
                        json!({"requestId":format!("control-{session}-{n}"),
                        "method":"cutex/userInput/queue/list","params":{}}),
                    );
                    assert_eq!(response.0, 200, "{}", response.1);
                    controls.lock().unwrap().push(start.elapsed().as_micros());
                }
            }));
        }
        barrier.wait();
        // Stop streams even if an assertion in a request worker failed.
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
        stop.store(true, Ordering::Relaxed);
        for writer in writers {
            writer.join().unwrap();
        }
        assert!(
            results.into_iter().all(|result| result.is_ok()),
            "HTTP fixture worker failed"
        );
    });
    assert_eq!(
        HTTP_SENDS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .iter()
            .filter(|client| sessions.iter().any(|session| client.starts_with(session)))
            .count(),
        AGENTS * REQUESTS
    );
    assert!(native_events.load(Ordering::Relaxed) >= AGENTS);
    let mut samples = samples.lock().unwrap();
    samples.sort_unstable();
    let mut controls = controls.lock().unwrap();
    controls.sort_unstable();
    assert_eq!(samples.len(), AGENTS * REQUESTS);
    assert_eq!(controls.len(), AGENTS * REQUESTS);
    eprintln!(
        "cutex-control-load {}",
        json!({"schema":1,"kind":"http","operation":"user_input_submit+queue_list","agents":AGENTS,"requests":samples.len(),
        "nativeEvents":native_events.load(Ordering::Relaxed),
        "liveInputEvents":serde_json::to_value(events.live_delta_stats()).unwrap()["inputDeltas"],"p50Us":percentile(&samples,50),
        "p95Us":percentile(&samples,95),"maxUs":samples.last().unwrap(),
        "controlP50Us":percentile(&controls,50),"controlP95Us":percentile(&controls,95),
        "controlMaxUs":controls.last().unwrap(),"listenerAdmissionCovered":false})
    );
}

fn command(session: &str, client: &str) -> UserInputSubmitCommand {
    UserInputSubmitCommand {
        management_request_id: format!("request-{client}"),
        cutex_session_id: session.into(),
        thread_id: format!("thread-{session}"),
        params: UserInputSubmitParams {
            join_delegation: None,
            client_user_message_id: client.into(),
            origin: UserInputOrigin {
                kind: UserInputOriginKind::Other,
                client_id: "isolated-load".into(),
            },
            strategy: UserInputStrategy::Auto,
            input: vec![json!({"type":"text","text":"isolated fixture","text_elements":[]})],
            expected_turn_id: None,
        },
    }
}

fn percentile(samples: &[u128], percent: usize) -> u128 {
    let index = (samples.len() * percent).div_ceil(100).saturating_sub(1);
    samples[index.min(samples.len() - 1)]
}

#[test]
#[ignore = "isolated production-path capacity measurement; run explicitly with --nocapture"]
fn ten_streaming_sessions_preserve_submit_identity_and_event_order() {
    const AGENTS: usize = 10;
    const REQUESTS: usize = 10;
    const DELTAS: usize = 50;
    let root = std::env::temp_dir().join(format!("cutex-control-load-{}", Uuid::new_v4()));
    let repository = Arc::new(UserInputRepository::open(root.join("input")).unwrap());
    let events =
        Arc::new(EventRepository::open(root.join("events"), "isolated-load-host").unwrap());
    let subscription = events
        .page_and_subscribe(ReplayQuery::default(), 2048)
        .unwrap();
    let barrier = Arc::new(Barrier::new(AGENTS * 2 + 1));
    let sends = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let samples = Arc::new(Mutex::new(Vec::new()));
    let run = Uuid::new_v4();

    std::thread::scope(|scope| {
        for agent in 0..AGENTS {
            let session = format!("cutex.load-{run}-{agent}");
            let stream_session = session.clone();
            let (stream_events, stream_barrier) = (Arc::clone(&events), Arc::clone(&barrier));
            scope.spawn(move || {
                stream_barrier.wait();
                for delta in 0..DELTAS {
                    stream_events
                        .append(PendingEvent {
                            cutex_session_id: stream_session.clone(),
                            host_id: "isolated-load-host".into(),
                            source: EventSource::AppServer,
                            schema: Some(AppServerSchema {
                                protocol: "codex-app-server".into(),
                                major_version: 2,
                                version: "load-fixture".into(),
                                sha256: "0".repeat(64),
                                channel: AppServerSchemaChannel::Experimental,
                                capabilities: json!({}),
                                extensions: vec![],
                            }),
                            correlation: EventCorrelation {
                                runtime_generation: Some(1),
                                thread_id: Some(format!("thread-{stream_session}")),
                                turn_id: Some("busy-turn".into()),
                                ..Default::default()
                            },
                            native: Some(NativeMessage {
                                kind: NativeMessageKind::Notification,
                                message: json!({"method":"item/agentMessage/delta", "params":{
                                "threadId":format!("thread-{stream_session}"),"turnId":"busy-turn",
                                "itemId":"stream-item","delta":format!("fixture-{delta}")}}),
                            }),
                            cutex: None,
                        })
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            let (repository, barrier, sends, samples) = (
                Arc::clone(&repository),
                Arc::clone(&barrier),
                Arc::clone(&sends),
                Arc::clone(&samples),
            );
            scope.spawn(move || {
                barrier.wait();
                for request in 0..REQUESTS {
                    let client = format!("client-{agent}-{request}");
                    for retry in 0..2 {
                        let started = Instant::now();
                        let lock = session_input_lock(&session).unwrap();
                        let _guard = lock.lock().unwrap();
                        let result = submit_v2_with_repository(command(&session, &client), &repository,
                            |_, _| Some(Some("busy-turn".into())),
                            |_, _| panic!("tracked new identity must not read native summary"),
                            |_, _| Ok(json!({"thread":{"turns":[{"id":"busy-turn","status":"inProgress","items":[]}]}})),
                            |command, method, _, disposition| {
                                assert_eq!(method, "turn/steer");
                                assert!(sends.lock().unwrap().insert(command.params.client_user_message_id.clone()), "native duplicate");
                                Ok(UserInputSubmitExecution { join_refusal: None, disposition, app_server_accepted: true,
                                    native_request_id: None, native_method: Some(method.into()),
                                    turn_id: Some("busy-turn".into()), queue: None, queue_paused: false })
                            },
                            |_, _| panic!("busy auto input must not flush"),
                        ).unwrap();
                        assert_eq!(result.disposition, if retry == 0 { UserInputDisposition::Steered } else { UserInputDisposition::Deduplicated });
                        if retry == 0 { samples.lock().unwrap().push(started.elapsed().as_micros()); }
                    }
                    assert!(repository.list(&session).unwrap().1.is_empty());
                }
            });
        }
        barrier.wait();
    });
    assert_eq!(sends.lock().unwrap().len(), AGENTS * REQUESTS);
    assert!(!subscription
        .lagged
        .load(std::sync::atomic::Ordering::SeqCst));
    let observed: Vec<_> = subscription.receiver.try_iter().collect();
    assert_eq!(observed.len(), AGENTS * DELTAS);
    assert!(observed
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
    for agent in 0..AGENTS {
        let session = format!("cutex.load-{run}-{agent}");
        assert_eq!(
            observed
                .iter()
                .filter(|event| event.cutex_session_id == session)
                .count(),
            DELTAS
        );
    }
    let mut samples = samples.lock().unwrap();
    samples.sort_unstable();
    eprintln!(
        "cutex-control-load {}",
        json!({"schema":1,"kind":"native","operation":"submit+event_append","agents":AGENTS,"requests":samples.len(),
        "nativeEvents":observed.len(),"p50Us":percentile(&samples,50),"p95Us":percentile(&samples,95),
        "maxUs":samples.last().unwrap()})
    );
    drop(subscription);
    drop(repository);
    drop(events);
    std::fs::remove_dir_all(root).unwrap();
}


/// Arrival schedulers never await a delta commit or HTTP response. Each stream
/// has a fixed finite unbounded queue: overload is measured as scheduling/queue
/// delay, never hidden by reducing offered input. HTTP starts use independent
/// workers at predetermined deadlines, including coordinated-omission latency.
#[test]
#[ignore = "isolated matched open-loop HTTP attribution probe"]
fn matched_open_loop_http_submit_stage_attribution() {
    const AGENTS:usize=10;
    const DELTAS:usize=200;
    const REQUESTS:usize=10;
    const DELTA_MS:u64=20;
    const REQUEST_MS:u64=400;
    let home=crate::cli_app::test_home::IsolatedTestHome::new_private("open-loop-http").unwrap();
    let mut store=cutex::session::model::CutexSessionStore::default();
    let sessions:Vec<_>=(0..AGENTS).map(|i|format!("cutex.openloop-{i}")).collect();
    for session in &sessions {
        let mut record=cutex::session::model::CutexSessionRecord::new(session.clone(),Some(session.trim_start_matches("cutex.").into()),current_host_name(),home.root().display().to_string(),None).unwrap();
        record.registration_class=cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        record.runtime_generation=1;record.exposed_to_backend=false;
        store.sessions.insert(session.clone(),record);
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    let events=management_v2_repository().unwrap();events.materialize_recovery_reset(&[],true).unwrap();
    assert_eq!(http_exchange(&sessions[0],json!({"requestId":"warm","method":"cutex/session/get","params":{}})).0,200);
    HTTP_HANDLER_SAMPLES.get_or_init(Default::default).lock().unwrap().clear();
    let before=cutex::management::v2::control_timing::snapshot();
    let start=Instant::now()+Duration::from_millis(200);
    let samples=Mutex::new(Vec::<(u128,u128,u128,String)>::new());
    let controls=Mutex::new(Vec::<(u128,u128,u128)>::new());
    let deltas=Mutex::new(Vec::<(u128,u128,u128)>::new());
    fn wait_due(due:Instant) {if let Some(left)=due.checked_duration_since(Instant::now()){std::thread::sleep(left);}}
    std::thread::scope(|scope| {
        for (i,session) in sessions.iter().enumerate() {
            let (tx,rx)=std::sync::mpsc::channel();
            scope.spawn(move || {
                for n in 0..DELTAS {
                    let due=start+Duration::from_millis(n as u64*DELTA_MS+i as u64);
                    wait_due(due);tx.send((n,due,Instant::now())).unwrap();
                }
            });
            let deltas=&deltas;
            scope.spawn(move || {
                for (n,due,offered) in rx {
                    let entered=Instant::now();append_http_delta(events,session,n);
                    deltas.lock().unwrap().push((offered.duration_since(due).as_micros(),entered.duration_since(offered).as_micros(),entered.elapsed().as_micros()));
                }
            });
            for n in 0..REQUESTS {
                let samples=&samples;
                scope.spawn(move || {
                    let due=start+Duration::from_millis(100+n as u64*REQUEST_MS+i as u64*10);
                    wait_due(due);let entered=Instant::now();
                    let command=command(session,&format!("{session}-{n}"));
                    let mut body=json!({"requestId":command.management_request_id,"method":"cutex/userInput/submit","params":{"clientUserMessageId":command.params.client_user_message_id,"origin":command.params.origin,"strategy":"auto","input":command.params.input}});
                    let response=http_exchange(session,body.clone());
                    assert_eq!(response.0,200,"{}",response.1);
                    samples.lock().unwrap().push((entered.duration_since(due).as_micros(),entered.elapsed().as_micros(),due.elapsed().as_micros(),command.management_request_id.clone()));
                    // Preserve the original closed-loop fixture's complete
                    // 100-new + 100-receipt-replay + 100-identity-retry mix.
                    // Each independent logical chain arrives on schedule;
                    // dependent retries follow its response, never add sends.
                    assert_eq!(http_exchange(session,body.clone()),response);
                    body["requestId"]=json!(format!("retry-{session}-{n}"));
                    let replay=http_exchange(session,body);
                    assert_eq!(replay.0,200,"{}",replay.1);
                    assert_eq!(replay.1.pointer("/cutex/result/disposition").and_then(Value::as_str),Some("deduplicated"));
                });
                let controls=&controls;
                scope.spawn(move || {
                    let due=start+Duration::from_millis(250+n as u64*REQUEST_MS+i as u64*10);
                    wait_due(due);let entered=Instant::now();
                    let response=http_exchange(session,json!({"requestId":format!("control-{session}-{n}"),"method":"cutex/userInput/queue/list","params":{}}));
                    assert_eq!(response.0,200,"{}",response.1);
                    controls.lock().unwrap().push((entered.duration_since(due).as_micros(),entered.elapsed().as_micros(),due.elapsed().as_micros()));
                });
            }
        }
    });
    let after=cutex::management::v2::control_timing::snapshot();
    let samples=samples.lock().unwrap();let controls=controls.lock().unwrap();let deltas=deltas.lock().unwrap();
    assert_eq!(samples.len(),AGENTS*REQUESTS);assert_eq!(controls.len(),AGENTS*REQUESTS);assert_eq!(deltas.len(),AGENTS*DELTAS);
    assert_eq!(HTTP_SENDS.get().unwrap().lock().unwrap().len(),AGENTS*REQUESTS);
    fn distribution(values:impl Iterator<Item=u128>)->Value {
        let mut values:Vec<_>=values.collect();values.sort_unstable();
        json!({"samples":values.len(),"p50Us":percentile(&values,50),"p95Us":percentile(&values,95),"maxUs":values.last(),"sumUs":values.iter().sum::<u128>()})
    }
    let handlers=HTTP_HANDLER_SAMPLES.get().unwrap().lock().unwrap();
    let paired_handler_us:Vec<_>=samples.iter().map(|sample|handlers.iter().find(|(id,_)|id==&sample.3).unwrap().1).collect();
    let paired_transport_us:Vec<_>=samples.iter().zip(&paired_handler_us).map(|(sample,handler)|sample.1.saturating_sub(*handler)).collect();
    eprintln!("cutex-control-load {}",json!({"schema":2,"kind":"http_open_loop","operation":"user_input_submit+queue_list","agents":AGENTS,"requests":samples.len(),"nativeEvents":deltas.len(),"deltaPeriodMs":DELTA_MS,"deltaOfferedPerSecond":AGENTS*1000/DELTA_MS as usize,"requestPeriodPerSessionMs":REQUEST_MS,"fixedArrivalSchedule":true,"receiptReplays":100,"identityRetries":100,"wireSubmitRequests":300,"listenerAdmissionCovered":false,
        "submitHandlerWall":distribution(paired_handler_us.iter().copied()),"submitOutsideHandler":distribution(paired_transport_us.iter().copied()),"submitService":distribution(samples.iter().map(|v|v.1)),"submitArrivalToResponse":distribution(samples.iter().map(|v|v.2)),"submitSchedulerLateness":distribution(samples.iter().map(|v|v.0)),
        "controlService":distribution(controls.iter().map(|v|v.1)),"controlArrivalToResponse":distribution(controls.iter().map(|v|v.2)),"controlSchedulerLateness":distribution(controls.iter().map(|v|v.0)),
        "deltaService":distribution(deltas.iter().map(|v|v.2)),"deltaQueueWait":distribution(deltas.iter().map(|v|v.1)),"deltaSchedulerLateness":distribution(deltas.iter().map(|v|v.0)),
        "timingBefore":before,"timingAfter":after,"liveInputEvents":serde_json::to_value(events.live_delta_stats()).unwrap()["inputDeltas"]}));
}
