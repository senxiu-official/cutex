fn disposal_during_native_rpc_blocks_bus_submit(size_retry: bool) {
    use crate::app_server::client::{AppServerClient, AppServerClientOptions, AppServerEndpoint};
    use crate::management::v2::agent_bus_state::AgentBusQueuedMessage;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut message = message();
    message.id = format!("disposed-bus-{}", uuid::Uuid::new_v4());
    message.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
    message.trigger_turn = false;
    message.content = "Full body retained while the size retry is refused. ".repeat(100);
    let binding = binding();
    let repository = agent_bus_message_repository().unwrap();
    let params = super::super::inter_agent_params(
        &binding.thread_id,
        "receiver",
        &binding.owner_id,
        &message,
    )
    .unwrap();
    repository
        .record_queued(AgentBusQueuedMessage {
            owner_cutex_session_id: binding.owner_id.clone(),
            message_id: message.id.clone(),
            from_cutex_session_id: message.from_cutex_session_id.clone(),
            to_cutex_session_id: binding.owner_id.clone(),
            from_runtime_agent_id: None,
            to_runtime_agent_id: Some(message.to.clone()),
            delivery_mode: "passive".into(),
            content: message.content.clone(),
            queued_at: Utc::now(),
            canonical_envelope: message.clone(),
            semantic_sha256: super::super::inter_agent_semantic_sha256(&params),
        })
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let submits = Arc::new(AtomicUsize::new(0));
    let count = submits.clone();
    let owner = binding.owner_id.clone();
    let input = message.id.clone();
    let peer = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        while let Ok(tungstenite::Message::Text(text)) = ws.read() {
            let request: serde_json::Value = serde_json::from_str(text.as_ref()).unwrap();
            let method = request["method"].as_str().unwrap_or("");
            if request.get("id").is_none() {
                continue;
            }
            let result = match method {
                "initialize" => json!({"externalInputVersion":1,"externalInputVersions":[1,2],
                    "externalInputDeliveries":["after_turn","passive"]}),
                "thread/externalInput/status" => {
                    if !size_retry {
                        crate::management::v2::input_disposal::request(&owner, &input).unwrap();
                    }
                    let p = &request["params"];
                    json!({"version":1,"ownerId":p["ownerId"],"threadId":p["threadId"],"runtimeGeneration":p["runtimeGeneration"],
                        "statuses":[{"messageId":p["messages"][0]["messageId"],"semanticSha256":p["messages"][0]["semanticSha256"],
                            "deliveryState":"unknown","receipt":null,"processing":{"state":"none","attemptId":null,"reason":null}}]})
                }
                "thread/externalInput/submit" => {
                    count.fetch_add(1, Ordering::SeqCst);
                    // Persist disposal after a definite first rejection, before
                    // the production spill/retry callback can send again.
                    crate::management::v2::input_disposal::request(&owner, &input).unwrap();
                    ws.send(tungstenite::Message::Text(
                        json!({"id":request["id"],"error":{
                            "code":-32602,"message":"receiver canonical byte policy exceeded"
                        }})
                        .to_string()
                        .into(),
                    ))
                    .unwrap();
                    continue;
                }
                "thread/read" => {
                    json!({"thread":{"id":"unused","status":{"type":"idle"},"turns":[]}})
                }
                _ => json!({}),
            };
            if ws
                .send(tungstenite::Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .is_err()
            {
                break;
            }
        }
    });
    let client = AppServerClient::connect(AppServerClientOptions::new(
        AppServerEndpoint::LoopbackWebSocket {
            url: format!("ws://{address}/"),
            bearer_token: None,
        },
    ))
    .unwrap();
    let mut connection = Some(ExternalInputClient::disposal_loopback(
        binding.clone(),
        client,
    ));
    let registration = crate::agent_bus::model::AgentBusRegisterRequest {
        id: message.to.clone(),
        name: "receiver".into(),
        base_name: None,
        thread_name: None,
        path_key: None,
        session_id: Some(binding.thread_id.clone()),
        profile: "test".into(),
        cwd: ".".into(),
        pid: std::process::id(),
        host_id: None,
        groups: vec![],
        registration_class: crate::agent_bus::model::AgentRegistrationClass::Persistent,
    };
    let options = AppServerAgentBusBridgeOptions::new(registration, &binding.thread_id)
        .with_cutex_session_id(&binding.owner_id);
    let status = Arc::new(Mutex::new(AppServerAgentBusBridgeStatus {
        runtime_agent_id: message.to.clone(),
        thread_id: binding.thread_id.clone(),
        running: true,
        registered: true,
        pending_ack_count: 0,
        submitted_count: 0,
        acknowledged_count: 0,
        last_poll_at: None,
        last_message_id: None,
        last_submission_id: None,
        last_error: None,
    }));
    struct NoAck;
    impl RuntimeAgentBus for NoAck {
        fn register(
            &self,
            _: &crate::agent_bus::model::AgentBusRegisterRequest,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn unregister(&self, _: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn poll(&self, _: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
            panic!("messages supplied to real dispatcher")
        }
        fn ack(&self, _: &str, _: &[String]) -> anyhow::Result<usize> {
            panic!("disposed input must never be acknowledged as delivered")
        }
    }
    let outcome = deliver(
        &NoAck,
        &options,
        binding.runtime_generation,
        vec![message.clone()],
        &status,
        &mut connection,
    )
    .unwrap();
    drop(connection);
    peer.join().unwrap();
    assert!(outcome.retained_pending);
    assert_eq!(
        submits.load(Ordering::SeqCst),
        usize::from(size_retry),
        "no submit after the durable disposal request"
    );
    let error = status.lock().unwrap().last_error.clone().unwrap();
    assert!(
        error.contains("input disposed by owner"),
        "must stop at the disposal fence, not another fixture error: {error}"
    );
    assert_ne!(
        repository
            .snapshot_by_message_id(&message.id)
            .unwrap()
            .unwrap()
            .state,
        "delivered"
    );
    if size_retry {
        assert!(
            crate::documents::DocumentStore::open_default()
                .unwrap()
                .of_source("message", &message.id)
                .unwrap()
                .len()
                > 0,
            "the actual size-spill branch must run before its retry is fenced"
        );
    }
}

#[test]
fn disposed_during_status_never_reaches_codex_bus_submit() {
    if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::disposed_during_status_never_reaches_codex_bus_submit") { return; }
    disposal_during_native_rpc_blocks_bus_submit(false);
}

#[test]
fn disposed_after_size_refusal_never_reaches_codex_bus_retry_submit() {
    if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::disposed_after_size_refusal_never_reaches_codex_bus_retry_submit") { return; }
    disposal_during_native_rpc_blocks_bus_submit(true);
}
