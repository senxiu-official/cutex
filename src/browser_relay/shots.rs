//! Trusted turn-completed capture. Never mints a lease, model request, or turn.
//! The two bounded workers only use an already dispatched/delivered command's
//! exact live lease/page/runtime stamp. Failures never block model completion.
use super::{
    commands::{Admission, Authority, Plan, Status, COMMAND_TIMEOUT},
    model::*,
    state::{Relay, State},
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
const MAX_PENDING: usize = 256;
const MAX_COMPLETED: usize = 512;
const TRACK_TTL: Duration = Duration::from_secs(600);
#[derive(Clone, PartialEq, Eq, Hash)]
struct Turn {
    session: String,
    thread: String,
    generation: u64,
    turn: String,
}
impl Turn {
    fn from(agent: &super::auth::AgentContext) -> Self {
        Self {
            session: agent.session.clone(),
            thread: agent.thread.clone(),
            generation: agent.generation,
            turn: agent.turn.clone(),
        }
    }
}
#[derive(Clone)]
struct Candidate {
    authority: Authority,
    lease: String,
    target: Target,
    generation: String,
    page: u64,
    at: Instant,
}
#[derive(Default)]
pub(crate) struct Tracking {
    used: HashMap<Turn, HashMap<String, Candidate>>,
    completed: HashMap<Turn, Instant>,
    pub dropped: u64,
}
impl Tracking {
    fn sweep(&mut self) {
        let now = Instant::now();
        self.completed
            .retain(|_, at| now.saturating_duration_since(*at) < TRACK_TTL);
        self.used.retain(|_, items| {
            items.retain(|_, c| now.saturating_duration_since(c.at) < TRACK_TTL);
            !items.is_empty()
        });
    }
}
pub(crate) fn mark_delivered(state: &mut State, key: &str) {
    mark_used(state, key, true);
}
pub(crate) fn mark_dispatched(state: &mut State, key: &str) {
    mark_used(state, key, false);
}
fn mark_used(state: &mut State, key: &str, delivered: bool) {
    state.turn_shots.sweep();
    let Some(request) = state.requests.get(key).filter(|r| {
        r.purpose == "agent"
            && if delivered {
                r.status == Status::Succeeded && r.body.is_some()
            } else {
                r.status.sent()
            }
    }) else {
        return;
    };
    let Some(lease_id) = request.lease_id.as_ref().or(request.open_lease.as_ref()) else {
        return;
    };
    let Some(lease) = state.leases.get(lease_id).filter(|l| {
        l.view.state == "active"
            && l.generation == request.generation
            && l.view.cutex_session_id == request.authority.agent.session
    }) else {
        return;
    };
    let Some(page) = lease.page_generation else {
        return;
    };
    let turn = Turn::from(&request.authority.agent);
    if state.turn_shots.completed.contains_key(&turn) {
        return;
    }
    let count: usize = state.turn_shots.used.values().map(HashMap::len).sum();
    let already = state
        .turn_shots
        .used
        .get(&turn)
        .is_some_and(|items| items.contains_key(lease_id));
    if state.turn_shots.completed.len() >= MAX_COMPLETED || count >= MAX_PENDING && !already {
        state.turn_shots.dropped = state.turn_shots.dropped.saturating_add(1);
        return;
    }
    let candidate = Candidate {
        authority: request.authority.clone(),
        lease: lease_id.clone(),
        target: lease.view.target.clone(),
        generation: lease.generation.clone(),
        page,
        at: Instant::now(),
    };
    state
        .turn_shots
        .used
        .entry(turn)
        .or_default()
        .insert(lease_id.clone(), candidate);
}
fn live(relay: &Relay, candidate: &Candidate) -> bool {
    let identity = relay.state.lock().ok().and_then(|state| {
        state
            .devices
            .get(&candidate.target.device_id)
            .filter(|d| d.generation == candidate.generation && d.sender.is_some())
            .map(|d| d.identity.clone())
    });
    // Paired-client validation can read revocation state. Never do that I/O
    // under the relay-wide mutex; recheck the exact generation afterwards.
    let Some(identity) = identity.filter(|identity| (identity.valid)()) else {
        return false;
    };
    relay.state.lock().is_ok_and(|state| {
        state.leases.get(&candidate.lease).is_some_and(|l| {
            l.view.state == "active"
                && l.view.target == candidate.target
                && l.generation == candidate.generation
                && l.view.cutex_session_id == candidate.authority.agent.session
                && l.page_generation == Some(candidate.page)
        }) && state
            .devices
            .get(&candidate.target.device_id)
            .is_some_and(|d| {
                d.generation == candidate.generation
                    && d.sender.is_some()
                    && d.identity.id == identity.id
            })
    })
}
fn take(relay: &Relay, session: &str, thread: &str, generation: u64, turn: &str) -> Vec<Candidate> {
    let Ok(mut state) = relay.state.lock() else {
        return Vec::new();
    };
    state.turn_shots.sweep();
    let key = Turn {
        session: session.into(),
        thread: thread.into(),
        generation,
        turn: turn.into(),
    };
    if state.turn_shots.completed.contains_key(&key) {
        return Vec::new();
    }
    let Some(items) = state.turn_shots.used.remove(&key) else {
        return Vec::new();
    };
    if state.turn_shots.completed.len() >= MAX_COMPLETED {
        state.turn_shots.dropped = state.turn_shots.dropped.saturating_add(items.len() as u64);
        return Vec::new();
    }
    state.turn_shots.completed.insert(key, Instant::now());
    items.into_values().collect()
}
#[cfg(test)]
pub(crate) fn take_count_for_test(
    relay: &Relay,
    session: &str,
    thread: &str,
    generation: u64,
    turn: &str,
) -> usize {
    take(relay, session, thread, generation, turn).len()
}
struct Job {
    relay: Arc<Relay>,
    candidate: Candidate,
    deadline: Instant,
}
static WORKERS: OnceLock<Option<mpsc::SyncSender<Job>>> = OnceLock::new();
fn workers() -> Option<&'static mpsc::SyncSender<Job>> {
    WORKERS
        .get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel::<Job>(MAX_PENDING);
            let rx = Arc::new(Mutex::new(rx));
            for index in 0..2 {
                let rx = Arc::clone(&rx);
                if std::thread::Builder::new()
                    .name(format!("browser-turn-shot-{index}"))
                    .spawn(move || loop {
                        let job = {
                            let Ok(rx) = rx.lock() else {
                                return;
                            };
                            rx.recv()
                        };
                        let Ok(job) = job else {
                            return;
                        };
                        capture(job);
                    })
                    .is_err()
                {
                    return None;
                }
            }
            Some(tx)
        })
        .as_ref()
}
pub fn completed(relay: &Arc<Relay>, session: &str, thread: &str, generation: u64, turn: &str) {
    if !identifier(session) || !identifier(thread) || !identifier(turn) {
        return;
    }
    let candidates = take(relay, session, thread, generation, turn);
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    for candidate in candidates {
        if !live(relay, &candidate) {
            continue;
        }
        let accepted = workers().is_some_and(|tx| {
            tx.try_send(Job {
                relay: Arc::clone(relay),
                candidate,
                deadline,
            })
            .is_ok()
        });
        if !accepted {
            if let Ok(mut state) = relay.state.lock() {
                state.turn_shots.dropped = state.turn_shots.dropped.saturating_add(1);
            }
        }
    }
}
fn capture(job: Job) {
    let Job {
        relay,
        candidate,
        deadline,
    } = job;
    if Instant::now() >= deadline
        || !(candidate.authority.identity_current)()
        || !live(&relay, &candidate)
    {
        return;
    }
    let expected = candidate.clone();
    let weak = Arc::downgrade(&relay);
    let check: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        (expected.authority.identity_current)()
            && weak.upgrade().is_some_and(|relay| live(&relay, &expected))
    });
    let id = format!("turn-shot-{}", uuid::Uuid::new_v4().simple());
    let authority = Authority {
        agent: candidate.authority.agent.clone(),
        current: Arc::clone(&check),
        identity_current: Arc::clone(&candidate.authority.identity_current),
    };
    let admission = Admission {
        key: id.clone(),
        request_id: id.clone(),
        digest: id.clone(),
        authority,
        plan: Plan {
            target: candidate.target.clone(),
            generation: candidate.generation.clone(),
            lease_id: Some(candidate.lease.clone()),
            operation: json!({"kind":"tab.screenshot"}),
        },
        title: "Turn-end screenshot".into(),
        deadline,
        purpose: "owner-turn-shot",
        claim: None,
    };
    if relay.enqueue(admission).is_err() {
        return;
    }
    let result = (|| -> Result<()> {
        loop {
            let receipt = relay.receipt(&id)?;
            if receipt.status.terminal() {
                if receipt.status != Status::Succeeded || Instant::now() >= deadline || !check() {
                    return Err(RelayError::new("target_closed"));
                }
                let value = receipt
                    .content
                    .ok_or(RelayError::new("request_result_not_retained"))?;
                // Provider result validation already fully decoded this image.
                // Reuse that proof and retained byte permit; no second decoder
                // slot and no second compressed allocation for B10 storage.
                let image = value
                    .decoded_image
                    .as_ref()
                    .ok_or(RelayError::new("invalid_result"))?;
                if Instant::now() >= deadline || !check() {
                    return Err(RelayError::new("target_closed"));
                }
                super::shot_store::insert(&candidate.authority.agent, image, || {
                    Instant::now() < deadline && check()
                })?;
                return Ok(());
            }
            let state = relay
                .state
                .lock()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            let _ = relay
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .map_err(|_| RelayError::new("browser_unavailable"))?;
        }
    })();
    if result.is_err() {
        let _ = relay.cancel(&id, "cancelled");
    }
    // This is private consumption, not model delivery/browser-used marking.
    if let Ok(mut state) = relay.state.lock() {
        if let Some(request) = state.requests.get_mut(&id) {
            request.body = None;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_turn_shot_empty_or_wrong_completion_cannot_authorize() {
        let relay = Relay::new();
        assert!(take(&relay, "s", "t", 1, "turn").is_empty());
        assert!(relay.state.lock().unwrap().turn_shots.completed.is_empty());
    }
    #[test]
    fn browser_turn_shot_exact_stamp_once_and_bounded() {
        let relay = Relay::new();
        let agent = super::super::auth::AgentContext {
            session: "s".into(),
            thread: "t".into(),
            generation: 2,
            turn: "turn".into(),
            runtime: "r".into(),
            credential_identity: "a".into(),
        };
        let candidate = Candidate {
            authority: Authority {
                agent: agent.clone(),
                current: Arc::new(|| true),
                identity_current: Arc::new(|| true),
            },
            lease: "l".into(),
            target: Target {
                host_id: "h".into(),
                device_id: "d".into(),
                guest_id: Some("g".into()),
            },
            generation: "gen".into(),
            page: 1,
            at: Instant::now(),
        };
        relay
            .state
            .lock()
            .unwrap()
            .turn_shots
            .used
            .insert(Turn::from(&agent), HashMap::from([("l".into(), candidate)]));
        assert!(take(&relay, "s", "t", 1, "turn").is_empty());
        assert!(take(&relay, "s", "t", 2, "other").is_empty());
        assert_eq!(take(&relay, "s", "t", 2, "turn").len(), 1);
        assert!(take(&relay, "s", "t", 2, "turn").is_empty());
        assert_eq!(relay.state.lock().unwrap().turn_shots.completed.len(), 1);
    }
    #[test]
    fn browser_turn_shot_fake_device_capture_stores_owner_image_and_expired_queue_never_dispatches()
    {
        let _decode_guard = super::super::images::TEST_DECODE_LOCK.lock().unwrap();
        use super::super::state::{Binding, DesktopIdentity};
        use crate::config::instance::{set_test_selection, Selection};
        use base64::Engine;
        let previous = crate::config::instance::selected().unwrap();
        let root = std::env::temp_dir().join(format!("browser-capture-{}", uuid::Uuid::new_v4()));
        set_test_selection(Selection::Custom(root.clone()));
        let relay = Arc::new(Relay::new());
        let identity = DesktopIdentity {
            id: "root:fixture".into(),
            valid: Arc::new(|| true),
        };
        let registered = relay
            .register(
                identity.clone(),
                Registration {
                    device_id: "d".into(),
                    host_id: "local".into(),
                    provider_id: "browser".into(),
                    executor: "inApp".into(),
                },
                10000,
                30000,
            )
            .unwrap();
        let binding = Binding {
            identity: &identity,
            device_id: "d",
            generation: registered["connectionGeneration"].as_str().unwrap(),
            key: registered["connectionKey"].as_str().unwrap(),
        };
        let subscription = relay.attach(&binding, None).unwrap();
        subscription.receiver.recv().unwrap();
        let lease = relay
            .grant_checked(
                &binding,
                GrantRequest {
                    device_id: "d".into(),
                    guest_id: "g".into(),
                    cutex_session_id: "s".into(),
                    title: "Page".into(),
                    url: "about:blank".into(),
                    source: GrantSource::Owner {},
                },
                "Agent".into(),
            )
            .unwrap();
        relay.activate(&binding, &lease.lease_id).unwrap();
        relay
            .state
            .lock()
            .unwrap()
            .leases
            .get_mut(&lease.lease_id)
            .unwrap()
            .page_generation = Some(1);
        let agent = super::super::auth::AgentContext {
            session: "s".into(),
            thread: "t".into(),
            generation: 1,
            turn: "turn".into(),
            runtime: "r".into(),
            credential_identity: "fixture".into(),
        };
        let candidate = Candidate {
            authority: Authority {
                agent,
                current: Arc::new(|| false),
                identity_current: Arc::new(|| true),
            },
            lease: lease.lease_id.clone(),
            target: lease.target.clone(),
            generation: binding.generation.into(),
            page: 1,
            at: Instant::now(),
        };
        // Active turn is over (current false). Only the internal identity fence
        // and original page/lease stamp permit this owner-purpose operation.
        capture(Job {
            relay: Arc::clone(&relay),
            candidate: candidate.clone(),
            deadline: Instant::now() - Duration::from_millis(1),
        });
        assert!(subscription.receiver.try_recv().is_err());
        let worker_relay = Arc::clone(&relay);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let binding = Binding {
                identity: &identity,
                device_id: "d",
                generation: registered["connectionGeneration"].as_str().unwrap(),
                key: registered["connectionKey"].as_str().unwrap(),
            };
            let marker = subscription
                .receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            let frame = worker_relay
                .prepare_frame(&binding, &marker)
                .unwrap()
                .unwrap();
            assert_eq!(frame["purpose"], "owner-turn-shot");
            assert_eq!(frame["operation"], json!({"kind":"tab.screenshot"}));
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 3)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            let result = json!({"dispatchSeq":frame["dispatchSeq"],"target":frame["target"],"operationKind":"tab.screenshot","status":"succeeded","result":{"kind":"tab.screenshot","pageGeneration":1,"mimeType":"image/png","data":base64::engine::general_purpose::STANDARD.encode(out.into_inner()),"width":2,"height":3,"viewport":{"width":2,"height":3},"captureRect":{"x":0,"y":0,"width":2,"height":3},"imageScale":{"x":1,"y":1},"displayScale":1}});
            worker_relay
                .accept_result(
                    &binding,
                    frame["requestId"].as_str().unwrap(),
                    serde_json::from_value(result).unwrap(),
                )
                .unwrap();
            done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        capture(Job {
            relay: Arc::clone(&relay),
            candidate: candidate.clone(),
            deadline: Instant::now() + COMMAND_TIMEOUT,
        });
        let items = super::super::shot_store::list("s", "turn").unwrap();
        assert_eq!(items["items"].as_array().unwrap().len(), 1);
        let shot = &items["items"][0];
        assert_eq!(shot["width"], 2);
        assert_eq!(
            super::super::shot_store::read(shot["id"].as_str().unwrap()).unwrap()["mimeType"],
            "image/png"
        );
        let count = relay.state.lock().unwrap().requests.len();
        relay
            .state
            .lock()
            .unwrap()
            .leases
            .get_mut(&lease.lease_id)
            .unwrap()
            .page_generation = Some(2);
        capture(Job {
            relay: Arc::clone(&relay),
            candidate,
            deadline: Instant::now() + COMMAND_TIMEOUT,
        });
        assert_eq!(relay.state.lock().unwrap().requests.len(), count);
        done_tx.send(()).unwrap();
        worker.join().unwrap();
        set_test_selection(previous);
        std::fs::remove_dir_all(root).unwrap();
    }
}
