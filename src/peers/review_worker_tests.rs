//! r226 worker regressions: bounded maintenance, request isolation and durable close.
use super::{
    model,
    store::{Peer, Store},
    tests, token, worker,
};
use crate::asks::{
    model::*,
    store::{AskState, AskStore},
};
use crate::platform::file_stamp::FileStamp;
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
}
impl Fixture {
    fn new(url: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("peer-review-worker-{}", uuid::Uuid::new_v4()));
        let peers = Store::at(root.join("runtime/peers"));
        let asks = AskStore::at(root.join("runtime/asks"));
        let (_, template, mut peer) = tests::remote_record();
        peer.urls = vec![url.into()];
        peer.last_seen_at = Some(super::now());
        peers.insert(peer.clone(), &token::generate()).unwrap();
        peers.settings(&json!({"enabled":true})).unwrap();
        Self {
            root,
            peers,
            asks,
            peer,
            template,
        }
    }
    fn outgoing(&self, n: usize, pending: bool) -> AskRecord {
        let mut a = self.template.clone();
        a.ask_id = format!("ask-{n:032x}");
        a.action_id = format!("review-{n}");
        a.hold = Some(Hold::PeerUnreachable);
        let remote = a.remote.as_mut().unwrap();
        remote.sync.pending_ops = if pending {
            vec![model::Op {
                op_id: crate::asks::new_id("pop"),
                op: "create".into(),
                outcome: None,
                reason: None,
            }]
        } else {
            vec![]
        };
        a
    }
    fn incoming(&self, n: usize, closed: bool) -> AskRecord {
        let mut a = self.outgoing(n, false);
        let request = a.remote.take().unwrap().request;
        a.requester = Requester::Peer {
            peer_id: self.peer.peer_id.clone(),
            cutex_session_id: "remote-requester".into(),
            name: None,
            label: "beta".into(),
        };
        a.hold = None;
        a.execution = if closed {
            ExecutionState::Answered
        } else {
            ExecutionState::Queued
        };
        a.closed = closed.then_some(CloseOutcome::Completed);
        a.closed_at = closed.then(super::now);
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
                s.asks
                    .extend(records.into_iter().map(|a| (a.ask_id.clone(), a)));
            })
            .unwrap();
    }
    fn history(&self, n: usize) -> Vec<AskRecord> {
        (1000..1000 + n)
            .map(|i| {
                let mut a = if i % 2 == 0 {
                    self.outgoing(i, false)
                } else {
                    self.incoming(i, true)
                };
                a.execution = ExecutionState::Answered;
                a.closed = Some(CloseOutcome::Completed);
                a.closed_at = Some(super::now());
                if let Some(r) = &mut a.remote {
                    r.sync.local_terminal = true;
                }
                if let Some(i) = &mut a.peer_inbound {
                    i.delivery_stopped = true;
                }
                a
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn request(stream: &mut TcpStream) -> (String, Value) {
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
            let (line, body) = request(&mut stream);
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
fn history_only_maintenance_never_opens_a_write_transaction() {
    for status in ["missing", "removedByPeer"] {
        let f = Fixture::new("http://127.0.0.1:1");
        f.seed(f.history(1000));
        if status == "missing" {
            f.peers.remove(&f.peer.peer_id).unwrap();
        } else {
            f.peers
                .with(|s| {
                    let peer = s.peers.get_mut(&f.peer.peer_id).unwrap();
                    peer.status = "removedByPeer".into();
                    peer.enabled = false;
                    Ok(())
                })
                .unwrap();
        }
        let path = f.asks.dir().join("asks-v1.json");
        let before = f.asks.snapshot().unwrap();
        let stamp = FileStamp::of(&path).unwrap();
        worker::take_maintenance_transactions();
        let started = Instant::now();
        for _ in 0..3 {
            worker::maintain(&f.peers, &f.asks).unwrap();
        }
        eprintln!(
            "r226 debug maintain: {status}, 1000 historical asks, 3 idle sweeps: {:?}",
            started.elapsed()
        );
        worker::removed(&f.asks, &f.peer.peer_id).unwrap();
        assert_eq!(worker::take_maintenance_transactions(), 0, "{status}");
        assert_eq!(f.asks.snapshot().unwrap(), before, "{status}");
        assert_eq!(FileStamp::of(&path).unwrap(), stamp, "{status}");
    }
}

#[test]
fn removed_peer_work_is_batched_once_without_rewriting_history() {
    let f = Fixture::new("http://127.0.0.1:1");
    let history = f.history(500);
    let mut records = history.clone();
    records.extend((1..=20).map(|i| f.outgoing(i, true)));
    records.extend((21..=40).map(|i| f.incoming(i, false)));
    f.seed(records);
    f.asks
        .with_state(|s| {
            s.peer_cancellations.insert(
                "ask-cancelled".into(),
                (
                    f.peer.peer_id.clone(),
                    model::Op {
                        op_id: "op-cancelled".into(),
                        op: "close".into(),
                        outcome: None,
                        reason: None,
                    },
                ),
            );
        })
        .unwrap();
    f.peers.remove(&f.peer.peer_id).unwrap();
    let before = f.asks.snapshot().unwrap();
    worker::take_maintenance_transactions();
    worker::maintain(&f.peers, &f.asks).unwrap();
    assert_eq!(worker::take_maintenance_transactions(), 1);
    let after = f.asks.snapshot().unwrap();
    assert_eq!(after.revision, before.revision + 1);
    assert!(after.peer_cancellations.is_empty());
    for old in history {
        assert_eq!(after.asks[&old.ask_id], old);
    }
    for n in 1..=40 {
        let a = &after.asks[&format!("ask-{n:032x}")];
        assert!(!a.is_open());
        assert!(a
            .remote
            .as_ref()
            .is_none_or(|r| r.sync.pending_ops.is_empty()));
        assert!(a
            .peer_inbound
            .as_ref()
            .is_none_or(|i| i.peer_deliveries.is_empty()));
    }
    worker::maintain(&f.peers, &f.asks).unwrap();
    assert_eq!(worker::take_maintenance_transactions(), 0);
    assert_eq!(f.asks.snapshot().unwrap(), after);
}

#[test]
fn unrelated_history_and_future_retries_do_not_repeat_policy_reads() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let f = Fixture::new(&format!("http://{}", listener.local_addr().unwrap()));
    let mut records = f.history(1000);
    let mut future = f.outgoing(1, true);
    future.remote.as_mut().unwrap().sync.next_attempt_at =
        Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
    records.push(future);
    let mut unrelated = f.outgoing(2, true);
    unrelated.remote.as_mut().unwrap().peer_id = tests::peer().peer_id;
    records.push(unrelated);
    f.seed(records);
    worker::take_send_store_reads();
    worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
    assert_eq!(worker::take_send_store_reads(), 1);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn one_incompatible_create_fails_only_that_ask_and_later_work_is_sent() {
    let (url, server) = server(
        |index, line, _| {
            assert!(line.starts_with("PUT /peer/v1/asks/"));
            if index == 0 {
                (400, json!({"code":"peer_protocol_error","retryable":false}))
            } else {
                (200, snapshot(line, false))
            }
        },
        2,
    );
    let f = Fixture::new(&url);
    let mut records = f.history(250);
    records.extend([f.outgoing(1, true), f.outgoing(2, true)]);
    f.seed(records);
    worker::take_send_store_reads();
    worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
    assert_eq!(server.join().unwrap().len(), 2);
    assert_eq!(worker::take_send_store_reads(), 3);
    let state = f.asks.snapshot().unwrap();
    let bad = &state.asks[&format!("ask-{:032x}", 1)];
    assert_eq!(bad.execution, ExecutionState::Failed);
    assert_eq!(
        bad.remote.as_ref().unwrap().sync.last_error.as_deref(),
        Some("peer_request_incompatible")
    );
    let good = &state.asks[&format!("ask-{:032x}", 2)];
    assert!(good.remote.as_ref().unwrap().sync.accepted);
    assert!(good.remote.as_ref().unwrap().sync.pending_ops.is_empty());
    let peer = f.peers.get(&f.peer.peer_id).unwrap();
    assert_eq!(peer.status, "online");
    assert!(peer.last_error.is_none() && peer.next_probe_at.is_none());
}

#[test]
fn one_incompatible_event_dead_letters_only_that_ask_and_later_events_are_sent() {
    let (url, server) = server(
        |index, line, data| {
            assert!(line.starts_with("POST /peer/v1/events "));
            if index == 0 {
                (400, json!({"code":"peer_protocol_error","retryable":false}))
            } else {
                (
                    200,
                    json!({"acked":data["events"].as_array().unwrap().iter().map(|e| e["eventId"].clone()).collect::<Vec<_>>()}),
                )
            }
        },
        2,
    );
    let f = Fixture::new(&url);
    f.seed(vec![f.incoming(1, true), f.incoming(2, true)]);
    worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
    assert_eq!(server.join().unwrap().len(), 2);
    let state = f.asks.snapshot().unwrap();
    let bad = state.asks[&format!("ask-{:032x}", 1)]
        .peer_inbound
        .as_ref()
        .unwrap();
    assert!(bad.result_undeliverable && bad.delivery_stopped && bad.peer_deliveries.is_empty());
    assert_eq!(bad.dead_letters.len(), 1);
    assert_eq!(bad.dead_letters[0].code, "peer_request_incompatible");
    let good = state.asks[&format!("ask-{:032x}", 2)]
        .peer_inbound
        .as_ref()
        .unwrap();
    assert!(good.peer_deliveries.is_empty());
    assert!(!good.result_undeliverable);
    assert_eq!(f.peers.get(&f.peer.peer_id).unwrap().status, "online");
}

#[test]
fn policy_is_reloaded_after_a_slow_request_before_the_next_request() {
    let (sender, receiver) = std::sync::mpsc::channel::<Store>();
    let (url, server) = server(
        move |_, line, _| {
            receiver
                .recv()
                .unwrap()
                .settings(&json!({"enabled":false}))
                .unwrap();
            (200, snapshot(line, false))
        },
        1,
    );
    let f = Fixture::new(&url);
    f.seed(vec![f.outgoing(1, true), f.outgoing(2, true)]);
    sender.send(f.peers.clone()).unwrap();
    worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
    assert_eq!(server.join().unwrap().len(), 1);
    let state = f.asks.snapshot().unwrap();
    let queued = &state.asks[&format!("ask-{:032x}", 2)]
        .remote
        .as_ref()
        .unwrap()
        .sync;
    assert!(!queued.accepted);
    assert_eq!(queued.failures, 0);
    assert_eq!(queued.pending_ops[0].op, "create");
}

#[test]
fn disable_keeps_a_durable_close_that_flushes_after_reenable_and_restart() {
    for global in [false, true] {
        let (url, server) = server(
            |_, line, data| {
                assert!(line.starts_with("POST /peer/v1/asks/") && line.contains("/ops "));
                assert_eq!(data["op"], "close");
                assert_eq!(data["outcome"], "abandoned");
                (200, snapshot(line, true))
            },
            1,
        );
        let f = Fixture::new(&url);
        let mut a = f.outgoing(1, false);
        a.remote.as_mut().unwrap().sync.accepted = true;
        f.seed(vec![a]);
        if global {
            f.peers.settings(&json!({"enabled":false})).unwrap();
        } else {
            f.peers
                .with(|s| {
                    s.peers.get_mut(&f.peer.peer_id).unwrap().enabled = false;
                    Ok(())
                })
                .unwrap();
        }
        worker::maintain(&f.peers, &f.asks).unwrap();
        let disabled = f.asks.snapshot().unwrap();
        let id = format!("ask-{:032x}", 1);
        let terminated = &disabled.asks[&id];
        assert_eq!(terminated.execution, ExecutionState::Interrupted);
        let reason = if global {
            "peersDisabled"
        } else {
            "peerDisabled"
        };
        assert_eq!(
            terminated.remote.as_ref().unwrap().sync.pending_ops[0]
                .reason
                .as_deref(),
            Some(reason)
        );
        worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
        assert_eq!(f.asks.snapshot().unwrap().deliveries, disabled.deliveries);
        if global {
            f.peers.settings(&json!({"enabled":true})).unwrap();
        } else {
            f.peers
                .with(|s| {
                    s.peers.get_mut(&f.peer.peer_id).unwrap().enabled = true;
                    Ok(())
                })
                .unwrap();
        }
        let restarted = AskStore::at(f.asks.dir().to_path_buf());
        worker::send_peer(&f.peers, &restarted, &f.peer.peer_id).unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
        let state = restarted.snapshot().unwrap();
        assert!(state.asks[&id]
            .remote
            .as_ref()
            .unwrap()
            .sync
            .pending_ops
            .is_empty());
        assert_eq!(state.asks[&id].execution, ExecutionState::Interrupted);
        assert_eq!(state.deliveries, disabled.deliveries);
    }
}

#[test]
fn response_decode_errors_and_rejected_credentials_still_stop_the_peer_pass() {
    for (status, response, expected) in [
        (200, json!({"malformed":"snapshot"}), "versionMismatch"),
        (
            401,
            json!({"code":"peer_credential_required"}),
            "credentialRejected",
        ),
    ] {
        let (url, server) = server(move |_, _, _| (status, response.clone()), 1);
        let f = Fixture::new(&url);
        f.seed(vec![f.outgoing(1, true), f.outgoing(2, true)]);
        worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
        let peer = f.peers.get(&f.peer.peer_id).unwrap();
        assert_eq!(peer.status, expected);
        assert!(peer.next_probe_at.is_some());
        assert_eq!(peer.probe_failures, 1);
        worker::maintain(&f.peers, &f.asks).unwrap();
        let state = f.asks.snapshot().unwrap();
        for n in 1..=2 {
            let a = &state.asks[&format!("ask-{n:032x}")];
            assert!(a.is_open());
            assert_eq!(a.remote.as_ref().unwrap().sync.pending_ops[0].op, "create");
            assert_eq!(
                a.hold,
                Some(if expected == "credentialRejected" {
                    Hold::PeerUnreachable
                } else {
                    Hold::PeerVersionMismatch
                })
            );
        }
        f.peers
            .with(|s| {
                s.peers.get_mut(&f.peer.peer_id).unwrap().next_probe_at =
                    Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
                Ok(())
            })
            .unwrap();
        worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
        assert_eq!(f.peers.get(&f.peer.peer_id).unwrap().probe_failures, 1);
    }
}

#[test]
fn maintenance_rechecks_contact_and_outbox_age_after_concurrent_updates() {
    let f = Fixture::new("http://127.0.0.1:1");
    let old = (Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    let mut outgoing = f.outgoing(1, false);
    outgoing.created_at = old.clone();
    let remote = outgoing.remote.as_mut().unwrap();
    remote.sync.accepted = false;
    remote.sync.last_received_at = old.clone();
    let mut accepted_but_stale = outgoing.clone();
    accepted_but_stale.ask_id = format!("ask-{:032x}", 3);
    f.seed(vec![outgoing, f.incoming(2, true), accepted_but_stale]);
    let outgoing_id = format!("ask-{:032x}", 1);
    let incoming_id = format!("ask-{:032x}", 2);
    let stale_id = format!("ask-{:032x}", 3);
    f.asks
        .with_state(|s| {
            s.asks
                .get_mut(&incoming_id)
                .unwrap()
                .peer_inbound
                .as_mut()
                .unwrap()
                .queued_at = Some(old);
        })
        .unwrap();
    worker::maintain_after_inspect(&f.peers, &f.asks, || {
        f.asks
            .with_state(|s| {
                s.asks
                    .get_mut(&outgoing_id)
                    .unwrap()
                    .remote
                    .as_mut()
                    .unwrap()
                    .sync
                    .accepted = true;
                s.asks
                    .get_mut(&stale_id)
                    .unwrap()
                    .remote
                    .as_mut()
                    .unwrap()
                    .sync
                    .accepted = true;
                s.asks
                    .get_mut(&outgoing_id)
                    .unwrap()
                    .remote
                    .as_mut()
                    .unwrap()
                    .sync
                    .last_received_at = super::now();
                s.asks
                    .get_mut(&incoming_id)
                    .unwrap()
                    .peer_inbound
                    .as_mut()
                    .unwrap()
                    .queued_at = Some(super::now());
            })
            .unwrap();
    })
    .unwrap();
    let current = f.asks.snapshot().unwrap();
    assert!(current.asks[&outgoing_id].is_open());
    let incoming = current.asks[&incoming_id].peer_inbound.as_ref().unwrap();
    assert!(!incoming.result_undeliverable && !incoming.delivery_stopped);
    assert_eq!(incoming.peer_deliveries.len(), 1);
    assert!(incoming.dead_letters.is_empty());
    let stale = &current.asks[&stale_id];
    assert_eq!(stale.execution, ExecutionState::Interrupted);
    assert_eq!(
        stale.remote.as_ref().unwrap().sync.last_error.as_deref(),
        Some("outcomeUnknown")
    );
}

#[test]
fn incompatible_terminal_close_is_removed_without_redelivery_or_blocking_later_work() {
    let (url, server) = server(
        |index, line, _| {
            if index == 0 {
                assert!(line.starts_with("POST /peer/v1/asks/") && line.contains("/ops "));
                (400, json!({"code":"peer_protocol_error","retryable":false}))
            } else {
                assert!(line.starts_with("PUT /peer/v1/asks/"));
                (200, snapshot(line, false))
            }
        },
        2,
    );
    let f = Fixture::new(&url);
    f.seed(vec![f.outgoing(1, true), f.outgoing(2, true)]);
    let id = format!("ask-{:032x}", 1);
    worker::fail(&f.asks, &id, "peerDisabled", true).unwrap();
    let before = f.asks.snapshot().unwrap();
    worker::send_peer(&f.peers, &f.asks, &f.peer.peer_id).unwrap();
    assert_eq!(server.join().unwrap().len(), 2);
    let after = f.asks.snapshot().unwrap();
    let current = &after.asks[&id];
    assert_eq!(current.execution, before.asks[&id].execution);
    assert_eq!(current.closed, before.asks[&id].closed);
    assert!(current.remote.as_ref().unwrap().sync.local_terminal);
    assert!(current.remote.as_ref().unwrap().sync.pending_ops.is_empty());
    assert_eq!(
        current.remote.as_ref().unwrap().sync.last_error.as_deref(),
        Some("peer_request_incompatible")
    );
    assert_eq!(
        after
            .deliveries
            .values()
            .filter(|delivery| delivery.ask_id == id)
            .collect::<Vec<_>>(),
        before
            .deliveries
            .values()
            .filter(|delivery| delivery.ask_id == id)
            .collect::<Vec<_>>()
    );
    assert!(
        after.asks[&format!("ask-{:032x}", 2)]
            .remote
            .as_ref()
            .unwrap()
            .sync
            .accepted
    );
    assert_eq!(f.peers.get(&f.peer.peer_id).unwrap().status, "online");
}
