//! Actual repository -> owned asks route -> response projection, private only.
use super::*;
use crate::asks::receipt_uncertainty::{
    read_context,
    read_views::{self},
};
use crate::asks::{model::*, store::tests as fixtures};
use crate::management::v2::{
    agent_bus_state::{AgentBusMessageRepository, AgentBusQueuedMessage},
    hot_store,
};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore};
use std::{path::PathBuf, sync::Arc};

struct Fixture {
    store: AskStore,
    bus: AgentBusMessageRepository,
    root: PathBuf,
    ask_id: String,
    input: read_views::ReceiptReadInput,
    sessions: Arc<CutexSessionStore>,
}
impl Fixture {
    fn new() -> Self {
        let (store, dir) = fixtures::temp_store("receipt-http");
        let root = dir.with_extension("bus");
        let bus = AgentBusMessageRepository::open(&root).unwrap();
        let id = format!("receipt-{}", uuid::Uuid::new_v4().simple());
        let recipient = format!("cutex.{}", uuid::Uuid::new_v4());
        let sender = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut input = read_views::tests::input();
        {
            let envelope = input.snapshot.external_input.as_mut().unwrap();
            envelope.message.id = id.clone();
            envelope.owner_id = recipient.clone();
            envelope.message.source.id = sender.clone();
            envelope.semantic_sha256 = envelope.digest();
            input.snapshot.message_id = id.clone();
            input.snapshot.to_cutex_session_id = recipient.clone();
            input.snapshot.from_cutex_session_id = Some(sender.clone());
            input.snapshot.semantic_sha256 = Some(envelope.semantic_sha256.clone());
            let status = input
                .snapshot
                .external_input_last_observed
                .as_mut()
                .unwrap();
            status.message_id = id.clone();
            status.semantic_sha256 = envelope.semantic_sha256.clone();
            let evidence = input.terminal.as_mut().unwrap();
            evidence.message_id = id.clone();
            evidence.recipient_id = recipient.clone();
            evidence.semantic_sha256 = envelope.semantic_sha256.clone();
            evidence.observed_at = chrono::Utc::now();
            let local = input.snapshot.local_inbox_receipt.as_mut().unwrap();
            local["messageId"] = json!(id);
            local["state"] = json!("queued");
        }
        let message=serde_json::from_value(json!({"id":id,"from":sender,"to":recipient,
            "fromCutexSessionId":sender,"toCutexSessionId":recipient,"content":"synthetic revision",
            "deliveryMode":"after_turn","triggerTurn":true,"createdAtEpochSecs":1,"senderKind":"agent"})).unwrap();
        bus.record_queued_isolated(AgentBusQueuedMessage {
            owner_cutex_session_id: recipient.clone(),
            message_id: id.clone(),
            from_cutex_session_id: Some(sender.clone()),
            to_cutex_session_id: recipient.clone(),
            from_runtime_agent_id: Some("synthetic-sender".into()),
            to_runtime_agent_id: Some("synthetic-recipient".into()),
            delivery_mode: "after_turn".into(),
            content: "synthetic revision".into(),
            queued_at: chrono::Utc::now(),
            canonical_envelope: message,
            semantic_sha256: "c".repeat(64),
        })
        .unwrap();
        let envelope = input.snapshot.external_input.as_ref().unwrap().clone();
        bus.freeze_external_input(&recipient, &id, |_| Ok(envelope.clone()))
            .unwrap();
        bus.record_local_inbox(
            &recipient,
            &id,
            input.snapshot.local_inbox_receipt.as_ref().unwrap(),
        )
        .unwrap();
        bus.observe_external_input(
            &id,
            &envelope,
            input
                .snapshot
                .external_input_last_observed
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        assert!(bus
            .observe_settlement(&id, input.terminal.clone(), chrono::Utc::now())
            .unwrap());
        let mut new = fixtures::new_ask(&format!("ask-{id}"), "synthetic");
        new.target_cutex_session_id = recipient.clone();
        new.requester = Requester::Agent {
            cutex_session_id: sender,
            name: None,
        };
        let ask_id = store
            .with_state(|state| {
                let ask = state.create_ask(new, chrono::Utc::now()).unwrap().0;
                let revision =
                    serde_json::from_value(json!({"id":"v2","text":"synthetic retained revision",
                "messageId":id,"sentAt":"2026-10-27T08:00:00Z","level":"delivered"}))
                    .unwrap();
                state
                    .asks
                    .get_mut(&ask.ask_id)
                    .unwrap()
                    .revisions
                    .push(revision);
                ask.ask_id
            })
            .unwrap();
        let mut record = CutexSessionRecord::new(
            recipient.clone(),
            None,
            "test-host".into(),
            "synthetic".into(),
            None,
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        record.runtime_generation = 9;
        record.current_runtime_agent_id = Some("synthetic-runtime".into());
        record.claude_session=Some(serde_json::from_value(json!({"deployment":{"python":"synthetic",
            "adapter_source":"synthetic","claude_cli":"synthetic","claude_home":"synthetic","tui_binary":"synthetic"},
            "thread_id":"thread","state_dir":"synthetic","model":"synthetic"})).unwrap());
        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert(recipient, record);
        Self {
            store,
            bus,
            root,
            ask_id,
            input,
            sessions: Arc::new(sessions),
        }
    }
    fn unlocked(path: PathBuf) -> bool {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&file).is_ok()
    }
    fn route(&self, route: Route, sessions: anyhow::Result<Arc<CutexSessionStore>>) -> Value {
        let sessions = std::cell::RefCell::new(Some(sessions));
        route_json_with_receipt_loader(&self.store, "/v2/asks", &route, &|asks| {
            assert!(
                Self::unlocked(self.store.dir().join("asks-v1.lock")),
                "route must release asks before bus read"
            );
            read_context::load_with(
                asks,
                |ids| self.bus.receipt_read_inputs(ids),
                || {
                    assert!(Self::unlocked(self.store.dir().join("asks-v1.lock")));
                    assert!(
                        Self::unlocked(self.root.join("agent-bus-message-state.lock")),
                        "fresh observer is read after bus unlock"
                    );
                    sessions.borrow_mut().take().unwrap()
                },
            )
        })
        .unwrap_or_else(|error| panic!("route failed: {}", error.code))
    }
    fn generations(&self) -> Vec<Option<u64>> {
        let db = self.root.join(hot_store::DATABASE_FILE);
        vec![
            hot_store::generation(&self.store.dir().join(hot_store::DATABASE_FILE), "asks")
                .unwrap(),
            hot_store::generation(&db, "agent_bus").unwrap(),
            hot_store::generation(&db, "agent_bus_settlement_evidence").unwrap(),
        ]
    }
}

#[test]
fn receipt_http_collection_and_detail_use_exact_retained_evidence_without_read_writes() {
    let fixture = Fixture::new();
    let before = fixture.generations();
    let state = fixture.store.snapshot().unwrap();
    let original = fixture
        .bus
        .snapshot_by_message_id(&fixture.input.snapshot.message_id)
        .unwrap()
        .unwrap();
    let inputs = fixture
        .bus
        .receipt_read_inputs(&[fixture.input.snapshot.message_id.clone()])
        .unwrap();
    assert_eq!(
        inputs.len(),
        1,
        "canonical reader must return the retained row"
    );
    assert!(inputs.values().next().unwrap().terminal.is_some());
    assert!(inputs.values().all(|input| input.observer.is_none()));
    let list = fixture.route(Route::Collection, Ok(fixture.sessions.clone()));
    let detail = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Ok(fixture.sessions.clone()),
    );
    for row in [&list["asks"][0], &detail["ask"]] {
        assert_eq!(
            row["revisions"][0]["receiptProjection"]["state"],
            "uncertain"
        );
        assert_eq!(row["revisions"][0]["level"], "delivered");
        assert_eq!(
            row["revisions"][0]["receiptProjection"]["submissionGeneration"],
            7
        );
        assert_eq!(
            row["revisions"][0]["receiptProjection"]["observingGeneration"],
            9
        );
    }
    assert_eq!(
        detail["ask"]["revisions"][0]["text"],
        "synthetic retained revision"
    );
    assert_eq!(fixture.generations(), before);
    assert_eq!(
        fixture
            .bus
            .snapshot_by_message_id(&original.message_id)
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        serde_json::to_value(fixture.store.snapshot().unwrap()).unwrap(),
        serde_json::to_value(state).unwrap()
    );
    assert_eq!(
        fixture
            .bus
            .receipt_read_inputs(&[original.message_id.clone()])
            .unwrap()[&original.message_id]
            .terminal,
        inputs[&original.message_id].terminal
    );
}

#[test]
fn receipt_http_requires_independent_current_observer_and_recovers_same_generation_after_failure() {
    let fixture = Fixture::new();
    let before = fixture.generations();
    let mut stale = (*fixture.sessions).clone();
    stale
        .sessions
        .values_mut()
        .next()
        .unwrap()
        .runtime_generation = 8;
    let stale = fixture.route(Route::Ask(fixture.ask_id.clone()), Ok(Arc::new(stale)));
    assert_eq!(
        stale["ask"]["revisions"][0]["receiptProjection"]["state"],
        "unknown"
    );
    let unavailable = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Err(anyhow::anyhow!("synthetic transient identity read failure")),
    );
    assert_eq!(
        unavailable["ask"]["revisions"][0]["receiptProjection"]["state"],
        "unknown"
    );
    let recovered = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Ok(fixture.sessions.clone()),
    );
    assert_eq!(
        recovered["ask"]["revisions"][0]["receiptProjection"]["state"],
        "uncertain"
    );
    assert_eq!(fixture.generations(), before);
}

#[test]
fn receipt_http_rejects_stale_submission_generation_and_preserves_actual_a4_and_ack() {
    let fixture = Fixture::new();
    let id = fixture.input.snapshot.message_id.clone();
    let database = fixture.root.join(hot_store::DATABASE_FILE);
    let rows = hot_store::verified_rows(&database, "agent_bus_settlement_evidence")
        .unwrap()
        .unwrap();
    let key = ("evidence".to_string(), id.clone());
    let original = rows.rows[&key].clone();
    let mut stale: Value = serde_json::from_slice(&original).unwrap();
    stale["record"]["evidence"]["runtimeGeneration"] = json!(6);
    hot_store::commit(
        &database,
        "agent_bus_settlement_evidence",
        rows.generation,
        vec![hot_store::RowChange::Put {
            collection: key.0.clone(),
            key: id.clone(),
            value: serde_json::to_vec(&stale).unwrap(),
        }],
    )
    .unwrap();
    let before = fixture.generations();
    let view = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Ok(fixture.sessions.clone()),
    );
    assert_eq!(
        view["ask"]["revisions"][0]["receiptProjection"]["state"],
        "unknown"
    );
    assert_eq!(fixture.generations(), before);
    let envelope = fixture.input.snapshot.external_input.as_ref().unwrap();
    let mut receipt = crate::app_server::external_input::Receipt {
        schema: "codex.external-input-receipt.v1".into(),
        receipt_id: String::new(),
        owner_id: envelope.owner_id.clone(),
        thread_id: envelope.thread_id.clone(),
        message_id: id.clone(),
        response_item_id: id.clone(),
        semantic_sha256: envelope.semantic_sha256.clone(),
        turn_id: "actual".into(),
        ordinal: 1,
    };
    receipt.receipt_id = receipt.digest_id();
    let mut status = fixture
        .input
        .snapshot
        .external_input_last_observed
        .as_ref()
        .unwrap()
        .clone();
    status.delivery_state = crate::app_server::external_input::DeliveryState::ContextPersisted;
    status.receipt = Some(receipt.clone());
    status.processing = crate::app_server::external_input::ProcessingStatus {
        state: crate::app_server::external_input::ProcessingState::None,
        attempt_id: None,
        reason: None,
    };
    fixture
        .bus
        .observe_external_input(&id, envelope, &status)
        .unwrap();
    let retained_a4_snapshot = fixture.bus.snapshot_by_message_id(&id).unwrap().unwrap();
    let before = fixture.generations();
    let a4 = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Err(anyhow::anyhow!("synthetic offline observer")),
    );
    assert_eq!(
        a4["ask"]["revisions"][0]["receiptProjection"]["state"],
        "inContext"
    );
    assert_eq!(fixture.generations(), before);
    let after = fixture.bus.snapshot_by_message_id(&id).unwrap().unwrap();
    assert_eq!(after, retained_a4_snapshot);
    assert_eq!(
        after.external_input_last_observed.unwrap().receipt,
        Some(receipt)
    );
    assert!(
        after.external_input_receipt.is_none(),
        "view must not synthesize a new stored A4"
    );
    fixture
        .store
        .with_state(|state| {
            crate::asks::revisions::acknowledge(
                state,
                &fixture.input.snapshot.to_cutex_session_id,
                &fixture.ask_id,
                "v2",
                "synthetic ack",
                chrono::Utc::now(),
            )
        })
        .unwrap()
        .unwrap();
    let ack = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Ok(fixture.sessions.clone()),
    );
    assert_eq!(
        ack["ask"]["revisions"][0]["receiptProjection"]["state"],
        "acknowledged"
    );
}

#[test]
fn receipt_http_same_runtime_evidence_still_requires_independent_current_identity() {
    let fixture = Fixture::new();
    let mut evidence = fixture.input.terminal.as_ref().unwrap().clone();
    evidence.observing_generation = None; // frozen/submitting occurrence 7
    evidence.observed_at = chrono::Utc::now();
    fixture
        .bus
        .observe_settlement(
            &evidence.message_id,
            Some(evidence.clone()),
            chrono::Utc::now(),
        )
        .unwrap();
    let before = fixture.generations();
    let mut same = (*fixture.sessions).clone();
    same.sessions
        .values_mut()
        .next()
        .unwrap()
        .runtime_generation = 7;
    let known = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Ok(Arc::new(same.clone())),
    );
    assert_eq!(
        known["ask"]["revisions"][0]["receiptProjection"]["state"],
        "uncertain"
    );
    let unavailable = fixture.route(
        Route::Ask(fixture.ask_id.clone()),
        Err(anyhow::anyhow!("synthetic identity unavailable")),
    );
    assert_eq!(
        unavailable["ask"]["revisions"][0]["receiptProjection"]["state"],
        "unknown"
    );
    same.sessions
        .values_mut()
        .next()
        .unwrap()
        .runtime_generation = 8;
    let stale = fixture.route(Route::Ask(fixture.ask_id.clone()), Ok(Arc::new(same)));
    assert_eq!(
        stale["ask"]["revisions"][0]["receiptProjection"]["state"],
        "unknown"
    );
    assert_eq!(fixture.generations(), before);
}

#[test]
fn receipt_reader_does_not_create_or_migrate_an_absent_bus_store() {
    let root = std::env::temp_dir().join(format!(
        "receipt-read-absent-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let repository = AgentBusMessageRepository::open(&root).unwrap();
    let before = std::fs::read_dir(&root).unwrap().count();
    assert!(repository
        .receipt_read_inputs(&["missing".into()])
        .unwrap()
        .is_empty());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), before);
    assert!(!root.join(hot_store::DATABASE_FILE).exists());
    assert!(!root.join("agent-bus-message-state.lock").exists());
    let legacy = root.join("agent-bus-message-state.json");
    let bytes = b"{\"version\":6,\"messages\":{}}";
    std::fs::write(&legacy, bytes).unwrap();
    assert!(repository
        .receipt_read_inputs(&["missing".into()])
        .unwrap()
        .is_empty());
    assert_eq!(std::fs::read(legacy).unwrap(), bytes);
    assert!(!root.join(hot_store::DATABASE_FILE).exists());
    assert!(!root.join("agent-bus-message-state.lock").exists());
}

#[test]
fn receipt_bus_stuck_actual_list_view_adds_uncertainty_without_live_actions_or_receipt_writes() {
    struct EmptyLive;
    impl super::super::agent_bus_stuck::LiveBus for EmptyLive {
        fn queued(&self) -> anyhow::Result<Vec<crate::agent_bus::queue::QueuedAgentBusMessage>> {
            Ok(vec![])
        }
        fn withdraw(&self, _: &[String], _: &str) -> anyhow::Result<Vec<(String, String)>> {
            panic!("read projection must not withdraw")
        }
    }
    let fixture = Fixture::new();
    let before = fixture.generations();
    let view = super::super::agent_bus_stuck::list_view_with_receipt_loader(
        &fixture.bus,
        &EmptyLive,
        0,
        None,
        10,
        chrono::Utc::now().timestamp() as u64,
        &|_| None,
        &|ids| {
            read_context::load_messages_with(
                ids,
                |ids| fixture.bus.receipt_read_inputs(ids),
                || {
                    assert!(Fixture::unlocked(
                        fixture.root.join("agent-bus-message-state.lock")
                    ));
                    Ok(fixture.sessions.clone())
                },
            )
        },
    )
    .unwrap();
    assert_eq!(
        view["messages"][0]["messageId"],
        fixture.input.snapshot.message_id
    );
    assert_eq!(view["messages"][0]["state"], "inbox_accepted");
    assert_eq!(
        view["messages"][0]["receiptProjection"]["state"],
        "uncertain"
    );
    assert_eq!(
        view["messages"][0]["receiptProjection"]["confirmedLevel"],
        "sent"
    );
    assert_eq!(fixture.generations(), before);
    assert!(fixture
        .bus
        .snapshot_by_message_id(&fixture.input.snapshot.message_id)
        .unwrap()
        .unwrap()
        .external_input_receipt
        .is_none());
}
