//! A synthetic loopback native peer: no daemon, model, persisted runtime or token.
use super::*;
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use tungstenite::{Message, WebSocket};

fn read(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        match socket.read().unwrap() {
            Message::Text(text) => return serde_json::from_str(text.as_ref()).unwrap(),
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
