#[test]
fn disposed_bus_input_never_reaches_native_or_legacy_claude_submit() {
    use cutex::management::v2::agent_bus_state::{
        agent_bus_message_repository, AgentBusQueuedMessage,
    };
    use serde_json::json;
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("disposed-claude-bus").unwrap();
    let native = crate::cli_app::asks_runtime::disposal_dispatch_tests::NativeProbe::connect(
        home.root(),
        true,
    );
    let owner = &native.record.cutex_session_id;
    for native_ingress in [false, true] {
        for disposed in [true, false] {
            native.submits.store(0, std::sync::atomic::Ordering::SeqCst);
            let message: cutex::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
                "id": format!("disposed-claude-{}", uuid::Uuid::new_v4()), "kind":"message",
                "from":"sender", "to":native.record.current_runtime_agent_id,
                "fromCutexSessionId":"cutex.sender", "toCutexSessionId":owner,
                "content":"do not submit", "deliveryMode":"passive", "triggerTurn":false,
                "createdAtEpochSecs":1, "senderKind":"agent"
            }))
            .unwrap();
            let repository = agent_bus_message_repository().unwrap();
            let params = cutex::app_server::bus_bridge::inter_agent_params(
                "thread-q", "receiver", owner, &message,
            )
            .unwrap();
            repository
                .record_queued(AgentBusQueuedMessage {
                    owner_cutex_session_id: owner.clone(),
                    message_id: message.id.clone(),
                    from_cutex_session_id: message.from_cutex_session_id.clone(),
                    to_cutex_session_id: owner.clone(),
                    from_runtime_agent_id: None,
                    to_runtime_agent_id: native.record.current_runtime_agent_id.clone(),
                    delivery_mode: "passive".into(),
                    content: message.content.clone(),
                    queued_at: chrono::Utc::now(),
                    canonical_envelope: message.clone(),
                    semantic_sha256: cutex::app_server::bus_bridge::inter_agent_semantic_sha256(
                        &params,
                    ),
                })
                .unwrap();
            if disposed {
                cutex::management::v2::input_disposal::request(owner, &message.id).unwrap();
            }
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let polled = message.clone();
            let bus_peer = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = cutex::http::server::read_simple_http_request(&mut stream).unwrap();
                assert_eq!(request.method, "GET");
                cutex::http::server::write_json_response(
                    &mut stream,
                    200,
                    "OK",
                    &json!({"messages":[polled]}),
                )
                .unwrap();
            });
            let bus = cutex::agent_bus::client::AgentBusHttpClient::new(
                format!("http://{address}"),
                None,
            );
            let projections = std::cell::Cell::new(0);
            let result = deliver_messages_with(
                owner,
                &bus,
                &mut HashMap::new(),
                |_| {
                    Some(if native_ingress {
                        json!({"externalInputVersion":1,"externalInputVersions":[1,2],"externalInputDeliveries":["after_turn","passive"],
                "input":{"views":true,"deliveryModes":["passive"]}})
                    } else {
                        json!({})
                    })
                },
                |sessions, thread, id, message, meta, v2| {
                    projections.set(projections.get() + 1);
                    claude_message_params(sessions, thread, id, message, meta, v2)
                },
            );
            bus_peer.join().unwrap();
            if disposed {
                result.unwrap();
                assert_eq!(
                    projections.get(),
                    1,
                    "the real dispatcher processed the polled canonical message"
                );
            } else {
                let error = result.unwrap_err();
                assert!(
                    format!("{error:#}").contains("undisposed control reached native submit"),
                    "the same bus dispatcher must reach submit with disposal absent: {error:#}"
                );
            }
            assert_eq!(
                native.submits.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(!disposed)
            );
            assert_eq!(
                repository
                    .snapshot_by_message_id(&message.id)
                    .unwrap()
                    .unwrap()
                    .state,
                "pending"
            );
        }
    }
}
