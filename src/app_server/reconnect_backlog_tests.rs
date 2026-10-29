use super::*;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc;
use tungstenite::{Message, WebSocket};

fn read_json(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = socket.read().unwrap() {
            return serde_json::from_str(text.as_ref()).unwrap();
        }
    }
}

fn write_json(socket: &mut WebSocket<TcpStream>, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .unwrap();
}

/// An old worker can be busy handling an event after its socket has died.
/// Recovery must see the transport failure without waiting for that worker,
/// then resume the same thread/generation on the still-running adapter.
#[test]
fn claude_socket_failure_is_visible_before_backlog_drains_and_reconnects() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let directory = std::env::temp_dir().join(format!("cx-reconnect-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (event_entered_tx, event_entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let (close_tx, close_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for occurrence in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let init = read_json(&mut socket);
            assert_eq!(init["method"], "initialize");
            write_json(
                &mut socket,
                serde_json::json!({"id":init["id"],"result":{}}),
            );
            assert_eq!(read_json(&mut socket)["method"], "initialized");
            let resume = read_json(&mut socket);
            assert_eq!(
                resume["method"], "thread/resume",
                "recovery must never start a model turn"
            );
            assert_eq!(resume["params"]["threadId"], "same-thread");
            write_json(
                &mut socket,
                serde_json::json!({"id":resume["id"],"result":{"thread":{"id":"same-thread"}}}),
            );
            if occurrence == 0 {
                write_json(
                    &mut socket,
                    serde_json::json!({"method":"test/backlog","params":{}}),
                );
                close_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                // Abrupt transport loss, while this same listener remains alive.
                socket.get_ref().shutdown(Shutdown::Both).unwrap();
            } else {
                let request = read_json(&mut socket);
                assert_eq!(request["method"], "test/after-reconnect");
                write_json(
                    &mut socket,
                    serde_json::json!({"id":request["id"],"result":"recovered"}),
                );
                let _ = socket.read();
            }
        }
    });
    let manager = AppServerRuntimeManager::new(Arc::new(
        move |_: &AppServerRuntimeEventContext, event: &AppServerEvent| {
            if matches!(event, AppServerEvent::Notification(n) if n.method == "test/backlog") {
                event_entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
            Ok(())
        },
    ));
    let endpoint = super::super::client::AppServerEndpoint::LoopbackWebSocket {
        url: format!("ws://{address}/"),
        bearer_token: None,
    };
    let connect = || {
        manager.connect_endpoint(
            "cutex.reconnect-test",
            endpoint.clone(),
            directory.join("events.jsonl").display().to_string(),
            AppServerSchemaIdentity {
                version: "test".into(),
                sha256: "test".into(),
            },
            ThreadBootstrap::Resume(ThreadResumeParams {
                thread_id: "same-thread".into(),
                ..Default::default()
            }),
            16,
            "claude_subscription",
            None,
        )
    };
    connect().unwrap();
    event_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let old_handle = manager.handle("cutex.reconnect-test").unwrap();
    close_tx.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let disconnected = loop {
        if !manager
            .status("cutex.reconnect-test")
            .unwrap()
            .unwrap()
            .connected
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let failure = manager.handle("cutex.reconnect-test").err();
    let generation_failure = manager
        .handle_for_generation("cutex.reconnect-test", 16)
        .err();
    let status = manager.status("cutex.reconnect-test").unwrap().unwrap();
    release_tx.send(()).unwrap();
    assert!(
        disconnected,
        "transport health stayed connected while event sink was blocked"
    );
    assert!(failure
        .unwrap()
        .to_string()
        .contains("control connection failed"));
    assert!(generation_failure.is_some());
    assert!(status.last_error.is_some());
    connect().unwrap();
    let status = manager.status("cutex.reconnect-test").unwrap().unwrap();
    assert!(status.connected);
    assert_eq!(status.runtime_generation, 16);
    assert_eq!(status.thread_id, "same-thread");
    let new_handle = manager
        .handle_for_generation("cutex.reconnect-test", 16)
        .unwrap();
    assert!(!old_handle.same_connection(&new_handle));
    assert!(old_handle.disconnect_reason().is_some());
    assert_eq!(
        new_handle
            .request("test/after-reconnect", serde_json::json!({}))
            .unwrap(),
        "recovered"
    );
    manager.disconnect("cutex.reconnect-test").unwrap();
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
