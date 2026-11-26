use super::*;

fn row(id: &str, state: &str, turn_id: Option<&str>) -> StoredAgentBusMessage {
    let mut row: StoredAgentBusMessage = serde_json::from_value(json!({
        "ownerCutexSessionId":"recipient", "snapshot": {"messageId":id,
            "fromCutexSessionId":"sender", "toCutexSessionId":"recipient",
            "deliveryMode":"passive", "content":format!("body {id}"), "state":state},
        "queuedAt":"2026-11-16T00:00:00Z", "updatedAt":"2026-11-16T00:00:00Z",
    }))
    .unwrap();
    row.meta = Some(AgentBusMessageMeta {
        context_turn_id: turn_id.map(str::to_string),
        ..Default::default()
    });
    row
}

fn store(rows: Vec<StoredAgentBusMessage>) -> AgentBusMessageStore {
    let mut store = empty_store();
    store.messages = rows
        .into_iter()
        .map(|row| (row.snapshot.message_id.clone(), row))
        .collect();
    store
}

fn request(query: &str) -> SimpleHttpRequest {
    SimpleHttpRequest {
        method: "GET".into(),
        path: format!("/v2/sessions/recipient/bus-messages{query}"),
        headers: HashMap::new(),
        body: vec![],
    }
}

#[test]
fn bounded_bus_keeps_every_nonterminal_and_only_visible_linked_terminal() {
    let mut rows = vec![];
    for state in [
        "pending",
        "inbox_accepted",
        "quarantined",
        "stale_held",
        "new_unknown_state",
    ] {
        for n in 0..50 {
            rows.push(row(&format!("{state}-{n}"), state, Some("ancient")));
        }
    }
    rows.extend([
        row("old-delivered", "delivered", Some("ancient")),
        row("visible", "delivered", Some("visible-turn")),
        row("item-id", "discarded", Some("otherwise-old")),
        row("old-failed", "failed", Some("ancient")),
    ]);
    let store = store(rows);
    let window = Window::from_native(
        &json!({"result":{"thread":{"turns":[{"id":"visible-turn","items":[{"id":"item-id"}]}]}}}),
    );
    let (selected, more) = select(&store, "recipient", Direction::Received, &window);
    assert_eq!(selected.len(), 252);
    let ids: BTreeSet<_> = selected
        .iter()
        .map(|row| row.snapshot.message_id.as_str())
        .collect();
    assert!(ids.contains("visible") && ids.contains("item-id"));
    assert!(!ids.contains("old-delivered") && !ids.contains("old-failed"));
    assert!(more);
    assert!(select(&store, "unrelated", Direction::Received, &window)
        .0
        .is_empty());
}

#[test]
fn bounded_bus_unlinked_fallback_is_exactly_twenty_and_does_not_consume_visible_budget() {
    let mut rows: Vec<_> = (0..41)
        .map(|n| row(&format!("m{n:03}"), "delivered", None))
        .collect();
    rows.push(row("visible", "delivered", Some("turn")));
    rows.push(row("required", "quarantined", None));
    let store = store(rows);
    let window = Window::from_native(&json!({"result":{"thread":{"turns":[{"id":"turn"}]}}}));
    let (selected, more) = select(&store, "recipient", Direction::Received, &window);
    let ids: BTreeSet<_> = selected
        .iter()
        .map(|row| row.snapshot.message_id.as_str())
        .collect();
    assert_eq!(selected.len(), 22);
    assert!(ids.contains("m021") && ids.contains("m040") && !ids.contains("m020"));
    assert!(ids.contains("visible") && ids.contains("required") && more);
    assert_eq!(
        select(&store, "recipient", Direction::Received, &Window::default())
            .0
            .len(),
        21
    );
}

#[test]
fn bounded_bus_sent_uses_sender_window_and_does_not_cap_pending_at_legacy_two_hundred() {
    let mut rows: Vec<_> = (0..250)
        .map(|n| row(&format!("pending-{n}"), "pending", Some("old")))
        .collect();
    let mut visible = row("visible", "delivered", Some("other-recipient-turn"));
    visible.meta.as_mut().unwrap().facts.sender_turn_id = Some("sender-turn".into());
    let mut old = row("old", "delivered", Some("sender-turn"));
    old.meta.as_mut().unwrap().facts.sender_turn_id = Some("old-sender-turn".into());
    rows.extend([visible, old]);
    let store = store(rows);
    let window =
        Window::from_native(&json!({"result":{"thread":{"turns":[{"id":"sender-turn"}]}}}));
    let (selected, more) = select(&store, "sender", Direction::Sent, &window);
    assert_eq!(selected.len(), 251);
    assert!(selected
        .iter()
        .any(|row| row.snapshot.message_id == "visible"));
    assert!(!selected.iter().any(|row| row.snapshot.message_id == "old"));
    assert!(more);
}

#[test]
fn bounded_bus_keeps_outcome_unknown_even_if_raw_delivery_looks_terminal() {
    let mut message = row("uncertain", "delivered", Some("ancient"));
    // The view remains uncertain even when the observations are not eligible
    // for closure; a projection must not turn their absence into success.
    message.observed_settlement = Some(serde_json::from_value(json!({
        "evidenceToken":"evidence", "evidence":{
            "schemaVersion":1,"runtime":"claude","recipientId":"recipient","runtimeGeneration":1,
            "threadId":"thread","messageId":"uncertain","semanticSha256":"a".repeat(64),
            "localSemanticSha256":"a".repeat(64),"attemptId":uuid::Uuid::new_v4().to_string(),
            "processingState":"held","actualTurnId":"ancient","terminalStatus":"interrupted",
            "observedAt":Utc::now(),"observationSha256":"b".repeat(64)
        }, "closure":null
    })).unwrap());
    let store = store(vec![message]);
    let (selected, more) = select(&store, "recipient", Direction::Received, &Window::default());
    assert_eq!(selected.len(), 1);
    assert!(!more);
    assert_eq!(
        row_value(selected[0], Direction::Received).unwrap()["settlement"]["state"],
        "outcome_unknown"
    );
}

#[test]
fn bounded_bus_keyset_pages_are_scoped_stable_and_complete_across_state_changes() {
    let mut store = store(
        (0..123)
            .map(|n| row(&format!("m{n:03}"), "delivered", Some("old")))
            .collect(),
    );
    let mut query = parse_query("recipient", &request("?limit=17")).unwrap();
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    loop {
        let response = page(&store, "recipient", &query).unwrap();
        let list = response["messages"].as_array().unwrap();
        assert!(list.len() <= 17);
        assert!(list
            .windows(2)
            .all(|pair| pair[0]["messageId"].as_str() < pair[1]["messageId"].as_str()));
        for value in list {
            assert!(seen.insert(value["messageId"].as_str().unwrap().to_string()));
        }
        pages += 1;
        let Some(cursor) = response["page"]["nextCursor"].as_str() else {
            break;
        };
        assert_eq!(response["page"]["hasMore"], true);
        assert!(parse_query("other", &request(&format!("?cursor={cursor}"))).is_err());
        assert!(parse_query(
            "recipient",
            &request(&format!("?direction=sent&cursor={cursor}"))
        )
        .is_err());
        query = parse_query("recipient", &request(&format!("?limit=17&cursor={cursor}"))).unwrap();
        // A state mutation must never change its keyset position.
        let row = store.messages.get_mut("m001").unwrap();
        row.snapshot.state = "quarantined".into();
        row.updated_at = Utc::now().to_rfc3339();
    }
    assert_eq!(seen.len(), 123);
    assert_eq!(pages, 8);
}

#[test]
fn bounded_bus_query_opt_in_validation_and_admission_classification() {
    assert!(!opted_in(&request("")).unwrap());
    assert!(!opted_in(&request("?bus=full")).unwrap());
    assert!(opted_in(&request("?bus=bounded")).unwrap());
    for query in ["?bus=unknown", "?bus=bounded&bus=full", "?bus="] {
        assert!(opted_in(&request(query)).is_err());
    }
    for query in [
        "?limit=0",
        "?limit=201",
        "?limit=-1",
        "?direction=all",
        "?cursor=",
        "?cursor=invalid",
        "?limit=1&limit=2",
    ] {
        assert!(
            parse_query("recipient", &request(query)).is_err(),
            "{query}"
        );
    }
    let bad = Cursor {
        version: 2,
        session: "recipient".into(),
        direction: Direction::Received,
        before: Key(0, "m".into()),
        filter: None,
    };
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&bad).unwrap());
    assert!(parse_query("recipient", &request(&format!("?cursor={encoded}"))).is_err());
    assert_eq!(
        parse_path("/v2/sessions/cutex.a/bus-messages"),
        Some("cutex.a".into())
    );
    assert!(parse_path("/v2/sessions/a/b/bus-messages").is_none());
    assert_eq!(
        crate::management::v2::control_capacity::classify(&request("")),
        crate::management::v2::control_capacity::WorkClass::HeavyRead
    );
    assert_eq!(
        crate::management::v2::access::classify("GET", "/v2/sessions/recipient/bus-messages"),
        Some(crate::management::v2::access::Class::Scoped(
            crate::management::v2::access::Scoped::SessionHistory
        ))
    );
}

#[test]
fn bounded_bus_history_filters_turns_or_items_and_binds_cursor_to_the_exact_sets() {
    let mut rows: Vec<_> = (0..30)
        .map(|n| row(&format!("visible-{n:03}"), "delivered", Some("visible")))
        .collect();
    rows.extend((0..1000).map(|n| row(&format!("irrelevant-{n:04}"), "delivered", Some("other"))));
    rows.push(row("item-match", "delivered", Some("unrelated-turn")));
    let mut sent = row("sender-only", "delivered", Some("other-recipient-turn"));
    sent.meta.as_mut().unwrap().facts.sender_turn_id = Some("visible".into());
    rows.push(sent);
    let store = store(rows);
    let query = parse_query(
        "recipient",
        &request("?turnIds=visible&itemIds=item-match&limit=17"),
    )
    .unwrap();
    let first = page(&store, "recipient", &query).unwrap();
    assert_eq!(first["messages"].as_array().unwrap().len(), 17);
    let cursor = first["page"]["nextCursor"].as_str().unwrap();
    for wrong in ["", "&turnIds=other&itemIds=item-match", "&turnIds=visible"] {
        assert!(parse_query("recipient", &request(&format!("?cursor={cursor}{wrong}"))).is_err());
    }
    let next = parse_query(
        "recipient",
        &request(&format!(
            "?turnIds=visible&itemIds=item-match&limit=17&cursor={cursor}"
        )),
    )
    .unwrap();
    let second = page(&store, "recipient", &next).unwrap();
    assert_eq!(second["messages"].as_array().unwrap().len(), 14);
    assert_eq!(second["page"]["hasMore"], false);
    assert!(second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["messageId"] == "item-match"));
    let query = parse_query("sender", &request("?direction=sent&turnIds=visible")).unwrap();
    let sent = page(&store, "sender", &query).unwrap();
    assert_eq!(sent["messages"].as_array().unwrap().len(), 1);
    assert_eq!(sent["messages"][0]["messageId"], "sender-only");
    for query in [
        "?turnIds=",
        "?itemIds=",
        "?turnIds=a,,b",
        "?itemIds=a&itemIds=b",
    ] {
        assert!(
            parse_query("recipient", &request(query)).is_err(),
            "{query}"
        );
    }
    let too_many = vec!["a"; 201].join(",");
    assert!(parse_query("recipient", &request(&format!("?turnIds={too_many}"))).is_err());
}

#[test]
fn bounded_bus_legacy_wire_is_unchanged_and_pages_recover_omitted_rows() {
    let root = TestRoot::new();
    let store = store(
        (0..50)
            .map(|n| row(&format!("m{n:03}"), "delivered", None))
            .collect(),
    );
    fs::write(
        root.path().join(AGENT_BUS_STATE_FILE),
        serde_json::to_vec(&store).unwrap(),
    )
    .unwrap();
    let repo = AgentBusMessageRepository::open_read_only(root.path());
    let before = fs::read(root.path().join(AGENT_BUS_STATE_FILE)).unwrap();
    let full = repo.snapshot("recipient").unwrap();
    assert_eq!(full.len(), 50);
    let projected = repo
        .bounded_bootstrap("recipient", &Window::default())
        .unwrap();
    assert_eq!(projected["agentBusMessages"].as_array().unwrap().len(), 20);
    assert_eq!(projected["busMessagesPage"]["received"]["hasMore"], true);
    for selected in projected["agentBusMessages"].as_array().unwrap() {
        assert_eq!(
            Some(selected),
            full.iter()
                .find(|row| row["messageId"] == selected["messageId"])
        );
    }
    let sent = repo.sent_snapshot("sender", 200).unwrap();
    let projected_sent = repo
        .bounded_bootstrap("sender", &Window::default())
        .unwrap();
    for selected in projected_sent["agentBusSentMessages"].as_array().unwrap() {
        assert_eq!(
            Some(selected),
            sent.iter()
                .find(|row| row["messageId"] == selected["messageId"])
        );
    }
    let page = repo
        .history_page(
            "recipient",
            &parse_query("recipient", &request("?limit=200")).unwrap(),
        )
        .unwrap();
    assert_eq!(page["messages"], json!(full));
    assert_eq!(
        fs::read(root.path().join(AGENT_BUS_STATE_FILE)).unwrap(),
        before
    );
    assert_eq!(
        fs::read_dir(root.path()).unwrap().count(),
        1,
        "read-only operations create no lock/database"
    );
}

#[test]
fn indexed_history_matches_legacy_projection_and_skips_other_sessions() {
    let root = TestRoot::new();
    let mut selected = vec![
        row("received-old", "delivered", Some("old")),
        row("received-visible", "delivered", Some("visible")),
        row("received-pending", "pending", Some("old")),
    ];
    // Historical rows may carry a distinct routing recipient. The received
    // history predicate has always used the durable owner.
    selected[1].snapshot.to_cutex_session_id = "renamed-recipient".into();
    let mut sent = row("sent-visible", "failed", None);
    sent.owner_cutex_session_id = "elsewhere".into();
    sent.snapshot.from_cutex_session_id = Some("recipient".into());
    sent.meta.as_mut().unwrap().facts.sender_turn_id = Some("visible".into());
    selected.push(sent);
    let mut self_sent = row("self-sent", "delivered", None);
    self_sent.snapshot.from_cutex_session_id = Some("recipient".into());
    selected.push(self_sent);
    for i in 0..200 {
        let mut unrelated = row(&format!("unrelated-{i:03}"), "delivered", Some("visible"));
        unrelated.owner_cutex_session_id = "elsewhere".into();
        selected.push(unrelated);
    }
    let expected = store(selected);
    fs::write(
        root.path().join(AGENT_BUS_STATE_FILE),
        serde_json::to_vec(&expected).unwrap(),
    ).unwrap();
    let repo = AgentBusMessageRepository::open(root.path()).unwrap();
    repo.migrate_legacy_v1().unwrap();

    let window = Window::from_native(&json!({"result":{"thread":{"turns":[{"id":"visible"}]}}}));
    let (answer, work) = crate::store::metrics::measure(|| repo.bounded_bootstrap("recipient", &window));
    assert_eq!(answer.unwrap(), bounded(&expected, "recipient", &window).unwrap());
    assert_eq!(work.rows_visited, 6, "five message rows and the store version");
    assert_eq!(repo.snapshot("recipient").unwrap(), full_received(&expected, "recipient").unwrap());
    assert_eq!(repo.sent_snapshot("recipient", 1).unwrap(), full_sent(&expected, "recipient", 1).unwrap());
    for direction in ["received", "sent"] {
        let path = format!("?direction={direction}&limit=1");
        let query = parse_query("recipient", &request(&path)).unwrap();
        let first = repo.history_page("recipient", &query).unwrap();
        assert_eq!(first, page(&expected, "recipient", &query).unwrap());
        let cursor = first["page"]["nextCursor"].as_str().unwrap();
        let next = parse_query("recipient", &request(&format!("{path}&cursor={cursor}"))).unwrap();
        assert_eq!(
            repo.history_page("recipient", &next).unwrap(),
            page(&expected, "recipient", &next).unwrap(),
        );
    }
}

#[test]
fn bounded_bus_all_optional_fields_match_full_mode_rows_in_both_directions() {
    // A serialization fixture, not a permissible delivery lifecycle: populate
    // every optional snapshot field so no transport detail can silently vanish.
    let mut rich = row("rich", "failed", Some("recipient-turn"));
    rich.snapshot = serde_json::from_value(json!({
        "messageId":"rich","fromCutexSessionId":"sender","toCutexSessionId":"recipient",
        "deliveryMode":"passive","content":"full body","state":"failed","staleReleaseAuthorized":true,
        "semanticSha256":"a".repeat(64),"a2SubmissionId":"submission",
        "a4Receipt":{"schema":"receipt-schema","receiptId":"receipt","threadId":"thread",
            "messageId":"rich","semanticSha256":"a".repeat(64),"responseItemId":"response","turnId":"recipient-turn","rolloutOrdinal":3},
        "localInboxReceipt":{"receiptId":"local","nested":{"body":"keep this"}},
        "externalInput":{"version":1,"ownerId":"recipient","threadId":"thread","runtimeGeneration":7,
            "semanticSha256":"b".repeat(64),"message":{"id":"rich","source":{"kind":"agent","id":"sender"},
                "type":"message","delivery":"passive","text":"external text"}},
        "externalInputReceipt":{"schema":"external-schema","receiptId":"external-receipt","ownerId":"recipient",
            "threadId":"thread","messageId":"rich","semanticSha256":"b".repeat(64),"responseItemId":"response",
            "turnId":"recipient-turn","ordinal":3},
        "externalInputLastObserved":{"messageId":"rich","semanticSha256":"b".repeat(64),"deliveryState":"context_persisted",
            "receipt":{"schema":"external-schema","receiptId":"observed-receipt","ownerId":"recipient",
                "threadId":"thread","messageId":"rich","semanticSha256":"b".repeat(64),"responseItemId":"response",
                "turnId":"recipient-turn","ordinal":3},
            "processing":{"state":"held","attemptId":uuid::Uuid::new_v4().to_string(),"reason":"interrupted"}},
        "externalInputCommitGeneration":7,
        "presentation":{"version":1,"canonicalSha256":"a".repeat(64),
            "presentation":{"id":"presentation","source":{"kind":"service","id":"service"},"title":"title",
                "body":"presentation body","format":"plainText","references":[{"kind":"externalInput","id":"rich"}]},
            "frozen":null,"receipt":null,"commitGeneration":7,"lastError":"presentation error","nextAttemptAt":123},
        "error":{"code":"send_failed","message":"full failure reason","details":{"retryable":false}}
    })).unwrap();
    let meta = rich.meta.as_mut().unwrap();
    meta.sender = Some(AgentParty {
        id: "sender".into(),
        name: Some("Sender".into()),
        runtime: Some("codex".into()),
    });
    meta.recipient = Some(AgentParty {
        id: "recipient".into(),
        name: Some("Recipient".into()),
        runtime: Some("claude".into()),
    });
    meta.delivered_at = Some("2026-11-16T00:01:00Z".into());
    meta.facts = crate::agent_bus::model::AgentSenderFacts {
        narrower_capabilities: true,
        reply_only: true,
        wake_chain: Some(2),
        wake_downgraded: true,
        sender_turn_id: Some("sender-turn".into()),
    };
    rich.observed_settlement = Some(serde_json::from_value(json!({
        "evidenceToken":"evidence-token","evidence":{"schemaVersion":1,"runtime":"claude","recipientId":"recipient",
            "runtimeGeneration":7,"threadId":"thread","messageId":"rich","semanticSha256":"a".repeat(64),
            "localSemanticSha256":"a".repeat(64),"attemptId":uuid::Uuid::new_v4().to_string(),"processingState":"held",
            "actualTurnId":"recipient-turn","terminalStatus":"interrupted","observedAt":Utc::now(),
            "observationSha256":"c".repeat(64)},"closure":null
    })).unwrap());
    let settlement = json!(rich
        .observed_settlement
        .as_ref()
        .unwrap()
        .view(false, Utc::now()));
    let mut expected_received = serde_json::to_value(&rich.snapshot).unwrap();
    for key in [
        "staleReleaseAuthorized",
        "semanticSha256",
        "a2SubmissionId",
        "a4Receipt",
        "localInboxReceipt",
        "externalInput",
        "externalInputReceipt",
        "externalInputLastObserved",
        "externalInputCommitGeneration",
        "presentation",
        "error",
    ] {
        assert!(
            expected_received
                .get(key)
                .is_some_and(|value| !value.is_null()),
            "fixture missing {key}"
        );
    }
    expected_received.as_object_mut().unwrap().extend(json!({
        "body":"full body","deliveryKind":"note",
        "sender":{"id":"sender","name":"Sender","runtime":"codex"},
        "recipient":{"id":"recipient","name":"Recipient","runtime":"claude"},
        "narrowerCapabilities":true,"replyOnly":true,"wakeDowngraded":true,"senderTurnId":"sender-turn",
        "contextTurnId":"recipient-turn","deliveredAt":"2026-11-16T00:01:00Z","queuedAt":"2026-11-16T00:00:00Z",
        "settlement":settlement
    }).as_object().unwrap().clone());
    let expected_sent = json!({"messageId":"rich","toCutexSessionId":"recipient",
        "recipient":{"id":"recipient","name":"Recipient","runtime":"claude"},"deliveryMode":"passive",
        "deliveryKind":"note","body":"full body","state":"failed","sentAt":"2026-11-16T00:00:00Z",
        "contextTurnId":"recipient-turn","deliveredAt":"2026-11-16T00:01:00Z","wakeDowngraded":true,"senderTurnId":"sender-turn",
        "error":{"code":"send_failed","message":"full failure reason","details":{"retryable":false}},"settlement":settlement});
    let store = store(vec![rich]);
    let window = Window::from_native(
        &json!({"result":{"thread":{"turns":[{"id":"recipient-turn"},{"id":"sender-turn"}]}}}),
    );
    for (direction, session, expected) in [
        (Direction::Received, "recipient", expected_received),
        (Direction::Sent, "sender", expected_sent),
    ] {
        // These are the exact aggregation functions used by snapshot/sent_snapshot.
        let full = match direction {
            Direction::Received => full_received(&store, session).unwrap(),
            Direction::Sent => full_sent(&store, session, 200).unwrap(),
        };
        assert_eq!(full, vec![expected], "legacy wire contract, {direction:?}");
        let (bounded, _) = select(&store, session, direction, &window);
        assert_eq!(
            row_value(bounded[0], direction).unwrap(),
            full[0],
            "bounded {direction:?}"
        );
        let direction_name = if direction == Direction::Received {
            "received"
        } else {
            "sent"
        };
        let query =
            parse_query(session, &request(&format!("?direction={direction_name}"))).unwrap();
        assert_eq!(
            page(&store, session, &query).unwrap()["messages"],
            json!(full),
            "paged {direction:?}"
        );
    }
}

struct TestRoot(PathBuf);
impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bus-window-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bounded_bus_scale_fixture_reports_wire_size_and_time() {
    let receipt = "frozen receipt payload ".repeat(300);
    let mut rows: Vec<_> = (0..1605)
        .map(|n| {
            let mut row = row(
                &format!("m{n:04}"),
                if n < 1588 {
                    "delivered"
                } else if n < 1598 {
                    "quarantined"
                } else {
                    "inbox_accepted"
                },
                Some(if n < 1585 { "old" } else { "visible" }),
            );
            row.snapshot.local_inbox_receipt = Some(json!({"frozen":receipt}));
            row
        })
        .collect();
    rows.push(row("system-no-turn", "delivered", None));
    let store = store(rows);
    let window = Window::from_native(&json!({"result":{"thread":{"turns":[{"id":"visible"}]}}}));
    let started = std::time::Instant::now();
    let full: Vec<_> = store
        .messages
        .values()
        .map(|row| row_value(row, Direction::Received).unwrap())
        .collect();
    let full = serde_json::to_vec(&full).unwrap();
    let full_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = std::time::Instant::now();
    let (selected, _) = select(&store, "recipient", Direction::Received, &window);
    let selected: Vec<_> = selected
        .iter()
        .map(|row| row_value(row, Direction::Received).unwrap())
        .collect();
    let bounded = serde_json::to_vec(&selected).unwrap();
    let bounded_ms = started.elapsed().as_secs_f64() * 1000.0;
    let gzip_full = crate::http::server::gzip_bytes(&full).unwrap().len();
    let gzip_bounded = crate::http::server::gzip_bytes(&bounded).unwrap().len();
    eprintln!("BUS_SCALE full={} gzip={} build_ms={full_ms:.2} bounded={} gzip={} build_ms={bounded_ms:.2} rows={}", full.len(), gzip_full, bounded.len(), gzip_bounded, selected.len());
    assert_eq!(selected.len(), 21);
    assert!(bounded.len() * 20 < full.len());
    assert!(gzip_bounded * 10 < gzip_full);
}

/// GET-only live capture supplied by the private test driver. The live daemon
/// never runs this projection; timings are private replay, not production RTT.
#[test]
#[ignore]
fn bounded_bus_live_capture_replays_private_copy_without_touching_live_state() {
    let capture = std::env::var("CUTEX_BUS_SCALE_CAPTURE").expect("private capture path");
    let raw = fs::read(capture).unwrap();
    let response: Value = serde_json::from_slice(&raw).unwrap();
    let session = response["cutexSessionId"].as_str().unwrap();
    let window = Window::from_native(&response["native"]);
    let mut store = empty_store();
    for value in response["cutexState"]["agentBusMessages"]
        .as_array()
        .unwrap()
    {
        let mut record = row(
            value["messageId"].as_str().unwrap(),
            value["state"].as_str().unwrap(),
            value["contextTurnId"].as_str(),
        );
        record.owner_cutex_session_id = session.into();
        record.snapshot = serde_json::from_value(value.clone()).unwrap();
        // The wire carries a view, not the full persisted settlement record.
        // Preserve its mandatory-selection bit in this disposable replay only.
        if value["settlement"]["state"] == "outcome_unknown" {
            record.snapshot.state = "capture_outcome_unknown".into();
        }
        record.queued_at = value["queuedAt"].as_str().map(str::to_string);
        record.meta.as_mut().unwrap().facts.sender_turn_id =
            value["senderTurnId"].as_str().map(str::to_string);
        store
            .messages
            .insert(record.snapshot.message_id.clone(), record);
    }
    for value in response["cutexState"]["agentBusSentMessages"]
        .as_array()
        .unwrap()
    {
        let id = value["messageId"].as_str().unwrap();
        if !store.messages.contains_key(id) {
            let mut record = row(
                id,
                value["state"].as_str().unwrap(),
                value["contextTurnId"].as_str(),
            );
            record.owner_cutex_session_id = value["toCutexSessionId"].as_str().unwrap().into();
            record.snapshot.to_cutex_session_id = record.owner_cutex_session_id.clone();
            record.snapshot.from_cutex_session_id = Some(session.into());
            record.snapshot.content = value["body"].as_str().unwrap_or_default().into();
            if value["settlement"]["state"] == "outcome_unknown" {
                record.snapshot.state = "capture_outcome_unknown".into();
            }
            record.queued_at = value["sentAt"].as_str().map(str::to_string);
            store.messages.insert(id.into(), record);
        }
        store
            .messages
            .get_mut(id)
            .unwrap()
            .meta
            .as_mut()
            .unwrap()
            .facts
            .sender_turn_id = value["senderTurnId"].as_str().map(str::to_string);
    }
    let started = std::time::Instant::now();
    let full = serde_json::to_vec(&response).unwrap();
    let gzip_full = crate::http::server::gzip_bytes(&full).unwrap().len();
    let full_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = std::time::Instant::now();
    let (received, received_more) = select(&store, session, Direction::Received, &window);
    let (sent, sent_more) = select(&store, session, Direction::Sent, &window);
    let received_ids: BTreeSet<_> = received
        .iter()
        .map(|row| row.snapshot.message_id.as_str())
        .collect();
    let sent_ids: BTreeSet<_> = sent
        .iter()
        .map(|row| row.snapshot.message_id.as_str())
        .collect();
    let mut projected = response.clone();
    projected["cutexState"]["agentBusMessages"]
        .as_array_mut()
        .unwrap()
        .retain(|value| received_ids.contains(value["messageId"].as_str().unwrap()));
    projected["cutexState"]["agentBusSentMessages"]
        .as_array_mut()
        .unwrap()
        .retain(|value| sent_ids.contains(value["messageId"].as_str().unwrap()));
    projected["cutexState"]["busMessagesPage"] =
        json!({"received":{"hasMore":received_more},"sent":{"hasMore":sent_more}});
    let bounded = serde_json::to_vec(&projected).unwrap();
    let gzip_bounded = crate::http::server::gzip_bytes(&bounded).unwrap().len();
    let bounded_ms = started.elapsed().as_secs_f64() * 1000.0;
    let uncertain: Vec<_> = store
        .messages
        .values()
        .filter(|row| belongs(row, session, Direction::Received) && required(row))
        .collect();
    assert!(uncertain
        .iter()
        .all(|row| received_ids.contains(row.snapshot.message_id.as_str())));
    eprintln!("BUS_LIVE_CAPTURE full={} gzip={} encode_ms={full_ms:.2} bounded={} gzip={} select_clone_encode_ms={bounded_ms:.2} received_old={} received_new={} sent_old={} sent_new={} nonterminal_kept={}",
        full.len(), gzip_full, bounded.len(), gzip_bounded, response["cutexState"]["agentBusMessages"].as_array().unwrap().len(), received_ids.len(),
        response["cutexState"]["agentBusSentMessages"].as_array().unwrap().len(), projected["cutexState"]["agentBusSentMessages"].as_array().unwrap().len(), uncertain.len());
    assert!(bounded.len() < full.len());
}
