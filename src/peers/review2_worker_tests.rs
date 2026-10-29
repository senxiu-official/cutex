//! r226 round-two: local address refusals, unsupported stop and fixed diagnostics.
use super::{
    model, routes,
    store::{Peer, Store},
    tests, token, worker, Error,
};
use crate::asks::{
    model::*,
    store::{AskState, AskStore},
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    peers: Store,
    asks: AskStore,
    peer: Peer,
    template: AskRecord,
    routing: worker::TransportRoutingFixture,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("peer-review2-worker-{}", uuid::Uuid::new_v4()));
        let peers = Store::at(root.join("runtime/peers"));
        let asks = AskStore::at(root.join("runtime/asks"));
        let (_, mut template, mut peer) = tests::remote_record();
        let routing = worker::TransportRoutingFixture::new(&mut template);
        peer.urls = vec!["http://127.0.0.1:1".into()];
        peer.last_seen_at = Some(super::now());
        peers.insert(peer.clone(), &token::generate()).unwrap();
        peers.settings(&json!({"enabled":true})).unwrap();
        Self {
            root,
            peers,
            asks,
            peer,
            template,
            routing,
        }
    }
    fn outgoing(&self, n: usize) -> AskRecord {
        let mut a = self.template.clone();
        a.ask_id = format!("ask-{n:032x}");
        a.action_id = format!("review2-{n}");
        a.hold = Some(Hold::PeerUnreachable);
        a.remote.as_mut().unwrap().sync.pending_ops = vec![model::Op {
            op_id: crate::asks::new_id("pop"),
            op: "create".into(),
            outcome: None,
            reason: None,
        }];
        a
    }
    fn incoming(&self, n: usize) -> AskRecord {
        let mut a = self.outgoing(n);
        let request = a.remote.take().unwrap().request;
        a.requester = Requester::Peer {
            peer_id: self.peer.peer_id.clone(),
            cutex_session_id: "remote-requester".into(),
            name: None,
            label: "beta".into(),
        };
        a.hold = None;
        a.execution = ExecutionState::Answered;
        a.closed = Some(CloseOutcome::Completed);
        a.closed_at = Some(super::now());
        a.peer_inbound = Some(model::Inbound {
            request,
            dead_letters: vec![],
            queued_at: None,
            result_undeliverable: false,
            delivery_stopped: false,
            last_error: None,
            peer_deliveries: vec![],
            received_ops: vec![],
            next_attempt_at: None,
            failures: 0,
        });
        a
    }
    fn seed(&self, records: Vec<AskRecord>) {
        self.asks
            .with_state(|s| {
                *s = AskState::default();
                self.routing.seed_policy(s);
                s.asks
                    .extend(records.into_iter().map(|a| (a.ask_id.clone(), a)));
            })
            .unwrap();
    }
    fn urls(&self, urls: Vec<String>, allow_loopback: bool, probe_due: bool) {
        self.peers
            .with(|s| {
                let peer = s.peers.get_mut(&self.peer.peer_id).unwrap();
                peer.urls = urls;
                peer.allow_loopback = allow_loopback;
                if probe_due {
                    peer.next_probe_at =
                        Some((Utc::now() - chrono::Duration::hours(1)).to_rfc3339());
                }
                Ok(())
            })
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while !headers.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
        assert!(headers.len() < 16384);
    }
    let headers = String::from_utf8(headers).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (
        headers.lines().next().unwrap().to_string(),
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    )
}

fn server(
    mut reply: impl FnMut(usize, &str, &Value) -> (u16, Value) + Send + 'static,
    count: usize,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut calls = Vec::new();
        for index in 0..count {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing request {index}");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let (line, body) = read_request(&mut stream);
            let (status, response) = reply(index, &line, &body);
            let text = response.to_string();
            write!(stream, "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{text}", text.len()).unwrap();
            calls.push(line);
        }
        calls
    });
    (url, thread)
}

fn snapshot(line: &str, closed: bool) -> Value {
    let route = line.split_whitespace().nth(1).unwrap();
    let id = route
        .strip_prefix("/peer/v1/asks/")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    json!({"schemaVersion":1,"askId":id,"revision":2,
        "execution":if closed { "interrupted" } else { "queued" },
        "closed":if closed { Some("abandoned") } else { None },
        "hold":null,"review":null,"answer":null,"text":null,"interruptReason":null,"lastError":null})
}

#[test]
fn data_outbox_uses_second_url_when_first_address_is_rejected() {
    for outgoing in [true, false] {
        let f = Fixture::new();
        let (url, server) = server(
            move |_, line, data| {
                if outgoing {
                    assert!(line.starts_with("PUT /peer/v1/asks/"));
                    (200, snapshot(line, false))
                } else {
                    assert!(line.starts_with("POST /peer/v1/events "));
                    (
                        200,
                        json!({"acked":data["events"].as_array().unwrap().iter().map(|e| e["eventId"].clone()).collect::<Vec<_>>()}),
                    )
                }
            },
            1,
        );
        f.urls(vec!["http://192.0.2.1:1".into(), url.clone()], true, false);
        f.peers
            .with(|s| {
                let peer = s.peers.get_mut(&f.peer.peer_id).unwrap();
                peer.last_successful_url = None;
                // Keep this test on the data path even on a slow test host.
                peer.last_seen_at = Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
                Ok(())
            })
            .unwrap();
        f.seed(vec![if outgoing {
            f.outgoing(1)
        } else {
            f.incoming(1)
        }]);
        worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
        let peer = f.peers.get(&f.peer.peer_id).unwrap();
        assert_eq!(peer.status, "online");
        assert_eq!(peer.last_successful_url.as_deref(), Some(url.as_str()));
        assert_eq!(peer.probe_failures, 0);
        assert!(peer.last_error.is_none() && peer.next_probe_at.is_none());
        let state = f.asks.snapshot().unwrap();
        let ask = &state.asks[&format!("ask-{:032x}", 1)];
        if outgoing {
            let sync = &ask.remote.as_ref().unwrap().sync;
            assert!(sync.accepted && sync.pending_ops.is_empty());
            assert!(!sync.local_terminal && sync.last_error.is_none());
        } else {
            let inbound = ask.peer_inbound.as_ref().unwrap();
            assert!(inbound.peer_deliveries.is_empty() && inbound.dead_letters.is_empty());
            assert!(!inbound.result_undeliverable && !inbound.delivery_stopped);
            assert!(inbound.last_error.is_none());
        }
    }
}

#[test]
fn all_rejected_urls_preserve_outboxes_until_valid_address_and_hello_recovery() {
    for outgoing in [true, false] {
        for callback_refused in [true, false] {
            let f = Fixture::new();
            f.seed(
                (1..=2)
                    .map(|n| {
                        if outgoing {
                            f.outgoing(n)
                        } else {
                            f.incoming(n)
                        }
                    })
                    .collect(),
            );
            let urls = if callback_refused {
                vec!["http://127.0.0.1:1".into(), "http://[::1]:1".into()]
            } else {
                vec!["http://192.0.2.1:1".into(), "http://203.0.113.1:1".into()]
            };
            let expected = if callback_refused {
                "peer_callback_forbidden"
            } else {
                "peer_url_insecure"
            };
            f.urls(urls, false, false);
            let before = f.asks.snapshot().unwrap();
            worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
            let failed = f.peers.get(&f.peer.peer_id).unwrap();
            assert_eq!(failed.status, "unreachable");
            assert_eq!(failed.last_error.as_deref(), Some(expected));
            assert!(failed.next_probe_at.is_some());
            assert_eq!(failed.probe_failures, 1);
            let held = f.asks.snapshot().unwrap();
            for n in 1..=2 {
                let id = format!("ask-{n:032x}");
                let a = &held.asks[&id];
                assert_eq!(a.execution, before.asks[&id].execution);
                assert_eq!(a.closed, before.asks[&id].closed);
                if outgoing {
                    let r = a.remote.as_ref().unwrap();
                    assert_eq!(
                        r.sync.pending_ops,
                        before.asks[&id].remote.as_ref().unwrap().sync.pending_ops
                    );
                    assert!(!r.sync.local_terminal);
                    assert_eq!(r.sync.failures, if n == 1 { 1 } else { 0 });
                    if n == 1 {
                        assert_eq!(r.sync.last_error.as_deref(), Some(expected));
                    }
                } else {
                    let i = a.peer_inbound.as_ref().unwrap();
                    assert_eq!(
                        i.peer_deliveries,
                        before.asks[&id]
                            .peer_inbound
                            .as_ref()
                            .unwrap()
                            .peer_deliveries
                    );
                    assert!(i.dead_letters.is_empty());
                    assert!(!i.result_undeliverable && !i.delivery_stopped);
                    assert_eq!(i.failures, if n == 1 { 1 } else { 0 });
                    if n == 1 {
                        assert_eq!(i.last_error.as_deref(), Some(expected));
                    }
                }
            }
            assert_eq!(held.deliveries, before.deliveries);

            // Explicit hello refresh failures must also update the same circuit.
            let refresh = routes::refresh(&f.peers, &failed).unwrap_err();
            assert!(refresh.is_address_rejected());
            assert_eq!(f.peers.get(&f.peer.peer_id).unwrap().probe_failures, 2);

            let peer_id = f.peer.peer_id.clone();
            let (url, server) = server(
                move |index, line, data| match index {
                    0 => {
                        assert!(line.starts_with("GET /peer/v1/hello "));
                        (
                            200,
                            json!({"instanceId":peer_id,"protocol":1,"features":super::pair::FEATURES,"platform":"test"}),
                        )
                    }
                    1 => {
                        assert!(line.starts_with("GET /peer/v1/agents "));
                        (200, json!({"agents":[]}))
                    }
                    _ if outgoing => {
                        assert!(line.starts_with("PUT /peer/v1/asks/"));
                        (200, snapshot(line, false))
                    }
                    _ => {
                        assert!(line.starts_with("POST /peer/v1/events "));
                        (
                            200,
                            json!({"acked":data["events"].as_array().unwrap().iter().map(|e| e["eventId"].clone()).collect::<Vec<_>>()}),
                        )
                    }
                },
                4,
            );
            f.urls(vec![url], true, true);
            let restarted = AskStore::at(f.asks.dir());
            worker::send_peer(&f.peers, &restarted, &f.peer.peer_id).unwrap();
            assert_eq!(server.join().unwrap().len(), 4);
            let restored = f.peers.get(&f.peer.peer_id).unwrap();
            assert_eq!(restored.status, "online");
            assert_eq!(restored.probe_failures, 0);
            assert!(restored.next_probe_at.is_none());
            let state = restarted.snapshot().unwrap();
            for a in state.asks.values() {
                if outgoing {
                    let r = a.remote.as_ref().unwrap();
                    assert!(r.sync.accepted && r.sync.pending_ops.is_empty());
                } else {
                    let i = a.peer_inbound.as_ref().unwrap();
                    assert!(i.peer_deliveries.is_empty() && i.dead_letters.is_empty());
                    assert!(!i.result_undeliverable && !i.delivery_stopped);
                }
            }
        }
    }
}

#[test]
fn incompatible_stop_preserves_close_across_restart_and_one_terminal_delivery() {
    for close_accepted in [true, false] {
        let f = Fixture::new();
        let (url, server) = server(
            move |index, line, data| {
                assert!(line.starts_with("POST /peer/v1/asks/") && line.contains("/ops "));
                if index == 0 {
                    assert_eq!(data["op"], "stop");
                    (400, json!({"code":"peer_protocol_error","retryable":false}))
                } else {
                    assert_eq!(data["op"], "close");
                    assert_eq!(data["outcome"], "abandoned");
                    assert_eq!(data["reason"], "peer_request_incompatible");
                    if close_accepted {
                        (200, snapshot(line, true))
                    } else {
                        (400, json!({"code":"peer_protocol_error","retryable":false}))
                    }
                }
            },
            2,
        );
        f.urls(vec![url], true, false);
        let mut ask = f.outgoing(1);
        ask.execution = ExecutionState::Running;
        ask.hold = None;
        let r = ask.remote.as_mut().unwrap();
        r.sync.accepted = true;
        r.sync.last_remote_revision = 1;
        r.sync.pending_ops[0].op = "stop".into();
        let old_stop = r.sync.pending_ops[0].op_id.clone();
        f.seed(vec![ask]);
        worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
        let id = format!("ask-{:032x}", 1);
        let stopped = f.asks.snapshot().unwrap();
        let local = &stopped.asks[&id];
        assert_eq!(local.execution, ExecutionState::Interrupted);
        assert_eq!(local.closed, Some(CloseOutcome::Stopped));
        let r = local.remote.as_ref().unwrap();
        assert!(r.sync.local_terminal);
        assert_eq!(r.sync.state, "outcomeUnknown");
        assert_eq!(
            r.sync.interrupt_reason,
            Some(InterruptReason::OutcomeUnknown)
        );
        assert_eq!(
            r.sync.last_error.as_deref(),
            Some("peer_request_incompatible")
        );
        assert_eq!(r.sync.pending_ops.len(), 1);
        let close = &r.sync.pending_ops[0];
        assert_eq!(close.op, "close");
        assert_ne!(close.op_id, old_stop);
        assert_eq!(
            stopped
                .deliveries
                .values()
                .filter(|d| d.ask_id == id)
                .count(),
            1
        );

        let restarted = AskStore::at(f.asks.dir());
        assert_eq!(
            restarted.snapshot().unwrap().asks[&id]
                .remote
                .as_ref()
                .unwrap()
                .sync
                .pending_ops[0],
            *close
        );
        worker::send_peer(&f.peers, &restarted, &f.peer.peer_id).unwrap();
        assert_eq!(server.join().unwrap().len(), 2);
        let final_state = restarted.snapshot().unwrap();
        let local = &final_state.asks[&id];
        assert_eq!(local.execution, ExecutionState::Interrupted);
        assert_eq!(local.closed, Some(CloseOutcome::Stopped));
        let r = local.remote.as_ref().unwrap();
        assert!(r.sync.local_terminal && r.sync.pending_ops.is_empty());
        assert_eq!(r.sync.state, "outcomeUnknown");
        assert_eq!(
            r.sync.interrupt_reason,
            Some(InterruptReason::OutcomeUnknown)
        );
        assert_eq!(final_state.deliveries, stopped.deliveries);
        assert_eq!(f.peers.get(&f.peer.peer_id).unwrap().status, "online");
        if !close_accepted {
            // This diagnosis deliberately does not claim that execution stopped
            // remotely; the local outcome remains unknown and is delivered once.
            assert_eq!(
                r.sync.last_error.as_deref(),
                Some("peer_request_incompatible")
            );
        }
    }
}

#[test]
fn unrecognized_remote_error_codes_never_become_state_or_dead_letter_reasons() {
    const UNTRUSTED: &str = "untrusted_remote_diagnostic_marker";
    let (url, server) = server(
        |_, _, _| {
            (
                403,
                json!({"code":UNTRUSTED,"message":UNTRUSTED,"retryable":true}),
            )
        },
        2,
    );
    let f = Fixture::new();
    f.urls(vec![url], true, false);
    f.seed(vec![f.outgoing(1), f.incoming(2)]);
    worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
    assert_eq!(server.join().unwrap().len(), 2);
    let state = f.asks.snapshot().unwrap();
    let outgoing = &state.asks[&format!("ask-{:032x}", 1)];
    assert_eq!(outgoing.execution, ExecutionState::Failed);
    assert_eq!(
        outgoing.remote.as_ref().unwrap().sync.state,
        "peer_request_failed"
    );
    assert_eq!(
        outgoing.remote.as_ref().unwrap().sync.last_error.as_deref(),
        Some("peer_request_failed")
    );
    let incoming = state.asks[&format!("ask-{:032x}", 2)]
        .peer_inbound
        .as_ref()
        .unwrap();
    assert!(incoming.result_undeliverable && incoming.delivery_stopped);
    assert_eq!(incoming.dead_letters.len(), 1);
    assert_eq!(incoming.dead_letters[0].code, "peer_request_failed");
    assert_eq!(incoming.last_error.as_deref(), Some("peer_request_failed"));
    assert!(!serde_json::to_string(&state).unwrap().contains(UNTRUSTED));
    assert_eq!(f.peers.get(&f.peer.peer_id).unwrap().status, "online");

    // The worker also canonicalizes errors originating outside HTTP decoding.
    worker::note_error(&f.peers, &f.peer, &Error::new(UNTRUSTED, 503)).unwrap();
    let peer = f.peers.get(&f.peer.peer_id).unwrap();
    assert_eq!(peer.last_error.as_deref(), Some("peer_request_failed"));
    assert!(!serde_json::to_string(&peer.view())
        .unwrap()
        .contains(UNTRUSTED));
}
