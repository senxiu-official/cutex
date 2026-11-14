//! Real dispatch entrypoints against an owned loopback native endpoint.
use super::*;
use cutex::asks::model::*;
use cutex::session::model::CutexSessionRecord;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub(crate) struct NativeProbe {
    pub record: CutexSessionRecord,
    pub submits: Arc<AtomicUsize>,
    peer: Option<std::thread::JoinHandle<()>>,
    manager: &'static cutex::app_server::manager::AppServerRuntimeManager,
}

impl NativeProbe {
    pub(crate) fn connect(root: &std::path::Path, claude: bool) -> Self {
        let manager = if claude {
            super::super::claude_runtime::manager()
        } else {
            app_server_runtime::runtime_manager()
        };
        let session = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut record = CutexSessionRecord::new(
            session.clone(),
            Some("thread-q".into()),
            "private".into(),
            root.display().to_string(),
            None,
        )
        .unwrap();
        record.runtime_generation = 1;
        record.current_runtime_agent_id = Some(format!("runtime-{session}"));
        let path = cutex::session::store::cutex_sessions_path().unwrap();
        cutex::session::store::with_locked_session_rows(&path, |rows| {
            rows.insert_session(&session, record.clone());
            rows.commit()?;
            Ok(())
        })
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let submits = Arc::new(AtomicUsize::new(0));
        let count = submits.clone();
        let peer = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            while let Ok(frame) = ws.read() {
                let tungstenite::Message::Text(text) = frame else {
                    continue;
                };
                let request: serde_json::Value = serde_json::from_str(text.as_ref()).unwrap();
                let method = request["method"].as_str().unwrap_or("");
                if method.ends_with("/submit") || matches!(method, "turn/start" | "turn/steer") {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                if request.get("id").is_none() {
                    continue;
                }
                if method.ends_with("/submit") || method == "claude/input/status" {
                    let (code, message) = if method == "claude/input/status" {
                        (-32004, "no local inbox receipt")
                    } else {
                        (-32099, "undisposed control reached native submit")
                    };
                    ws.send(tungstenite::Message::Text(
                        json!({"id":request["id"],
                        "error":{"code":code,"message":message}})
                        .to_string()
                        .into(),
                    ))
                    .unwrap();
                    continue;
                }
                let result = match method {
                    "thread/resume" | "thread/read" => {
                        json!({"thread":{"id":"thread-q","status":{"type":"idle"},"turns":[]}})
                    }
                    "claude/input/status" => json!({"threadId":"thread-q","state":"unknown"}),
                    "claude/tasks/list" => {
                        json!({"tasks":[],"completeInventory":true,"nextCursor":null})
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
        manager
            .connect_adapter(cutex::runtime::claude::ClaudeObservation {
                record: record.clone(),
                endpoint: cutex::app_server::client::AppServerEndpoint::LoopbackWebSocket {
                    url: format!("ws://{address}/"),
                    bearer_token: None,
                },
                resume: cutex::app_server::commands::ThreadResumeParams {
                    thread_id: "thread-q".into(),
                    ..Default::default()
                },
                schema: Default::default(),
                diagnostic_path: root.join(format!("{session}.jsonl")),
            })
            .unwrap();
        // Prove the endpoint and counter work before checking a zero delta.
        manager
            .handle_for_generation(&session, 1)
            .unwrap()
            .request("turn/start", json!({"threadId":"thread-q"}))
            .unwrap();
        assert_eq!(submits.swap(0, Ordering::SeqCst), 1);
        Self {
            record,
            submits,
            peer: Some(peer),
            manager,
        }
    }
}

impl Drop for NativeProbe {
    fn drop(&mut self) {
        self.manager
            .disconnect(&self.record.cutex_session_id)
            .unwrap();
        self.peer.take().unwrap().join().unwrap();
    }
}

fn reserved_ask(session: &str, occurrence: u64) -> (String, Submission) {
    let store = AskStore::open_default().unwrap();
    store
        .with_state(|state| {
            let now = Utc::now().to_rfc3339();
            let (ask, _) = state
                .create_ask(
                    cutex::asks::store::NewAsk {
                        ask_id: None,
                        action_id: uuid::Uuid::new_v4().to_string(),
                        requester: Requester::Agent {
                            cutex_session_id: "cutex.requester".into(),
                            name: None,
                        },
                        target_cutex_session_id: session.into(),
                        target_name: None,
                        project_id: None,
                        body: AskBody {
                            text: "dispatch regression".into(),
                            text_artifact: None,
                            text_bytes: 19,
                            text_sha256: "a".repeat(64),
                            attachments: vec![],
                        },
                        title: None,
                        options: Default::default(),
                        follow_up_of: None,
                        parent_ask: None,
                        gate: None,
                    },
                    Utc::now(),
                )
                .unwrap();
            let id = ask.ask_id.clone();
            let message = format!("{id}:attempt-1");
            let ask = state.asks.get_mut(&id).unwrap();
            ask.admission_occurrence = occurrence;
            let mut attempt = AttemptRecord::submitting(1, message.clone(), 1, now.clone());
            attempt.admission_occurrence = occurrence;
            ask.attempts.push(attempt);
            state.leases.insert(
                session.into(),
                AdmissionLease {
                    cutex_session_id: session.into(),
                    holder: LeaseHolder::Ask {
                        ask_id: id.clone(),
                        attempt: 1,
                    },
                    native_message_id: message.clone(),
                    runtime_generation: 1,
                    turn_id: None,
                    since: now,
                },
            );
            (
                request_occurrence_id(&id, occurrence),
                Submission {
                    message_id: message,
                    source_agent: Some("cutex.requester".into()),
                    event_type: "ASK",
                    text: "dispatch regression".into(),
                    asks: vec![id],
                    attempt: Some(1),
                    view: None,
                },
            )
        })
        .unwrap()
}

#[test]
fn disposed_request_occurrence_never_reaches_real_ask_native_dispatch() {
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("disposed-ask-dispatch").unwrap();
    let native = NativeProbe::connect(home.root(), false);
    let session = &native.record.cutex_session_id;
    // Nonzero occurrence catches guards incorrectly rebuilding the legacy id.
    let (input_id, submission) = reserved_ask(session, 3);
    cutex::management::v2::input_disposal::request(session, &input_id).unwrap();
    let outcome = ManagementAskRuntime::default().submit_exclusive(session, 1, &submission);
    assert!(
        matches!(outcome, SubmitOutcome::Unsupported(ref reason) if reason.contains("input disposed by owner")),
        "{outcome:?}"
    );
    assert_eq!(native.submits.load(Ordering::SeqCst), 0);
}

#[test]
fn disposed_result_delivery_never_reaches_real_submit_or_steer_dispatch() {
    let home = crate::cli_app::test_home::IsolatedTestHome::new_private("disposed-result-dispatch")
        .unwrap();
    let native = NativeProbe::connect(home.root(), false);
    let session = &native.record.cutex_session_id;
    let delivery = format!("delivery-{}", uuid::Uuid::new_v4());
    let message = format!("event-{}", uuid::Uuid::new_v4());
    AskStore::open_default()
        .unwrap()
        .with_state(|state| {
            state.events.insert(
                message.clone(),
                EventOutboxRecord {
                    event_id: message.clone(),
                    cutex_session_id: session.clone(),
                    delivery_ids: vec![delivery.clone()],
                    native_message_id: message.clone(),
                    event_artifact: None,
                    turn_id: None,
                    acknowledged: false,
                    created_at: Utc::now().to_rfc3339(),
                    view: None,
                    compacted_at: None,
                    compaction_reminder_pending: false,
                },
            );
        })
        .unwrap();
    let submission = Submission {
        message_id: message,
        source_agent: None,
        event_type: "ANSWER",
        text: "result".into(),
        asks: vec!["ask-result".into()],
        attempt: None,
        view: None,
    };
    cutex::management::v2::input_disposal::request(session, &delivery).unwrap();
    for outcome in [
        ManagementAskRuntime::default().submit_exclusive(session, 1, &submission),
        ManagementAskRuntime::default().steer_note(session, 1, &submission),
    ] {
        assert!(
            matches!(outcome, SubmitOutcome::Unsupported(ref reason) if reason.contains("input disposed by owner")),
            "{outcome:?}"
        );
    }
    assert_eq!(native.submits.load(Ordering::SeqCst), 0);
}
