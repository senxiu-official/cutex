//! Real controller transport, fake native peer. No daemon/default ports or keys.
use super::*;
use crate::app_server::client::{
    AppServerClient, AppServerClientOptions, AppServerEndpoint, RequestHook,
};
use crate::app_server::external_input::Receipt;
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Mutex,
};

struct Environment {
    clock: AtomicI64,
    index: Mutex<Snapshot>,
}
impl transport::Environment for Environment {
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
    fn activity(&self, _: &str) -> anyhow::Result<Option<i64>> {
        Ok(Some(0))
    }
    fn snapshot(&self, _: &str) -> anyhow::Result<Option<Snapshot>> {
        Ok(Some(self.index.lock().unwrap().clone()))
    }
    fn binding(&self, _: &str, _: &str, _: u64) -> anyhow::Result<bool> {
        Ok(true)
    }
}
#[derive(Default)]
struct PeerState {
    notes: BTreeMap<String, (String, bool)>,
    note_submits: usize,
    contexts: Vec<String>,
    carriers: Vec<(String, serde_json::Value)>,
    busy: bool,
    latest: usize,
    lose_next_reply: bool,
}

fn peer(
    route: Route,
    state: Arc<Mutex<PeerState>>,
) -> (AppServerClient, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        let mut ws = tungstenite::accept(socket).unwrap();
        while let Ok(message) = ws.read() {
            if message.is_close() {
                break;
            }
            let Ok(text) = message.into_text() else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            if request.get("id").is_none() {
                continue;
            }
            let method = request["method"].as_str().unwrap();
            let p = &request["params"];
            let mut s = state.lock().unwrap();
            let passive = p["message"]["delivery"] == "passive" || p["deliveryMode"] == "passive";
            let result = if method == "initialize" {
                json!({"userAgent":"memory-fixture","externalInputVersion":1})
            } else if method == "thread/turns/list" {
                json!({"data":[{"id":format!("turn-{}",s.latest),"status":if s.busy {"inProgress"} else {"completed"}}],"nextCursor":null})
            } else if method.ends_with("/status") {
                let rows: Vec<_> = p["messages"].as_array().unwrap().iter().map(|key| {
                    let id = key["messageId"].as_str().unwrap();
                    let digest = key["semanticSha256"].as_str().unwrap();
                    let persisted = s.notes.get(id).is_some_and(|(d, done)| d == digest && *done);
                    let known = s.notes.contains_key(id);
                    if route == Route::Native {
                        let receipt = persisted.then(|| {
                            let mut r = Receipt { schema:"codex.external-input-receipt.v1".into(), receipt_id:String::new(),
                                owner_id:"cutex.s".into(),thread_id:"t".into(),message_id:id.into(),semantic_sha256:digest.into(),
                                response_item_id:id.into(),turn_id:format!("turn-{}",s.latest),ordinal:1 };
                            r.receipt_id=r.digest_id(); r
                        });
                        json!({"messageId":id,"semanticSha256":digest,"deliveryState":if persisted {"context_persisted"} else if known {"pending"} else {"unknown"},
                            "receipt":receipt,"processing":{"state":"none","attemptId":null,"reason":null}})
                    } else {
                        json!({"messageId":id,"semanticSha256":digest,"state":if persisted {"context_persisted"} else if known {"pending"} else {"unknown"},
                            "receipt":persisted.then(||json!({"receiptId":format!("r-{id}"),"threadId":"t","messageId":id,"semanticSha256":digest,
                                "responseItemId":format!("external-{id}"),"turnId":format!("turn-{}",s.latest),"rolloutOrdinal":1}))})
                    }
                }).collect();
                if route == Route::Native {
                    json!({"version":1,"ownerId":"cutex.s","threadId":"t","runtimeGeneration":1,"statuses":rows})
                } else {
                    json!({"schema":"cutex/inter-agent-delivery-status/v1","threadId":"t","statuses":rows})
                }
            } else if passive {
                s.note_submits += 1;
                let (id, digest) = if route == Route::Native {
                    let envelope: crate::app_server::external_input::Envelope =
                        serde_json::from_value(p.clone()).unwrap();
                    envelope.validate().unwrap();
                    assert_eq!(
                        envelope.message.source.kind,
                        crate::app_server::external_input::SourceKind::Service
                    );
                    (envelope.message.id, envelope.semantic_sha256)
                } else {
                    let digest = {
                        let hook_store = NoteStore::open(&root()).unwrap();
                        let pending = Pending {
                            captured_serial: 0,
                            id: p["messageId"].as_str().unwrap().into(),
                            snapshot: snapshot("new\n"),
                            text: p["content"].as_str().unwrap().into(),
                        };
                        let hook = Hook {
                            session: "cutex.s".into(),
                            thread: "t".into(),
                            generation: 1,
                            route,
                            store: Arc::new(hook_store),
                            environment: Arc::new(UnusedEnvironment),
                        };
                        let (_, canonical) = hook.submission(&pending);
                        assert_eq!(&canonical, p);
                        // Use the peer's JS framing algorithm domain/fields.
                        use sha2::{Digest, Sha256};
                        let mut h = Sha256::new();
                        h.update(b"cutex:inter-agent-message-semantic:v1\0");
                        for field in [p["messageId"].as_str().unwrap(), "cutex-memory", "cutex.s"] {
                            h.update((field.len() as u64).to_be_bytes());
                            h.update(field.as_bytes());
                        }
                        h.update(0u64.to_be_bytes());
                        for field in ["passive", p["content"].as_str().unwrap()] {
                            h.update((field.len() as u64).to_be_bytes());
                            h.update(field.as_bytes());
                        }
                        format!("{:x}", h.finalize())
                    };
                    (p["messageId"].as_str().unwrap().into(), digest)
                };
                s.notes.entry(id).or_insert((digest, false));
                if route == Route::Native {
                    json!({"version":1,"ownerId":"cutex.s","threadId":"t","runtimeGeneration":1,"statuses":[]})
                } else {
                    json!({"submissionId":"passive-note"})
                }
            } else if method == "turn/steer" {
                s.carriers.push((method.into(), p.clone()));
                json!({"turnId":"busy-turn"})
            } else {
                assert!(
                    transport::triggers_turn(&request, "t"),
                    "unexpected fixture method {method}"
                );
                s.carriers.push((method.into(), p.clone()));
                s.latest += 1;
                let newly: Vec<_> = s
                    .notes
                    .iter()
                    .filter(|(_, (_, done))| !done)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in newly {
                    s.notes.get_mut(&id).unwrap().1 = true;
                    s.contexts.push(id);
                }
                if s.lose_next_reply {
                    s.lose_next_reply = false;
                    drop(s);
                    let _ = ws.close(None);
                    break;
                }
                json!({"turn":{"id":format!("turn-{}",s.latest)}})
            };
            drop(s);
            if ws
                .send(tungstenite::Message::text(
                    json!({"id":request["id"],"result":result}).to_string(),
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
    (client, worker)
}

fn hook(store: Arc<NoteStore>, env: Arc<Environment>, route: Route) -> Arc<Hook> {
    Arc::new(Hook {
        session: "cutex.s".into(),
        thread: "t".into(),
        generation: 1,
        route,
        store,
        environment: env,
    })
}

fn carrier(method: &str) -> serde_json::Value {
    match method {
        "turn/start" => json!({"threadId":"t","input":[{"type":"text","text":"OWNER_ORIGINAL"}]}),
        "dsh/input/submit" => {
            json!({"threadId":"t","messageId":"another-service-input","source":{"kind":"service","id":"other-service"},
            "eventType":"delegation_result","text":"TRIGGER_ORIGINAL","admission":"exclusive_turn"})
        }
        "thread/inter_agent_message" => {
            json!({"threadId":"t","messageId":"another-agent-input","author":"peer","recipient":"cutex.s",
            "content":"TRIGGER_ORIGINAL","deliveryMode":"after_turn"})
        }
        _ => {
            use crate::app_server::external_input::*;
            let mut e = Envelope {
                version: 1,
                owner_id: "cutex.s".into(),
                thread_id: "t".into(),
                runtime_generation: 1,
                message: Message {
                    id: "another-service-input".into(),
                    source: Source {
                        kind: SourceKind::Service,
                        id: "other-service".into(),
                    },
                    event_type: "delegation_result".into(),
                    delivery: Delivery::AfterTurn,
                    text: "TRIGGER_ORIGINAL".into(),
                },
                view: None,
                semantic_sha256: String::new(),
            };
            e.semantic_sha256 = e.digest();
            serde_json::to_value(e).unwrap()
        }
    }
}

#[test]
fn cold_boundary_and_all_admission_routes_attach_once_without_changing_owner_payload() {
    for (runtime, route, carriers) in [
        (
            "codex",
            Route::Native,
            vec!["turn/start", "thread/externalInput/submit"],
        ),
        (
            "claude",
            Route::Native,
            vec!["turn/start", "thread/externalInput/submit"],
        ),
        (
            "dsh",
            Route::Legacy,
            vec![
                "turn/start",
                "dsh/input/submit",
                "thread/inter_agent_message",
            ],
        ),
        (
            "kimi",
            Route::Legacy,
            vec![
                "turn/start",
                "dsh/input/submit",
                "thread/inter_agent_message",
            ],
        ),
    ] {
        for method in carriers {
            let store = Arc::new(NoteStore::open(&root()).unwrap());
            store
                .launch("cutex.s", snapshot("old\n"), 0, false)
                .unwrap();
            store.turn_activity("cutex.s", "turn-0", 0, false).unwrap();
            let env = Arc::new(Environment {
                clock: AtomicI64::new(IDLE_AFTER_MS - 1),
                index: Mutex::new(snapshot("- [new](new.md) — 中文新记忆\n")),
            });
            let hook = hook(store.clone(), env.clone(), route);
            let state = Arc::new(Mutex::new(PeerState::default()));
            let (client, peer) = peer(route, state.clone());
            let handle = client.handle();
            handle.install_request_hook(hook.clone());
            let params = carrier(method);
            hook.before(&handle, &json!({"method":method,"params":params}));
            assert_eq!(
                state.lock().unwrap().note_submits,
                0,
                "{runtime}: short gap"
            );
            env.clock.store(IDLE_AFTER_MS, Ordering::SeqCst);
            state.lock().unwrap().busy = true;
            hook.before(&handle, &json!({"method":method,"params":params}));
            assert_eq!(state.lock().unwrap().note_submits, 0, "{runtime}: busy");
            state.lock().unwrap().busy = false;
            handle.request(method, params.clone()).unwrap();
            assert_eq!(
                state.lock().unwrap().note_submits,
                1,
                "{runtime}: exact 70 minutes"
            );
            assert_eq!(
                state.lock().unwrap().contexts.len(),
                1,
                "one service timeline item"
            );
            assert_eq!(
                state.lock().unwrap().carriers[0].1,
                params,
                "owner input untouched"
            );
            assert!(store
                .get("cutex.s")
                .unwrap()
                .unwrap()
                .value
                .pending
                .is_none());
            handle
                .request(
                    "turn/start",
                    json!({"threadId":"t","input":[{"type":"text","text":"another turn"}]}),
                )
                .unwrap();
            assert_eq!(
                state.lock().unwrap().note_submits,
                1,
                "{runtime}: unchanged index"
            );
            *env.index.lock().unwrap() = snapshot("changed again\n");
            handle
                .request("turn/steer", json!({"threadId":"t","input":[]}))
                .unwrap();
            assert_eq!(
                state.lock().unwrap().note_submits,
                1,
                "{runtime}: no steer note"
            );
            drop(client);
            peer.join().unwrap();
        }
    }
}

#[test]
fn a_lost_carrier_reply_and_daemon_reconnect_do_not_duplicate_the_native_note() {
    for route in [Route::Native, Route::Legacy] {
        let path = root();
        let store = Arc::new(NoteStore::open(&path).unwrap());
        store
            .launch("cutex.s", snapshot("old\n"), 0, false)
            .unwrap();
        store.turn_activity("cutex.s", "turn-0", 0, false).unwrap();
        let env = Arc::new(Environment {
            clock: AtomicI64::new(IDLE_AFTER_MS),
            index: Mutex::new(snapshot("new\n")),
        });
        let state = Arc::new(Mutex::new(PeerState {
            lose_next_reply: true,
            ..Default::default()
        }));
        let (client, worker) = peer(route, state.clone());
        let handle = client.handle();
        handle.install_request_hook(hook(store.clone(), env.clone(), route));
        assert!(handle
            .request("turn/start", json!({"threadId":"t","input":[]}))
            .is_err());
        assert!(store
            .get("cutex.s")
            .unwrap()
            .unwrap()
            .value
            .pending
            .is_some());
        drop(client);
        worker.join().unwrap();
        drop(store);
        let reopened = Arc::new(NoteStore::open(&path).unwrap());
        let (client, worker) = peer(route, state.clone());
        let handle = client.handle();
        handle.install_request_hook(hook(reopened.clone(), env, route));
        handle
            .request("turn/start", json!({"threadId":"t","input":[]}))
            .unwrap();
        assert_eq!(state.lock().unwrap().note_submits, 1);
        assert_eq!(state.lock().unwrap().contexts.len(), 1);
        assert!(reopened
            .get("cutex.s")
            .unwrap()
            .unwrap()
            .value
            .pending
            .is_none());
        assert_eq!(
            reopened.get("cutex.s").unwrap().unwrap().value.seen.index,
            "new\n"
        );
        drop(client);
        worker.join().unwrap();
    }
}

#[test]
fn memory_injection_disabled_installs_no_idle_hook_and_sends_no_memory_note() {
    let fixture = crate::memory::switch_test_support::disabled();
    let state = Arc::new(Mutex::new(PeerState::default()));
    let (client, worker) = peer(Route::Native,state.clone());
    let handle = client.handle();
    transport::install(&handle,&fixture.record.cutex_session_id,"t",0);
    let store = NoteStore::open(&crate::config::paths::config_dir().unwrap()).unwrap();
    assert!(store.get(&fixture.record.cutex_session_id).unwrap().is_none());
    handle.request("turn/start",json!({"threadId":"t","input":[]})).unwrap();
    assert_eq!(state.lock().unwrap().note_submits,0);
    assert!(state.lock().unwrap().contexts.is_empty());
    drop(handle);
    drop(client);
    worker.join().unwrap();
}
