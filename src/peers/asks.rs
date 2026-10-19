//! The local ask scheduler owns execution; this module owns transport state only.
use super::{
    model::{Create, Event, Inbound, Op, Remote, Snapshot, Sync},
    policy::{Caps, Visibility},
    store::{Peer, Store},
    Error, Result,
};
use crate::asks::{
    model::*,
    scheduler::SchedulerWorld,
    store::{AskError, AskState, AskStore, NewAsk},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

pub fn ask_error(error: Error) -> AskError {
    let code = match error.code.as_str() {
        "peer_not_found" => "peer_not_found",
        "peer_unavailable" => "peer_unavailable",
        "peer_unsupported" => "peer_unsupported",
        "recipient_outside_boundary" => "recipient_outside_boundary",
        "peer_asks_disabled" => "peer_asks_disabled",
        "delegation_loop" => "delegation_loop",
        "peer_hops_exceeded" => "peer_hops_exceeded",
        "remote_unsupported" => "remote_unsupported",
        "id_ambiguous" => "id_ambiguous",
        "id_not_found" => "id_not_found",
        "idempotency_conflict" => "idempotency_conflict",
        "peer_rate_limited" => "peer_rate_limited",
        "wake_not_allowed" => "wake_not_allowed",
        _ => "peer_unavailable",
    };
    AskError::new(code, error.code.clone()).with(error.details)
}
fn from_ask(e: AskError) -> Error {
    let mut error = Error::new(e.code, 400);
    error.details = e
        .extra
        .as_object()
        .map(|v| Value::Object(v.clone()))
        .unwrap_or_else(|| json!({}));
    error
}
pub fn same_requester(a: &Requester, b: &Requester) -> bool {
    match (a, b) {
        (
            Requester::Agent {
                cutex_session_id: a,
                ..
            },
            Requester::Agent {
                cutex_session_id: b,
                ..
            },
        ) => a == b,
        (
            Requester::Peer {
                peer_id: a,
                cutex_session_id: x,
                ..
            },
            Requester::Peer {
                peer_id: b,
                cutex_session_id: y,
                ..
            },
        ) => a == b && x == y,
        (Requester::Owner, Requester::Owner) => true,
        _ => false,
    }
}
pub fn snapshot(state: &AskState, ask: &AskRecord) -> Snapshot {
    Snapshot {
        remote_state: None,
        ask_id: ask.ask_id.clone(),
        revision: ask.revision,
        execution: ask.execution,
        hold: ask.hold.clone(),
        closed: ask.closed,
        review: ask.review,
        answer: ask
            .answer_id
            .as_ref()
            .and_then(|id| state.answers.get(id))
            .cloned(),
        text: None,
        interrupt_reason: ask.current_attempt().and_then(|a| a.interrupt_reason),
        last_error: ask
            .current_attempt()
            .and_then(|a| a.last_error.clone())
            .or_else(|| ask.peer_inbound.as_ref().and_then(|i| i.last_error.clone()))
            .map(|error| super::model::sanitize_diagnostic(&error)),
    }
}
pub fn queue_snapshots(state: &mut AskState, before: &AskState) {
    let events: Vec<_> = state
        .asks
        .values()
        .filter(|a| {
            a.peer_inbound.as_ref().is_some_and(|i| !i.delivery_stopped)
                && before
                    .asks
                    .get(&a.ask_id)
                    .is_none_or(|b| b.revision != a.revision)
        })
        .map(|a| Event {
            event_id: crate::asks::new_id("pev"),
            ask_id: a.ask_id.clone(),
            revision: a.revision,
            snapshot: snapshot(state, a),
        })
        .collect();
    for event in events {
        if let Some(inbound) = state
            .asks
            .get_mut(&event.ask_id)
            .and_then(|a| a.peer_inbound.as_mut())
        {
            // Snapshots subsume earlier states, including the latest answer. Bound backlog per ask.
            if inbound.peer_deliveries.is_empty() {
                inbound.queued_at = Some(super::now());
            }
            inbound.peer_deliveries.clear();
            inbound.peer_deliveries.push(event);
        }
    }
}
pub fn wire_snapshot(store: &AskStore, mut snapshot: Snapshot, label: &str) -> Result<Snapshot> {
    if let Some(answer) = &mut snapshot.answer {
        if let Some(id) = &answer.artifact_id {
            if let Some((_, bytes)) = store.read_artifact(id, None)? {
                let text = String::from_utf8_lossy(&bytes);
                snapshot.text = Some(if text.len() > 65536 {
                    format!(
                        "{}\n[full text stays on {label}]",
                        crate::asks::byte_prefix(&text, 65000)
                    )
                } else {
                    text.into_owned()
                });
            }
        }
        answer.artifact_id = None;
        answer.metadata.native_diff = None;
        answer.metadata.included_notes.clear();
        answer.metadata.profile = None;
        answer.metadata.model = None;
        answer.metadata.token_usage = None;
    }
    Ok(snapshot)
}
/// The peer policy as ask approval evidence binds it: the tell-wake fields
/// do not affect ask admission, so adding them (upgrade) keeps the evidence
/// of asks awaiting approval valid.
fn ask_policy_value(policy: &super::policy::Policy) -> Value {
    let mut value = serde_json::to_value(policy).unwrap_or_default();
    if let Some(inbound) = value.get_mut("inbound").and_then(Value::as_object_mut) {
        inbound.remove("tellWake");
        inbound.remove("tellWakesPerHour");
    }
    value
}
pub fn validate_inbound(peer: &Peer, target: &str) -> Result<()> {
    if !peer.enabled || peer.pending_until.is_some() {
        return Err(Error::new("peer_unavailable", 409));
    }
    let directory = crate::asks::directory::Directory::load().map_err(from_ask)?;
    let agent = directory
        .get(target)
        .filter(|agent| super::policy::visible(peer, agent))
        .ok_or_else(|| Error::new("recipient_outside_boundary", 403))?;
    if !super::policy::can_deliver(peer, agent) {
        return Err(Error::new("peer_asks_disabled", 403));
    }
    Ok(())
}
pub fn gate_inputs(
    world: &dyn SchedulerWorld,
    ask: &AskRecord,
) -> std::result::Result<crate::asks::gate::GateInputs, AskError> {
    let mut inputs = world.gate_inputs(ask.requester.session(), &ask.target_cutex_session_id)?;
    if let Requester::Peer {
        peer_id,
        cutex_session_id,
        ..
    } = &ask.requester
    {
        let store = Store::open().map_err(ask_error)?;
        let peer = store.get(peer_id).map_err(ask_error)?;
        if !store.snapshot().map_err(ask_error)?.settings.enabled {
            return Err(AskError::new("peer_unavailable", "peers are disabled"));
        }
        validate_inbound(&peer, &ask.target_cutex_session_id).map_err(ask_error)?;
        let source = ask
            .peer_inbound
            .as_ref()
            .ok_or_else(|| AskError::new("peer_unavailable", "missing peer request"))?;
        let needs = peer.policy.needs_review(&Caps::from(&inputs.target.caps));
        // Keep incomparable host-local roots and reach equal. The peer policy determines the gate.
        let mut caps = inputs.target.caps.clone();
        if needs {
            caps.sandbox = None;
        }
        inputs.requester = Some(crate::asks::gate::GateParty {
            cutex_session_id: format!("peer:{peer_id}:{cutex_session_id}"),
            name: Some(ask.requester.label()),
            caps,
            profile: None,
            approval_policy: None,
        });
        inputs.links_version = crate::approvals::request_digest(
            &json!({"peerId":peer_id,"userId":peer.user_id,"policyRevision":peer.policy_revision,"policy":ask_policy_value(&peer.policy),"claimedCaps":source.request.requester.caps,"assumedCaps":peer.policy.inbound.assumed_caps,"targetLinks":inputs.links_version}),
        );
        inputs.project_policy = if needs {
            "peer-review-required-v1"
        } else {
            "peer-review-skipped-v1"
        }
        .into();
    }
    Ok(inputs)
}
pub fn receive(store: &AskStore, peer: &Peer, id: &str, request: Create) -> Result<Value> {
    if !id
        .strip_prefix("ask-")
        .is_some_and(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::new("invalid_request", 400));
    }
    if request.text.trim().is_empty()
        || request.text.len() > crate::asks::INLINE_BRIEF_MAX_BYTES
        || request.title.len() > 1024
        || request.requester.cutex_session_id.len() > 128
        || request
            .requester
            .name
            .as_ref()
            .is_some_and(|n| n.len() > 128)
    {
        return Err(Error::new("invalid_request", 400));
    }
    if request.request_sha256 != request.digest() {
        return Err(Error::new("idempotency_conflict", 409));
    }
    if !matches!(
        request.options.context,
        ContextOption::None | ContextOption::Brief
    ) {
        return Err(Error::new("remote_unsupported", 400));
    }
    let state = store.snapshot()?;
    if let Some((owner, _)) = state.peer_cancellations.get(id) {
        return Err(Error::new(
            if owner == &peer.peer_id {
                "ask_gone"
            } else {
                "ask_id_conflict"
            },
            if owner == &peer.peer_id { 410 } else { 409 },
        ));
    }
    if let Some(ask) = state.asks.get(id) {
        if !matches!(&ask.requester,Requester::Peer{peer_id,..} if peer_id==&peer.peer_id) {
            return Err(Error::new("ask_id_conflict", 409));
        }
        if !ask
            .peer_inbound
            .as_ref()
            .is_some_and(|i| i.request.request_sha256 == request.request_sha256)
        {
            return Err(Error::new("idempotency_conflict", 409));
        }
        return Ok(serde_json::to_value(super::wire::WireSnapshotV1::from(
            &wire_snapshot(store, snapshot(&state, ask), &super::pair::label())?,
        ))?);
    }
    validate_inbound(peer, &request.target)?;
    if request.hops > Store::open()?.snapshot()?.settings.max_hops {
        return Err(Error::new("peer_hops_exceeded", 400));
    }
    let now = Utc::now();
    let mut options = request.options.clone();
    options.questions = QuestionRoute::Owner;
    let mut new = NewAsk {
        ask_id: Some(id.into()),
        action_id: format!("peer:{}:{id}", peer.peer_id),
        requester: Requester::Peer {
            peer_id: peer.peer_id.clone(),
            cutex_session_id: request.requester.cutex_session_id.clone(),
            name: request.requester.name.clone(),
            label: peer.label.clone(),
        },
        target_cutex_session_id: request.target.clone(),
        target_name: None,
        project_id: None,
        body: crate::asks::tools::ask_body(store, &request.text, vec![], &[], now)
            .map_err(from_ask)?,
        title: Some(request.title.clone()),
        options,
        follow_up_of: None,
        parent_ask: None,
        gate: None,
    };
    let directory = crate::asks::directory::Directory::load().map_err(from_ask)?;
    let target = directory
        .get(&request.target)
        .ok_or_else(|| Error::new("recipient_outside_boundary", 403))?;
    new.target_name = target.name.clone();
    new.project_id = target.project.clone();
    let inbound = Inbound {
        dead_letters: vec![],
        queued_at: None,
        result_undeliverable: false,
        delivery_stopped: false,
        last_error: None,
        request,
        peer_deliveries: vec![],
        received_ops: vec![],
        next_attempt_at: None,
        failures: 0,
    };
    let mut provisional = AskState::provisional(&new, now);
    provisional.peer_inbound = Some(inbound.clone());
    let inputs =
        gate_inputs(&crate::asks::scheduler::HostWorld::load(), &provisional).map_err(from_ask)?;
    new.gate = Some(crate::asks::gate::evaluate(
        &crate::asks::gate::ask_facts(&provisional),
        &inputs,
    ));
    store.with_state(|state| -> Result<()> {
        if state.peer_cancellations.contains_key(id) {
            return Err(Error::new("ask_gone", 410));
        }
        if let Some(existing) = state.asks.get(id) {
            if existing
                .peer_inbound
                .as_ref()
                .is_some_and(|i| i.request == inbound.request)
                && same_requester(&existing.requester, &new.requester)
            {
                return Ok(());
            }
            return Err(Error::new("idempotency_conflict", 409));
        }
        let incoming: Vec<_> = state
            .asks
            .values()
            .filter(|a| matches!(&a.requester,Requester::Peer{peer_id,..}if peer_id==&peer.peer_id))
            .collect();
        if incoming.iter().filter(|a| a.is_open()).count() >= peer.policy.inbound.max_open_asks
            || incoming
                .iter()
                .filter(|a| {
                    crate::asks::parse_time(&a.created_at)
                        .is_some_and(|t| now - t < chrono::Duration::hours(1))
                })
                .count()
                >= peer.policy.inbound.asks_per_hour
        {
            let mut e = Error::new("peer_rate_limited", 429);
            e.details = json!({"retryAfterSecs":60});
            return Err(e);
        }
        if new.options.if_busy == IfBusy::Fail {
            if let Some(e) = crate::asks::tools::busy_refusal(state, target, "if_busy=queue") {
                return Err(from_ask(e));
            }
        }
        let (mut ask, _) = state.create_ask(new, now).map_err(from_ask)?;
        ask.hops = inbound.request.hops;
        ask.peer_inbound = Some(inbound);
        state.asks.insert(id.into(), ask);
        Ok(())
    })??;
    let state = store.snapshot()?;
    Ok(serde_json::to_value(super::wire::WireSnapshotV1::from(
        &snapshot(&state, &state.asks[id]),
    ))?)
}
pub fn authorized<'a>(state: &'a AskState, peer: &Peer, id: &str) -> Result<&'a AskRecord> {
    state
        .asks
        .get(id)
        .filter(|a| matches!(&a.requester,Requester::Peer{peer_id,..}if peer_id==&peer.peer_id))
        .ok_or_else(|| Error::new("ask_not_found", 404))
}
// Reconciliation often returns an unchanged revision. It still proves contact,
// but must not replay execution, answers or result deliveries. Keep this check
// under the ask-store lock so a concurrent newer snapshot stays authoritative.
fn acknowledge_unchanged_snapshot(
    state: &mut AskState,
    peer: &Peer,
    snapshot: &Snapshot,
) -> Result<()> {
    let ask = state
        .asks
        .get_mut(&snapshot.ask_id)
        .filter(|a| a.remote.as_ref().is_some_and(|r| r.peer_id == peer.peer_id))
        .ok_or_else(|| Error::new("ask_not_found", 404))?;
    let remote = ask.remote.as_mut().unwrap();
    if snapshot.revision != remote.sync.last_remote_revision {
        return Ok(());
    }
    remote.sync.last_received_at = super::now();
    if remote.sync.local_terminal
        || ask.closed.is_some()
        || !matches!(
            ask.hold,
            Some(Hold::PeerUnreachable | Hold::PeerIdentityChanged | Hold::PeerVersionMismatch)
        )
    {
        return Ok(());
    }
    let unknown = snapshot.unknown_state_message();
    state.update_ask(&snapshot.ask_id, Utc::now(), |a| {
        // Restore the destination's hold, including an unknown state; only the
        // local transport/identity/version overlay is being removed.
        a.hold = unknown
            .as_ref()
            .map(|raw| Hold::PeerUnknown { raw: raw.clone() })
            .or_else(|| snapshot.hold.clone());
        let remote = a.remote.as_mut().unwrap();
        remote.sync.state = if unknown.is_some() {
            "remoteStateUnknown"
        } else {
            "synced"
        }
        .into();
        remote.sync.last_error = unknown.or_else(|| snapshot.last_error.clone());
        remote.sync.failures = 0;
    });
    Ok(())
}

pub fn apply(store: &AskStore, peer: &Peer, mut snapshot: Snapshot) -> Result<()> {
    if snapshot.text.as_ref().is_some_and(|t| t.len() > 65536)
        || snapshot
            .answer
            .as_ref()
            .is_some_and(|a| a.ask_id != snapshot.ask_id)
    {
        return Err(Error::new("invalid_request", 400));
    }
    snapshot.last_error = snapshot
        .last_error
        .as_deref()
        .map(super::model::sanitize_diagnostic);
    if let Some(answer) = &mut snapshot.answer {
        answer.metadata.last_error = answer
            .metadata
            .last_error
            .as_deref()
            .map(super::model::sanitize_diagnostic);
    }
    let state = store.snapshot()?;
    let ask = state
        .asks
        .get(&snapshot.ask_id)
        .filter(|a| a.remote.as_ref().is_some_and(|r| r.peer_id == peer.peer_id))
        .ok_or_else(|| Error::new("ask_not_found", 404))?;
    if snapshot.revision <= ask.remote.as_ref().unwrap().sync.last_remote_revision {
        return store.with_state(|state| acknowledge_unchanged_snapshot(state, peer, &snapshot))?;
    }
    let readers: Vec<_> = ask
        .requester
        .session()
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();
    let artifact = snapshot
        .text
        .as_ref()
        .map(|text| {
            store.put_artifact(
                text.as_bytes(),
                "text/plain",
                &readers,
                Some(&snapshot.ask_id),
                Utc::now(),
            )
        })
        .transpose()?;
    store.with_state(|state| -> Result<()> {
        let Some(ask) = state.asks.get(&snapshot.ask_id).cloned() else {
            return Err(Error::new("ask_not_found", 404));
        };
        let Some(remote) = ask.remote.as_ref().filter(|r| r.peer_id == peer.peer_id) else {
            return Err(Error::new("ask_not_found", 404));
        };
        if snapshot.revision <= remote.sync.last_remote_revision {
            return acknowledge_unchanged_snapshot(state, peer, &snapshot);
        }
        let now = Utc::now();
        let wire = super::wire::WireSnapshotV1::from(&snapshot);
        // Local outcomes are final even if the create acknowledgement was lost.
        if remote.sync.local_terminal || ask.closed.is_some() {
            let a = state.asks.get_mut(&snapshot.ask_id).unwrap();
            let r = a.remote.as_mut().unwrap();
            r.remote_state = Some(wire.clone().into());
            if wire.answer.is_some() {
                r.late_answer = Some(super::model::LateAnswer::new(wire, artifact.clone()));
            }
            r.sync.last_remote_revision = snapshot.revision;
            r.sync.last_received_at = super::now();
            r.sync.accepted = true;
            return Ok(());
        }
        if let Some(raw) = snapshot.unknown_state_message() {
            let hold = Hold::PeerUnknown { raw: raw.clone() };
            let changed = ask.hold.as_ref() != Some(&hold);
            state.update_ask(&snapshot.ask_id, now, |a| {
                a.hold = Some(hold);
                let r = a.remote.as_mut().unwrap();
                r.remote_state = Some(wire.into());
                r.sync.last_remote_revision = snapshot.revision;
                r.sync.last_received_at = super::now();
                r.sync.accepted = true;
                r.sync.state = "remoteStateUnknown".into();
                r.sync.last_error = Some(raw);
            });
            if changed {
                let a = state.asks[&snapshot.ask_id].clone();
                crate::asks::delivery::record_state_delivery(state, &a, "remoteStateUnknown", now);
            }
            return Ok(());
        }
        let mut answer_id = None;
        if let Some(mut answer) = snapshot.answer.clone() {
            answer.answer_id = format!(
                "ans-{}",
                crate::asks::sha256_hex(
                    format!("{}:{}", snapshot.ask_id, answer.revision).as_bytes()
                )
            );
            answer.artifact_id = artifact;
            answer.bytes = snapshot.text.as_ref().map_or(0, |t| t.len());
            answer.created_at = super::now();
            answer.preview = snapshot
                .text
                .as_deref()
                .and_then(super::model::diagnostic_preview);
            answer.metadata.native_diff = None;
            answer.metadata.included_notes.clear();
            answer_id = Some(answer.answer_id.clone());
            state
                .answers
                .entry(answer.answer_id.clone())
                .or_insert(answer);
        }
        state.update_ask(&snapshot.ask_id, now, |a| {
            a.execution = snapshot.execution;
            a.hold = snapshot.hold.clone();
            a.closed = snapshot.closed;
            a.review = snapshot.review;
            a.closed_at = snapshot.closed.map(|_| super::now());
            if let Some(id) = &answer_id {
                a.answer_id = Some(id.clone());
            }
            let r = a.remote.as_mut().unwrap();
            r.remote_state = Some(wire.into());
            r.sync.local_terminal = a.closed.is_some();
            r.sync.last_remote_revision = snapshot.revision;
            r.sync.last_received_at = super::now();
            r.sync.accepted = true;
            r.sync.state = "synced".into();
            r.sync.last_error = snapshot.last_error.clone();
            r.sync.interrupt_reason = snapshot.interrupt_reason;
            r.sync.failures = 0;
        });
        let updated = state.asks[&snapshot.ask_id].clone();
        if let Some(id) = answer_id {
            crate::asks::delivery::record_answer_delivery(state, &updated, &id, now);
        } else if snapshot.execution != ask.execution || snapshot.hold != ask.hold {
            crate::asks::delivery::record_state_delivery(state, &updated, "peerState", now);
        }
        Ok(())
    })??;
    Ok(())
}
pub fn other_hosts() -> Value {
    let Ok(store) = Store::open() else {
        return json!([]);
    };
    let Ok(state) = store.snapshot() else {
        return json!([]);
    };
    if !state.settings.enabled {
        return json!([]);
    }
    json!(state.peers.values().filter(|p|p.enabled&&p.pending_until.is_none()&&p.policy.outbound.visibility==Visibility::All).map(|p|{
        let cache=super::routes::cached(&store,p);
        let age=cache["fetchedAt"].as_str().and_then(crate::asks::parse_time).map(|at|(Utc::now()-at).num_seconds().max(0));
        let agents=cache["agents"].as_array().cloned().unwrap_or_default().into_iter().map(|mut a|{let name=a["name"].as_str().or_else(||a["cutexSessionId"].as_str()).unwrap_or("agent");a["address"]=json!(format!("{name}@{}",p.label));a}).collect::<Vec<_>>();
        json!({"peerId":p.peer_id,"label":p.label,"status":p.status,"cacheAgeSecs":age,"tells":p.features.iter().any(|f|f==super::tells::FEATURE),"tellWake":super::tells::supports_wake(p),"agents":agents})
    }).collect::<Vec<_>>())
}
pub fn resolve(target: &str) -> Result<(Peer, Value)> {
    let (who, label) = target
        .rsplit_once('@')
        .ok_or_else(|| Error::new("id_not_found", 404))?;
    let store = Store::open()?;
    let state = store.snapshot()?;
    let peer = state
        .peers
        .values()
        .find(|p| p.label == label && p.policy.outbound.visibility == Visibility::All)
        .cloned()
        .ok_or_else(|| Error::new("peer_not_found", 404))?;
    if !state.settings.enabled || !peer.enabled || peer.pending_until.is_some() {
        return Err(Error::new("peer_unavailable", 409));
    }
    if matches!(
        peer.status.as_str(),
        "identityChanged" | "versionMismatch" | "removedByPeer"
    ) {
        return Err(Error::new("peer_unsupported", 409));
    }
    let mut cache = super::routes::cached(&store, &peer);
    for attempt in 0..2 {
        let agents = cache["agents"].as_array().cloned().unwrap_or_default();
        let index = crate::short_ids::Index::new(agents.iter().filter_map(|a| {
            Some(crate::short_ids::Entry {
                id: a["cutexSessionId"].as_str()?.into(),
                name: a["name"].as_str().map(str::to_string),
            })
        }));
        match index.resolve(who, &index, crate::short_ids::settings().minimum()) {
            Ok(id) => {
                return Ok((
                    peer,
                    agents
                        .into_iter()
                        .find(|a| a["cutexSessionId"] == id)
                        .unwrap(),
                ))
            }
            Err(e) if attempt == 1 => {
                let mut error = from_ask(e);
                error.details["host"] = json!(label);
                if let Some(candidates) = error.details["candidates"].as_array_mut() {
                    for c in candidates {
                        for key in ["id", "short", "name"] {
                            if let Some(v) = c[key].as_str() {
                                c[key] = json!(format!("{v}@{label}"));
                            }
                        }
                    }
                }
                return Err(error);
            }
            _ => {
                cache = super::routes::refresh(&store, &peer).unwrap_or(cache);
            }
        }
    }
    Err(Error::new("id_not_found", 404))
}
pub fn delegate(
    ctx: &crate::asks::tools::ToolContext<'_>,
    target: &str,
    text: &str,
    title: Option<String>,
    options: AskOptions,
    action_id: &str,
) -> std::result::Result<Value, AskError> {
    let result = (|| -> Result<Value> {
        if text.trim().is_empty() || text.len() > crate::asks::INLINE_BRIEF_MAX_BYTES {
            return Err(Error::new("remote_unsupported", 400));
        }
        let (peer, target) = resolve(target)?;
        let state = ctx.store.snapshot()?;
        let parent = state.leases.get(ctx.caller).and_then(|l| match &l.holder {
            LeaseHolder::Ask { ask_id, .. } => Some(ask_id.clone()),
            _ => None,
        });
        let hops = state.inherited_hops(parent.as_deref()).saturating_add(1);
        let hops_exceeded = hops > Store::open()?.snapshot()?.settings.max_hops;
        let directory = ctx.world.directory().map_err(from_ask)?;
        let caps = ctx
            .world
            .gate_inputs(Some(ctx.caller), ctx.caller)
            .map_err(from_ask)?
            .target
            .caps;
        let mut options = options;
        options.questions = QuestionRoute::Owner;
        let target_id = target["cutexSessionId"]
            .as_str()
            .ok_or_else(|| Error::new("peer_protocol_error", 502))?
            .to_string();
        let mut request = Create {
            requester: super::model::Requester {
                cutex_session_id: ctx.caller.into(),
                name: directory
                    .get(ctx.caller)
                    .and_then(|a| a.name.as_deref())
                    .map(super::wire::sanitize_name),
                caps: Caps::from(&caps),
            },
            target: target_id.clone(),
            title: title.clone().unwrap_or_else(|| crate::asks::title_of(text)),
            text: text.into(),
            options: options.clone(),
            hops,
            request_sha256: String::new(),
        };
        request.request_sha256 = request.digest();
        let name = super::wire::sanitize_name(target["name"].as_str().unwrap_or(&target_id));
        let new = NewAsk {
            ask_id: None,
            action_id: action_id.into(),
            requester: Requester::Agent {
                cutex_session_id: ctx.caller.into(),
                name: request.requester.name.clone(),
            },
            target_cutex_session_id: target_id,
            target_name: Some(format!("{name}@{}", peer.label)),
            project_id: directory.get(ctx.caller).and_then(|a| a.project.clone()),
            body: crate::asks::tools::ask_body(
                ctx.store,
                text,
                vec![],
                &[ctx.caller.into()],
                ctx.now,
            )
            .map_err(from_ask)?,
            title,
            options,
            follow_up_of: None,
            parent_ask: parent,
            gate: None,
        };
        let (ask, created) = ctx.store.with_state(|s| -> Result<(AskRecord, bool)> {
            if s.asks
                .values()
                .filter(|a| {
                    a.remote.as_ref().is_some_and(|r| {
                        r.peer_id == peer.peer_id && !r.sync.pending_ops.is_empty()
                    })
                })
                .count()
                >= 200
            {
                return Err(Error::new("peer_rate_limited", 429));
            }
            if let Some(a) = s.find_by_action(&new.requester, action_id) {
                if !a
                    .remote
                    .as_ref()
                    .is_some_and(|r| r.peer_id == peer.peer_id && r.request == request)
                {
                    return Err(Error::new("idempotency_conflict", 409));
                }
                return Ok((a.clone(), false));
            }
            let (mut a, created) = s.create_ask(new, ctx.now).map_err(from_ask)?;
            a.hops = hops;
            a.hold = Some(Hold::PeerUnreachable);
            a.remote = Some(Remote {
                remote_state: None,
                late_answer: None,
                peer_id: peer.peer_id.clone(),
                label: peer.label.clone(),
                target_name: name,
                request,
                sync: Sync {
                    local_terminal: false,
                    reconcile_at: None,
                    interrupt_reason: None,
                    state: "queued".into(),
                    last_remote_revision: 0,
                    pending_ops: vec![Op {
                        op_id: crate::asks::new_id("pop"),
                        op: "create".into(),
                        outcome: None,
                        reason: None,
                    }],
                    last_error: None,
                    next_attempt_at: None,
                    last_received_at: super::now(),
                    failures: 0,
                    accepted: false,
                },
            });
            s.asks.insert(a.ask_id.clone(), a.clone());
            Ok((a, created))
        })??;
        if hops_exceeded {
            super::worker::fail(ctx.store, &ask.ask_id, "peer_hops_exceeded", false)?;
        }
        super::worker::wake();
        let ask = ctx.store.snapshot()?.asks[&ask.ask_id].clone();
        let mut view = crate::asks::view::requester_view(&ctx.store.snapshot()?, &ask);
        view["ok"] = json!(true);
        view["created"] = json!(created);
        view["detail"] = json!(if hops_exceeded {
            "Cross-host hop limit exceeded. The delegation failed; its state delivery is available.".to_string()
        } else {
            let arrival = if ctx.wait { crate::asks::tools::ARRIVAL_WAIT } else { crate::asks::tools::ARRIVAL_NO_WAIT };
            format!("Queued on the other host. Questions go to its owner. {arrival}")
        });
        Ok(view)
    })();
    result.map_err(ask_error)
}
pub fn queue_op(
    state: &mut AskState,
    id: &str,
    outcome: &str,
    now: DateTime<Utc>,
) -> std::result::Result<Value, AskError> {
    if !matches!(outcome, "stop" | "abandoned" | "accepted") {
        return Err(AskError::new(
            "remote_unsupported",
            "unsupported remote operation",
        ));
    }
    if outcome == "accepted"
        && state
            .asks
            .get(id)
            .is_none_or(|a| a.review != Some(ReviewState::AwaitingAcceptance))
    {
        return Err(AskError::new(
            "ask_not_awaiting_acceptance",
            "only a delegation whose result waits for review can be accepted",
        ));
    }
    state.update_ask(id, now, |a| {
        if outcome == "accepted" {
            a.review = Some(ReviewState::Accepted);
        } else if outcome == "abandoned" && a.options.review {
            a.review = Some(ReviewState::Abandoned);
        }
        let remote = a.remote.as_mut().unwrap();
        remote.sync.pending_ops.push(Op {
            op_id: crate::asks::new_id("pop"),
            op: if outcome == "stop" { "stop" } else { "close" }.into(),
            outcome: Some(outcome.into()),
            reason: None,
        });
        remote.sync.next_attempt_at = None;
        if outcome != "stop" {
            remote.sync.local_terminal = true;
            a.closed = Some(if outcome == "accepted" {
                CloseOutcome::Accepted
            } else {
                CloseOutcome::Abandoned
            });
            a.closed_at = Some(super::now());
        }
    });
    super::worker::wake();
    Ok(
        json!({"ok":true,"ask_id":id,"stopping":outcome=="stop","detail":"Remote operation queued."}),
    )
}

/// Commit the effect and the op receipt together, including runtime stop commands.
pub fn receive_op(store: &AskStore, peer: &Peer, id: &str, op: Op) -> Result<()> {
    if !id
        .strip_prefix("ask-")
        .is_some_and(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        || op
            .reason
            .as_ref()
            .is_some_and(|v| v.len() > 128 || v.chars().any(char::is_control))
        || op.op_id.is_empty()
        || op.op_id.len() > 128
        || !matches!(op.op.as_str(), "stop" | "close")
        || (op.op == "close"
            && !matches!(op.outcome.as_deref(), None | Some("accepted" | "abandoned")))
    {
        return Err(Error::new("remote_unsupported", 400));
    }
    store.with_state(|s| -> Result<()> {
        if !s.asks.contains_key(id) && op.op == "close" && op.outcome.as_deref() != Some("accepted")
        {
            if let Some((owner, old)) = s.peer_cancellations.get(id) {
                return if owner == &peer.peer_id && old == &op {
                    Ok(())
                } else {
                    Err(Error::new("idempotency_conflict", 409))
                };
            }
            if s.peer_cancellations
                .values()
                .filter(|(owner, _)| owner == &peer.peer_id)
                .count()
                >= 10000
            {
                return Err(Error::new("peer_rate_limited", 429));
            }
            s.peer_cancellations
                .insert(id.into(), (peer.peer_id.clone(), op));
            return Ok(());
        }
        let a = authorized(s, peer, id)?;
        if let Some(old) = a
            .peer_inbound
            .as_ref()
            .and_then(|i| i.received_ops.iter().find(|o| o.op_id == op.op_id))
        {
            return if old == &op {
                Ok(())
            } else {
                Err(Error::new("idempotency_conflict", 409))
            };
        }
        if a.is_open() {
            crate::asks::tools::close_in_state(
                s,
                None,
                id,
                if op.op == "stop" {
                    "stop"
                } else {
                    op.outcome.as_deref().unwrap_or("abandoned")
                },
                false,
                Utc::now(),
            )
            .map_err(from_ask)?;
        }
        s.asks
            .get_mut(id)
            .and_then(|a| a.peer_inbound.as_mut())
            .ok_or_else(|| Error::new("ask_not_found", 404))?
            .received_ops
            .push(op);
        Ok(())
    })??;
    Ok(())
}

/// Suggestions never broaden local target resolution or expose invisible peers.
pub fn suggestions(name: &str) -> Vec<String> {
    other_hosts()
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["agents"].as_array().into_iter().flatten())
        .filter(|a| a["name"].as_str() == Some(name))
        .filter_map(|a| a["address"].as_str().map(str::to_string))
        .take(10)
        .collect()
}
