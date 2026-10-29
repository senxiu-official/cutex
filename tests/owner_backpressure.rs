//! Private transport + actual managed event worker/journal. Demonstrates the
//! bounded backpressure contract of a slow owner sink without dropping events.
use cutex::app_server::client::{AppServerClientError, AppServerEndpoint, AppServerEvent};
use cutex::app_server::manager::{AppServerRuntimeEventContext, AppServerRuntimeManager};
use serde_json::{json, Value};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

fn read_json(ws: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = ws.read().unwrap() {
            return serde_json::from_str(text.as_ref()).unwrap();
        }
    }
}
fn write_json(ws: &mut WebSocket<TcpStream>, value: Value) {
    ws.send(Message::Text(value.to_string().into())).unwrap();
}

#[test]
fn slow_owner_sink_backpressures_rpc_then_recovers_without_event_loss() {
    let private = std::path::PathBuf::from(
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private home"),
    );
    assert!(private.join(".cutex-test-private-home").is_file());
    let root = private.join("slow-owner");
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(![24260, 24270].contains(&addr.port()));
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    let observed = Arc::new(Mutex::new(Vec::<u64>::new()));
    let sink_observed = observed.clone();
    let manager = AppServerRuntimeManager::new(Arc::new(
        move |_: &AppServerRuntimeEventContext, event: &AppServerEvent| {
            if let AppServerEvent::Notification(n) = event {
                if n.method == "test/owner" {
                    let sequence = n.params.as_ref().unwrap()["sequence"].as_u64().unwrap();
                    if sequence == 0 {
                        entered_tx.send(()).unwrap();
                        release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap();
                    }
                    sink_observed.lock().unwrap().push(sequence);
                }
            }
            Ok(())
        },
    ));
    let peer = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        let init = read_json(&mut ws);
        write_json(&mut ws, json!({"id":init["id"],"result":{}}));
        assert_eq!(read_json(&mut ws)["method"], "initialized");
        let resume = read_json(&mut ws);
        write_json(
            &mut ws,
            json!({"id":resume["id"],"result":{"thread":{"id":"owner-thread","status":{"type":"idle"}}}}),
        );
        write_json(
            &mut ws,
            json!({"method":"test/owner","params":{"sequence":0}}),
        );
        let request = read_json(&mut ws);
        assert_eq!(request["method"], "test/blocked");
        for sequence in 1..=1024 {
            write_json(
                &mut ws,
                json!({"method":"test/owner","params":{"sequence":sequence}}),
            );
        }
        write_json(&mut ws, json!({"id":request["id"],"result":"late"}));
        let recovered = read_json(&mut ws);
        assert_eq!(recovered["method"], "test/recovered");
        write_json(&mut ws, json!({"id":recovered["id"],"result":"ok"}));
        match ws.read() {
            Err(tungstenite::Error::Io(e)) => assert!(!matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )),
            _ => {}
        }
    });
    let mut record = cutex::session::model::CutexSessionRecord::new(
        "cutex.owner-sink-repro".into(),
        Some("owner-thread".into()),
        cutex::platform::host::current_host_name(),
        root.display().to_string(),
        None,
    )
    .unwrap();
    record.runtime_generation = 1;
    manager
        .connect_adapter(cutex::runtime::claude::ClaudeObservation {
            record,
            endpoint: AppServerEndpoint::LoopbackWebSocket {
                url: format!("ws://{addr}/"),
                bearer_token: None,
            },
            resume: cutex::app_server::commands::ThreadResumeParams {
                thread_id: "owner-thread".into(),
                ..Default::default()
            },
            schema: Default::default(),
            diagnostic_path: root.join("events.jsonl"),
        })
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let handle = manager.handle("cutex.owner-sink-repro").unwrap();
    let result = handle.request_with_timeout("test/blocked", json!({}), Duration::from_millis(300));
    let during = cutex::management::v2::control_timing::snapshot();
    release_tx.send(()).unwrap();
    assert!(matches!(result, Err(AppServerClientError::Timeout { .. })));
    assert!(
        during["paths"]["native.event_send"]["inFlight"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while observed.lock().unwrap().len() != 1025 {
        assert!(Instant::now() < deadline, "sink failed to recover");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*observed.lock().unwrap(), (0..=1024).collect::<Vec<u64>>());
    assert_eq!(
        handle
            .request_with_timeout("test/recovered", json!({}), Duration::from_secs(5))
            .unwrap(),
        "ok"
    );
    manager.disconnect("cutex.owner-sink-repro").unwrap();
    peer.join().unwrap();
    println!("slow sink: native send inFlight={}, RPC timed out; released sink: all 1025 events in order and later RPC succeeds", during["paths"]["native.event_send"]["inFlight"]);
}
