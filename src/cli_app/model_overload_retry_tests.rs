use super::*;
use cutex::app_server::client::{AppServerClient, AppServerClientOptions, AppServerEndpoint};
use cutex::app_server::external_input::Receipt;
use std::{
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};
use tungstenite::{Message as WsMessage, WebSocket};

fn read(socket: &mut WebSocket<TcpStream>) -> Option<Value> {
    loop {
        match socket.read().ok()? {
            WsMessage::Text(t) => return serde_json::from_str(t.as_ref()).ok(),
            _ => {}
        }
    }
}
fn write(socket: &mut WebSocket<TcpStream>, v: Value) {
    socket.send(WsMessage::Text(v.to_string().into())).unwrap();
}
fn response(e: &Envelope, persisted: bool) -> Value {
    let mut r = Receipt {
        schema: "codex.external-input-receipt.v1".into(),
        receipt_id: String::new(),
        owner_id: e.owner_id.clone(),
        thread_id: e.thread_id.clone(),
        message_id: e.message.id.clone(),
        semantic_sha256: e.semantic_sha256.clone(),
        response_item_id: e.message.id.clone(),
        turn_id: "continued".into(),
        ordinal: 1,
    };
    r.receipt_id = r.digest_id();
    json!({"version":1,"ownerId":e.owner_id,"threadId":e.thread_id,"runtimeGeneration":e.runtime_generation,
        "statuses":[{"messageId":e.message.id,"semanticSha256":e.semantic_sha256,
        "deliveryState":if persisted {"context_persisted"} else {"unknown"},
        "receipt":if persisted {serde_json::to_value(r).unwrap()} else {Value::Null},
        "processing":{"state":"none","attemptId":null,"reason":null}}]})
}
// A real loopback WebSocket with the real RPC client and receipt validator.
// Only initialize/resume/submit/status are accepted. Every submit is counted.
struct Server {
    client: AppServerClient,
    thread: std::thread::JoinHandle<()>,
    calls: Arc<Mutex<Vec<Value>>>,
}
impl Server {
    fn start(mode: &str, e: Envelope) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let mode = mode.to_string();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            while let Some(req) = read(&mut socket) {
                let m = req["method"].as_str().unwrap();
                if m == "initialized" {
                    continue;
                }
                seen.lock().unwrap().push(req.clone());
                let result = match m {
                    "initialize" => json!({}),
                    "thread/externalInput/resume" => json!({}),
                    "thread/externalInput/submit" => {
                        assert_eq!(req["params"]["message"]["text"], "Continue.");
                        assert_eq!(req["params"]["admission"], "exclusive_turn");
                        assert_eq!(req["params"]["message"]["id"], e.message.id);
                        if mode == "lost" || mode == "unresolved" {
                            continue;
                        }
                        if mode == "busy" {
                            write(
                                &mut socket,
                                json!({"id":req["id"],"error":{"code":-32600,"message":"busy","data":{"externalInputError":"busy","reason":"active_turn"}}}),
                            );
                            continue;
                        }
                        let mut result = response(&e, mode != "top-only" && mode != "accepted");
                        if mode == "accepted" {
                            result["statuses"][0]["deliveryState"] = json!("pending");
                            result["turnId"] = json!("continued");
                        }
                        if mode == "conflict" {
                            result["statuses"][0]["receipt"]["semanticSha256"] = json!("bad");
                        }
                        if mode == "top-only" {
                            result["turnId"] = json!("continued");
                        }
                        result
                    }
                    "thread/externalInput/status" => {
                        assert_eq!(
                            req["params"]["messages"][0],
                            serde_json::to_value(e.key()).unwrap()
                        );
                        response(&e, mode != "unresolved")
                    }
                    other => panic!("unexpected RPC {other}"),
                };
                write(&mut socket, json!({"id":req["id"],"result":result}));
            }
        });
        let mut options = AppServerClientOptions::new(AppServerEndpoint::LoopbackWebSocket {
            url: format!("ws://{address}/"),
            bearer_token: None,
        });
        options.request_timeout = Duration::from_millis(300);
        let client = AppServerClient::connect(options).unwrap();
        Self {
            client,
            thread,
            calls,
        }
    }
    fn count(&self, method: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == method)
            .count()
    }
    fn finish(self) {
        self.client.handle().shutdown().unwrap();
        drop(self.client);
        self.thread.join().unwrap();
    }
}
fn fixture() -> (Store, AskStore, String, Plan) {
    let dir = std::env::temp_dir().join(format!("ow-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::at(dir.join("retry.json"));
    let asks = AskStore::at(dir.join("asks"));
    let session = format!("cutex.retry-{}", uuid::Uuid::new_v4().simple());
    store.update(|s|s.terminal(&session,"thread",7,&json!({"id":"original","status":"failed","error":{"codexErrorInfo":"serverOverloaded"}}),None,Utc::now())).unwrap();
    let p = store.snapshot().unwrap().sessions[&session]
        .plan
        .clone()
        .unwrap();
    (store, asks, session, p)
}
fn due(p: &Plan) -> chrono::DateTime<Utc> {
    p.at.parse().unwrap()
}

#[test]
fn deferred_authority_after_firing_restores_only_proven_unsent_plan() {
    let (store,asks,session,plan)=fixture();
    fire(&store,&asks,&session,&plan,due(&plan),|_|Err(cutex::asks::pause_admission::Deferred.into())).unwrap();
    let pending=store.snapshot().unwrap().sessions[&session].plan.clone().unwrap();
    assert_eq!(pending.phase,Phase::Pending);
    assert_eq!(pending.message_id,plan.message_id);
    store.update(|s|s.sessions.get_mut(&session).unwrap().plan.as_mut().unwrap().phase=Phase::Uncertain).unwrap();
    postpone_unsent(&store,&session,&plan).unwrap();
    assert_eq!(store.snapshot().unwrap().sessions[&session].plan.as_ref().unwrap().phase,Phase::Uncertain);
}

#[test]
fn lifecycle_begins_after_eligibility_before_gate_reopen_keeps_unsent_identity() {
    let (store,asks,session,plan)=fixture();
    let server=Server::start("ok",envelope(&session,&plan));
    let (held_tx,held_rx)=std::sync::mpsc::channel();
    let (release_tx,release_rx)=std::sync::mpsc::channel();
    let worker=std::cell::RefCell::new(None);
    fire(&store,&asks,&session,&plan,due(&plan),|_| {
        *worker.borrow_mut()=Some(std::thread::spawn(move||cutex::asks::pause_admission::try_with_authority(|_|{
            held_tx.send(()).unwrap();release_rx.recv_timeout(Duration::from_secs(5)).unwrap();Ok(())
        }).unwrap()));
        held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        Ok(server.client.handle())
    }).unwrap();
    release_tx.send(()).unwrap();worker.into_inner().unwrap().join().unwrap();
    assert_eq!(server.count("thread/externalInput/resume"),0);
    assert_eq!(server.count("thread/externalInput/submit"),0);
    let pending=store.snapshot().unwrap().sessions[&session].plan.clone().unwrap();
    assert_eq!(pending.phase,Phase::Pending);assert_eq!(pending.message_id,plan.message_id);
    server.finish();
}

#[test]
fn project_pause_after_eligibility_prevents_both_reopen_and_retry_submission() {
    use cutex::asks::{project_control as p,routing_policy::{RoutingActor,RoutingContext}};
    let (store,asks,session,plan)=fixture();let server=Server::start("ok",envelope(&session,&plan));
    fire(&store,&asks,&session,&plan,due(&plan),|_|{
        asks.with_state(|s|{
            let ctx=RoutingContext{project_id:"retry-project".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
            p::pause(s,&ctx,&p::PauseRequest{action_id:"pause".into(),expected_epoch:0,mode:p::Mode::AfterTurn,scope:p::Scope::Members},
                &p::Membership{revision:0,members:[session.clone()].into(),excluded:Default::default()},"test-host",Utc::now()).unwrap();
        }).unwrap();Ok(server.client.handle())
    }).unwrap();
    assert_eq!(server.count("thread/externalInput/resume"),0);assert_eq!(server.count("thread/externalInput/submit"),0);
    let retained=store.snapshot().unwrap().sessions[&session].plan.clone().unwrap();
    assert_eq!(retained.phase,Phase::Pending);assert_eq!(retained.message_id,plan.message_id);
    server.finish();
}

#[test]
fn real_submit_once_and_receipt_bound_success() {
    let (store, asks, s, p) = fixture();
    let server = Server::start("ok", envelope(&s, &p));
    let h = server.client.handle();
    for _ in 0..3 {
        fire(&store, &asks, &s, &p, due(&p), |_| Ok(h.clone())).unwrap();
    }
    assert_eq!(server.count("thread/externalInput/submit"), 1);
    assert_eq!(server.count("thread/externalInput/resume"), 1);
    let plan = store.snapshot().unwrap().sessions[&s].plan.clone().unwrap();
    assert_eq!(plan.turn_id, "continued");
    assert_eq!(plan.phase, Phase::Running);
    // A duplicate original terminal cannot cancel the continuation.
    assert!(!observe(
        &store,
        &asks,
        &s,
        "thread",
        7,
        "turn/completed",
        &p.failed_turn,
        Utc::now()
    )
    .unwrap());
    observe(
        &store,
        &asks,
        &s,
        "thread",
        7,
        "turn/completed",
        &json!({"id":"continued","status":"completed"}),
        Utc::now(),
    )
    .unwrap();
    assert!(store.snapshot().unwrap().sessions[&s].plan.is_none());
    server.finish();
}

#[test]
fn lost_submit_reply_queries_receipt_without_resubmit() {
    let (store, asks, s, p) = fixture();
    let server = Server::start("lost", envelope(&s, &p));
    let h = server.client.handle();
    assert!(fire(&store, &asks, &s, &p, due(&p), |_| Ok(h.clone())).is_err());
    for _ in 0..3 {
        fire(&store, &asks, &s, &p, due(&p), |_| {
            panic!("must not retry eligibility")
        })
        .unwrap();
    }
    let plan = store.snapshot().unwrap().sessions[&s].plan.clone().unwrap();
    assert_eq!(plan.phase, Phase::Uncertain);
    reconcile(&store, &asks, &s, &plan, Utc::now(), || {
        panic!("before receipt deadline")
    })
    .unwrap();
    reconcile(
        &store,
        &asks,
        &s,
        &plan,
        Utc::now() + chrono::Duration::seconds(10),
        || Ok(h),
    )
    .unwrap();
    assert_eq!(
        store.snapshot().unwrap().sessions[&s]
            .plan
            .as_ref()
            .unwrap()
            .phase,
        Phase::Running
    );
    assert_eq!(server.count("thread/externalInput/submit"), 1);
    assert_eq!(server.count("thread/externalInput/status"), 1);
    server.finish();
}

#[test]
fn unknown_receipt_backoff_is_bounded_and_never_replays() {
    let (store, asks, s, p) = fixture();
    let server = Server::start("unresolved", envelope(&s, &p));
    let h = server.client.handle();
    assert!(fire(&store, &asks, &s, &p, due(&p), |_| Ok(h.clone())).is_err());
    let mut prior = None;
    for n in 1..=8 {
        let plan = store.snapshot().unwrap().sessions[&s].plan.clone().unwrap();
        let at: chrono::DateTime<Utc> = plan.receipt_check_at.as_ref().unwrap().parse().unwrap();
        if let Some(previous) = prior {
            assert!(at > previous);
        }
        reconcile(
            &store,
            &asks,
            &s,
            &plan,
            at - chrono::Duration::milliseconds(1),
            || panic!("early poll"),
        )
        .unwrap();
        reconcile(&store, &asks, &s, &plan, at, || Ok(h.clone())).unwrap();
        assert_eq!(server.count("thread/externalInput/status"), n);
        prior = Some(at);
    }
    assert!(store.snapshot().unwrap().sessions[&s].plan.is_none());
    assert_eq!(server.count("thread/externalInput/submit"), 1);
    server.finish();
}

#[test]
fn invalid_receipt_or_top_level_turn_alone_never_binds() {
    for mode in ["conflict", "top-only"] {
        let (store, asks, s, p) = fixture();
        let server = Server::start(mode, envelope(&s, &p));
        assert!(fire(&store, &asks, &s, &p, due(&p), |_| Ok(server
            .client
            .handle()))
        .is_err());
        let plan = store.snapshot().unwrap().sessions[&s].plan.clone().unwrap();
        assert_eq!(plan.phase, Phase::Uncertain);
        assert_eq!(plan.turn_id, "original");
        assert_eq!(server.count("thread/externalInput/submit"), 1);
        server.finish();
    }
}

#[test]
fn owner_stop_and_later_turn_cancel_before_send() {
    let (store, asks, s, p) = fixture();
    retry::cancel_with(&store, &asks, &s, "ownerStop").unwrap();
    fire(&store, &asks, &s, &p, due(&p), |_| {
        panic!("cancelled timer sent")
    })
    .unwrap();
    let (store, asks, s, p) = fixture();
    observe(
        &store,
        &asks,
        &s,
        "thread",
        7,
        "turn/started",
        &json!({"id":"later"}),
        Utc::now(),
    )
    .unwrap();
    fire(&store, &asks, &s, &p, due(&p), |_| {
        panic!("superseded timer sent")
    })
    .unwrap();
}

#[test]
fn failed_eligibility_never_opens_a_connection_or_leaves_claim() {
    for reason in [
        "offline",
        "runtimeRestarted",
        "ownerQueueOrPaused",
        "pausedOrLeaseChanged",
        "activeTurn",
    ] {
        let (store, asks, s, p) = fixture();
        fire(&store, &asks, &s, &p, due(&p), |_| {
            Err(anyhow::anyhow!(reason.to_string()))
        })
        .unwrap();
        assert!(store.snapshot().unwrap().sessions[&s].plan.is_none());
    }
}

#[test]
fn early_terminal_is_reprocessed_only_for_receipt_bound_turn() {
    let (store, asks, s, p) = fixture();
    store
        .update(|state| {
            let p = state.sessions.get_mut(&s).unwrap().plan.as_mut().unwrap();
            p.phase = Phase::Uncertain;
        })
        .unwrap();
    let message = json!({"threadId":"thread","turnId":"continued","item":{"id":"final","type":"agentMessage","phase":"final_answer","text":"answer before receipt"}});
    capture_early_message(&store, &s, "thread", 7, &message).unwrap();
    assert_eq!(
        store.snapshot().unwrap().sessions[&s]
            .plan
            .as_ref()
            .unwrap()
            .early_message,
        Some(message)
    );
    let end = json!({"id":"continued","status":"completed"});
    observe(
        &store,
        &asks,
        &s,
        "thread",
        7,
        "turn/completed",
        &end,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(
        store.snapshot().unwrap().sessions[&s]
            .plan
            .as_ref()
            .unwrap()
            .early_terminal,
        Some(end)
    );
    bound(&store, &asks, &s, &p, "continued").unwrap();
    assert!(store.snapshot().unwrap().sessions[&s].plan.is_none());
}

#[test]
fn native_pending_reservation_is_known_acceptance_and_busy_is_definite_refusal() {
    for mode in ["accepted", "busy"] {
        let (store, asks, s, p) = fixture();
        let server = Server::start(mode, envelope(&s, &p));
        fire(&store, &asks, &s, &p, due(&p), |_| {
            Ok(server.client.handle())
        })
        .unwrap();
        let state = store.snapshot().unwrap();
        if mode == "accepted" {
            assert_eq!(
                state.sessions[&s].plan.as_ref().unwrap().phase,
                Phase::Running
            );
        } else {
            assert!(state.sessions[&s].plan.is_none());
        }
        assert_eq!(server.count("thread/externalInput/submit"), 1);
        server.finish();
    }
}

#[test]
fn cancelled_unknown_terminal_cannot_resurrect_retry_chain() {
    let (store, asks, s, p) = fixture();
    store
        .update(|state| {
            state
                .sessions
                .get_mut(&s)
                .unwrap()
                .plan
                .as_mut()
                .unwrap()
                .phase = Phase::Uncertain
        })
        .unwrap();
    retry::cancel_with(&store, &asks, &s, "ownerStop").unwrap();
    let end = json!({"id":"late-unknown","status":"failed","error":{"codexErrorInfo":"serverOverloaded"}});
    assert!(!observe(
        &store,
        &asks,
        &s,
        "thread",
        7,
        "turn/completed",
        &end,
        Utc::now()
    )
    .unwrap());
    fire(&store, &asks, &s, &p, due(&p), |_| {
        panic!("resurrected timer")
    })
    .unwrap();
    retry::cancel_with(&store, &asks, &s, "ownerMessage").unwrap();
    assert!(observe(
        &store,
        &asks,
        &s,
        "thread",
        7,
        "turn/completed",
        &json!({"id":"new-owner","status":"failed","error":{"codexErrorInfo":"serverOverloaded"}}),
        Utc::now()
    )
    .unwrap());
}

#[test]
fn newer_native_history_fences_retry_even_if_observer_is_delayed() {
    let (_, _, _, p) = fixture();
    assert!(latest_failed_turn(
        &json!({"thread":{"id":"thread","turns":[p.failed_turn]}}),
        &p
    ));
    assert!(!latest_failed_turn(
        &json!({"thread":{"id":"thread","turns":[p.failed_turn,{"id":"later","status":"completed"}]}}),
        &p
    ));
    assert!(!latest_failed_turn(
        &json!({"thread":{"id":"other","turns":[p.failed_turn]}}),
        &p
    ));
    assert!(!latest_failed_turn(&json!({"thread":{"id":"thread"}}), &p));
}

#[test]
fn owner_stop_serializes_with_inflight_unknown_send_and_no_second_submit() {
    let (store, asks, s, p) = fixture();
    let server = Server::start("lost", envelope(&s, &p));
    std::thread::scope(|scope| {
        let h = server.client.handle();
        let fire_task = scope.spawn(|| fire(&store, &asks, &s, &p, due(&p), |_| Ok(h)));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while server.count("thread/externalInput/submit") == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(retry::cancel_with(&store, &asks, &s, "ownerStop").unwrap());
        assert!(fire_task.join().unwrap().is_err());
    });
    fire(&store, &asks, &s, &p, due(&p), |_| {
        panic!("sent after owner stop")
    })
    .unwrap();
    assert!(store.snapshot().unwrap().sessions[&s].plan.is_none());
    assert_eq!(server.count("thread/externalInput/submit"), 1);
    server.finish();
}

#[test]
fn p1_binding_commit_fault_recovers_without_second_submit() {
    let (store, asks, s, p) = fixture();
    let server = Server::start("lost", envelope(&s, &p));
    assert!(fire(&store, &asks, &s, &p, due(&p), |_| Ok(server
        .client
        .handle()))
    .is_err());
    assert!(
        bound_with_hook(&store, &asks, &s, &p, "continued", || anyhow::bail!(
            "crash after AskStore rebind"
        ))
        .is_err()
    );
    let recovered = store.snapshot().unwrap().sessions[&s].plan.clone().unwrap();
    assert_eq!(recovered.phase, Phase::Binding);
    assert_eq!(recovered.binding_turn_id.as_deref(), Some("continued"));
    bound(&store, &asks, &s, &recovered, "continued").unwrap();
    fire(&store, &asks, &s, &p, due(&p), |_| {
        panic!("must not resend")
    })
    .unwrap();
    assert_eq!(
        store.snapshot().unwrap().sessions[&s]
            .plan
            .as_ref()
            .unwrap()
            .phase,
        Phase::Running
    );
    assert_eq!(server.count("thread/externalInput/submit"), 1);
    server.finish();
}

#[test]
fn p2_cancel_and_disable_creation_fail_then_storage_recovers_zero_submit() {
    let dir = std::env::temp_dir().join(format!("retry-double-fault-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("retry.json");
    let store = Store::at(&path);
    let asks = AskStore::at(dir.join("asks"));
    let s = format!("cutex.double-fault-{}", uuid::Uuid::new_v4());
    store.update(|state|state.terminal(&s,"thread",7,&json!({"id":"original","status":"failed","error":{"codexErrorInfo":"serverOverloaded"}}),None,Utc::now())).unwrap();
    let p = store.snapshot().unwrap().sessions[&s].plan.clone().unwrap();
    let saved = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"broken journal").unwrap();
    let marker = path.with_extension("disabled");
    std::fs::create_dir(&marker).unwrap(); // creation must fail, not simulated success
    retry::dispatch_after_safety_cancel(&store, &s, "ownerStop", || Ok(())).unwrap();
    std::fs::write(&path, &saved).unwrap();
    std::fs::remove_dir(&marker).unwrap();
    let reopened = Store::at(&path);
    assert!(reopened.is_disabled());
    assert!(!marker.exists());
    assert_eq!(
        reopened.snapshot().unwrap().sessions[&s]
            .plan
            .as_ref()
            .unwrap()
            .phase,
        Phase::Pending
    );
    let server = Server::start("ok", envelope(&s, &p));
    for _ in 0..3 {
        fire(&reopened, &asks, &s, &p, due(&p), |_| {
            Ok(server.client.handle())
        })
        .unwrap();
    }
    assert_eq!(server.count("thread/externalInput/submit"), 0);
    assert_eq!(server.count("thread/externalInput/resume"), 0);
    server.finish();
}
