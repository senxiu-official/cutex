//! Process-local, replaceable text traffic. This module never touches disk,
//! durable sequence/cursor state, or a native transport.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use super::model::{EventEnvelope, NativeMessageKind, PendingEvent};
use serde::Serialize;
use serde_json::Value;

pub const WINDOW: Duration = Duration::from_millis(40);

/// One coalesced notification for either durable or replaceable traffic. The
/// generation check under this mutex prevents the check/sleep lost-wake race.
#[derive(Default)]
pub(crate) struct StreamWake {
    version: Mutex<u64>,
    ready: Condvar,
}
impl StreamWake {
    pub(crate) fn version(&self) -> u64 {
        *self.version.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn notify(&self) {
        let mut version = self.version.lock().unwrap_or_else(|e| e.into_inner());
        *version = version.wrapping_add(1);
        self.ready.notify_one();
    }
    pub(crate) fn wait(&self, since: u64, timeout: Duration) {
        let version = self.version.lock().unwrap_or_else(|e| e.into_inner());
        if *version == since {
            drop(
                self.ready
                    .wait_timeout(version, timeout)
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }
}
const FRAME_BYTES: usize = 64 * 1024;
const SUB_BYTES: usize = 256 * 1024;
const SESSION_BYTES: usize = 1024 * 1024;
const GLOBAL_BYTES: usize = 64 * 1024 * 1024;
const SESSION_KEYS: usize = 64;
const GLOBAL_KEYS: usize = 4096;
const SUB_FRAMES: usize = 128;
const MAX_SUBSCRIBERS: usize = 128;
const MAX_FENCES: usize = 4096;
const SUB_METADATA_BYTES: usize = 64 * 1024;
const ANCHOR_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub struct LiveCapacityError;
impl std::fmt::Display for LiveCapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("live delta subscriber capacity exceeded")
    }
}
impl std::error::Error for LiveCapacityError {}

pub fn is_live_method(method: &str) -> bool {
    super::native_events::is_live_text_delta_method(method)
}

pub fn is_live_pending(pending: &PendingEvent) -> bool {
    pending
        .schema
        .as_ref()
        .is_some_and(|schema| supports_full_text_terminal(&schema.capabilities))
        && pending.native.as_ref().is_some_and(|native| {
            native.kind == NativeMessageKind::Notification
                && native
                    .message
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(is_live_method)
        })
}

/// Strict negotiation: old runtimes may omit item completion after abort/EOF.
/// Their deltas must retain the original journal and takeover semantics.
pub fn supports_full_text_terminal(capabilities: &Value) -> bool {
    capabilities
        .pointer("/features/fullTextItemTerminal")
        .and_then(Value::as_bool)
        == Some(true)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Anchor {
    pub stream_id: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveFrame {
    pub version: u8,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_scope: Option<&'static str>,
    pub cutex_session_id: String,
    pub host_id: String,
    pub runtime_generation: u64,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub item_id: String,
    pub method: String,
    pub producer_epoch: String,
    pub revision: u64,
    pub start_offset: u64,
    pub end_offset: u64,
    pub anchor: Option<Anchor>,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    session: String,
    host: String,
    generation: u64,
    thread: String,
    turn: Option<String>,
    item: String,
    method: String,
    indices: String,
}
impl Key {
    fn metadata_bytes(&self) -> usize {
        // Include string allocations and conservative hash bucket overhead.
        2 * (std::mem::size_of::<Self>()
            + self.session.capacity()
            + self.host.capacity()
            + self.thread.capacity()
            + self.turn.as_ref().map_or(0, String::capacity)
            + self.item.capacity()
            + self.method.capacity()
            + self.indices.capacity())
    }
    fn of(frame: &LiveFrame) -> Self {
        Self {
            session: frame.cutex_session_id.clone(),
            host: frame.host_id.clone(),
            generation: frame.runtime_generation,
            thread: frame.thread_id.clone(),
            turn: frame.turn_id.clone(),
            item: frame.item_id.clone(),
            method: frame.method.clone(),
            indices: serde_json::to_string(&(
                frame.params.get("summaryIndex"),
                frame.params.get("contentIndex"),
            ))
            .unwrap(),
        }
    }
}

struct Item {
    frame: LiveFrame,
    since: Instant,
    pending_bytes: usize,
    resident_bytes: usize,
    suppressed: bool,
}
#[derive(Clone)]
struct Encoded {
    frame: Arc<LiveFrame>,
    json: Arc<str>,
    queued_at: Instant,
}
impl Encoded {
    fn new(frame: LiveFrame) -> Self {
        let json: Arc<str> = serde_json::to_string(&frame).expect("live JSON").into();
        Self {
            frame: Arc::new(frame),
            json,
            queued_at: Instant::now(),
        }
    }
    fn bytes(&self) -> usize {
        self.json.len()
    }
}
struct Subscriber {
    id: u64,
    life: Weak<()>,
    session: Option<String>,
    queue: VecDeque<Encoded>,
    bytes: usize,
    inflight: usize,
    suppressed: HashSet<Key>,
    disabled: bool,
    inflight_sessions: HashMap<String, usize>,
    wake: Option<Arc<StreamWake>>,
}
#[derive(Default)]
struct State {
    items: HashMap<Key, Item>,
    subscribers: Vec<Subscriber>,
    next_sub: u64,
    // Full terminal identities, bounded independently of queued text.
    closed: VecDeque<(String, u64, String, Option<String>, String)>,
    closed_turns: VecDeque<(String, u64, String, String)>,
    closed_runtimes: HashMap<String, u64>,
    turn_active: HashMap<(String, u64, String), bool>,
    rejected: VecDeque<Key>,
    generations: HashMap<String, u64>,
    stream_id: Option<String>,
    anchors: HashMap<String, Anchor>,
    epochs: HashMap<String, String>,
    stats: LiveStats,
}
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveStats {
    pub input_deltas: u64,
    pub fanout_frames: u64,
    pub gap_frames: u64,
    pub peak_subscriber_bytes: usize,
    pub peak_session_bytes: usize,
    pub peak_global_bytes: usize,
}
struct Inner {
    state: Mutex<State>,
    wake: Condvar,
    epoch: String,
    started: AtomicBool,
}
#[derive(Clone)]
pub struct LiveHub {
    inner: Arc<Inner>,
}

impl Default for LiveHub {
    fn default() -> Self {
        Self::new()
    }
}
impl LiveHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                epoch: uuid::Uuid::new_v4().to_string(),
                started: AtomicBool::new(false),
            }),
        }
    }
    fn start(&self) {
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let result = std::thread::Builder::new()
            .name("cutex-live-deltas".into())
            .spawn(move || loop {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if Arc::strong_count(&inner) == 1 {
                    break;
                }
                let state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                let delay = state
                    .items
                    .values()
                    .filter(|i| i.pending_bytes > 0)
                    .map(|i| (i.since + WINDOW).saturating_duration_since(now))
                    .min()
                    .unwrap_or(Duration::from_secs(1));
                let (mut state, _) = inner
                    .wake
                    .wait_timeout(state, delay)
                    .unwrap_or_else(|e| e.into_inner());
                flush_due(&mut state, Instant::now(), false, None);
            });
        if result.is_err() {
            self.inner.started.store(false, Ordering::Release);
        }
    }
    pub fn subscribe(
        &self,
        session: Option<String>,
    ) -> Result<LiveSubscription, LiveCapacityError> {
        self.subscribe_woken(session, None)
    }
    pub(crate) fn subscribe_woken(
        &self,
        session: Option<String>,
        wake: Option<Arc<StreamWake>>,
    ) -> Result<LiveSubscription, LiveCapacityError> {
        let life = Arc::new(());
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut state);
        if state.subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(LiveCapacityError);
        }
        let id = state.next_sub;
        state.next_sub = state.next_sub.wrapping_add(1);
        if state.subscribers.len() < MAX_SUBSCRIBERS {
            state.subscribers.push(Subscriber {
                id,
                life: Arc::downgrade(&life),
                session,
                queue: VecDeque::new(),
                bytes: 0,
                inflight: 0,
                suppressed: HashSet::new(),
                disabled: false,
                inflight_sessions: HashMap::new(),
                wake,
            });
        }
        Ok(LiveSubscription {
            inner: self.inner.clone(),
            id,
            _life: life,
        })
    }
    /// Recognition is independent of subscribers, budgets and validation. A
    /// recognized live notification must NEVER fall back into a durable append.
    pub fn publish(&self, pending: PendingEvent) -> bool {
        if !is_live_pending(&pending) {
            return false;
        }
        self.start();
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let anchor = state.anchors.get(&pending.cutex_session_id).cloned();
        let epoch = state
            .epochs
            .get(&pending.cutex_session_id)
            .cloned()
            .unwrap_or_else(|| self.inner.epoch.clone());
        drop(state);
        let Some(mut frame) = frame_from(&pending, &self.inner.epoch) else {
            return true;
        };
        frame.anchor = anchor;
        frame.producer_epoch = epoch;
        self.ingest(frame, Instant::now());
        self.inner.wake.notify_one();
        true
    }
    fn ingest(&self, mut frame: LiveFrame, now: Instant) {
        let key = Key::of(&frame);
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut state);
        state.stats.input_deltas += 1;
        if frame
            .anchor
            .as_ref()
            .is_some_and(|a| state.stream_id.as_ref().is_some_and(|s| s != &a.stream_id))
        {
            return;
        }
        if let Some(latest) = state.anchors.get(&key.session) {
            if frame
                .anchor
                .as_ref()
                .is_none_or(|a| a.stream_id == latest.stream_id && a.sequence < latest.sequence)
            {
                frame.anchor = Some(latest.clone());
            }
        }
        if state
            .epochs
            .get(&key.session)
            .is_some_and(|epoch| epoch != &frame.producer_epoch)
        {
            return;
        }
        if state
            .generations
            .get(&key.session)
            .is_some_and(|g| *g != key.generation)
        {
            return;
        }
        if state.closed.iter().any(|(s, g, t, u, i)| {
            s == &key.session
                && *g == key.generation
                && t == &key.thread
                && (u.is_none() || key.turn.is_none() || u == &key.turn)
                && i == &key.item
        }) {
            return;
        }
        if state.rejected.contains(&key) {
            return;
        }
        if state.closed_runtimes.get(&key.session) == Some(&key.generation)
            || state.closed_turns.iter().any(|(s, g, t, u)| {
                s == &key.session
                    && *g == key.generation
                    && t == &key.thread
                    && key.turn.as_ref() == Some(u)
            })
            || (key.turn.is_none()
                && state.turn_active.get(&(
                    key.session.clone(),
                    key.generation,
                    key.thread.clone(),
                )) == Some(&false))
        {
            return;
        }
        let input_bytes = frame.params["delta"].as_str().map_or(0, str::len);
        let bytes = serde_json::to_vec(&frame).map_or(FRAME_BYTES + 1, |v| v.len());
        let old = state.items.get(&key);
        if old.is_some_and(|item| item.suppressed) {
            return;
        }
        let key_over = old.is_none()
            && (state.items.len() >= GLOBAL_KEYS
                || state
                    .items
                    .keys()
                    .filter(|k| k.session == key.session)
                    .count()
                    >= SESSION_KEYS);
        let over = frame.kind == "gap"
            || !frame.params.is_object()
            || bytes > FRAME_BYTES
            || key_over
            || payload_bytes(&state, None) + bytes > GLOBAL_BYTES - 512 * 1024
            || payload_bytes(&state, Some(&key.session)) + bytes > SESSION_BYTES - 256 * 1024;
        if over {
            state.rejected.push_back(key.clone());
            while state.rejected.len() > GLOBAL_KEYS {
                state.rejected.pop_front();
            }
            if let Some(item) = state.items.get_mut(&key) {
                item.suppressed = true;
                item.pending_bytes = 0;
                item.frame.params = gap_params(&item.frame.params);
                item.resident_bytes = serde_json::to_vec(&item.frame).unwrap().len();
            }
            frame.kind = "gap";
            frame.gap_scope = Some("item");
            frame.params = gap_params(&frame.params);
            frame.end_offset = frame.start_offset;
            fanout(&mut state, Encoded::new(frame));
            return;
        }
        // Unknown native metadata cannot be silently overwritten by merging.
        let changed = old.is_some_and(|item| {
            item.frame.anchor != frame.anchor
                || metadata(&item.frame.params) != metadata(&frame.params)
        });
        let full = old.is_some_and(|item| item.pending_bytes + bytes > FRAME_BYTES);
        if changed || full {
            flush_key(&mut state, &key);
        }
        if let Some(item) = state.items.get_mut(&key) {
            let end = item.frame.end_offset;
            let revision = item.frame.revision.saturating_add(1);
            if item.pending_bytes == 0 {
                frame.start_offset = end;
                frame.end_offset = end + input_bytes as u64;
                frame.revision = revision;
                item.frame = frame;
                item.since = now;
                item.pending_bytes = bytes;
                item.resident_bytes = bytes;
            } else {
                item.frame.params["delta"].as_str().expect("delta");
                let mut text = item.frame.params["delta"].as_str().unwrap().to_string();
                text.push_str(frame.params["delta"].as_str().unwrap());
                item.frame.params["delta"] = Value::String(text);
                item.frame.end_offset += input_bytes as u64;
                item.frame.revision = revision;
                item.resident_bytes = serde_json::to_vec(&item.frame).unwrap().len();
                item.pending_bytes = item.resident_bytes;
            }
        } else {
            frame.end_offset = input_bytes as u64;
            state.items.insert(
                key,
                Item {
                    frame,
                    since: now,
                    pending_bytes: bytes,
                    resident_bytes: bytes,
                    suppressed: false,
                },
            );
        }
        record_peaks(&mut state);
    }
    /// Observe only committed durable events, including those from the shared
    /// journal. No repository locks are acquired from inside this hub.
    pub fn observe(&self, event: &EventEnvelope) {
        let mut timing = super::control_timing::Trace::metrics_only(super::control_timing::Path::LiveObserve);
        timing.stage(super::control_timing::Stage::LockWait);
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        timing.stage(super::control_timing::Stage::Fanout);
        if state
            .stream_id
            .as_deref()
            .is_some_and(|s| s != event.stream_id)
        {
            state.items.clear();
            state.closed.clear();
            state.closed_turns.clear();
            state.closed_runtimes.clear();
            state.turn_active.clear();
            state.rejected.clear();
            state.generations.clear();
            state.anchors.clear();
            for sub in &mut state.subscribers {
                sub.queue.clear();
                sub.bytes = sub.inflight;
                sub.suppressed.clear();
            }
        }
        state.stream_id = Some(event.stream_id.clone());
        seed_anchor(&mut state, event);
        let session = &event.cutex_session_id;
        // Flush previous text at every durable barrier for this session.
        flush_due(&mut state, Instant::now(), true, Some(session));
        let generation = event.correlation.runtime_generation.or_else(|| {
            event
                .cutex
                .as_ref()
                .and_then(|m| m.params.get("runtimeGeneration"))
                .and_then(Value::as_u64)
        });
        if let Some(generation) = generation {
            if state
                .generations
                .get(session)
                .is_some_and(|old| *old > generation)
            {
                return;
            }
            if state
                .generations
                .get(session)
                .is_some_and(|old| *old != generation)
            {
                state.items.retain(|k, _| &k.session != session);
                for sub in &mut state.subscribers {
                    remove_queued(sub, |k| &k.session == session);
                    sub.suppressed.retain(|k| &k.session != session);
                }
            }
            if state.generations.len() < GLOBAL_KEYS || state.generations.contains_key(session) {
                state.generations.insert(session.clone(), generation);
            }
        }
        let method = event
            .native
            .as_ref()
            .and_then(|m| m.message.get("method"))
            .and_then(Value::as_str)
            .or_else(|| event.cutex.as_ref().map(|m| m.method.as_str()))
            .unwrap_or("");
        let terminal = matches!(
            method,
            "item/completed"
                | "turn/completed"
                | "turn/failed"
                | "turn/cancelled"
                | "cutex/runtime/disconnected"
                | "cutex/runtime/stopped"
                | "cutex/runtime/updated"
        );
        if let Some(generation) = generation {
            if matches!(method, "turn/started" | "item/started") {
                state.closed_runtimes.remove(session);
                if let Some(thread) = event.correlation.thread_id.as_ref() {
                    if state.turn_active.len() < GLOBAL_KEYS
                        || state.turn_active.contains_key(&(
                            session.clone(),
                            generation,
                            thread.clone(),
                        ))
                    {
                        state
                            .turn_active
                            .insert((session.clone(), generation, thread.clone()), true);
                    }
                }
            }
            if matches!(method, "turn/completed" | "turn/failed" | "turn/cancelled") {
                if let Some(thread) = event.correlation.thread_id.as_ref() {
                    if let Some(turn) = event.correlation.turn_id.as_ref() {
                        state.closed_turns.push_back((
                            session.clone(),
                            generation,
                            thread.clone(),
                            turn.clone(),
                        ));
                        while state.closed_turns.len() > MAX_FENCES {
                            state.closed_turns.pop_front();
                        }
                    }
                    if state.turn_active.len() < GLOBAL_KEYS
                        || state.turn_active.contains_key(&(
                            session.clone(),
                            generation,
                            thread.clone(),
                        ))
                    {
                        state
                            .turn_active
                            .insert((session.clone(), generation, thread.clone()), false);
                    }
                }
            }
            if matches!(
                method,
                "cutex/runtime/disconnected" | "cutex/runtime/stopped"
            ) && (state.closed_runtimes.len() < GLOBAL_KEYS
                || state.closed_runtimes.contains_key(session))
            {
                state.closed_runtimes.insert(session.clone(), generation);
            }
        }
        if !terminal {
            return;
        }
        let item_id = event.correlation.item_id.clone().or_else(|| {
            event
                .native
                .as_ref()
                .and_then(|m| m.message.pointer("/params/item/id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let matches = |k: &Key| {
            &k.session == session
                && generation.is_none_or(|g| k.generation == g)
                && event
                    .correlation
                    .thread_id
                    .as_ref()
                    .is_none_or(|t| &k.thread == t)
                && event
                    .correlation
                    .turn_id
                    .as_ref()
                    .is_none_or(|t| k.turn.as_ref().is_none_or(|v| v == t))
                && item_id.as_ref().is_none_or(|i| &k.item == i)
        };
        let keys: Vec<_> = state.items.keys().filter(|k| matches(k)).cloned().collect();
        for k in keys {
            state.items.remove(&k);
            state.closed.push_back((
                k.session.clone(),
                k.generation,
                k.thread.clone(),
                k.turn.clone(),
                k.item.clone(),
            ));
        }
        // Even an item that had no live frames must reject late deltas.
        if let (Some(g), Some(t), Some(i)) = (
            generation,
            event.correlation.thread_id.clone(),
            item_id.clone(),
        ) {
            state
                .closed
                .push_back((session.clone(), g, t, event.correlation.turn_id.clone(), i));
        }
        while state.closed.len() > MAX_FENCES {
            state.closed.pop_front();
        }
        for sub in &mut state.subscribers {
            sub.suppressed.retain(|k| !matches(k));
        }
    }
    pub fn seed_anchor(&self, event: &EventEnvelope) {
        seed_anchor(
            &mut self.inner.state.lock().unwrap_or_else(|e| e.into_inner()),
            event,
        );
    }
    /// A local observer lost its transport; its next bytes cannot be assumed
    /// contiguous even when another process still owns the durable connection.
    pub fn invalidate(&self, session: &str, generation: u64) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<_> = state
            .items
            .keys()
            .filter(|k| k.session == session && k.generation == generation)
            .cloned()
            .collect();
        for key in keys {
            let Some(item) = state.items.remove(&key) else {
                continue;
            };
            let mut gap = item.frame;
            gap.kind = "gap";
            gap.gap_scope = Some("item");
            gap.params = gap_params(&gap.params);
            let marker = Encoded::new(gap);
            fanout(&mut state, marker.clone());
            for sub in &mut state.subscribers {
                if sub.session.as_deref().is_none_or(|s| s == session) {
                    let was_disabled = sub.disabled;
                    suppress(sub, key.clone());
                    if !was_disabled && sub.disabled {
                        enqueue_subscription_gap(sub, &marker.frame);
                    }
                }
            }
        }
        if state.epochs.len() < GLOBAL_KEYS || state.epochs.contains_key(session) {
            state
                .epochs
                .insert(session.into(), uuid::Uuid::new_v4().to_string());
        }
    }
    pub fn stats(&self) -> LiveStats {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stats
            .clone()
    }

    /// Existing encoded lengths; includes queued frames and outstanding delivery
    /// handles, including dead subscribers until their storage is reclaimed.
    pub(crate) fn queue_diagnostics(&self) -> serde_json::Value {
        let Ok(state) = self.inner.state.try_lock() else {
            return serde_json::json!({"available":false});
        };
        serde_json::json!({"available":true,
            "residentSubscribers":state.subscribers.len(),
            "subscribersWithLiveHandles":state.subscribers.iter().filter(|s| s.life.strong_count() > 0).count(),
            "queuedFrames":state.subscribers.iter().map(|s| s.queue.len()).sum::<usize>(),
            "queuedAndInflightEncodedBytes":state.subscribers.iter().map(|s| s.bytes).sum::<usize>(),
            "inflightEncodedBytes":state.subscribers.iter().map(|s| s.inflight).sum::<usize>(),
            "byteDefinition":"compact_json_frame_per_subscriber_including_delivery_handles",
            "residentCoalescingItems":state.items.len(),
            "peaks":state.stats})
    }
}

fn seed_anchor(state: &mut State, event: &EventEnvelope) {
    if state
        .stream_id
        .as_ref()
        .is_some_and(|s| s != &event.stream_id)
    {
        return;
    }
    let session = &event.cutex_session_id;
    if state.anchors.len() >= GLOBAL_KEYS && !state.anchors.contains_key(session) {
        return;
    }
    let anchor = state
        .anchors
        .entry(session.clone())
        .or_insert_with(|| Anchor {
            stream_id: event.stream_id.clone(),
            sequence: 0,
        });
    if anchor.stream_id == event.stream_id {
        anchor.sequence = anchor.sequence.max(event.sequence);
    }
}

fn metadata(params: &Value) -> Value {
    let mut value = params.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("delta");
    }
    value
}
fn gap_params(params: &Value) -> Value {
    let map: serde_json::Map<String, Value> = ["summaryIndex", "contentIndex"]
        .into_iter()
        .filter_map(|k| params.get(k).map(|v| (k.to_string(), v.clone())))
        .collect();
    if map.is_empty() {
        Value::Null
    } else {
        Value::Object(map)
    }
}
fn frame_from(pending: &PendingEvent, epoch: &str) -> Option<LiveFrame> {
    let raw = &pending.native.as_ref()?.message;
    let params = raw.get("params")?;
    params.get("delta")?.as_str()?;
    for index in ["summaryIndex", "contentIndex"] {
        if params
            .get(index)
            .is_some_and(|v| !v.is_null() && v.as_u64().is_none())
        {
            return None;
        }
    }
    let c = &pending.correlation;
    let thread = c.thread_id.as_ref()?;
    let item = c.item_id.as_ref()?;
    // Bound all identity/metadata allocations before retaining a clone.
    if pending.cutex_session_id.len()
        + pending.host_id.len()
        + thread.len()
        + item.len()
        + c.turn_id.as_ref().map_or(0, String::len)
        > 1024
    {
        return None;
    }
    Some(LiveFrame {
        version: 1,
        kind: if super::event_blobs::encoded_len(params) <= FRAME_BYTES {
            "delta"
        } else {
            "gap"
        },
        gap_scope: None,
        cutex_session_id: pending.cutex_session_id.clone(),
        host_id: pending.host_id.clone(),
        runtime_generation: c.runtime_generation?,
        thread_id: thread.clone(),
        turn_id: c.turn_id.clone(),
        item_id: item.clone(),
        method: raw.get("method")?.as_str()?.into(),
        producer_epoch: epoch.into(),
        revision: 1,
        start_offset: 0,
        end_offset: 0,
        anchor: None,
        params: if super::event_blobs::encoded_len(params) <= FRAME_BYTES {
            params.clone()
        } else {
            gap_params(params)
        },
    })
}
fn prune(state: &mut State) {
    state.subscribers.retain(|s| s.life.strong_count() > 0);
}
fn payload_bytes(state: &State, session: Option<&str>) -> usize {
    state
        .items
        .iter()
        .filter(|(k, _)| session.is_none_or(|s| k.session == s))
        .map(|(_, i)| i.resident_bytes)
        .sum::<usize>()
        + state
            .subscribers
            .iter()
            .map(|s| match session {
                None => s.bytes,
                Some(session) => {
                    s.inflight_sessions.get(session).copied().unwrap_or(0)
                        + s.queue
                            .iter()
                            .filter(|e| e.frame.cutex_session_id == session)
                            .map(Encoded::bytes)
                            .sum::<usize>()
                }
            })
            .sum::<usize>()
}
fn remove_queued(sub: &mut Subscriber, predicate: impl Fn(&Key) -> bool) {
    sub.queue.retain(|e| !predicate(&Key::of(&e.frame)));
    sub.bytes = sub.inflight + sub.queue.iter().map(Encoded::bytes).sum::<usize>();
}

fn suppress(sub: &mut Subscriber, key: Key) {
    if sub.disabled || sub.suppressed.contains(&key) {
        return;
    }
    let bytes = sub
        .suppressed
        .iter()
        .map(Key::metadata_bytes)
        .sum::<usize>();
    if bytes + key.metadata_bytes() > SUB_METADATA_BYTES {
        sub.suppressed.clear();
        sub.disabled = true;
    } else {
        sub.suppressed.insert(key);
    }
}

fn enqueue_subscription_gap(sub: &mut Subscriber, frame: &LiveFrame) {
    let mut gap = frame.clone();
    gap.kind = "gap";
    gap.gap_scope = Some("subscription");
    gap.params = gap_params(&gap.params);
    sub.queue.clear();
    sub.bytes = sub.inflight;
    let encoded = Encoded::new(gap);
    if sub.bytes + encoded.bytes() <= SUB_BYTES {
        sub.bytes += encoded.bytes();
        sub.queue.push_back(encoded);
    }
    if let Some(wake) = &sub.wake {
        wake.notify();
    }
}
fn flush_key(state: &mut State, key: &Key) {
    let Some(item) = state.items.get_mut(key) else {
        return;
    };
    if item.pending_bytes == 0 || item.suppressed {
        return;
    }
    let encoded = Encoded::new(item.frame.clone());
    item.pending_bytes = 0;
    // Keep offsets/revision/metadata, never the flushed text payload.
    item.frame.params["delta"] = Value::String(String::new());
    item.resident_bytes = serde_json::to_vec(&item.frame).unwrap().len();
    fanout(state, encoded);
}
fn flush_due(state: &mut State, now: Instant, force: bool, session: Option<&str>) {
    prune(state);
    let keys: Vec<_> = state
        .items
        .iter()
        .filter(|(k, i)| {
            i.pending_bytes > 0
                && session.is_none_or(|s| k.session == s)
                && (force || now.duration_since(i.since) >= WINDOW)
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in keys {
        flush_key(state, &key);
    }
}
fn fanout(state: &mut State, encoded: Encoded) {
    state.stats.fanout_frames += 1;
    if encoded.frame.kind == "gap" {
        state.stats.gap_frames += 1;
    }
    let key = Key::of(&encoded.frame);
    let mut total = payload_bytes(state, None);
    let mut session_total = payload_bytes(state, Some(&key.session));
    for index in 0..state.subscribers.len() {
        if state.subscribers[index]
            .session
            .as_ref()
            .is_some_and(|s| s != &key.session)
            || state.subscribers[index].disabled
            || state.subscribers[index].suppressed.contains(&key)
        {
            continue;
        }
        let global_over = total + encoded.bytes() > GLOBAL_BYTES - 512 * 1024;
        let session_over = session_total + encoded.bytes() > SESSION_BYTES - 256 * 1024;
        let sub = &mut state.subscribers[index];
        let previous = sub.bytes;
        let previous_session = sub
            .queue
            .iter()
            .filter(|q| q.frame.cutex_session_id == key.session)
            .map(Encoded::bytes)
            .sum::<usize>();
        let full = sub.bytes + encoded.bytes() > SUB_BYTES - 4096
            || sub.queue.len() >= SUB_FRAMES
            || global_over
            || session_over;
        if full {
            // A subscription-wide gap repairs every range discarded here. It is
            // constant space, including when many items/sessions were queued.
            let mut gap = (*encoded.frame).clone();
            gap.kind = "gap";
            gap.gap_scope = Some("subscription");
            gap.params = gap_params(&gap.params);
            let lost: Vec<_> = sub
                .queue
                .iter()
                .map(|e| Key::of(&e.frame))
                .chain(std::iter::once(key.clone()))
                .collect();
            for lost_key in lost {
                suppress(sub, lost_key);
            }
            sub.queue.clear();
            sub.bytes = sub.inflight;
            let gap = Encoded::new(gap);
            state.stats.gap_frames += 1;
            // In-flight credit can occupy one full frame; reserve sufficient
            // room for the small control marker even at the payload limit.
            if sub.bytes + gap.bytes() <= SUB_BYTES {
                sub.bytes += gap.bytes();
                sub.queue.push_back(gap);
            }
        } else {
            // Merge adjacent same-item ranges for a slow consumer. Metadata and
            // offset equality are required; replacing a lone fragment is unsafe.
            let can_merge = sub.queue.back().is_some_and(|last| {
                last.frame.kind == "delta"
                    && encoded.frame.kind == "delta"
                    && Key::of(&last.frame) == key
                    && last.frame.anchor == encoded.frame.anchor
                    && last.frame.end_offset == encoded.frame.start_offset
                    && metadata(&last.frame.params) == metadata(&encoded.frame.params)
                    && last.bytes() + encoded.bytes() <= FRAME_BYTES
            });
            if can_merge {
                let last = sub.queue.pop_back().unwrap();
                sub.bytes -= last.bytes();
                let mut frame = (*last.frame).clone();
                let mut text = frame.params["delta"].as_str().unwrap().to_string();
                text.push_str(encoded.frame.params["delta"].as_str().unwrap());
                frame.params["delta"] = Value::String(text);
                frame.end_offset = encoded.frame.end_offset;
                frame.revision = encoded.frame.revision;
                let mut merged = Encoded::new(frame);
                merged.queued_at = last.queued_at;
                sub.bytes += merged.bytes();
                sub.queue.push_back(merged);
            } else {
                sub.bytes += encoded.bytes();
                sub.queue.push_back(encoded.clone());
            }
        }
        total = total.saturating_sub(previous) + sub.bytes;
        session_total = session_total.saturating_sub(previous_session)
            + sub
                .queue
                .iter()
                .filter(|q| q.frame.cutex_session_id == key.session)
                .map(Encoded::bytes)
                .sum::<usize>();
        if let Some(wake) = &sub.wake {
            wake.notify();
        }
    }
    record_peaks(state);
}

fn record_peaks(state: &mut State) {
    let mut sessions: HashMap<&str, usize> = HashMap::new();
    for (key, item) in &state.items {
        *sessions.entry(&key.session).or_default() += item.resident_bytes;
    }
    for sub in &state.subscribers {
        for q in &sub.queue {
            *sessions.entry(&q.frame.cutex_session_id).or_default() += q.bytes();
        }
        for (session, bytes) in &sub.inflight_sessions {
            *sessions.entry(session).or_default() += bytes;
        }
    }
    let global = sessions.values().sum();
    let session = sessions.values().copied().max().unwrap_or(0);
    let subscriber = state.subscribers.iter().map(|s| s.bytes).max().unwrap_or(0);
    state.stats.peak_global_bytes = state.stats.peak_global_bytes.max(global);
    state.stats.peak_session_bytes = state.stats.peak_session_bytes.max(session);
    state.stats.peak_subscriber_bytes = state.stats.peak_subscriber_bytes.max(subscriber);
}

pub struct LiveSubscription {
    inner: Arc<Inner>,
    id: u64,
    _life: Arc<()>,
}
impl LiveSubscription {
    pub fn pop(&self) -> Option<LiveDelivery> {
        self.pop_internal(None)
    }
    pub(crate) fn pop_ready(&self, stream_id: &str, sequence: u64) -> Option<LiveDelivery> {
        self.pop_internal(Some((stream_id, sequence)))
    }
    fn pop_internal(&self, delivered: Option<(&str, u64)>) -> Option<LiveDelivery> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let sub = state.subscribers.iter_mut().find(|s| s.id == self.id)?;
        let front = sub.queue.front()?;
        let waiting = front.frame.kind == "delta"
            && delivered.is_some_and(|(stream, seq)| {
                front
                    .frame
                    .anchor
                    .as_ref()
                    .is_some_and(|a| a.stream_id == stream && a.sequence > seq)
            });
        if waiting && front.queued_at.elapsed() < ANCHOR_WAIT {
            return None;
        }
        let mut encoded = sub.queue.pop_front()?;
        if waiting {
            let key = Key::of(&encoded.frame);
            sub.bytes -= encoded.bytes();
            let mut gap = (*encoded.frame).clone();
            gap.kind = "gap";
            gap.gap_scope = Some("item");
            gap.params = gap_params(&gap.params);
            suppress(sub, key);
            if sub.disabled {
                gap.gap_scope = Some("subscription");
                sub.queue.clear();
                sub.bytes = sub.inflight;
            }
            encoded = Encoded::new(gap);
            sub.bytes += encoded.bytes();
        }
        sub.inflight += encoded.bytes();
        *sub.inflight_sessions
            .entry(encoded.frame.cutex_session_id.clone())
            .or_default() += encoded.bytes();
        Some(LiveDelivery {
            inner: self.inner.clone(),
            id: self.id,
            encoded,
            _life: self._life.clone(),
        })
    }
}
pub struct LiveDelivery {
    inner: Arc<Inner>,
    id: u64,
    encoded: Encoded,
    _life: Arc<()>,
}
impl LiveDelivery {
    pub fn frame(&self) -> &LiveFrame {
        &self.encoded.frame
    }
    pub fn json(&self) -> &str {
        &self.encoded.json
    }
}
impl Drop for LiveDelivery {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sub) = state.subscribers.iter_mut().find(|s| s.id == self.id) {
            sub.inflight = sub.inflight.saturating_sub(self.encoded.bytes());
            sub.bytes = sub.bytes.saturating_sub(self.encoded.bytes());
            if let Some(bytes) = sub
                .inflight_sessions
                .get_mut(&self.encoded.frame.cutex_session_id)
            {
                *bytes = bytes.saturating_sub(self.encoded.bytes());
                if *bytes == 0 {
                    sub.inflight_sessions
                        .remove(&self.encoded.frame.cutex_session_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn terminal_capability_is_strict_and_missing_runtimes_keep_legacy_path() {
        assert!(!supports_full_text_terminal(&json!({})));
        assert!(!supports_full_text_terminal(
            &json!({"features":{"fullTextItemTerminal":false}})
        ));
        assert!(!supports_full_text_terminal(
            &json!({"features":{"fullTextItemTerminal":"true"}})
        ));
        assert!(supports_full_text_terminal(
            &json!({"features":{"fullTextItemTerminal":true}})
        ));
    }
    #[test]
    fn stream_wake_does_not_lose_notification_before_wait_begins() {
        let wake = Arc::new(StreamWake::default());
        let version = wake.version();
        wake.notify();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            wake.wait(version, Duration::from_secs(5));
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("notification before wait must prevent sleeping");
        worker.join().unwrap();
    }
    fn frame(item: &str, text: &str) -> LiveFrame {
        LiveFrame {
            version: 1,
            kind: "delta",
            gap_scope: None,
            cutex_session_id: "s".into(),
            host_id: "h".into(),
            runtime_generation: 1,
            thread_id: "thread".into(),
            turn_id: Some("turn".into()),
            item_id: item.into(),
            method: "item/agentMessage/delta".into(),
            producer_epoch: "epoch".into(),
            revision: 1,
            start_offset: 0,
            end_offset: 0,
            anchor: Some(Anchor {
                stream_id: "stream".into(),
                sequence: 1,
            }),
            params: json!({"threadId":"thread","turnId":"turn","itemId":item,"delta":text}),
        }
    }
    fn due(hub: &LiveHub, at: Instant) {
        flush_due(&mut hub.inner.state.lock().unwrap(), at, false, None);
    }

    #[test]
    fn queue_diagnostics_track_delivery_and_retained_dead_subscriber() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        hub.ingest(frame("i", "hello"), now);
        due(&hub, now + WINDOW);
        let queued = hub.queue_diagnostics();
        assert_eq!(queued["queuedFrames"], 1);
        let bytes = queued["queuedAndInflightEncodedBytes"].as_u64().unwrap();
        assert!(bytes > 0);
        let delivery = sub.pop().unwrap();
        let inflight = hub.queue_diagnostics();
        assert_eq!(inflight["queuedFrames"], 0);
        assert_eq!(inflight["inflightEncodedBytes"], bytes);
        assert_eq!(inflight["queuedAndInflightEncodedBytes"], bytes);
        drop(sub);
        assert_eq!(hub.queue_diagnostics()["queuedAndInflightEncodedBytes"], bytes);
        drop(delivery);
        assert_eq!(hub.queue_diagnostics()["queuedAndInflightEncodedBytes"], 0);
        let guard = hub.inner.state.lock().unwrap();
        assert_eq!(hub.queue_diagnostics()["available"], false);
        drop(guard);
    }
    #[test]
    fn idle_input_always_waits_window_and_preserves_utf8_offsets() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        hub.ingest(frame("i", "你"), now);
        due(&hub, now + Duration::from_millis(39));
        assert!(sub.pop().is_none());
        due(&hub, now + WINDOW);
        let one = sub.pop().unwrap();
        assert_eq!((one.frame().start_offset, one.frame().end_offset), (0, 3));
        assert_eq!(one.frame().params["delta"], "你");
        drop(one);
        hub.ingest(frame("i", "🙂"), now + WINDOW);
        due(&hub, now + WINDOW + WINDOW);
        let two = sub.pop().unwrap();
        assert_eq!((two.frame().start_offset, two.frame().end_offset), (3, 7));
        assert_eq!(two.frame().revision, 2);
    }
    #[test]
    fn merge_is_per_item_index_and_keeps_unknown_metadata() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        let mut first = frame("i", "a");
        first.params["summaryIndex"] = json!(0);
        first.params["future"] = json!({"x":1});
        hub.ingest(first.clone(), now);
        first.params["delta"] = json!("b");
        hub.ingest(first.clone(), now);
        first.params["summaryIndex"] = json!(1);
        first.params["delta"] = json!("c");
        hub.ingest(first, now);
        due(&hub, now + WINDOW);
        let mut texts = Vec::new();
        while let Some(d) = sub.pop() {
            assert_eq!(d.frame().params["future"], json!({"x":1}));
            texts.push(d.frame().params["delta"].as_str().unwrap().to_string());
        }
        texts.sort();
        assert_eq!(texts, vec!["ab", "c"]);
    }
    #[test]
    fn metadata_change_flushes_without_resetting_identity_offsets() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        let mut one = frame("i", "a");
        one.params["future"] = json!(1);
        hub.ingest(one.clone(), now);
        one.params["future"] = json!(2);
        one.params["delta"] = json!("b");
        hub.ingest(one, now);
        due(&hub, now + WINDOW);
        let first = sub.pop().unwrap();
        let second = sub.pop().unwrap();
        assert_eq!(first.frame().params["future"], 1);
        assert_eq!(second.frame().params["future"], 2);
        assert_eq!(second.frame().start_offset, 1);
    }
    #[test]
    fn slow_client_gap_is_explicit_and_payload_stays_bounded_including_inflight() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        let text = "x".repeat(30_000);
        for n in 0..64 {
            hub.ingest(frame(&n.to_string(), &text), now);
            due(&hub, now + WINDOW);
        }
        let state = hub.inner.state.lock().unwrap();
        let slow = &state.subscribers[0];
        assert!(slow.queue.iter().any(|q| q.frame.kind == "gap"));
        assert!(slow.bytes <= SUB_BYTES);
        assert!(payload_bytes(&state, Some("s")) <= SESSION_BYTES);
        assert!(payload_bytes(&state, None) <= GLOBAL_BYTES);
        drop(state);
        let delivery = sub.pop().unwrap();
        let bytes = delivery.json().len();
        assert_eq!(
            hub.inner.state.lock().unwrap().subscribers[0].inflight,
            bytes
        );
        drop(delivery);
        assert_eq!(hub.inner.state.lock().unwrap().subscribers[0].inflight, 0);
    }
    #[test]
    fn fast_subscriber_does_not_share_slow_suppression() {
        let hub = LiveHub::new();
        let slow = hub.subscribe(None).unwrap();
        let fast = hub.subscribe(Some("s".into())).unwrap();
        let now = Instant::now();
        let text = "a".repeat(20_000);
        let mut got = 0;
        for n in 0..30 {
            hub.ingest(frame("i", &text), now + WINDOW * n);
            due(&hub, now + WINDOW * (n + 1));
            while let Some(d) = fast.pop() {
                assert_eq!(d.frame().kind, "delta");
                got += d.frame().params["delta"].as_str().unwrap().len();
            }
        }
        assert_eq!(got, 600_000);
        assert!(std::iter::from_fn(|| slow.pop()).any(|d| d.frame().kind == "gap"));
    }
    fn completed() -> EventEnvelope {
        EventEnvelope {
            contract_version: 2,
            event_id: "event".into(),
            cursor: "cursor".into(),
            stream_id: "stream".into(),
            sequence: 2,
            received_at: "now".into(),
            cutex_session_id: "s".into(),
            host_id: "h".into(),
            source: super::super::model::EventSource::AppServer,
            sensitivity: "owner".into(),
            schema: None,
            correlation: super::super::model::EventCorrelation {
                runtime_generation: Some(1),
                thread_id: Some("thread".into()),
                turn_id: Some("turn".into()),
                item_id: Some("i".into()),
                ..Default::default()
            },
            native: Some(super::super::model::NativeMessage {
                kind: NativeMessageKind::Notification,
                message: json!({"method":"item/completed","params":{"item":{"id":"i","text":"full"}}}),
            }),
            cutex: None,
        }
    }
    #[test]
    fn completion_flushes_and_fences_late_text_even_without_previous_live() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        hub.ingest(frame("i", "full"), now);
        hub.observe(&completed());
        assert_eq!(sub.pop().unwrap().frame().params["delta"], "full");
        hub.ingest(frame("i", "late"), now);
        due(&hub, now + WINDOW);
        assert!(sub.pop().is_none());
        let other = LiveHub::new();
        let other_sub = other.subscribe(None).unwrap();
        other.observe(&completed());
        other.ingest(frame("i", "late"), now);
        due(&other, now + WINDOW);
        assert!(other_sub.pop().is_none());
    }
    #[test]
    fn reset_and_generation_change_clear_old_live() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        hub.observe(&completed());
        hub.ingest(frame("j", "old"), now);
        let mut event = completed();
        event.stream_id = "new".into();
        event.correlation.runtime_generation = Some(2);
        hub.observe(&event);
        hub.ingest(frame("j", "late-old-generation"), now);
        due(&hub, now + WINDOW);
        assert!(sub.pop().is_none());
    }
    #[test]
    fn session_filter_and_frame_count_are_bounded() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(Some("other".into())).unwrap();
        let now = Instant::now();
        for n in 0..100 {
            hub.ingest(frame(&n.to_string(), "text"), now);
        }
        due(&hub, now + WINDOW);
        assert!(sub.pop().is_none());
        assert!(hub.inner.state.lock().unwrap().items.len() <= SESSION_KEYS);
    }
    #[test]
    fn anchor_is_session_local_and_replay_seed_never_moves_backwards() {
        let hub = LiveHub::new();
        let mut event = completed();
        event.native.as_mut().unwrap().message["method"] = json!("item/started");
        event.sequence = 10;
        hub.observe(&event);
        let mut other = event.clone();
        other.cutex_session_id = "other".into();
        other.sequence = 20;
        hub.observe(&other);
        let mut old = event.clone();
        old.sequence = 5;
        hub.seed_anchor(&old);
        let state = hub.inner.state.lock().unwrap();
        assert_eq!(state.anchors["s"].sequence, 10);
        assert_eq!(state.anchors["other"].sequence, 20);
    }
    #[test]
    fn observer_loss_is_gap_and_suppression_not_false_continuity() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        hub.ingest(frame("i", "before"), now);
        due(&hub, now + WINDOW);
        assert_eq!(sub.pop().unwrap().frame().kind, "delta");
        hub.invalidate("s", 1);
        let gap = sub.pop().unwrap();
        assert_eq!(gap.frame().kind, "gap");
        drop(gap);
        hub.ingest(frame("i", "late old transport"), now);
        due(&hub, now + WINDOW);
        assert!(sub.pop().is_none());
        assert_eq!(hub.inner.state.lock().unwrap().epochs.len(), 1);
    }
    #[test]
    fn escaped_text_frames_remain_below_wire_limit_and_round_trip() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        let text = "\\\"\n".repeat(1000);
        let mut received = String::new();
        for _ in 0..12 {
            hub.ingest(frame("i", &text), now);
            while let Some(d) = sub.pop() {
                assert!(d.json().len() <= FRAME_BYTES);
                received.push_str(d.frame().params["delta"].as_str().unwrap());
            }
        }
        due(&hub, now + WINDOW);
        while let Some(d) = sub.pop() {
            assert!(d.json().len() <= FRAME_BYTES);
            received.push_str(d.frame().params["delta"].as_str().unwrap());
        }
        assert_eq!(received, text.repeat(12));
    }
    #[test]
    fn turn_and_runtime_terminal_fence_items_not_previously_seen_live() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        let mut end = completed();
        end.correlation.item_id = None;
        end.native.as_mut().unwrap().message["method"] = json!("turn/completed");
        hub.observe(&end);
        hub.ingest(frame("never-seen", "late"), now);
        due(&hub, now + WINDOW);
        assert!(sub.pop().is_none());
        let mut disconnected = completed();
        disconnected.native = None;
        disconnected.correlation = Default::default();
        disconnected.cutex = Some(super::super::model::CutexMessage {
            method: "cutex/runtime/disconnected".into(),
            params: json!({"runtimeGeneration":1}),
        });
        hub.observe(&disconnected);
        let mut late = frame("another-never-seen", "late");
        late.turn_id = Some("other-turn".into());
        hub.ingest(late, now);
        due(&hub, now + WINDOW);
        assert!(sub.pop().is_none());
    }
    #[test]
    fn future_anchor_stays_queued_and_expiration_is_an_explicit_gap() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        let mut pending = frame("i", "text");
        pending.anchor.as_mut().unwrap().sequence = 10;
        hub.ingest(pending.clone(), now);
        due(&hub, now + WINDOW);
        assert!(sub.pop_ready("stream", 9).is_none());
        assert_eq!(
            sub.pop_ready("stream", 10).unwrap().frame().params["delta"],
            "text"
        );
        pending.item_id = "j".into();
        hub.ingest(pending, now);
        due(&hub, now + WINDOW);
        hub.inner.state.lock().unwrap().subscribers[0]
            .queue
            .front_mut()
            .unwrap()
            .queued_at = Instant::now() - ANCHOR_WAIT - Duration::from_millis(1);
        let gap = sub.pop_ready("stream", 9).unwrap();
        assert_eq!(gap.frame().kind, "gap");
        assert_eq!(gap.frame().gap_scope, Some("item"));
    }
    #[test]
    fn adjacent_ranges_do_not_merge_across_durable_anchor_barriers() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        hub.ingest(frame("i", "a"), now);
        due(&hub, now + WINDOW);
        let mut next = frame("i", "b");
        next.anchor.as_mut().unwrap().sequence = 2;
        hub.ingest(next, now + WINDOW);
        due(&hub, now + WINDOW + WINDOW);
        let a = sub.pop_ready("stream", 1).unwrap();
        assert_eq!(a.frame().params["delta"], "a");
        drop(a);
        assert!(sub.pop_ready("stream", 1).is_none());
        let b = sub.pop_ready("stream", 2).unwrap();
        assert_eq!(b.frame().params["delta"], "b");
        assert_eq!((b.frame().start_offset, b.frame().end_offset), (1, 2));
    }
    #[test]
    fn subscriber_limit_is_explicit_and_drop_releases_capacity() {
        let hub = LiveHub::new();
        let mut subs: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| hub.subscribe(None).unwrap())
            .collect();
        assert!(hub.subscribe(None).is_err());
        subs.pop();
        assert!(hub.subscribe(None).is_ok());
    }
    #[test]
    fn reasoning_gap_keeps_indices_and_suppression_metadata_has_its_own_bound() {
        let hub = LiveHub::new();
        let sub = hub.subscribe(None).unwrap();
        let now = Instant::now();
        let mut big = frame("reasoning", &"x".repeat(FRAME_BYTES));
        big.params["summaryIndex"] = json!(7);
        hub.ingest(big, now);
        let gap = sub.pop().unwrap();
        assert_eq!(gap.frame().kind, "gap");
        assert_eq!(gap.frame().params["summaryIndex"], 7);
        drop(gap);
        for n in 0..64 {
            hub.ingest(frame(&format!("{n}-{}", "i".repeat(600)), "x"), now);
        }
        hub.invalidate("s", 1);
        let state = hub.inner.state.lock().unwrap();
        let slow = &state.subscribers[0];
        assert!(
            slow.suppressed
                .iter()
                .map(Key::metadata_bytes)
                .sum::<usize>()
                <= SUB_METADATA_BYTES
        );
        assert!(slow.disabled);
        assert!(slow
            .queue
            .iter()
            .any(|e| e.frame.gap_scope == Some("subscription")));
    }
}
