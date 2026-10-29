// Exercises the real management callback and durable queue against a private
// managed transport. This is an embedded integration test, not a live daemon.
fn callback_flush_probe(inline_baseline: bool) {
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
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
    let session = format!("cutex.{}", uuid::Uuid::new_v4());
    let private = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap());
    let mut record = cutex::session::model::CutexSessionRecord::new(
        session.clone(),
        Some("thread-q".into()),
        current_host_name(),
        private.display().to_string(),
        None,
    )
    .unwrap();
    record.runtime_generation = 1;
    let mut sessions = cutex::session::model::CutexSessionStore::default();
    sessions.sessions.insert(session.clone(), record.clone());
    cutex::session::store::save_cutex_session_store(&sessions).unwrap();
    let repository = user_input_repository().unwrap();
    let command = queue_command(&session, "slow-history");
    repository
        .enqueue(
            &session,
            "thread-q",
            &command.management_request_id,
            &command.params,
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let peer = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        let initialize = read_json(&mut ws);
        assert_eq!(initialize["method"], "initialize");
        write_json(&mut ws, json!({"id":initialize["id"],"result":{}}));
        assert_eq!(read_json(&mut ws)["method"], "initialized");
        let resume = read_json(&mut ws);
        assert_eq!(resume["method"], "thread/resume");
        write_json(
            &mut ws,
            json!({"id":resume["id"],"result":{"thread":{"id":"thread-q","status":{"type":"idle"},"turns":[]}}}),
        );
        let history = read_json(&mut ws);
        assert_eq!(history["method"], "thread/read");
        started_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // Became busy before flush: preserve the queue and never submit twice.
        write_json(
            &mut ws,
            json!({"id":history["id"],"result":{"thread":{"id":"thread-q","turns":[{"id":"other-turn","status":"inProgress","items":[]}]}}}),
        );
        let _ = ws.read();
    });
    let manager = app_server_runtime::runtime_manager();
    manager
        .connect_adapter(cutex::runtime::claude::ClaudeObservation {
            record,
            endpoint: cutex::app_server::client::AppServerEndpoint::LoopbackWebSocket {
                url: format!("ws://{addr}/"),
                bearer_token: None,
            },
            resume: cutex::app_server::commands::ThreadResumeParams {
                thread_id: "thread-q".into(),
                ..Default::default()
            },
            schema: cutex::app_server::journal::AppServerSchemaIdentity::default(),
            diagnostic_path: private.join("callback-events.jsonl"),
        })
        .unwrap();
    let key = session.clone();
    let (returned_tx, returned_rx) = mpsc::sync_channel(1);
    let callback = std::thread::spawn(move || {
        let result = if inline_baseline {
            // r264's exact idle-event continuation after occurrence checks.
            flush_queued_if_idle_for_occurrence(&key, 1, "thread-q").map(|_| ())
        } else {
            let raw = json!({"method":"turn/completed","params":{"threadId":"thread-q","turn":{"id":"prior-turn","status":"completed"}}});
            let event =
                AppServerEvent::Notification(cutex::app_server::protocol::RpcNotification {
                    method: "turn/completed".into(),
                    params: raw.get("params").cloned(),
                    raw,
                });
            handle_runtime_event_for_generation(&key, 1, &event)
        };
        returned_tx.send(result).unwrap();
    });
    let history_started = started_rx.recv_timeout(Duration::from_secs(5));
    let early = returned_rx.recv_timeout(Duration::from_millis(300));
    // Always release the native endpoint before assertions / teardown.
    release_tx.send(()).unwrap();
    history_started.expect("real deferred flush reached history RPC");
    let returned_early = early.is_ok();
    match early {
        Ok(result) => result.unwrap(),
        Err(_) => returned_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap(),
    }
    callback.join().unwrap();
    // The continuation owns this same lane across read/submit settlement.
    {
        let lane = session_input_lock(&session).unwrap();
        let _settled = lane.reserve().unwrap();
    }
    assert_eq!(repository.list(&session).unwrap().1.len(), 1);
    manager.disconnect(&session).unwrap();
    peer.join().unwrap();
    assert_eq!(
        returned_early, !inline_baseline,
        "event callback must return while native history remains held"
    );
}

#[test]
fn idle_event_flush_returns_before_native_history_reply() {
    if test_runs_in_private_home("idle_event_flush_returns_before_native_history_reply") {
        return;
    }
    callback_flush_probe(false);
}

#[test]
fn inline_idle_flush_reproduces_owner_callback_wait() {
    if test_runs_in_private_home("inline_idle_flush_reproduces_owner_callback_wait") {
        return;
    }
    callback_flush_probe(true);
}
