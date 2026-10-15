//! Cross-host agent_tell: the receiving route (policy, dedupe, delivery) and
//! the sending tool against a scripted peer host.
use super::{policy, store::Store, tells, tests::peer, token};
use crate::asks::{
    scheduler::tests::world,
    store::tests::{temp_store, REQUESTER},
    tools::{self, TellSend, ToolContext},
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const LOCAL: &str = "cutex.00000000-0000-0000-0000-0000000000c1";
const OTHER: &str = "cutex.00000000-0000-0000-0000-0000000000c2";
const REMOTE: &str = "cutex.00000000-0000-0000-0000-0000000000d1";

struct Instance {
    root: std::path::PathBuf,
    previous: crate::config::instance::Selection,
}

impl Instance {
    fn new(enabled: bool) -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("peer-tells-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = crate::config::instance::selected().unwrap();
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
            root.join(".cutex"),
        ));
        Store::open()
            .unwrap()
            .settings(&json!({"enabled":enabled}))
            .unwrap();
        Self { root, previous }
    }

    fn agents(&self, agents: &[(&str, &str)]) {
        let mut sessions = crate::session::model::CutexSessionStore::default();
        for (id, name) in agents {
            let mut record = crate::session::model::CutexSessionRecord::new(
                (*id).into(),
                None,
                "fixture-host".into(),
                self.root.to_string_lossy().into(),
                None,
            )
            .unwrap();
            record.agent_enabled = true;
            record.formal_agent_name = Some((*name).into());
            sessions.sessions.insert((*id).into(), record);
        }
        crate::session::store::save_cutex_session_store(&sessions).unwrap();
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        crate::config::instance::set_test_selection(self.previous.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn request(id: &str, target: &str, text: &str) -> Value {
    json!({"schemaVersion":1,"tellId":id,"requester":{"cutexSessionId":REMOTE,"name":"remote-worker"},"target":target,"text":text})
}

fn tell_id(n: u8) -> String {
    format!("ptl-{:032x}", n)
}

type Delivered = RefCell<
    Vec<(
        String,
        String,
        crate::agent_bus::peer_tell::PeerTellFacts,
        String,
    )>,
>;

fn recorder(delivered: &Delivered) -> impl Fn(&tells::Delivery<'_>) -> super::Result<String> + '_ {
    move |d: &tells::Delivery<'_>| {
        delivered.borrow_mut().push((
            d.target.into(),
            d.text.into(),
            d.facts.clone(),
            d.message_id.into(),
        ));
        Ok(format!("bus-{}", delivered.borrow().len()))
    }
}

#[test]
fn receive_delivers_once_dedupes_and_applies_inbound_policy() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = Instance::new(true);
    instance.agents(&[(LOCAL, "local-worker"), (OTHER, "private-worker")]);
    let store = Store::open().unwrap();
    let mut p = peer();
    p.label = "alpha".into();
    p.policy.inbound.exports = vec![policy::Export::Agent {
        agent: LOCAL.into(),
    }];
    let receipts = store.dir.join("tell-receipts.json");
    let delivered = Delivered::default();
    let deliver = recorder(&delivered);

    // Unexported and unknown targets: same refusal, nothing delivered or stored.
    for target in [OTHER, "cutex.unknown"] {
        let e =
            tells::receive(&store, &p, request(&tell_id(1), target, "fyi"), &deliver).unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.status),
            ("recipient_outside_boundary", 403)
        );
    }
    assert!(!receipts.exists(), "receipts are created on use only");

    let first = tells::receive(
        &store,
        &p,
        request(&tell_id(1), LOCAL, "fyi /tmp/a"),
        &deliver,
    )
    .unwrap();
    assert_eq!(first["tellId"], tell_id(1));
    assert_eq!(first["deliveryKind"], "note");
    assert_eq!(first["target"]["name"], "local-worker");
    assert!(first.get("deduplicated").is_none());
    {
        let sent = delivered.borrow();
        assert_eq!(sent.len(), 1);
        let (target, text, facts, message_id) = &sent[0];
        assert_eq!((target.as_str(), text.as_str()), (LOCAL, "fyi /tmp/a"));
        assert_eq!(facts.sender_label(), "remote-worker@alpha");
        assert_eq!(facts.peer_id, p.peer_id);
        assert_eq!(facts.sender_id, REMOTE);
        assert!(message_id.starts_with(crate::agent_bus::peer_tell::MESSAGE_ID_PREFIX));
    }
    // A retry (lost response) answers from the receipt; no second note.
    let again = tells::receive(
        &store,
        &p,
        request(&tell_id(1), LOCAL, "fyi /tmp/a"),
        &deliver,
    )
    .unwrap();
    assert_eq!(again["deduplicated"], true);
    assert_eq!(again["messageId"], first["messageId"]);
    assert_eq!(delivered.borrow().len(), 1);
    // The same ID with other content is a conflict.
    let e =
        tells::receive(&store, &p, request(&tell_id(1), LOCAL, "changed"), &deliver).unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("idempotency_conflict", 409));
    // Malformed requests.
    for bad in [
        request("tell-1", LOCAL, "x"),
        request(&tell_id(2), LOCAL, " "),
        request(&tell_id(2), LOCAL, &"x".repeat(tells::TEXT_MAX + 1)),
    ] {
        assert_eq!(
            tells::receive(&store, &p, bad, &deliver).unwrap_err().code,
            "invalid_request"
        );
    }
    let mut future = request(&tell_id(2), LOCAL, "x");
    future["schemaVersion"] = json!(2);
    assert_eq!(
        tells::receive(&store, &p, future, &deliver)
            .unwrap_err()
            .code,
        "peer_protocol_error"
    );

    // inbound.accept=false refuses like asks.
    let mut closed = p.clone();
    closed.policy.inbound.asks = false;
    let e =
        tells::receive(&store, &closed, request(&tell_id(3), LOCAL, "x"), &deliver).unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("peer_asks_disabled", 403));
    // A disabled peer relationship refuses.
    let mut disabled = p.clone();
    disabled.enabled = false;
    assert_eq!(
        tells::receive(
            &store,
            &disabled,
            request(&tell_id(3), LOCAL, "x"),
            &deliver
        )
        .unwrap_err()
        .code,
        "peer_unavailable"
    );
    // The hourly limit counts this peer's tells.
    let mut limited = p.clone();
    limited.policy.inbound.asks_per_hour = 1;
    let e =
        tells::receive(&store, &limited, request(&tell_id(4), LOCAL, "x"), &deliver).unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("peer_rate_limited", 429));
    assert_eq!(delivered.borrow().len(), 1);
    // A failed local delivery stores no receipt: the retry delivers.
    let failing = |_: &tells::Delivery<'_>| -> super::Result<String> {
        Err(super::Error::new("bus_unavailable", 503))
    };
    assert_eq!(
        tells::receive(&store, &p, request(&tell_id(5), LOCAL, "y"), &failing)
            .unwrap_err()
            .status,
        503
    );
    tells::receive(&store, &p, request(&tell_id(5), LOCAL, "y"), &deliver).unwrap();
    assert_eq!(delivered.borrow().len(), 2);

    // Globally disabled peers refuse before any target lookup.
    store.settings(&json!({"enabled":false})).unwrap();
    assert_eq!(
        tells::receive(&store, &p, request(&tell_id(6), LOCAL, "x"), &deliver)
            .unwrap_err()
            .code,
        "peer_unavailable"
    );
    drop(instance);
}

/// A scripted peer host: answers each request with the next (status, body)
/// and records the request lines and bodies.
fn scripted(
    responses: Vec<(u16, Value)>,
) -> (
    String,
    Arc<Mutex<Vec<(String, Value)>>>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    let task = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        for (status, body) in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("listener: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = vec![];
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let length = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut data = vec![0u8; length];
            stream.read_exact(&mut data).unwrap();
            log.lock().unwrap().push((
                head.lines().next().unwrap().to_string(),
                serde_json::from_slice(&data).unwrap_or(Value::Null),
            ));
            let body = body.to_string();
            stream
                .write_all(format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .unwrap();
        }
    });
    (url, seen, task)
}

fn remote_peer(url: &str) -> super::store::Peer {
    let store = Store::open().unwrap();
    let mut p = peer();
    p.urls = vec![url.into()];
    p.inbound_token_sha256 = token::hash(&token::generate());
    p.policy.outbound.visibility = policy::Visibility::All;
    store.insert(p.clone(), &token::generate()).unwrap();
    crate::config::atomic::write_private_pretty_json_atomic(
        &store.dir.join(format!("directory-{}.json", p.peer_id)),
        &json!({"fetchedAt":super::now(),"agents":[{"cutexSessionId":REMOTE,"name":"worker","short":"d1","status":"idle","runtime":"codex"}]}),
        "directory fixture",
    )
    .unwrap();
    p
}

fn call(
    tool: &str,
    args: Value,
    action_id: &str,
    store: &crate::asks::store::AskStore,
    w: &crate::asks::scheduler::tests::FakeWorld,
    local_sends: &RefCell<usize>,
) -> Value {
    let send = |_: &TellSend<'_>| {
        *local_sends.borrow_mut() += 1;
        Ok(json!({"ok":true}))
    };
    let chain = |_: &str, _: &str, _: &str| None;
    let clock = chrono::Utc::now;
    let sleep = || ();
    let ctx = ToolContext {
        store,
        world: w,
        caller: REQUESTER,
        now: chrono::Utc::now(),
        send: &send,
        wake_chain: &chain,
        clock: &clock,
        sleep: &sleep,
        wait: true,
        facade: tools::FACADE_DELEGATIONS,
    };
    tools::handle(
        &ctx,
        json!({"tool":tool,"arguments":args,"action_id":action_id})
            .to_string()
            .as_bytes(),
    )
}

#[test]
fn agent_tell_to_name_at_host_sends_once_and_maps_refusals() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = Instance::new(true);
    let (w, _wd) = world("xtell");
    let (asks, _sd) = temp_store("xtell");
    let local = RefCell::new(0usize);
    let ok = json!({"schemaVersion":1,"tellId":"x","messageId":"bus-1","deliveryKind":"note","recipientState":"busy","target":{"cutexSessionId":REMOTE,"name":"worker"}});
    let (url, seen, server) = scripted(vec![
        (200, ok),
        (403, json!({"code":"recipient_outside_boundary"})),
        (404, json!({"code":"not_found"})),
    ]);
    let p = remote_peer(&url);

    let sent = call(
        "agent_tell",
        json!({"to":"worker@beta","text":"fyi: see /tmp/a"}),
        "t1",
        &asks,
        &w,
        &local,
    );
    assert_eq!(sent["ok"], true, "{sent}");
    assert_eq!(sent["to"], "worker@beta");
    assert_eq!(sent["host"], "beta");
    assert_eq!(sent["peerId"], p.peer_id);
    assert_eq!(sent["deliveryKind"], "note");
    assert_eq!(sent["recipientState"], "busy");
    assert!(sent["detail"]
        .as_str()
        .unwrap()
        .starts_with("Delivered to host beta."));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].0.starts_with("POST /peer/v1/tells "));
        let body = &seen[0].1;
        assert_eq!(body["target"], REMOTE);
        assert_eq!(body["text"], "fyi: see /tmp/a");
        assert_eq!(body["requester"]["cutexSessionId"], REQUESTER);
        assert_eq!(body["requester"]["name"], "Eva");
        assert!(body["tellId"].as_str().unwrap().starts_with("ptl-"));
        assert_eq!(body["tellId"], sent["tellId"]);
    }
    // The same action replays locally without another request.
    let replay = call(
        "agent_tell",
        json!({"to":"worker@beta","text":"fyi: see /tmp/a"}),
        "t1",
        &asks,
        &w,
        &local,
    );
    assert_eq!(replay["tellId"], sent["tellId"]);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(*local.borrow(), 0, "never the local bus");

    // Options that cannot cross hosts are refused before any request.
    for (args, needle) in [
        (json!({"to":"worker@beta","text":"x","wake":true}), "wake"),
        (
            json!({"to":"worker@beta","text":"x","attachments":["a.png"]}),
            "attachments",
        ),
        (
            json!({"to":"worker@beta","text":"x","supersedes":"v2"}),
            "revisions",
        ),
    ] {
        let refused = call("agent_tell", args, "t2", &asks, &w, &local);
        assert_eq!(refused["code"], "remote_unsupported", "{refused}");
        assert_eq!(refused["status"], "no_write");
        assert!(
            refused["detail"].as_str().unwrap().contains(needle),
            "{refused}"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 1);

    // A remote policy refusal passes through; nothing is kept locally.
    let refused = call(
        "agent_tell",
        json!({"to":"worker@beta","text":"x"}),
        "t3",
        &asks,
        &w,
        &local,
    );
    assert_eq!(refused["code"], "recipient_outside_boundary", "{refused}");
    // An older peer without the route.
    let old = call(
        "agent_tell",
        json!({"to":"worker@beta","text":"x"}),
        "t4",
        &asks,
        &w,
        &local,
    );
    assert_eq!(old["code"], "remote_unsupported", "{old}");
    assert_eq!(old["detail"], tells::UNSUPPORTED_DETAIL);
    server.join().unwrap();
    assert_eq!(seen.lock().unwrap().len(), 3);
    let state = asks.snapshot().unwrap();
    assert!(state.asks.is_empty(), "a tell never creates a delegation");

    // Unknown remote names and disabled peers are refused like delegation.
    let missing = call(
        "agent_tell",
        json!({"to":"worker@gamma","text":"x"}),
        "t5",
        &asks,
        &w,
        &local,
    );
    assert_eq!(missing["code"], "peer_not_found", "{missing}");
    Store::open()
        .unwrap()
        .settings(&json!({"enabled":false}))
        .unwrap();
    let off = call(
        "agent_tell",
        json!({"to":"worker@beta","text":"x"}),
        "t6",
        &asks,
        &w,
        &local,
    );
    assert_eq!(off["code"], "peer_unavailable", "{off}");
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[test]
fn alarm_and_other_tools_refuse_remote_targets_with_a_clear_detail() {
    crate::asks::short_ids::tests::context("xtell-alarm", |ctx| {
        let alarm =
            crate::asks::short_ids::normalize(ctx, "alarm_set", json!({"to":"worker@beta"}))
                .unwrap_err();
        assert_eq!(alarm.code, "remote_unsupported");
        assert!(alarm.detail.contains("alarms are local to a host"));
        let archive = crate::asks::short_ids::normalize(
            ctx,
            "agent_archive",
            json!({"cutex_session_id":"worker@beta"}),
        )
        .unwrap_err();
        assert_eq!(archive.code, "remote_unsupported");
        // agent_tell keeps name@host for the remote path.
        assert_eq!(
            crate::asks::short_ids::normalize(ctx, "agent_tell", json!({"to":"worker@beta"}))
                .unwrap()["to"],
            "worker@beta"
        );
        // The facade's ID preflight sees the model's arguments before
        // `tools::handle` removes `action_id`: a retry key must pass.
        let args = json!({"to":"worker@beta","text":"x","action_id":"retry-1"});
        assert_eq!(
            crate::asks::short_ids::normalize(ctx, "agent_tell", args.clone()).unwrap(),
            args
        );
    });
}

#[test]
fn remote_tell_detail_never_suggests_wake() {
    for state in ["idle", "busy", "offline"] {
        let detail =
            tools::tell_detail_for("worker@beta", state, false, false, tools::FACADE_DELEGATIONS, false);
        assert!(!detail.contains("wake=true"), "{detail}");
    }
    let local = tools::tell_detail("worker", "idle", false, false, tools::FACADE_DELEGATIONS);
    assert!(local.contains("wake=true"), "local tells keep the hint: {local}");
}

#[test]
fn hello_advertises_tells_without_requiring_it() {
    let features = super::pair::features();
    assert!(features.iter().any(|f| f == tells::FEATURE));
    let mut p = peer();
    let old = json!({"instanceId":p.peer_id,"protocol":1,"features":super::pair::FEATURES});
    super::pair::validate_hello(&p, &old).unwrap();
    assert!(!super::pair::negotiated_features(&old)
        .iter()
        .any(|f| f == tells::FEATURE));
    p.features = super::pair::declared_features(&json!({"features":["asks.v1","tells.v1"]}));
    assert!(p.features.iter().any(|f| f == tells::FEATURE));
}
