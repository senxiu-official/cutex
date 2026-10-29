use super::{
    store::{Peer, Store},
    Error, Result,
};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use serde_json::{json, Value};
use std::net::TcpStream;

pub fn changed(peer: &str, reason: &str) {
    crate::management::v2::documents::publish_host_event(
        "cutex/peers/changed",
        json!({"peerId":peer,"reason":reason}),
    );
}
fn body(request: &SimpleHttpRequest) -> Result<Value> {
    if request.body.len() > 128 * 1024 {
        return Err(Error::new("request_too_large", 413));
    }
    if request.body.is_empty() {
        Ok(json!({}))
    } else {
        Ok(serde_json::from_slice(&request.body)?)
    }
}
fn respond(stream: &mut TcpStream, result: Result<Value>, status: u16) -> anyhow::Result<()> {
    match result {
        Ok(value) => write_json_response(
            stream,
            status,
            if status == 201 { "Created" } else { "OK" },
            &value,
        ),
        Err(error) => write_json_response(
            stream,
            error.status,
            match error.status {
                400 => "Bad Request",
                401 => "Unauthorized",
                403 => "Forbidden",
                404 => "Not Found",
                409 => "Conflict",
                413 => "Payload Too Large",
                429 => "Too Many Requests",
                _ => "Service Unavailable",
            },
            &error.value(),
        ),
    }
}
pub fn exported(peer: &Peer) -> Result<Value> {
    let directory = crate::asks::directory::Directory::load()
        .map_err(|_| Error::new("persistence_unavailable", 503))?;
    let state = crate::asks::store::AskStore::open_default()?.snapshot()?;
    let agents:Vec<_>=directory.agents.values().filter(|a|super::policy::visible(peer,a)).map(|a|json!({
        "cutexSessionId":a.cutex_session_id,"name":a.name,"short":crate::short_ids::Index::new(directory.agents.values().filter(|a|super::policy::visible(peer,a)).map(|a|crate::short_ids::Entry{id:a.cutex_session_id.clone(),name:a.name.clone()})).short(&a.cutex_session_id,6),
        "status":state.presence.get(&a.cutex_session_id).map(|p|match p.status(){"idle"=>"idle","busy"=>"busy",_=>"offline"}).unwrap_or("offline"),"runtime":if a.claude{"claude"}else{"codex"}
    })).collect();
    Ok(json!({"fetchedAt":super::now(),"agents":agents}))
}
pub fn refresh(store: &Store, peer: &Peer) -> Result<Value> {
    let result = refresh_inner(store, peer);
    if let Err(error) = &result {
        super::worker::note_error(store, peer, error)?;
    }
    result
}
fn refresh_inner(store: &Store, peer: &Peer) -> Result<Value> {
    let hello = super::client::call(store, peer, "GET", "/peer/v1/hello", None)?;
    super::profiles::note_hello(&peer.peer_id, &hello);
    store.with(|s| {
        if let Some(p) = s.peers.get_mut(&peer.peer_id) {
            p.features = super::pair::declared_features(&hello);
            p.negotiated_features = super::pair::negotiated_features(&hello);
        }
        Ok(())
    })?;
    if let Err(error) = super::pair::validate_hello(peer, &hello) {
        return Err(error);
    }
    let peer = &store.get(&peer.peer_id)?;
    let value = super::client::call(store, peer, "GET", "/peer/v1/agents", None)?;
    let mut value = value;
    let agents = value["agents"]
        .as_array_mut()
        .ok_or_else(|| Error::new("peer_protocol_error", 502))?;
    if agents.len() > 10000 {
        return Err(Error::new("peer_protocol_error", 502));
    }
    for agent in agents {
        if !agent["cutexSessionId"]
            .as_str()
            .is_some_and(|id| id.len() <= 128 && !id.chars().any(char::is_control))
        {
            return Err(Error::new("peer_protocol_error", 502));
        }
        if let Some(name) = agent["name"].as_str() {
            agent["name"] = json!(super::wire::sanitize_name(name));
        }
    }
    value["fetchedAt"] = json!(super::now());
    crate::config::atomic::write_private_pretty_json_atomic(
        &store.dir.join(format!("directory-{}.json", peer.peer_id)),
        &value,
        "peer directory",
    )?;
    store.with(|s| {
        if let Some(p) = s.peers.get_mut(&peer.peer_id) {
            p.status = "online".into();
            p.last_error = None;
            p.probe_failures = 0;
            p.next_probe_at = None;
            p.last_seen_at = Some(super::now());
        }
        Ok(())
    })?;
    Ok(value)
}
pub fn cached(store: &Store, peer: &Peer) -> Value {
    std::fs::read(store.dir.join(format!("directory-{}.json", peer.peer_id)))
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_else(|| json!({"fetchedAt":null,"agents":[]}))
}
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let mut audited_peer = None;
    let result = (|| {
        let store = Store::open()?;
        let path = request.path.split('?').next().unwrap_or("");
        let data = body(request)?;
        if request.method == "POST" && path == "/peer/v1/pair" {
            return super::pair::receive(&store, &data);
        }
        let peer = store.authenticate(
            super::token::bearer(request).unwrap_or(""),
            request
                .headers
                .get("x-cutex-peer-instance")
                .map(String::as_str)
                .unwrap_or(""),
        )?;
        audited_peer = Some(peer.clone());
        if peer.pending_until.is_some()
            && !matches!(
                path,
                "/peer/v1/hello" | "/peer/v1/confirm" | "/peer/v1/abort"
            )
        {
            return Err(Error::new("pair_incomplete", 409));
        }
        let result = match (request.method.as_str(), path) {
            ("GET", "/peer/v1/hello") => Ok(super::pair::hello()),
            ("POST", "/peer/v1/confirm") => super::pair::confirm(&store, &peer),
            ("POST", "/peer/v1/abort") => {
                if peer.pending_until.is_none() {
                    return Err(Error::new("peer_already_active", 409));
                }
                store.remove(&peer.peer_id)?;
                Ok(json!({"removed":true}))
            }
            ("POST", "/peer/v1/unpair") => {
                store.with(|s| {
                    if let Some(p) = s.peers.get_mut(&peer.peer_id) {
                        p.enabled = false;
                        p.status = "removedByPeer".into();
                    }
                    Ok(())
                })?;
                let _ = std::fs::remove_file(store.secret_path(&peer.peer_id)?);
                super::worker::removed_with_reason(
                    &crate::asks::store::AskStore::open_default()?,
                    &peer.peer_id,
                    "removedByPeer",
                )?;
                changed(&peer.peer_id, "removedByPeer");
                Ok(json!({"removed":true}))
            }
            ("GET", "/peer/v1/agents") => exported(&peer),
            ("GET", super::profiles::ROUTE | super::profiles::AVATAR_ROUTE) => {
                super::profiles::handle(path)
            }
            ("POST", super::tells::ROUTE) => {
                super::tells::receive(&store, &peer, data, &super::tells::bus_delivery)
            }
            ("POST", "/peer/v1/events") => {
                if data["fromInstanceId"].as_str() != Some(&peer.peer_id) {
                    return Err(Error::new("peer_credential_required", 401));
                }
                let events: Vec<super::wire::WireEventV1> =
                    serde_json::from_value(data["events"].clone())
                        .map_err(|_| Error::new("peer_protocol_error", 400))?;
                if events.len() > 100 {
                    return Err(Error::new("invalid_request", 400));
                }
                let asks = crate::asks::store::AskStore::open_default()?;
                let mut acked = vec![];
                for event in events {
                    if event.snapshot.schema_version != 1 {
                        return Err(Error::new("peer_protocol_error", 400));
                    }
                    if event.ask_id != event.snapshot.ask_id
                        || event.revision != event.snapshot.revision
                        || event.event_id.len() > 128
                    {
                        return Err(Error::new("invalid_request", 400));
                    }
                    super::asks::apply(&asks, &peer, event.snapshot.into())?;
                    acked.push(event.event_id);
                }
                Ok(json!({"acked":acked}))
            }
            (_, path) if path.starts_with("/peer/v1/asks/") => {
                let tail = path.trim_start_matches("/peer/v1/asks/");
                let (id, rest) = tail.split_once('/').unwrap_or((tail, ""));
                if !id
                    .strip_prefix("ask-")
                    .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    return Err(Error::new("invalid_request", 400));
                }
                let asks = crate::asks::store::AskStore::open_default()?;
                match (request.method.as_str(), rest) {
                    ("PUT", "") => {
                        let wire: super::wire::WireCreateV1 = serde_json::from_value(data)
                            .map_err(|_| Error::new("peer_protocol_error", 400))?;
                        let create = super::model::Create::try_from(wire)
                            .map_err(|_| Error::new("peer_protocol_error", 400))?;
                        super::asks::receive(&asks, &peer, id, create)
                    }
                    ("GET", "") => {
                        let s = asks.lookup_snapshot(&[id])?;
                        let a = super::asks::authorized(&s, &peer, id)?;
                        Ok(serde_json::to_value(super::wire::WireSnapshotV1::from(
                            &super::asks::wire_snapshot(
                                &asks,
                                super::asks::snapshot(&s, a),
                                &super::pair::label(),
                            )?,
                        ))?)
                    }
                    ("POST", "ops") => {
                        let op: super::model::Op = serde_json::from_value(data)?;
                        if op.op_id.is_empty()
                            || op.op_id.len() > 128
                            || !matches!(op.op.as_str(), "stop" | "close")
                        {
                            return Err(Error::new("remote_unsupported", 400));
                        }
                        super::asks::receive_op(&asks, &peer, id, op)?;
                        let s = asks.lookup_snapshot(&[id])?;
                        if s.peer_cancellations
                            .get(id)
                            .is_some_and(|(owner, _)| owner == &peer.peer_id)
                        {
                            return Ok(
                                json!({"schemaVersion":1,"askId":id,"revision":1,"execution":"interrupted","closed":"abandoned","interruptReason":"stopped","lastError":"expired"}),
                            );
                        }
                        let a = super::asks::authorized(&s, &peer, id)?;
                        Ok(serde_json::to_value(super::wire::WireSnapshotV1::from(
                            &super::asks::wire_snapshot(
                                &asks,
                                super::asks::snapshot(&s, a),
                                &super::pair::label(),
                            )?,
                        ))?)
                    }
                    _ => Err(Error::new("not_found", 404)),
                }
            }
            _ => Err(Error::new("not_found", 404)),
        };
        result
    })();
    if let Ok(store) = Store::open() {
        super::audit::record(
            &store,
            "inbound",
            audited_peer.as_ref(),
            &request.path,
            &result,
            request.body.len(),
            serde_json::from_slice::<Value>(&request.body).ok().as_ref(),
        );
    }
    respond(stream, result, 200)
}
/// Root-identity peer actions (owner-local-only; cutex-desktop
/// docs/research/host-ownership-20261019.md §5 limit 2): pairing with
/// another host (invite / add) and enabling a peer. Only owner-local
/// (`users::request_is_owner_local`: the local desktop, the CLI, an
/// SSH-tunnelled desktop's CLI-issued owner token); a paired device's token
/// on loopback gets 403 `owner_local_only`. Disabling, removing, labels,
/// urls and policy stay as before (loopback).
pub fn owner_local_action(method: &str, path: &str, data: &Value) -> Option<&'static str> {
    match (method, path) {
        ("POST", "/v2/peers/invites") | ("POST", "/v2/peers") => Some("peerPair"),
        ("PATCH", path) if path.starts_with("/v2/peers/") && data.get("enabled") == Some(&Value::Bool(true)) => {
            Some("peerEnable")
        }
        _ => None,
    }
}

pub fn handle_v2(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let result = (|| {
        if request.method != "GET" && !crate::management::listeners::current_request_is_loopback() {
            return Err(Error::new("loopback_only", 403));
        }
        let store = Store::open()?;
        let data = body(request)?;
        let path = request.path.split('?').next().unwrap_or("");
        if let Some(action) = owner_local_action(&request.method, path, &data) {
            if !crate::management::users::request_is_owner_local() {
                let mut error = Error::new("owner_local_only", 403);
                error.details = crate::management::users::owner_local_only(action).details;
                return Err(error);
            }
        }
        match (request.method.as_str(), path) {
            ("POST", "/v2/peers/invites") => super::pair::invite(&data),
            ("POST", "/v2/peers") => super::pair::add(&store, &data),
            ("GET", "/v2/peers") => {
                let state = store.snapshot()?;
                let asks = crate::asks::store::AskStore::open_default()?;
                Ok(json!({"peers":peer_views(&state, &asks)?.into_values().collect::<Vec<_>>()}))
            }
            _ => {
                let tail = path
                    .strip_prefix("/v2/peers/")
                    .ok_or_else(|| Error::new("not_found", 404))?;
                let (id, rest) = tail.split_once('/').unwrap_or((tail, ""));
                let peer = store.get(id)?;
                match (request.method.as_str(), rest) {
                    ("GET", "inflight") => {
                        let state = crate::asks::store::AskStore::open_default()?.snapshot()?;
                        Ok(
                            json!({"asks":state.asks.values().filter(|a| a.is_open() && (a.remote.as_ref().is_some_and(|r|r.peer_id==id) || matches!(&a.requester,crate::asks::model::Requester::Peer{peer_id,..} if peer_id==id))).map(|a|crate::asks::view::owner_view(&state,a)).collect::<Vec<_>>()}),
                        )
                    }
                    ("GET", "audit") => {
                        let limit = request
                            .path
                            .split("limit=")
                            .nth(1)
                            .and_then(|v| v.split('&').next())
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(100)
                            .min(1000);
                        let text = std::fs::read_to_string(store.dir.join("peer-audit.jsonl"))
                            .unwrap_or_default();
                        let entries: Vec<Value> = text
                            .lines()
                            .rev()
                            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                            .filter(|v| v["peerId"] == id)
                            .take(limit)
                            .collect();
                        Ok(json!({"entries":entries}))
                    }
                    ("GET", "agents") => {
                        refresh(&store, &peer).or_else(|_| Ok(cached(&store, &peer)))
                    }
                    ("DELETE", "") => {
                        let _ = super::client::call(
                            &store,
                            &peer,
                            "POST",
                            "/peer/v1/unpair",
                            Some(&json!({})),
                        );
                        store.remove(id)?;
                        super::profiles::forget(id);
                        super::worker::removed(&crate::asks::store::AskStore::open_default()?, id)?;
                        changed(id, "removed");
                        Ok(json!({"removed":true}))
                    }
                    ("PATCH", "") => {
                        if let Some(urls) = data.get("urls") {
                            for url in serde_json::from_value::<Vec<String>>(urls.clone())? {
                                super::client::addresses(&url)?;
                            }
                        }
                        let next = store.with(|s| {
                            let mut next = s
                                .peers
                                .get(id)
                                .cloned()
                                .ok_or_else(|| Error::new("peer_not_found", 404))?;
                            for (key, value) in data
                                .as_object()
                                .ok_or_else(|| Error::new("invalid_request", 400))?
                            {
                                match key.as_str() {
                                    "label" => {
                                        let label = value
                                            .as_str()
                                            .filter(|s| super::valid_label(s))
                                            .ok_or_else(|| Error::new("invalid_request", 400))?;
                                        next.label = label.into();
                                    }
                                    "urls" => {
                                        let urls: Vec<String> =
                                            serde_json::from_value(value.clone())?;
                                        if urls.is_empty() || urls.len() > 8 {
                                            return Err(Error::new("invalid_request", 400));
                                        }
                                        next.urls = urls;
                                        next.last_seen_at = None;
                                        next.next_probe_at = None;
                                        next.status = "unreachable".into();
                                    }
                                    "enabled" => {
                                        next.enabled = value
                                            .as_bool()
                                            .ok_or_else(|| Error::new("invalid_request", 400))?
                                    }
                                    "policy" => {
                                        next.policy = next.policy.patched(value)?;
                                        next.policy_revision += 1;
                                    }
                                    _ => return Err(Error::new("invalid_request", 400)),
                                }
                            }
                            if s.peers
                                .values()
                                .any(|p| p.peer_id != id && p.label == next.label)
                            {
                                return Err(Error::new("label_taken", 409));
                            }
                            s.peers.insert(id.into(), next.clone());
                            Ok(next)
                        })?;
                        super::worker::maintain(
                            &store,
                            &crate::asks::store::AskStore::open_default()?,
                        )?;
                        super::worker::wake();
                        changed(id, "updated");
                        Ok(json!({"peer":next.view()}))
                    }
                    _ => Err(Error::new("not_found", 404)),
                }
            }
        }
    })();
    respond(
        stream,
        result,
        if request.method == "POST" { 201 } else { 200 },
    )
}

#[derive(Default)]
struct PeerActivity {
    pending: usize,
    oldest: Option<chrono::DateTime<chrono::Utc>>,
    dead: usize,
    outbound: usize,
    inbound: usize,
}
impl PeerActivity {
    fn queued(&mut self, count: usize, at: &str) {
        self.pending += count;
        if count != 0 {
            if let Some(at) = crate::asks::parse_time(at) {
                self.oldest = Some(self.oldest.map_or(at, |old| old.min(at)));
            }
        }
    }
    fn view(&self, peer: &Peer, enabled: bool) -> Value {
        let mut view = peer.view();
        if !enabled && peer.status != "removedByPeer" {
            view["state"] = json!("disabled");
        }
        let age = self
            .oldest
            .map(|t| (chrono::Utc::now() - t).num_seconds().max(0));
        let oldest = self.oldest.map(crate::asks::now_rfc3339);
        view["outboxPending"] = json!(self.pending);
        view["outboxOldestAt"] = json!(oldest);
        view["outboxOldestAgeSecs"] = json!(age);
        view["deadLetterCount"] = json!(self.dead);
        view["inflight"] = json!({"outbound":self.outbound,"inbound":self.inbound});
        view["outbox"] = json!({"pending":self.pending,"oldestAt":oldest,"oldestAgeSecs":age});
        view
    }
}

/// One borrowed scan for all peers, without copying ask payloads per peer.
pub(crate) fn peer_views(
    peers: &super::store::State,
    asks: &crate::asks::store::AskStore,
) -> Result<std::collections::BTreeMap<String, Value>> {
    let activity = asks.inspect(|state| {
        let mut activity = std::collections::BTreeMap::<String, PeerActivity>::new();
        for ask in state.asks.values() {
            if let Some(remote) = &ask.remote {
                let stats = activity.entry(remote.peer_id.clone()).or_default();
                stats.outbound += usize::from(ask.is_open());
                stats.queued(remote.sync.pending_ops.len(), &ask.created_at);
            }
            if let crate::asks::model::Requester::Peer { peer_id, .. } = &ask.requester {
                let stats = activity.entry(peer_id.clone()).or_default();
                stats.inbound += usize::from(ask.is_open());
                if let Some(inbound) = &ask.peer_inbound {
                    stats.queued(
                        inbound.peer_deliveries.len(),
                        inbound.queued_at.as_deref().unwrap_or(&ask.created_at),
                    );
                    stats.dead += inbound.dead_letters.len();
                }
            }
        }
        activity
    })?;
    Ok(peers
        .peers
        .values()
        .map(|peer| {
            let empty = PeerActivity::default();
            let stats = activity.get(&peer.peer_id).unwrap_or(&empty);
            (
                peer.peer_id.clone(),
                stats.view(peer, peers.settings.enabled),
            )
        })
        .collect())
}

/// Age is presentation-only. Stable timestamps determine whether a host event is needed.
pub(crate) fn peer_change_key(view: &Value) -> Value {
    let mut key = view.clone();
    if let Some(object) = key.as_object_mut() {
        object.remove("outboxOldestAgeSecs");
    }
    if let Some(outbox) = key.get_mut("outbox").and_then(Value::as_object_mut) {
        outbox.remove("oldestAgeSecs");
    }
    key
}

pub fn peer_view(store: &Store, peer: &Peer) -> Value {
    let result = (|| -> Result<Value> {
        let mut peers = store.snapshot()?;
        peers.peers = std::collections::BTreeMap::from([(peer.peer_id.clone(), peer.clone())]);
        let asks = crate::asks::store::AskStore::open_default()?;
        Ok(peer_views(&peers, &asks)?
            .remove(&peer.peer_id)
            .unwrap_or_else(|| peer.view()))
    })();
    result.unwrap_or_else(|_| peer.view())
}
