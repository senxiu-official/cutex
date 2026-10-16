//! Independent per-peer transport; expiry and diagnostics never wait for network I/O.
use super::{
    model::{DeadLetter, Op, Snapshot},
    store::{Peer, Store},
    Error, Result,
};
use crate::asks::{
    model::*,
    store::{AskState, AskStore},
};
use chrono::Utc;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::Duration,
};

fn signal() -> &'static (Mutex<bool>, Condvar) {
    static SIGNAL: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
    SIGNAL.get_or_init(|| (Mutex::new(false), Condvar::new()))
}
pub fn wake() {
    let (lock, cv) = signal();
    if let Ok(mut pending) = lock.lock() {
        *pending = true;
        cv.notify_one();
    }
}
pub fn spawn() -> anyhow::Result<()> {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("cutex-peers".into())
        .spawn(|| {
            let running = Arc::new(Mutex::new(BTreeSet::<String>::new()));
            let mut views = BTreeMap::new();
            loop {
                if let Ok(store) = Store::open() {
                    // Merely starting a daemon with peers disabled does not create a peer store.
                    if store.dir.exists() {
                        if let Ok(asks) = AskStore::open_default() {
                            let _ = maintain(&store, &asks);
                            if let Ok(state) = store.snapshot() {
                                let projected =
                                    super::routes::peer_views(&state, &asks).unwrap_or_default();
                                for peer in state.peers.values() {
                                    if let Some(view) = projected.get(&peer.peer_id) {
                                        let key = super::routes::peer_change_key(view);
                                        if views.get(&peer.peer_id) != Some(&key) {
                                            super::routes::changed(&peer.peer_id, "state");
                                            views.insert(peer.peer_id.clone(), key);
                                        }
                                    }
                                    if !state.settings.enabled
                                        || !peer.enabled
                                        || peer.pending_until.is_some()
                                        || peer.status == "removedByPeer"
                                    {
                                        continue;
                                    }
                                    let id = peer.peer_id.clone();
                                    if !running.lock().unwrap().insert(id.clone()) {
                                        continue;
                                    }
                                    let task_running = Arc::clone(&running);
                                    let store = store.clone();
                                    let asks = asks.clone();
                                    let thread_id = id.clone();
                                    let result = std::thread::Builder::new()
                                        .name("cutex-peer-transport".into())
                                        .spawn(move || {
                                            // Remove the running marker even if one peer task panics.
                                            let _ = std::panic::catch_unwind(|| {
                                                send_peer(&store, &asks, &thread_id)
                                            });
                                            task_running
                                                .lock()
                                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                                .remove(&thread_id);
                                        });
                                    if result.is_err() {
                                        running.lock().unwrap().remove(&id);
                                    }
                                }
                                views.retain(|id, _| state.peers.contains_key(id));
                            }
                        }
                    }
                }
                let (lock, cv) = signal();
                let guard = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (mut guard, _) = cv
                    .wait_timeout_while(guard, Duration::from_secs(1), |p| !*p)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = false;
            }
        })?;
    Ok(())
}
fn due(at: &Option<String>) -> bool {
    at.as_deref()
        .and_then(crate::asks::parse_time)
        .is_none_or(|t| t <= Utc::now())
}
pub fn retry_at(failures: u32) -> String {
    let base = (2u64.saturating_pow(failures.min(9))).clamp(2, 300);
    let jitter = 0.8 + f64::from(uuid::Uuid::new_v4().as_bytes()[0]) / 255.0 * 0.4;
    (Utc::now() + chrono::Duration::milliseconds((base as f64 * jitter * 1000.0) as i64))
        .to_rfc3339()
}
fn max_age(hours: u32) -> chrono::Duration {
    #[cfg(debug_assertions)]
    if crate::config::instance::is_isolated()
        && std::env::var_os("CUTEX_TEST_FORBID_REAL_HOME").is_some()
    {
        if let Some(s) = std::env::var("CUTEX_PEER_TEST_MAX_AGE_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| (1..=3600).contains(v))
        {
            return chrono::Duration::seconds(s);
        }
    }
    chrono::Duration::hours(hours as i64)
}
fn interrupt(code: &str) -> InterruptReason {
    match code {
        "peerRemoved" => InterruptReason::PeerRemoved,
        "peerDisabled" => InterruptReason::PeerDisabled,
        "peersDisabled" => InterruptReason::PeersDisabled,
        "removedByPeer" => InterruptReason::RemovedByPeer,
        _ => InterruptReason::OutcomeUnknown,
    }
}
fn fail_in_state(s: &mut AskState, id: &str, code: &str, unknown: bool) {
    if s.asks.get(id).is_none_or(|a| {
        a.closed.is_some() || a.remote.as_ref().is_none_or(|r| r.sync.local_terminal)
    }) {
        return;
    }
    s.update_ask(id, Utc::now(), |a| {
        a.execution = if unknown {
            ExecutionState::Interrupted
        } else {
            ExecutionState::Failed
        };
        a.hold = None;
        a.closed = Some(if unknown {
            CloseOutcome::Stopped
        } else {
            CloseOutcome::Completed
        });
        a.closed_at = Some(super::now());
        let r = a.remote.as_mut().unwrap();
        r.sync.local_terminal = true;
        r.sync.last_error = Some(code.into());
        r.sync.interrupt_reason = unknown.then(|| interrupt(code));
        r.sync.state = code.into();
        r.sync.pending_ops.clear();
        // Even an unacknowledged PUT may already execute remotely. Expiry and
        // disable retain cancellation until the peer can receive it again.
        if matches!(
            code,
            "peer_unreachable" | "outcomeUnknown" | "peerDisabled" | "peersDisabled"
        ) {
            r.sync.pending_ops.push(Op {
                op_id: crate::asks::new_id("pop"),
                op: "close".into(),
                outcome: Some("abandoned".into()),
                reason: Some(
                    if matches!(code, "peerDisabled" | "peersDisabled") {
                        code
                    } else {
                        "expired"
                    }
                    .into(),
                ),
            });
            r.sync.next_attempt_at = None;
        }
    });
    if let Some(a) = s.asks.get(id).cloned() {
        crate::asks::delivery::record_state_delivery(s, &a, code, Utc::now());
    }
}

pub fn fail(asks: &AskStore, id: &str, code: &str, unknown: bool) -> Result<()> {
    asks.with_state(|s| fail_in_state(s, id, code, unknown))?;
    Ok(())
}

fn has_peer_work(ask: &AskRecord) -> bool {
    if let Some(remote) = &ask.remote {
        (ask.is_open() && !remote.sync.local_terminal) || !remote.sync.pending_ops.is_empty()
    } else if let Some(inbound) = &ask.peer_inbound {
        ask.is_open() || !inbound.peer_deliveries.is_empty()
    } else {
        false
    }
}

fn remove_ask_in_state(s: &mut AskState, id: &str, code: &str) -> Result<()> {
    let Some(ask) = s.asks.get(id).filter(|ask| has_peer_work(ask)) else {
        return Ok(());
    };
    if ask.remote.is_some() {
        fail_in_state(s, id, code, true);
        if let Some(remote) = s.asks.get_mut(id).and_then(|ask| ask.remote.as_mut()) {
            remote.sync.pending_ops.clear();
        }
    } else if matches!(ask.requester, Requester::Peer { .. }) {
        if ask.is_open() {
            crate::asks::tools::close_in_state(s, None, id, "abandoned", false, Utc::now())
                .map_err(|_| Error::new("persistence_unavailable", 503))?;
        }
        if let Some(a) = s.asks.get_mut(id) {
            if let Some(attempt) = a.attempts.last_mut() {
                attempt.interrupt_reason = Some(interrupt(code));
            }
            if let Some(i) = &mut a.peer_inbound {
                i.peer_deliveries.clear();
                i.queued_at = None;
                i.delivery_stopped = true;
                i.last_error = Some(code.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static MAINTENANCE_TRANSACTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn take_maintenance_transactions() -> usize {
    MAINTENANCE_TRANSACTIONS.with(|count| count.replace(0))
}

pub fn removed(asks: &AskStore, peer_id: &str) -> Result<()> {
    removed_with_reason(asks, peer_id, "peerRemoved")
}
pub fn removed_with_reason(asks: &AskStore, peer_id: &str, code: &str) -> Result<()> {
    let (ids, has_receipts) = asks.inspect(|state| {
        let ids: Vec<_> = state.asks.values().filter(|ask| has_peer_work(ask))
        .filter(|ask| ask.remote.as_ref().is_some_and(|r| r.peer_id == peer_id)
            || matches!(&ask.requester, Requester::Peer { peer_id: id, .. } if id == peer_id))
        .map(|ask| ask.ask_id.clone()).collect();
        let has_receipts = state
            .peer_cancellations
            .values()
            .any(|(owner, _)| owner == peer_id);
        (ids, has_receipts)
    })?;
    if ids.is_empty() && !has_receipts {
        return Ok(());
    }
    #[cfg(test)]
    MAINTENANCE_TRANSACTIONS.with(|count| count.set(count.get() + 1));
    asks.with_state(|s| -> Result<()> {
        s.peer_cancellations
            .retain(|_, (owner, _)| owner != peer_id);
        for id in &ids {
            remove_ask_in_state(s, id, code)?;
        }
        Ok(())
    })??;
    Ok(())
}
pub fn note_error(store: &Store, peer: &Peer, error: &Error) -> Result<()> {
    if !error.is_protocol()
        && !error.is_transport()
        && !error.is_address_rejected()
        && error.code != "identity_changed"
        && error.status != 401
    {
        return Ok(());
    }
    let code = error.request_failure_code();
    store.with(|s| {
        if let Some(p) = s.peers.get_mut(&peer.peer_id) {
            p.last_error = Some(
                if let Some(missing) = error.details.get("missingFeatures") {
                    format!("{code}: {missing}")
                } else {
                    code.into()
                },
            );
            p.probe_failures = p.probe_failures.saturating_add(1);
            p.next_probe_at = Some(retry_at(p.probe_failures));
            p.status = if error.code == "identity_changed" {
                "identityChanged"
            } else if error.status == 401 {
                "credentialRejected"
            } else if error.is_protocol() {
                "versionMismatch"
            } else {
                "unreachable"
            }
            .into();
        }
        Ok(())
    })
}
fn hold_in_state(s: &mut AskState, id: &str, value: Hold) {
    if s.asks
        .get(id)
        .is_none_or(|a| a.closed.is_some() || a.hold.as_ref() == Some(&value))
    {
        return;
    }
    s.update_ask(id, Utc::now(), |a| a.hold = Some(value));
    let a = s.asks[id].clone();
    crate::asks::delivery::record_state_delivery(s, &a, "peerHold", Utc::now());
}

fn dead_letter_in_state(s: &mut AskState, id: &str, code: &str) {
    if let Some(i) = s.asks.get_mut(id).and_then(|a| a.peer_inbound.as_mut()) {
        for event in i.peer_deliveries.drain(..) {
            i.dead_letters.push(DeadLetter {
                event,
                failed_at: super::now(),
                code: code.into(),
            });
        }
        i.result_undeliverable = true;
        i.delivery_stopped = true;
        i.queued_at = None;
        i.last_error = Some(code.into());
    }
}

fn prune_dead_letters(s: &mut AskState, peers: &BTreeSet<String>) {
    // One scan for every affected peer, rather than rescanning all ask history
    // for each expired outbox. Retention remains newest 100 events per peer.
    let mut entries = BTreeMap::<String, Vec<(String, String, String)>>::new();
    for a in s.asks.values() {
        let Requester::Peer { peer_id, .. } = &a.requester else {
            continue;
        };
        if !peers.contains(peer_id) {
            continue;
        }
        if let Some(inbound) = &a.peer_inbound {
            entries
                .entry(peer_id.clone())
                .or_default()
                .extend(inbound.dead_letters.iter().map(|d| {
                    (
                        d.failed_at.clone(),
                        a.ask_id.clone(),
                        d.event.event_id.clone(),
                    )
                }));
        }
    }
    let mut removals = BTreeMap::<String, BTreeSet<String>>::new();
    for events in entries.values_mut() {
        events.sort();
        for (_, ask, event) in events.iter().take(events.len().saturating_sub(100)) {
            removals
                .entry(ask.clone())
                .or_default()
                .insert(event.clone());
        }
    }
    for (ask, events) in removals {
        if let Some(i) = s.asks.get_mut(&ask).and_then(|a| a.peer_inbound.as_mut()) {
            i.dead_letters
                .retain(|d| !events.contains(&d.event.event_id));
        }
    }
}

pub fn dead_letter(
    store: &Store,
    asks: &AskStore,
    peer: &Peer,
    id: &str,
    code: &str,
) -> Result<()> {
    asks.with_state(|s| {
        dead_letter_in_state(s, id, code);
        prune_dead_letters(s, &BTreeSet::from([peer.peer_id.clone()]));
    })?;
    audit_dead_letter(store, peer, id, code);
    Ok(())
}

fn audit_dead_letter(store: &Store, peer: &Peer, id: &str, code: &str) {
    super::audit::record(
        store,
        "deadLetter",
        Some(peer),
        &format!("/peer/v1/asks/{id}"),
        &Err(Error::new(code, 410)),
        0,
        None,
    );
}

enum Maintenance {
    Remove {
        id: String,
        code: &'static str,
    },
    Fail {
        id: String,
        code: &'static str,
        unknown: bool,
    },
    Hold {
        id: String,
        hold: Hold,
    },
    DeadLetter {
        id: String,
        peer_id: String,
    },
}

/// Local lifecycle sweep: inspect history once, mutate only active work/outboxes,
/// and commit all changes together. Inert historical asks never cause a write.
pub fn maintain(store: &Store, asks: &AskStore) -> Result<()> {
    maintain_inner(store, asks, || {})
}

#[cfg(test)]
pub(super) fn maintain_after_inspect(
    store: &Store,
    asks: &AskStore,
    before_commit: impl FnOnce(),
) -> Result<()> {
    maintain_inner(store, asks, before_commit)
}

fn maintain_inner(store: &Store, asks: &AskStore, before_commit: impl FnOnce()) -> Result<()> {
    let mut peers = store.snapshot()?;
    let now = Utc::now();
    let ttl = max_age(peers.settings.outbox_max_age_hours);
    let expired: Vec<_> = peers
        .peers
        .values()
        .filter(|p| {
            p.pending_until
                .as_deref()
                .and_then(crate::asks::parse_time)
                .is_some_and(|t| t < now)
        })
        .map(|p| p.peer_id.clone())
        .collect();
    for id in expired {
        store.remove(&id)?;
        peers.peers.remove(&id);
    }
    let (actions, removed_peers) = asks.inspect(|state| {
        let mut actions = Vec::new();
        let mut removed_peers = BTreeSet::new();
        let removed_reason = |id: &str| match peers.peers.get(id) {
            None => Some("peerRemoved"),
            Some(p) if p.status == "removedByPeer" => Some("removedByPeer"),
            _ => None,
        };
        for (owner, _) in state.peer_cancellations.values() {
            if removed_reason(owner).is_some() {
                removed_peers.insert(owner.clone());
            }
        }
        for a in state.asks.values().filter(|a| has_peer_work(a)) {
            if let Some(r) = &a.remote {
                if let Some(code) = removed_reason(&r.peer_id) {
                    removed_peers.insert(r.peer_id.clone());
                    actions.push(Maintenance::Remove {
                        id: a.ask_id.clone(),
                        code,
                    });
                    continue;
                }
                if !a.is_open() || r.sync.local_terminal {
                    continue;
                }
                let p = &peers.peers[&r.peer_id];
                if !peers.settings.enabled || !p.enabled {
                    actions.push(Maintenance::Fail {
                        id: a.ask_id.clone(),
                        code: if !peers.settings.enabled {
                            "peersDisabled"
                        } else {
                            "peerDisabled"
                        },
                        unknown: true,
                    });
                    continue;
                }
                let age = now
                    - crate::asks::parse_time(if r.sync.accepted {
                        &r.sync.last_received_at
                    } else {
                        &a.created_at
                    })
                    .unwrap_or(now);
                if age > ttl {
                    let unknown = r.sync.accepted
                        || matches!(p.status.as_str(), "identityChanged" | "versionMismatch");
                    actions.push(Maintenance::Fail {
                        id: a.ask_id.clone(),
                        code: if unknown {
                            "outcomeUnknown"
                        } else {
                            "peer_unreachable"
                        },
                        unknown,
                    });
                } else {
                    let hold = match p.status.as_str() {
                        "identityChanged" => Some(Hold::PeerIdentityChanged),
                        "versionMismatch" => Some(Hold::PeerVersionMismatch),
                        "credentialRejected" => Some(Hold::PeerUnreachable),
                        _ => None,
                    };
                    if let Some(hold) = hold.filter(|hold| a.hold.as_ref() != Some(hold)) {
                        actions.push(Maintenance::Hold {
                            id: a.ask_id.clone(),
                            hold,
                        });
                    }
                }
            } else if let (Requester::Peer { peer_id, .. }, Some(i)) =
                (&a.requester, &a.peer_inbound)
            {
                if let Some(code) = removed_reason(peer_id) {
                    removed_peers.insert(peer_id.clone());
                    actions.push(Maintenance::Remove {
                        id: a.ask_id.clone(),
                        code,
                    });
                } else if !i.peer_deliveries.is_empty()
                    && now
                        - crate::asks::parse_time(i.queued_at.as_deref().unwrap_or(&a.created_at))
                            .unwrap_or(now)
                        > ttl
                {
                    actions.push(Maintenance::DeadLetter {
                        id: a.ask_id.clone(),
                        peer_id: peer_id.clone(),
                    });
                }
            }
        }
        (actions, removed_peers)
    })?;
    if actions.is_empty() && removed_peers.is_empty() {
        return Ok(());
    }
    let mut dead = Vec::new();
    before_commit();
    #[cfg(test)]
    MAINTENANCE_TRANSACTIONS.with(|count| count.set(count.get() + 1));
    asks.with_state(|s| -> Result<()> {
        s.peer_cancellations
            .retain(|_, (owner, _)| !removed_peers.contains(owner));
        let mut expired_peers = BTreeSet::new();
        for action in actions {
            match action {
                Maintenance::Remove { id, code } => remove_ask_in_state(s, &id, code)?,
                Maintenance::Fail {
                    id,
                    mut code,
                    mut unknown,
                } => {
                    if matches!(code, "peer_unreachable" | "outcomeUnknown") {
                        let Some(a) = s.asks.get(&id) else {
                            continue;
                        };
                        let Some(r) = &a.remote else {
                            continue;
                        };
                        let received = if r.sync.accepted {
                            &r.sync.last_received_at
                        } else {
                            &a.created_at
                        };
                        if now - crate::asks::parse_time(received).unwrap_or(now) <= ttl {
                            continue;
                        }
                        unknown = r.sync.accepted
                            || peers.peers.get(&r.peer_id).is_some_and(|p| {
                                matches!(p.status.as_str(), "identityChanged" | "versionMismatch")
                            });
                        code = if unknown {
                            "outcomeUnknown"
                        } else {
                            "peer_unreachable"
                        };
                    }
                    fail_in_state(s, &id, code, unknown);
                }
                Maintenance::Hold { id, hold } => hold_in_state(s, &id, hold),
                Maintenance::DeadLetter { id, peer_id } => {
                    // An acknowledgement may replace the old outbox with a new,
                    // non-expired event between inspection and the transaction.
                    if s.asks.get(&id).is_some_and(|a| {
                        a.peer_inbound.as_ref().is_some_and(|i| {
                            !i.peer_deliveries.is_empty()
                                && now
                                    - crate::asks::parse_time(
                                        i.queued_at.as_deref().unwrap_or(&a.created_at),
                                    )
                                    .unwrap_or(now)
                                    > ttl
                        })
                    }) {
                        dead_letter_in_state(s, &id, "peer_event_expired");
                        expired_peers.insert(peer_id.clone());
                        dead.push((id, peer_id));
                    }
                }
            }
        }
        if !expired_peers.is_empty() {
            prune_dead_letters(s, &expired_peers);
        }
        Ok(())
    })??;
    for (id, peer_id) in dead {
        if let Some(peer) = peers.peers.get(&peer_id) {
            audit_dead_letter(store, peer, &id, "peer_event_expired");
        }
    }
    Ok(())
}
pub fn tick(store: &Store, asks: &AskStore) -> Result<()> {
    maintain(store, asks)?;
    for id in store.snapshot()?.peers.keys() {
        send_peer(store, asks, id)?;
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    static SEND_STORE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn take_send_store_reads() -> usize {
    SEND_STORE_READS.with(|reads| reads.replace(0))
}

fn send_state(store: &Store) -> Result<super::store::State> {
    #[cfg(test)]
    SEND_STORE_READS.with(|reads| reads.set(reads.get() + 1));
    store.snapshot()
}

fn current_sender(store: &Store, id: &str) -> Result<Option<Peer>> {
    let state = send_state(store)?;
    Ok(state
        .peers
        .get(id)
        .filter(|p| {
            state.settings.enabled
                && p.enabled
                && p.pending_until.is_none()
                && p.status == "online"
                && due(&p.next_probe_at)
        })
        .cloned())
}

fn has_due_send(ask: &AskRecord, id: &str) -> bool {
    if let Some(r) = ask.remote.as_ref().filter(|r| r.peer_id == id) {
        due(&r.sync.next_attempt_at)
            && (!r.sync.pending_ops.is_empty()
                || (r.sync.accepted
                    && ask.is_open()
                    && !r.sync.local_terminal
                    && due(&r.sync.reconcile_at)))
    } else if let (Requester::Peer { peer_id, .. }, Some(i)) = (&ask.requester, &ask.peer_inbound) {
        peer_id == id && !i.peer_deliveries.is_empty() && due(&i.next_attempt_at)
    } else {
        false
    }
}

pub fn send_peer(store: &Store, asks: &AskStore, id: &str) -> Result<()> {
    let state = send_state(store)?;
    let Some(peer) = state.peers.get(id) else {
        return Ok(());
    };
    if !state.settings.enabled
        || !peer.enabled
        || peer.pending_until.is_some()
        || peer.status == "removedByPeer"
    {
        return Ok(());
    }
    if !due(&peer.next_probe_at) {
        return Ok(());
    }
    let refresh = peer.status != "online"
        || peer
            .last_seen_at
            .as_deref()
            .and_then(crate::asks::parse_time)
            .is_none_or(|t| {
                (Utc::now() - t).num_seconds() >= state.settings.refresh_agents_secs as i64
            });
    if refresh {
        if super::routes::refresh(store, peer).is_err() {
            return Ok(());
        }
        if peer.status != "online" {
            asks.with_state(|s| {
                for a in s.asks.values_mut() {
                    if let Some(r) = a.remote.as_mut().filter(|r| r.peer_id == id) {
                        r.sync.next_attempt_at = None;
                        r.sync.reconcile_at = None;
                    }
                    if matches!(&a.requester, Requester::Peer {peer_id,..} if peer_id==id) {
                        if let Some(i) = a.peer_inbound.as_mut() {
                            i.next_attempt_at = None;
                        }
                    }
                }
            })?;
        }
    }
    // Owner profiles (peer-owner-profile): first round, heartbeat or resync.
    super::profiles::maybe_sync(store, id, refresh);
    let pending = asks.inspect(|s| {
        s.asks
            .values()
            .filter(|a| has_due_send(a, id))
            .cloned()
            .collect::<Vec<_>>()
    })?;
    for a in &pending {
        if let Some(r) = a.remote.as_ref().filter(|r| r.peer_id == id) {
            if !due(&r.sync.next_attempt_at) {
                continue;
            }
            let op = r.sync.pending_ops.first();
            let reconcile = op.is_none()
                && r.sync.accepted
                && a.is_open()
                && !r.sync.local_terminal
                && due(&r.sync.reconcile_at);
            if op.is_none() && !reconcile {
                continue;
            }
            let route = format!("/peer/v1/asks/{}", a.ask_id);
            // Only selected, due work incurs the fresh policy read.
            let Some(peer) = current_sender(store, id)? else {
                break;
            };
            let result = match op {
                Some(o) if o.op == "create" => super::client::call(
                    store,
                    &peer,
                    "PUT",
                    &route,
                    Some(&serde_json::to_value(&r.request)?),
                ),
                Some(o) => super::client::call(
                    store,
                    &peer,
                    "POST",
                    &format!("{route}/ops"),
                    Some(&serde_json::to_value(o)?),
                ),
                None => super::client::call(store, &peer, "GET", &route, None),
            }
            .and_then(|v| {
                let snap: super::wire::WireSnapshotV1 = serde_json::from_value(v)
                    .map_err(|_| Error::new("peer_protocol_error", 502))?;
                if snap.schema_version != 1 {
                    return Err(Error::new("peer_protocol_error", 502));
                }
                let snap: Snapshot = snap.into();
                if snap.ask_id != a.ask_id {
                    return Err(Error::new("peer_protocol_error", 502));
                }
                super::asks::apply(asks, &peer, snap)
            });
            match result {
                Ok(()) => {
                    asks.with_state(|s| {
                        if let Some(r) = s.asks.get_mut(&a.ask_id).and_then(|a| a.remote.as_mut()) {
                            if let Some(op) = op {
                                r.sync.pending_ops.retain(|o| o.op_id != op.op_id);
                            }
                            r.sync.failures = 0;
                            r.sync.next_attempt_at = None;
                            r.sync.reconcile_at =
                                Some((Utc::now() + chrono::Duration::minutes(10)).to_rfc3339());
                        }
                    })?;
                }
                Err(e) => {
                    note_error(store, &peer, &e)?;
                    let code = e.request_failure_code();
                    // A close for an unknown id is acknowledged: no remote execution remains.
                    if matches!(e.status, 404 | 410) && op.is_some_and(|o| o.op == "close") {
                        asks.with_state(|s| {
                            if let Some(r) =
                                s.asks.get_mut(&a.ask_id).and_then(|a| a.remote.as_mut())
                            {
                                r.sync.pending_ops.retain(|o| {
                                    Some(o.op_id.as_str()) != op.map(|o| o.op_id.as_str())
                                });
                            }
                        })?;
                    } else if !r.sync.local_terminal
                        && r.sync.accepted
                        && op.is_some_and(|op| op.op == "stop")
                        && code == "peer_request_incompatible"
                    {
                        // An unsupported stop says nothing about remote execution.
                        // End the mirror once, but durably try ordinary close next.
                        asks.with_state(|s| {
                            if s.asks.get(&a.ask_id).is_none_or(|a| {
                                a.closed.is_some()
                                    || a.remote.as_ref().is_none_or(|r| r.sync.local_terminal)
                            }) {
                                return;
                            }
                            fail_in_state(s, &a.ask_id, "outcomeUnknown", true);
                            if let Some(r) =
                                s.asks.get_mut(&a.ask_id).and_then(|a| a.remote.as_mut())
                            {
                                r.sync.last_error = Some(code.into());
                                if let Some(close) = r.sync.pending_ops.first_mut() {
                                    close.reason = Some(code.into());
                                }
                            }
                        })?;
                    } else if r.sync.local_terminal && code == "peer_request_incompatible" {
                        // This operation cannot succeed on this endpoint. Keep
                        // the existing local outcome, remove only this receipt
                        // from the queue, and continue unrelated work.
                        asks.with_state(|s| {
                            if let Some(r) =
                                s.asks.get_mut(&a.ask_id).and_then(|a| a.remote.as_mut())
                            {
                                if let Some(op) = op {
                                    r.sync.pending_ops.retain(|o| o.op_id != op.op_id);
                                }
                                r.sync.last_error = Some(code.into());
                                r.sync.failures = 0;
                                r.sync.next_attempt_at = None;
                            }
                        })?;
                    } else if !r.sync.local_terminal
                        && !e.retryable
                        && !e.is_protocol()
                        && !e.is_address_rejected()
                        && e.code != "identity_changed"
                        && e.status != 401
                    {
                        fail(asks, &a.ask_id, code, false)?;
                    } else {
                        asks.with_state(|s| {
                            if let Some(r) =
                                s.asks.get_mut(&a.ask_id).and_then(|a| a.remote.as_mut())
                            {
                                r.sync.failures = r.sync.failures.saturating_add(1);
                                r.sync.next_attempt_at = Some(retry_at(r.sync.failures));
                                r.sync.last_error = Some(code.into());
                            }
                        })?;
                    }
                    if e.is_transport()
                        || e.is_protocol()
                        || e.is_address_rejected()
                        || e.code == "identity_changed"
                        || e.status == 401
                    {
                        break;
                    }
                }
            }
        } else if let (Requester::Peer { peer_id, .. }, Some(i)) = (&a.requester, &a.peer_inbound) {
            if peer_id != id || i.peer_deliveries.is_empty() || !due(&i.next_attempt_at) {
                continue;
            }
            let mut events = i.peer_deliveries.clone();
            for e in &mut events {
                e.snapshot =
                    super::asks::wire_snapshot(asks, e.snapshot.clone(), &super::pair::label())?;
            }
            let Some(peer) = current_sender(store, id)? else {
                break;
            };
            let result=super::client::call(store,&peer,"POST","/peer/v1/events",Some(&json!({"fromInstanceId":super::instance_id(),"events":events.iter().map(super::wire::WireEventV1::from).collect::<Vec<_>>()}))).and_then(|v|{
                let acked=v["acked"].as_array().ok_or_else(||Error::new("peer_protocol_error",502))?;
                if events.iter().any(|e| !acked.iter().any(|id| id==&e.event_id)) { return Err(Error::new("peer_protocol_error",502)); }
                Ok(v)
            });
            match result {
                Ok(v) => {
                    asks.with_state(|s| {
                        if let Some(i) = s
                            .asks
                            .get_mut(&a.ask_id)
                            .and_then(|a| a.peer_inbound.as_mut())
                        {
                            let acked = v["acked"].as_array().unwrap();
                            i.peer_deliveries
                                .retain(|e| !acked.iter().any(|v| v == &e.event_id));
                            if i.peer_deliveries.is_empty() {
                                i.queued_at = None;
                            }
                            i.failures = 0;
                            i.next_attempt_at = None;
                        }
                    })?;
                }
                Err(e) => {
                    note_error(store, &peer, &e)?;
                    let code = e.request_failure_code();
                    if !e.retryable
                        && !e.is_protocol()
                        && !e.is_address_rejected()
                        && e.code != "identity_changed"
                        && e.status != 401
                    {
                        dead_letter(store, asks, &peer, &a.ask_id, code)?;
                    } else {
                        asks.with_state(|s| {
                            if let Some(i) = s
                                .asks
                                .get_mut(&a.ask_id)
                                .and_then(|a| a.peer_inbound.as_mut())
                            {
                                i.failures = i.failures.saturating_add(1);
                                i.next_attempt_at = Some(retry_at(i.failures));
                                i.last_error = Some(code.into());
                            }
                        })?;
                    }
                    if e.is_transport()
                        || e.is_protocol()
                        || e.is_address_rejected()
                        || e.code == "identity_changed"
                        || e.status == 401
                    {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}
