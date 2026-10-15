use super::*;

#[test]
fn agent_target_resolves_id_display_name_and_unique_base_name() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "agent-a".to_string(),
            sample_bus_agent(
                "agent-a",
                "aria-it.124f234",
                Some("aria-it"),
                Some("124f234"),
            ),
        );
        state_lock.agents.insert(
            "agent-b".to_string(),
            sample_bus_agent("agent-b", "writer.4455667", Some("writer"), Some("4455667")),
        );
    }

    assert_eq!(resolve_agent_target(&state, "agent-a").unwrap(), "agent-a");
    assert_eq!(
        resolve_agent_target(&state, "aria-it.124f234").unwrap(),
        "agent-a"
    );
    assert_eq!(resolve_agent_target(&state, "writer").unwrap(), "agent-b");
}

#[test]
fn agent_target_rejects_ambiguous_base_name() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "agent-a".to_string(),
            sample_bus_agent(
                "agent-a",
                "aria-it.124f234",
                Some("aria-it"),
                Some("124f234"),
            ),
        );
        state_lock.agents.insert(
            "agent-b".to_string(),
            sample_bus_agent(
                "agent-b",
                "aria-it.7654321",
                Some("aria-it"),
                Some("7654321"),
            ),
        );
    }

    let err = resolve_agent_target(&state, "aria-it").expect_err("base name is ambiguous");
    assert!(err.to_string().contains("ambiguous"));
}

#[test]
fn agent_visibility_follows_group_overlap() {
    let mut state = AgentBusState::default();
    let mut ceo = sample_bus_agent("agent-ceo", "ceo.111", Some("ceo"), Some("111"));
    ceo.groups = vec!["project:alpha".to_string(), "shared".to_string()];
    let mut data = sample_bus_agent("agent-data", "data.222", Some("data"), Some("222"));
    data.groups = vec!["project:alpha".to_string()];
    let mut eval = sample_bus_agent("agent-eval", "eval.333", Some("eval"), Some("333"));
    eval.groups = vec!["project:beta".to_string()];
    state.agents.insert(ceo.id.clone(), ceo);
    state.agents.insert(data.id.clone(), data);
    state.agents.insert(eval.id.clone(), eval);

    let visible = visible_agents_for_request(&state, Some("agent-ceo"), false)
        .into_iter()
        .map(|agent| agent.id)
        .collect::<HashSet<_>>();

    assert!(visible.contains("agent-ceo"));
    assert!(visible.contains("agent-data"));
    assert!(!visible.contains("agent-eval"));
}

#[test]
fn agent_target_resolves_session_id_and_respects_sender_groups() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    {
        let mut leader = sample_bus_agent("leader", "leader.111", Some("leader"), Some("111"));
        leader.groups = vec!["project:alpha".to_string()];
        let mut worker = sample_bus_agent("worker", "worker.222", Some("worker"), Some("222"));
        worker.session_id = Some("019e-worker".to_string());
        worker.groups = vec!["project:alpha".to_string()];
        let mut hidden =
            sample_bus_agent("agent-hidden", "hidden.333", Some("hidden"), Some("333"));
        hidden.groups = vec!["project:beta".to_string()];
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert("leader".to_string(), leader);
        state_lock.agents.insert("worker".to_string(), worker);
        state_lock.agents.insert("agent-hidden".to_string(), hidden);
    }

    assert_eq!(
        resolve_agent_target_for_sender(&state, "019e-worker", Some("leader"), false).unwrap(),
        "worker"
    );
    let err = resolve_agent_target_for_sender(&state, "hidden", Some("leader"), false)
        .expect_err("hidden group should not resolve");
    assert!(err.to_string().contains("No visible"));
    assert_eq!(
        resolve_agent_target_for_sender(&state, "hidden", Some("leader"), true).unwrap(),
        "agent-hidden"
    );
    assert_eq!(
        resolve_agent_target_for_sender(&state, "hidden.333", Some("leader"), true).unwrap(),
        "agent-hidden"
    );
    assert_eq!(
        resolve_agent_target_for_sender(&state, "agent-hidden", Some("leader"), false).unwrap(),
        "agent-hidden"
    );
}

#[test]
fn peer_agent_target_resolution_respects_sender_groups() {
    let mut worker = sample_bus_agent("remote-worker", "worker.222", Some("worker"), Some("222"));
    worker.groups = vec!["waveline".to_string(), "project:alpha".to_string()];
    let mut hidden = sample_bus_agent("remote-hidden", "hidden.333", Some("hidden"), Some("333"));
    hidden.groups = vec!["project:beta".to_string()];
    let agents = vec![worker, hidden];
    let sender_groups = vec!["waveline".to_string()];

    assert_eq!(
        resolve_agent_target_from_agent_list(&agents, "worker", Some(&sender_groups), false)
            .expect("worker should resolve")
            .id,
        "remote-worker"
    );
    assert!(
        resolve_agent_target_from_agent_list(&agents, "hidden", Some(&sender_groups), false)
            .is_err()
    );
    assert_eq!(
        resolve_agent_target_from_agent_list(&agents, "hidden", Some(&sender_groups), true)
            .expect("all groups should resolve hidden")
            .id,
        "remote-hidden"
    );
}

#[test]
fn agent_groups_update_can_bridge_live_agents() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    {
        let mut alpha = sample_bus_agent("agent-alpha", "alpha.111", Some("alpha"), Some("111"));
        alpha.session_id = Some("session-alpha".to_string());
        alpha.groups = vec!["project:alpha".to_string()];
        let mut beta = sample_bus_agent("agent-beta", "beta.222", Some("beta"), Some("222"));
        beta.groups = vec!["project:beta".to_string()];
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert("agent-alpha".to_string(), alpha);
        state_lock.agents.insert("agent-beta".to_string(), beta);
    }

    update_agent_groups(
        &state,
        "session-alpha",
        &["shared".to_string()],
        AgentGroupUpdateMode::Add,
    )
    .expect("group update by session id should succeed");

    let state_lock = state.lock().expect("state lock should not be poisoned");
    assert!(state_lock
        .agents
        .get("agent-alpha")
        .expect("alpha should exist")
        .groups
        .contains(&"shared".to_string()));
}

#[test]
fn agent_sender_label_prefers_current_thread_base_name() {
    let agent = sample_bus_agent(
        "cutex.orchid.scgpt.421708310f",
        "msgbot-1.8de58c1",
        Some("msgbot-1"),
        Some("8de58c1"),
    );

    assert_eq!(agent_sender_label(&agent), "msgbot-1");
}

#[test]
fn user_originated_agent_bus_messages_preserve_native_input_metadata() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));

    enqueue_agent_bus_message_once(
        &state,
        "user",
        "agent-a",
        "agent-a",
        "raw phone text",
        AgentBusEnvelopeKind::Message,
        AgentDeliveryMode::AfterTurn,
        AgentMessageKind::User,
        Some("mobile".to_string()),
        Some(UserSubmitMode::Queue),
        None,
        None,
        None,
        Some("phone-1".to_string()),
        now_epoch_secs(),
    )
    .expect("enqueue should succeed");

    let message = {
        let state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock
            .messages
            .get("agent-a")
            .expect("queue should exist")
            .front()
            .expect("message should exist")
            .clone()
    };
    assert_eq!(message.content, "raw phone text");
    assert_eq!(message.sender_kind, AgentMessageKind::User);
    assert_eq!(message.display_source.as_deref(), Some("mobile"));
    assert_eq!(message.submit_mode, Some(UserSubmitMode::Queue));

    let encoded = serde_json::to_value(&message).expect("message should encode");
    assert_eq!(
        encoded.get("content").and_then(Value::as_str),
        Some("raw phone text")
    );
    assert_eq!(
        encoded.get("senderKind").and_then(Value::as_str),
        Some("user")
    );
    assert_eq!(
        encoded.get("displaySource").and_then(Value::as_str),
        Some("mobile")
    );
    assert_eq!(
        encoded.get("submitMode").and_then(Value::as_str),
        Some("queue")
    );
    assert_eq!(
        encoded.get("externalMessageId").and_then(Value::as_str),
        Some("phone-1")
    );
}

#[test]
fn agent_messages_are_removed_only_after_ack() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    let message = AgentBusMessage {
        id: "message-1".to_string(),
        kind: AgentBusEnvelopeKind::Message,
        from: "agent-b".to_string(),
        to: "agent-a".to_string(),
        from_cutex_session_id: None,
        to_cutex_session_id: None,
        content: "hello".to_string(),
        delivery_mode: AgentDeliveryMode::AfterTurn,
        trigger_turn: true,
        created_at_epoch_secs: now_epoch_secs(),
        sender_kind: AgentMessageKind::Agent,
        display_source: None,
        submit_mode: None,
        control_type: None,
        control_payload: None,
        external_action_id: None,
        external_message_id: None,
    };
    let encoded = serde_json::to_value(&message).expect("message should encode");
    assert_eq!(
        encoded.get("deliveryMode").and_then(Value::as_str),
        Some("after_turn")
    );
    assert!(encoded.get("triggerTurn").is_some());
    assert!(encoded.get("trigger_turn").is_none());
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock
            .messages
            .entry("agent-a".to_string())
            .or_default()
            .push_back(message);
    }

    {
        let state_lock = state.lock().expect("state lock should not be poisoned");
        assert_eq!(
            state_lock
                .messages
                .get("agent-a")
                .expect("queue should exist")
                .len(),
            1
        );
    }

    assert_eq!(
        ack_agent_messages(&state, "agent-a", &["message-1".to_string()])
            .expect("ack should succeed"),
        1
    );
    assert!(state
        .lock()
        .expect("state lock should not be poisoned")
        .messages
        .get("agent-a")
        .is_none());
    assert_eq!(
        ack_agent_messages(&state, "agent-a", &["message-1".to_string()])
            .expect("duplicate ack should succeed"),
        0
    );
}

#[test]
fn agent_prune_uses_heartbeat_not_local_pid() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "remote-agent".to_string(),
            AgentBusAgent {
                id: "remote-agent".to_string(),
                name: "eva-worker.1234567".to_string(),
                base_name: Some("eva-worker".to_string()),
                thread_name: Some("eva-worker".to_string()),
                path_key: Some("1234567".to_string()),
                session_id: None,
                cutex_session_id: None,
                profile: "orchid".to_string(),
                cwd: "E:\\Projects\\agent".to_string(),
                pid: u32::MAX,
                host_id: Some("winbox".to_string()),
                groups: vec!["project:1234567".to_string()],
                registration_class: AgentRegistrationClass::LocalOnly,
                last_seen_epoch_secs: now_epoch_secs(),
            },
        );
        state_lock.agents.insert(
            "stale-agent".to_string(),
            AgentBusAgent {
                id: "stale-agent".to_string(),
                name: "old-worker.7654321".to_string(),
                base_name: Some("old-worker".to_string()),
                thread_name: Some("old-worker".to_string()),
                path_key: Some("7654321".to_string()),
                session_id: None,
                cutex_session_id: None,
                profile: "orchid".to_string(),
                cwd: "/tmp/old".to_string(),
                pid: u32::MAX,
                host_id: None,
                groups: vec!["project:7654321".to_string()],
                registration_class: AgentRegistrationClass::LocalOnly,
                last_seen_epoch_secs: now_epoch_secs().saturating_sub(121),
            },
        );
    }

    prune_stale_agents(&state).expect("prune should succeed");
    let state_lock = state.lock().expect("state lock should not be poisoned");
    assert!(state_lock.agents.contains_key("remote-agent"));
    assert!(!state_lock.agents.contains_key("stale-agent"));
}

#[test]
fn agent_prune_preserves_fresh_dead_same_host_pid() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    let local_host = current_host_name();
    let now = now_epoch_secs();
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "dead-local".to_string(),
            AgentBusAgent {
                id: "dead-local".to_string(),
                name: "dead-local.7654321".to_string(),
                base_name: Some("dead-local".to_string()),
                thread_name: Some("dead-local".to_string()),
                path_key: Some("7654321".to_string()),
                session_id: Some("019e-dead-local".to_string()),
                cutex_session_id: None,
                profile: "orchid".to_string(),
                cwd: "/tmp/dead-local".to_string(),
                pid: u32::MAX,
                host_id: Some(local_host),
                groups: vec!["project:7654321".to_string()],
                registration_class: AgentRegistrationClass::LocalOnly,
                last_seen_epoch_secs: now,
            },
        );
        state_lock
            .messages
            .entry("dead-local".to_string())
            .or_default()
            .push_back(AgentBusMessage {
                id: "message-1".to_string(),
                kind: AgentBusEnvelopeKind::Message,
                from: "sender".to_string(),
                to: "dead-local".to_string(),
                from_cutex_session_id: None,
                to_cutex_session_id: None,
                content: "hello".to_string(),
                delivery_mode: AgentDeliveryMode::AfterTurn,
                trigger_turn: true,
                created_at_epoch_secs: now,
                sender_kind: AgentMessageKind::Agent,
                display_source: None,
                submit_mode: None,
                control_type: None,
                control_payload: None,
                external_action_id: None,
                external_message_id: None,
            });
    }

    prune_stale_agents(&state).expect("prune should succeed");
    let state_lock = state.lock().expect("state lock should not be poisoned");
    assert!(state_lock.agents.contains_key("dead-local"));
    assert!(state_lock.messages.contains_key("dead-local"));
}

#[test]
fn agent_prune_removes_dead_same_host_pid_after_grace() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    let local_host = current_host_name();
    let now = 10_000;
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "dead-local".to_string(),
            AgentBusAgent {
                id: "dead-local".to_string(),
                name: "dead-local.7654321".to_string(),
                base_name: Some("dead-local".to_string()),
                thread_name: Some("dead-local".to_string()),
                path_key: Some("7654321".to_string()),
                session_id: Some("019e-dead-local".to_string()),
                cutex_session_id: None,
                profile: "orchid".to_string(),
                cwd: "/tmp/dead-local".to_string(),
                pid: u32::MAX,
                host_id: Some(local_host.clone()),
                groups: vec!["project:7654321".to_string()],
                registration_class: AgentRegistrationClass::LocalOnly,
                last_seen_epoch_secs: now,
            },
        );
        state_lock
            .messages
            .entry("dead-local".to_string())
            .or_default()
            .push_back(AgentBusMessage {
                id: "message-1".to_string(),
                kind: AgentBusEnvelopeKind::Message,
                from: "sender".to_string(),
                to: "dead-local".to_string(),
                from_cutex_session_id: None,
                to_cutex_session_id: None,
                content: "hello".to_string(),
                delivery_mode: AgentDeliveryMode::AfterTurn,
                trigger_turn: true,
                created_at_epoch_secs: now,
                sender_kind: AgentMessageKind::Agent,
                display_source: None,
                submit_mode: None,
                control_type: None,
                control_payload: None,
                external_action_id: None,
                external_message_id: None,
            });
    }

    prune_stale_agents_with_checker(
        &state,
        now + AGENT_BUS_PID_PRUNE_GRACE_SECS + 1,
        &local_host,
        |_| false,
    )
    .expect("prune should succeed");
    let state_lock = state.lock().expect("state lock should not be poisoned");
    assert!(!state_lock.agents.contains_key("dead-local"));
    assert!(!state_lock.messages.contains_key("dead-local"));
}

#[test]
fn agent_prune_preserves_fresh_missing_host_id_legacy_local() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    let now = now_epoch_secs();
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "legacy-local".to_string(),
            AgentBusAgent {
                id: "legacy-local".to_string(),
                name: "legacy-local.7654321".to_string(),
                base_name: Some("legacy-local".to_string()),
                thread_name: Some("legacy-local".to_string()),
                path_key: Some("7654321".to_string()),
                session_id: Some("019e-legacy-local".to_string()),
                cutex_session_id: None,
                profile: "orchid".to_string(),
                cwd: "/tmp/legacy-local".to_string(),
                pid: u32::MAX,
                host_id: None,
                groups: vec!["project:7654321".to_string()],
                registration_class: AgentRegistrationClass::LocalOnly,
                last_seen_epoch_secs: now,
            },
        );
    }

    prune_stale_agents(&state).expect("prune should succeed");
    let state_lock = state.lock().expect("state lock should not be poisoned");
    assert!(state_lock.agents.contains_key("legacy-local"));
}

#[test]
fn agent_prune_treats_stale_missing_host_id_as_legacy_local() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "legacy-local".to_string(),
            AgentBusAgent {
                id: "legacy-local".to_string(),
                name: "legacy-local.7654321".to_string(),
                base_name: Some("legacy-local".to_string()),
                thread_name: Some("legacy-local".to_string()),
                path_key: Some("7654321".to_string()),
                session_id: Some("019e-legacy-local".to_string()),
                cutex_session_id: None,
                profile: "orchid".to_string(),
                cwd: "/tmp/legacy-local".to_string(),
                pid: u32::MAX,
                host_id: None,
                groups: vec!["project:7654321".to_string()],
                registration_class: AgentRegistrationClass::LocalOnly,
                last_seen_epoch_secs: now_epoch_secs().saturating_sub(121),
            },
        );
    }

    prune_stale_agents(&state).expect("prune should succeed");
    let state_lock = state.lock().expect("state lock should not be poisoned");
    assert!(!state_lock.agents.contains_key("legacy-local"));
}

#[test]
fn agent_bus_registry_rehydrates_agents_after_restart() {
    let _guard = env_lock().lock().expect("env lock should not be poisoned");
    let temp_home = std::env::temp_dir().join(format!("cutex-home-{}", Uuid::new_v4()));
    let old_home = std::env::var_os("HOME");
    fs::create_dir_all(temp_home.join(".cutex").join("runtime"))
        .expect("temp runtime dir should be created");
    unsafe {
        std::env::set_var("HOME", &temp_home);
    }

    let mut agent = sample_bus_agent(
        "agent-alpha",
        "alpha.1234567",
        Some("alpha"),
        Some("1234567"),
    );
    agent.session_id = Some("session-alpha".to_string());
    agent.last_seen_epoch_secs = 1;
    let mut state = AgentBusState::default();
    state.agents.insert(agent.id.clone(), agent);
    save_agent_bus_registry_locked(&state).expect("registry should be saved");

    let loaded = load_agent_bus_state_from_registry().expect("registry should load");
    let loaded_agent = loaded
        .agents
        .get("agent-alpha")
        .expect("agent should be restored");
    assert_eq!(loaded_agent.session_id.as_deref(), Some("session-alpha"));
    assert_eq!(loaded_agent.name, "alpha.1234567");
    assert!(loaded_agent.last_seen_epoch_secs > 1);
    assert!(loaded.messages.is_empty());

    match old_home {
        Some(value) => unsafe { std::env::set_var("HOME", value) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    let _ = fs::remove_dir_all(temp_home);
}

#[test]
fn agent_bus_dedupes_recent_messages_with_the_same_explicit_id() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    let first = enqueue_agent_bus_message_once(
        &state,
        "agent.ceo",
        "agent-worker",
        "worker.1234567",
        "[message from agent.ceo] please report",
        AgentBusEnvelopeKind::Message,
        AgentDeliveryMode::AfterTurn,
        AgentMessageKind::Agent,
        None,
        None,
        None,
        None,
        None,
        Some("ceo-report-1".to_string()),
        1_000,
    )
    .expect("first send should enqueue");
    let duplicate = enqueue_agent_bus_message_once(
        &state,
        "agent.ceo",
        "agent-worker",
        "worker.1234567",
        "[message from agent.ceo] please report",
        AgentBusEnvelopeKind::Message,
        AgentDeliveryMode::AfterTurn,
        AgentMessageKind::Agent,
        None,
        None,
        None,
        None,
        None,
        Some("ceo-report-1".to_string()),
        1_000 + AGENT_BUS_DEDUPE_WINDOW_SECS,
    )
    .expect("duplicate send should be accepted");

    assert!(!first.deduplicated);
    assert!(duplicate.deduplicated);
    assert_eq!(first.record.id, duplicate.record.id);
    assert_eq!(
        state
            .lock()
            .expect("state lock should not be poisoned")
            .messages
            .get("agent-worker")
            .expect("queue should exist")
            .len(),
        1
    );

    let later = enqueue_agent_bus_message_once(
        &state,
        "agent.ceo",
        "agent-worker",
        "worker.1234567",
        "[message from agent.ceo] please report",
        AgentBusEnvelopeKind::Message,
        AgentDeliveryMode::AfterTurn,
        AgentMessageKind::Agent,
        None,
        None,
        None,
        None,
        None,
        Some("ceo-report-1".to_string()),
        1_001 + AGENT_BUS_DEDUPE_WINDOW_SECS,
    )
    .expect("later send should enqueue again");
    assert!(!later.deduplicated);
    assert_ne!(first.record.id, later.record.id);
    assert_eq!(
        state
            .lock()
            .expect("state lock should not be poisoned")
            .messages
            .get("agent-worker")
            .expect("queue should exist")
            .len(),
        2
    );
}

#[test]
fn agent_bus_does_not_dedupe_distinct_external_user_messages() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    let first = enqueue_agent_bus_message_once(
        &state,
        "user",
        "agent-worker",
        "worker.1234567",
        "test",
        AgentBusEnvelopeKind::Message,
        AgentDeliveryMode::AfterTurn,
        AgentMessageKind::User,
        Some("mobile".to_string()),
        Some(UserSubmitMode::NextToolCall),
        None,
        None,
        None,
        Some("local-message-1".to_string()),
        1_000,
    )
    .expect("first user send should enqueue");
    let second = enqueue_agent_bus_message_once(
        &state,
        "user",
        "agent-worker",
        "worker.1234567",
        "test",
        AgentBusEnvelopeKind::Message,
        AgentDeliveryMode::AfterTurn,
        AgentMessageKind::User,
        Some("mobile".to_string()),
        Some(UserSubmitMode::NextToolCall),
        None,
        None,
        None,
        Some("local-message-2".to_string()),
        1_001,
    )
    .expect("second user send should enqueue");

    assert!(!first.deduplicated);
    assert!(!second.deduplicated);
    assert_ne!(first.record.id, second.record.id);
    let messages = state
        .lock()
        .expect("state lock should not be poisoned")
        .messages
        .get("agent-worker")
        .expect("queue should exist")
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0].external_message_id.as_deref(),
        Some("local-message-1")
    );
    assert_eq!(
        messages[1].external_message_id.as_deref(),
        Some("local-message-2")
    );
}

#[test]
fn agent_bus_send_request_accepts_camel_case_trigger_turn_as_legacy_passive() {
    let request: AgentBusSendRequest = serde_json::from_value(serde_json::json!({
        "to": "worker",
        "from": "leader",
        "content": "queue this for later",
        "triggerTurn": false
    }))
    .expect("camelCase triggerTurn should parse");

    assert_eq!(request.to, "worker");
    assert_eq!(request.from.as_deref(), Some("leader"));
    assert_eq!(request.content, "queue this for later");
    assert_eq!(request.resolved_delivery_mode(), AgentDeliveryMode::Passive);
}

#[test]
fn agent_bus_send_request_accepts_delivery_mode() {
    let request: AgentBusSendRequest = serde_json::from_value(serde_json::json!({
        "to": "worker",
        "from": "leader",
        "content": "handle after your current turn",
        "deliveryMode": "after_turn"
    }))
    .expect("deliveryMode should parse");

    assert_eq!(
        request.resolved_delivery_mode(),
        AgentDeliveryMode::AfterTurn
    );
}

#[test]
fn agent_bus_sender_name_uses_from_agent_id_when_from_is_absent() {
    let state = std::sync::Arc::new(Mutex::new(AgentBusState::default()));
    {
        let mut state_lock = state.lock().expect("state lock should not be poisoned");
        state_lock.agents.insert(
            "agent-alpha".to_string(),
            AgentBusAgent {
                id: "agent-alpha".to_string(),
                name: "alpha.1234567".to_string(),
                base_name: Some("alpha".to_string()),
                thread_name: Some("alpha".to_string()),
                path_key: Some("1234567".to_string()),
                session_id: Some("session-alpha".to_string()),
                cutex_session_id: None,
                profile: "orchid".to_string(),
                cwd: "/tmp/alpha".to_string(),
                pid: u32::MAX,
                host_id: None,
                groups: vec!["grp-a".to_string()],
                registration_class: AgentRegistrationClass::LocalOnly,
                last_seen_epoch_secs: now_epoch_secs(),
            },
        );
    }
    let request: AgentBusSendRequest = serde_json::from_value(serde_json::json!({
        "to": "worker",
        "fromAgentId": "agent-alpha",
        "content": "hello"
    }))
    .expect("fromAgentId should parse");

    assert_eq!(
        resolve_agent_message_sender_name(&state, &request),
        "alpha.1234567"
    );
}
