//! A synthetic loopback native peer: no daemon, model, persisted runtime or token.
use super::*;
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use tungstenite::{Message, WebSocket};

fn read(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        match socket.read().unwrap() {
            Message::Text(text) => {
                let request: Value = serde_json::from_str(text.as_ref()).unwrap();
                let result = match request.get("method").and_then(Value::as_str) {
                    Some("claude/tasks/list") => Some(json!({"tasks":[],"completeInventory":false})),
                    Some("thread/backgroundTerminals/list" | "thread/loaded/list") => Some(json!({"data":[],"nextCursor":null})),
                    _ => None,
                };
                if let Some(result) = result { write(socket, json!({"id":request["id"],"result":result})); continue; }
                return request;
            },
            Message::Ping(bytes) => socket.send(Message::Pong(bytes)).unwrap(),
            message => panic!("unexpected fixture message: {message:?}"),
        }
    }
}
fn write(socket: &mut WebSocket<TcpStream>, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .unwrap();
}

struct Sink {
    ledger: Option<Arc<super::super::single_writer::Ledger>>,
    live: Arc<Mutex<Vec<Value>>>,
    durable: Arc<Mutex<Vec<Value>>>,
    done: mpsc::Sender<()>,
    boundaries: Arc<std::sync::atomic::AtomicUsize>,
}

#[test]
fn journal_failure_mid_turn_keeps_two_runtime_transports_connected_and_clears_finished_turns() {
    use crate::management::v2::{
        model::*, repository::EventRepository, runtime_journal::RuntimeJournal,
    };
    struct JournalSink {
        journal: Arc<RuntimeJournal>,
        durable: mpsc::Sender<String>,
    }
    impl AppServerRuntimeEventSink for JournalSink {
        fn handle_event(
            &self,
            context: &AppServerRuntimeEventContext,
            event: &AppServerEvent,
        ) -> anyhow::Result<()> {
            if let AppServerEvent::Notification(n) = event {
                let session = context.cutex_session_id.clone();
                let method = n.method.clone();
                let durable = self.durable.clone();
                self.journal.submit(
                    PendingEvent {
                        cutex_session_id: session.clone(),
                        host_id: "fixture".into(),
                        source: EventSource::Cutex,
                        schema: None,
                        native: None,
                        correlation: EventCorrelation::default(),
                        cutex: Some(CutexMessage {
                            method: "cutex/test/observation".into(),
                            params: json!({"method":n.method}),
                        }),
                    },
                    1024,
                    Box::new(move |envelope| {
                        assert!(envelope.is_some());
                        durable.send(format!("{session}:{method}"))?;
                        Ok(())
                    }),
                )?;
            }
            Ok(())
        }
    }
    let root = std::env::temp_dir().join(format!("cx-journal-live-{}", uuid::Uuid::new_v4()));
    let repository = Arc::new(EventRepository::open(root.join("management"), "fixture").unwrap());
    let blocked = Arc::new(AtomicBool::new(false));
    let repo = repository.clone();
    let fail = blocked.clone();
    let journal = Arc::new(RuntimeJournal::new(
        root.join("management/events.jsonl"),
        move |p| {
            if fail.load(Ordering::SeqCst) {
                return Err(std::io::Error::from_raw_os_error(225).into());
            }
            repo.append(p).map(Some)
        },
    ));
    let (durable_tx, durable_rx) = mpsc::channel();
    let manager = AppServerRuntimeManager::new(Arc::new(JournalSink {
        journal: journal.clone(),
        durable: durable_tx,
    }));
    let mut peers = Vec::new();
    let mut controls = Vec::new();
    for session in ["journal-a", "journal-b"] {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (control_tx, control_rx) = mpsc::channel::<Option<bool>>();
        peers.push(thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let initialize = read(&mut socket);
            write(&mut socket, json!({"id":initialize["id"],"result":{"userAgent":"journal-test"}}));
            assert_eq!(read(&mut socket)["method"], "initialized");
            let resume = read(&mut socket);
            write(&mut socket, json!({"id":resume["id"],"result":{"thread":{"id":"thread-1","status":{"type":"idle"}}}}));
            while let Some(running) = control_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                let method = if running { "turn/started" } else { "turn/completed" };
                write(&mut socket, json!({"method":method,"params":{"threadId":"thread-1","turn":{"id":"t","status":if running {"inProgress"} else {"completed"}}}}));
            }
            let _ = socket.close(None);
        }));
        manager
            .connect_endpoint(
                session,
                AppServerEndpoint::LoopbackWebSocket {
                    url: format!("ws://{address}"),
                    bearer_token: None,
                },
                root.join(format!("{session}.jsonl")).display().to_string(),
                AppServerSchemaIdentity {
                    version: "fixture".into(),
                    sha256: "fixture".into(),
                },
                ThreadBootstrap::Resume(ThreadResumeParams {
                    thread_id: "thread-1".into(),
                    ..Default::default()
                }),
                1,
                "host",
                None,
            )
            .unwrap();
        control_tx.send(Some(true)).unwrap();
        assert_eq!(
            durable_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            format!("{session}:turn/started")
        );
        assert_eq!(
            manager
                .status(session)
                .unwrap()
                .unwrap()
                .active_turn_id
                .as_deref(),
            Some("t")
        );
        controls.push((session, control_tx));
    }
    let checkpoint = repository.checkpoint().unwrap();
    blocked.store(true, Ordering::SeqCst);
    for (_, control) in &controls {
        control.send(Some(false)).unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if journal
            .health()
            .is_some_and(|health| health["bufferedEvents"] == 2)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "both runtimes must keep receiving while journal is blocked"
        );
        thread::sleep(Duration::from_millis(10));
    }
    for (session, _) in &controls {
        let status = manager.status(session).unwrap().unwrap();
        assert!(status.connected);
        assert!(
            status.active_turn_id.is_none(),
            "completion still updates live runtime state"
        );
        assert!(manager.handle_for_generation(session, 1).is_ok());
    }
    assert!(durable_rx.try_recv().is_err());
    assert_eq!(repository.checkpoint().unwrap(), checkpoint);
    blocked.store(false, Ordering::SeqCst);
    journal.tick();
    for _ in 0..2 {
        assert!(durable_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .ends_with(":turn/completed"));
    }
    assert!(journal.health().is_none());
    // A real transport loss while another turn runs must expose unknown, never
    // retain that turn as running. It does not fabricate a completion event.
    for (_, control) in &controls {
        control.send(Some(true)).unwrap();
    }
    for _ in 0..2 {
        durable_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    for (_, control) in &controls {
        control.send(None).unwrap();
    }
    for peer in peers {
        peer.join().unwrap();
    }
    for (session, _) in &controls {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = manager.status(session).unwrap().unwrap();
            if !status.connected {
                assert!(status.active_turn_id.is_none());
                assert_eq!(status.thread_status.unwrap()["type"], "unknown");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        manager.disconnect(session).unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reconnect_rederives_turn_from_resume_snapshot_after_disconnected_unknown() {
    let root = std::env::temp_dir().join(format!("cx-reconnect-turn-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (close_tx, close_rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        for (state, turn) in [
            ("active", "old-turn"),
            ("idle", "old-turn"),
            ("active", "new-turn"),
        ] {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let init = read(&mut socket);
            write(
                &mut socket,
                json!({"id":init["id"],"result":{"userAgent":"resume-fixture"}}),
            );
            assert_eq!(read(&mut socket)["method"], "initialized");
            let resume = read(&mut socket);
            assert_eq!(resume["method"], "thread/resume");
            write(
                &mut socket,
                json!({"id":resume["id"],"result":{"thread":{"id":"thread-1","status":{"type":state}},
                "initialTurnsPage":{"data":[{"id":turn,"status":"inProgress","items":[]}]}}}),
            );
            close_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let _ = socket.close(None);
        }
    });
    let manager = AppServerRuntimeManager::without_event_sink();
    for expected in [Some("old-turn"), None, Some("new-turn")] {
        manager
            .connect_endpoint(
                "resume-test",
                AppServerEndpoint::LoopbackWebSocket {
                    url: format!("ws://{address}"),
                    bearer_token: None,
                },
                root.join("native.jsonl").display().to_string(),
                AppServerSchemaIdentity {
                    version: "fixture".into(),
                    sha256: "fixture".into(),
                },
                ThreadBootstrap::Resume(ThreadResumeParams {
                    thread_id: "thread-1".into(),
                    ..Default::default()
                }),
                1,
                "host",
                None,
            )
            .unwrap();
        let status = manager.status("resume-test").unwrap().unwrap();
        assert!(status.connected);
        assert_eq!(status.active_turn_id.as_deref(), expected);
        close_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = manager.status("resume-test").unwrap().unwrap();
            if !status.connected {
                assert!(status.active_turn_id.is_none());
                assert_eq!(status.thread_status.unwrap()["type"], "unknown");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
    manager.disconnect("resume-test").unwrap();
    peer.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
impl AppServerRuntimeEventSink for Sink {
    fn handle_live_connection_boundary(
        &self,
        _: &AppServerRuntimeEventContext,
    ) -> anyhow::Result<()> {
        self.boundaries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn event_ledger(&self) -> Option<Arc<super::super::single_writer::Ledger>> {
        self.ledger.clone()
    }
    fn handle_live_event_from_connection(
        &self,
        _: &AppServerRuntimeEventContext,
        event: &AppServerEvent,
        _: &AppServerHandle,
    ) -> anyhow::Result<()> {
        if let AppServerEvent::Notification(n) = event {
            self.live.lock().unwrap().push(n.raw.clone());
        }
        Ok(())
    }
    fn handle_event(
        &self,
        _: &AppServerRuntimeEventContext,
        event: &AppServerEvent,
    ) -> anyhow::Result<()> {
        if let AppServerEvent::Notification(n) = event {
            self.durable.lock().unwrap().push(n.raw.clone());
            if n.method == "turn/completed" {
                let _ = self.done.send(());
            }
        }
        Ok(())
    }
    fn handle_unowned_event(
        &self,
        _: &AppServerRuntimeEventContext,
        event: &AppServerEvent,
        _: &AppServerHandle,
    ) -> anyhow::Result<()> {
        if matches!(event, AppServerEvent::Notification(n) if n.method == "turn/completed") {
            let _ = self.done.send(());
        }
        Ok(())
    }
}

#[test]
fn live_native_receive_is_once_per_connection_owned_or_unowned_and_never_diagnostic() {
    use super::super::single_writer::{Ledger, ProcessIdentity, Role};
    for capability in [
        Some(json!(true)),
        Some(json!(false)),
        None,
        Some(json!("true")),
    ] {
        let enabled = capability.as_ref() == Some(&Value::Bool(true));
        for unowned in [false, true] {
            let root = std::env::temp_dir().join(format!("cx-live-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let identity = |pid| ProcessIdentity {
                pid,
                started_at: None,
            };
            let owner = Arc::new(Ledger::new(&root, identity(502), Role::Owner, |_| true));
            let mut owner_connection = owner.open("cutex.live-test", 1);
            owner_connection.activate_at(super::super::single_writer::now_ms() - 10_000);
            let ledger = unowned
                .then(|| Arc::new(Ledger::new(&root, identity(501), Role::Launcher, |_| true)));
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let (go_tx, go_rx) = mpsc::channel();
            let (finish_tx, finish_rx) = mpsc::channel();
            let capability = capability.clone();
            let peer = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                let initialize = read(&mut socket);
                assert_eq!(initialize["method"], "initialize");
                // A truthy value at the wrong nested path must not enable it.
                let mut initialized = json!({"userAgent":"live-fixture","capabilities":{"features":{"fullTextItemTerminal":true}}});
                if let Some(capability) = capability {
                    initialized["fullTextItemTerminal"] = capability;
                }
                write(
                    &mut socket,
                    json!({"id":initialize["id"],"result":initialized}),
                );
                assert_eq!(read(&mut socket)["method"], "initialized");
                let resume = read(&mut socket);
                assert_eq!(resume["method"], "thread/resume");
                write(
                    &mut socket,
                    json!({"id":resume["id"],"result":{"thread":{"id":"thread-1","status":{"type":"active"}}}}),
                );
                // Active with no turn named: the connect asks which turn runs
                // (`recover_active_turn`, r274.2 item 4).
                let turns = read(&mut socket);
                assert_eq!(turns["method"], "thread/turns/list");
                write(
                    &mut socket,
                    json!({"id":turns["id"],"result":{"data":[{"id":"t","status":"inProgress","items":[]}],"nextCursor":null}}),
                );
                go_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                for (method, params) in [
                    (
                        "item/started",
                        json!({"threadId":"thread-1","turnId":"t","item":{"id":"m","type":"agentMessage","text":""}}),
                    ),
                    (
                        "item/agentMessage/delta",
                        json!({"threadId":"thread-1","turnId":"t","itemId":"m","delta":"汉"}),
                    ),
                    (
                        "item/agentMessage/delta",
                        json!({"threadId":"thread-1","turnId":"t","itemId":"m","delta":"🙂"}),
                    ),
                    (
                        "item/reasoning/textDelta",
                        json!({"threadId":"thread-1","turnId":"t","itemId":"r","contentIndex":0,"delta":"reason"}),
                    ),
                    (
                        "item/reasoning/summaryTextDelta",
                        json!({"threadId":"thread-1","turnId":"t","itemId":"r","summaryIndex":0,"delta":"summary"}),
                    ),
                    (
                        "item/completed",
                        json!({"threadId":"thread-1","turnId":"t","item":{"id":"m","type":"agentMessage","text":"汉🙂"}}),
                    ),
                    (
                        "turn/completed",
                        json!({"threadId":"thread-1","turn":{"id":"t","status":"completed"}}),
                    ),
                ] {
                    write(
                        &mut socket,
                        json!({"method":method,"params":params,"emittedAtMs":super::super::single_writer::now_ms()}),
                    );
                }
                let _ = finish_rx.recv_timeout(Duration::from_secs(5));
            });
            let (done_tx, done_rx) = mpsc::channel();
            let live = Arc::new(Mutex::new(vec![]));
            let durable = Arc::new(Mutex::new(vec![]));
            let boundaries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let manager = AppServerRuntimeManager::new(Arc::new(Sink {
                ledger,
                live: live.clone(),
                durable: durable.clone(),
                done: done_tx,
                boundaries: boundaries.clone(),
            }));
            let diagnostic = root.join("native.jsonl");
            manager
                .connect_endpoint(
                    "cutex.live-test",
                    AppServerEndpoint::LoopbackWebSocket {
                        url: format!("ws://{address}"),
                        bearer_token: None,
                    },
                    diagnostic.to_string_lossy().into(),
                    AppServerSchemaIdentity {
                        version: "fixture".into(),
                        sha256: "fixture".into(),
                    },
                    ThreadBootstrap::Resume(ThreadResumeParams {
                        thread_id: "thread-1".into(),
                        ..Default::default()
                    }),
                    1,
                    "host",
                    None,
                )
                .unwrap();
            go_tx.send(()).unwrap();
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(
                boundaries.load(Ordering::SeqCst),
                usize::from(enabled),
                "connection start invalidates prior partial text once"
            );
            let live = live.lock().unwrap();
            let text: String = live
                .iter()
                .filter(|raw| raw["method"] == "item/agentMessage/delta")
                .map(|raw| raw["params"]["delta"].as_str().unwrap())
                .collect();
            if enabled {
                assert_eq!(
                    text, "汉🙂",
                    "live fragments exactly once even when this connection is unowned"
                );
                assert_eq!(
                    live.iter()
                        .filter(|raw| raw["method"] == "item/reasoning/textDelta")
                        .count(),
                    1
                );
                assert_eq!(
                    live.iter()
                        .filter(|raw| raw["method"] == "item/reasoning/summaryTextDelta")
                        .count(),
                    1
                );
            } else {
                assert!(
                    live.is_empty(),
                    "legacy and non-boolean capability must never enter live-only"
                );
            }
            let durable = durable.lock().unwrap();
            if enabled {
                assert!(durable.iter().all(|raw| {
                    !crate::management::v2::native_events::is_live_text_delta_method(
                        raw["method"].as_str().unwrap(),
                    )
                }));
            } else if !unowned {
                let text: String = durable
                    .iter()
                    .filter(|raw| raw["method"] == "item/agentMessage/delta")
                    .map(|raw| raw["params"]["delta"].as_str().unwrap())
                    .collect();
                assert_eq!(
                    text, "汉🙂",
                    "legacy durable deltas are preserved completely"
                );
            }
            if unowned {
                assert!(durable.is_empty());
            } else {
                assert_eq!(
                    durable
                        .iter()
                        .find(|raw| raw["method"] == "item/completed")
                        .unwrap()["params"]["item"]["text"],
                    "汉🙂"
                );
            }
            let diagnostic_text = std::fs::read_to_string(&diagnostic).unwrap_or_default();
            if !unowned {
                assert!(
                    diagnostic_text.contains("item/completed"),
                    "durable finals remain diagnostic"
                );
            }
            for method in [
                "item/agentMessage/delta",
                "item/reasoning/textDelta",
                "item/reasoning/summaryTextDelta",
            ] {
                assert_eq!(
                    diagnostic_text.contains(method),
                    !unowned && !enabled,
                    "diagnostic text gate follows the exact native capability"
                );
            }
            drop(live);
            drop(durable);
            finish_tx.send(()).unwrap();
            peer.join().unwrap();
            manager.disconnect("cutex.live-test").unwrap();
            assert_eq!(
                boundaries.load(Ordering::SeqCst),
                if enabled { 2 } else { 0 },
                "owned/unowned disconnect invalidates unfinished text once"
            );
            drop(owner_connection);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
