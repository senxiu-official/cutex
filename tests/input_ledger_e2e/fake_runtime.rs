//! Scriptable fake runtimes on a loopback WebSocket (JSON-RPC like the
//! native app-server): a Codex app-server and a Claude adapter
//! (`cute-claude-app-server`). The daemon reaches them through a seeded
//! session binding (see `daemon::seed_sessions`), the same way
//! tests/queue_order_smoke.py does it on Windows.
//!
//! Injectable: response delays, refusals per input marker, interrupt,
//! disconnect (all peers or refuse reconnects), a missing `turn/completed`,
//! Claude `stageChanged` / `contextPersisted`, ExternalInput statuses
//! (`Held(PlanMode)`, `RetryableError`, `Conflict`).
//!
//! Both fakes keep an input inbox for what is not an owner `turn/start` /
//! `turn/steer`: ExternalInput (`thread/externalInput/submit|status`, the
//! daemon's `CUTEX_TEST_FAKE_EXTERNAL_INPUT` hook for Codex, Claude r8 native
//! ingress) and the Claude adapter's `claude/input/submit|status` (local-inbox
//! receipts, `admission: exclusive_turn` for delegations). An inbox input
//! runs like the real runtimes: after-turn starts its own turn when idle (or
//! after the running one), soon joins a running turn, passive rides along
//! with the next turn; a completed turn persists its context (an A4 receipt
//! whose `receiptId` is `Receipt::digest_id`).
#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Codex,
    Claude,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Codex => "codex",
            Kind::Claude => "claude",
        }
    }
}

/// ExternalInput processing status a fake reports for a bus message (RC8).
/// `Persisted` is the normal lifecycle (pending → claimed → context
/// persisted with a receipt); the others hold every input not yet persisted.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeStatus {
    Persisted,
    Pending,
    Held(&'static str),
    RetryableError,
    Conflict,
}

#[derive(Debug, Clone)]
pub struct Script {
    /// 057 harness opt-in only: simulate atomic owner request + carried inputs.
    /// This fake does not prove the production adapters implement that contract.
    pub owner_input_carries: bool,
    /// Delay before answering `turn/start` / `turn/steer` / `claude/input/submit`.
    pub start_delay: Duration,
    pub steer_delay: Duration,
    /// Never emit `turn/completed` (for completion, interrupt).
    pub drop_turn_completed: bool,
    /// After a steer, emit Claude `stageChanged` registered → offered and,
    /// when the turn completes, `contextPersisted`.
    pub claude_stages: bool,
    /// Claude `input.sameTurnSteering`.
    pub claude_same_turn_steering: bool,
    /// Claude advertises r8 native ingress (`externalInput*` capabilities).
    pub claude_native_ingress: bool,
    /// Claude native ingress also takes `soon` (a wake joins a running turn).
    pub claude_soon: bool,
    /// Echo the steered `userMessage` item with its clientId (Codex insertion).
    pub echo_steer_item: bool,
    /// Error object answered for an input whose first text equals the key.
    pub refuse: HashMap<String, Value>,
    /// Answer `initialize` this late (a runtime that is still starting:
    /// its connection succeeds only after the delay).
    pub initialize_delay: Duration,
    /// Refuse new WebSocket connections (runtime "offline").
    pub refuse_connections: bool,
    /// Status reported by `thread/externalInput/status` (and what the inbox
    /// does with inputs that are not persisted yet).
    pub external_status: NativeStatus,
    /// Per input: the first key the input's text contains overrides
    /// `external_status` for it (S12: one wake each held / retry / conflict).
    pub external_status_by_text: Vec<(String, NativeStatus)>,
    /// `thread/read` / `thread/turns/list` fail (history unreadable, S18).
    pub history_unreadable: bool,
    /// Complete a turn this fake started (`turn/start`, an inbox input)
    /// after this long.
    pub auto_complete_after: Option<Duration>,
    /// An after-turn inbox input waiting behind a turn starts its own turn
    /// this long after that turn completed (when still idle).
    pub inbox_turn_delay: Duration,
    /// A completed turn ends with an `agentMessage` item (a delegation's
    /// result is the handler's final message).
    pub final_message: bool,
    /// Answer the Agent Bus delivery's pre-start probes this late, with the
    /// state at answer time: Codex `thread/read` without `includeTurns: true`
    /// (`ExternalInputClient::thread_busy`), Claude `claude/input/status`
    /// for an input it does not hold yet (the r7 receipt probe of
    /// `claude_runtime::deliver_messages`, before its busy check). S9: the owner's message reaches an idle agent before
    /// the bus has acted on the turn end, so both are ready at once.
    pub probe_delay: Duration,
    /// A steer stays pending (not read by the turn yet), like the real
    /// runtimes between two model steps: Codex keeps it in
    /// `turn_state.pending_input`, Claude in its unread steer queue (only
    /// `stageChanged registered`, never `offered`). It is not in the turn's
    /// items and not echoed; `turn/interrupt` DROPS it (Codex
    /// `clear_pending` on abort, Claude r18 `drop_unread_steers`); a turn
    /// that completes reads it (appended to its items), and so does the next
    /// `FakeRuntime::step` (at that tool boundary, r274.2 S27).
    pub pending_steer: bool,
    /// Claude: a steer still pending when its turn completes is not read by
    /// that turn; it slips to the next one, which the adapter starts with it
    /// as its input (`userMessage{id: <new>, clientId}` at that turn's start;
    /// `contextPersisted` when that turn completes). r274.2 S28d.
    pub claude_slip_steers: bool,
    /// Codex: answer `thread/resume` without turns (the running turn only
    /// in `thread/turns/list`), as the Claude adapter always does.
    pub resume_without_turns: bool,
    /// Codex: the rollout `thread/read` reports (`thread.path`, with
    /// `historyMode: "paginated"`): cli_app `session_history` backs it up
    /// before `thread/revert` (S33). `None`: no path (no revert).
    pub history_path: Option<String>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            owner_input_carries: false,
            start_delay: Duration::ZERO,
            steer_delay: Duration::ZERO,
            drop_turn_completed: false,
            claude_stages: true,
            claude_same_turn_steering: true,
            claude_native_ingress: true,
            claude_soon: false,
            echo_steer_item: true,
            refuse: HashMap::new(),
            initialize_delay: Duration::ZERO,
            refuse_connections: false,
            external_status: NativeStatus::Persisted,
            external_status_by_text: Vec::new(),
            history_unreadable: false,
            auto_complete_after: None,
            inbox_turn_delay: Duration::from_millis(500),
            final_message: true,
            probe_delay: Duration::ZERO,
            pending_steer: false,
            claude_slip_steers: false,
            resume_without_turns: false,
            history_path: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Call {
    pub at: Instant,
    pub method: String,
    pub params: Value,
}

struct Peer {
    tx: mpsc::Sender<String>,
    alive: Arc<AtomicBool>,
}

/// One input that is not an owner turn/start / turn/steer.
#[derive(Debug, Clone)]
pub struct InboxEntry {
    pub id: String,
    /// ExternalInput envelope (`thread/externalInput/submit`), else the
    /// `claude/input/submit` params.
    pub envelope: Option<Value>,
    pub params: Value,
    pub semantic: String,
    /// `after_turn` / `passive` / `soon`.
    pub delivery: String,
    /// `queued` / `processing` / `completed` / `interrupted` / `failed`.
    pub state: String,
    pub turn: Option<String>,
    pub attempt: Option<String>,
    pub persisted: bool,
    pub ordinal: u64,
    pub owner: String,
    pub generation: Value,
}

struct State {
    kind: Kind,
    thread: String,
    connection_id: String,
    me: Weak<Mutex<State>>,
    turns: Vec<Value>,
    calls: Vec<Call>,
    peers: Vec<Peer>,
    script: Script,
    /// Claude busy inputs of the active turn: (messageId, clientId).
    pending_claude: Vec<(String, String)>,
    connections: usize,
    inbox: Vec<InboxEntry>,
    ordinal: u64,
    /// Steers not read yet (`Script::pending_steer`): (turn id, item).
    pending_steers: Vec<(String, Value)>,
    pending_owner_carries: HashMap<String, Vec<String>>,
}

pub struct FakeRuntime {
    pub kind: Kind,
    pub thread: String,
    pub url: String,
    pub port: u16,
    /// Claude: the adapter connection id (`claude.<connection id>` is the
    /// runtime agent id the bus registration must carry).
    pub connection_id: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl FakeRuntime {
    pub fn start(kind: Kind, workspace: &str) -> Self {
        Self::start_with(kind, workspace, Script::default())
    }

    pub fn start_with(kind: Kind, workspace: &str, script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake runtime");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        let connection_id = format!("fixture-connection-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let state = Arc::new_cyclic(|me| {
            Mutex::new(State {
                kind,
                thread: thread.clone(),
                connection_id: connection_id.clone(),
                me: me.clone(),
                turns: Vec::new(),
                calls: Vec::new(),
                peers: Vec::new(),
                script,
                pending_claude: Vec::new(),
                connections: 0,
                inbox: Vec::new(),
                ordinal: 0,
                pending_steers: Vec::new(),
                pending_owner_carries: HashMap::new(),
            })
        });
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Ctx { kind, thread: thread.clone(), workspace: workspace.to_string(), state: state.clone() };
        let accept_stop = stop.clone();
        let accept = std::thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if ctx.state.lock().unwrap().script.refuse_connections {
                            drop(stream);
                            continue;
                        }
                        let ctx = ctx.clone();
                        let stop = accept_stop.clone();
                        std::thread::spawn(move || serve(ctx, stream, stop));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Self { kind, thread, url: format!("ws://127.0.0.1:{port}"), port, connection_id, state, stop, accept: Some(accept) }
    }

    pub fn script(&self, edit: impl FnOnce(&mut Script)) {
        edit(&mut self.state.lock().unwrap().script);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn calls_of(&self, method: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|c| c.method == method).collect()
    }

    pub fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }

    pub fn connected_peers(&self) -> usize {
        self.state.lock().unwrap().peers.iter().filter(|p| p.alive.load(Ordering::Relaxed)).count()
    }

    pub fn wait_call(&self, method: &str, count: usize, timeout: Duration) -> Vec<Call> {
        let until = Instant::now() + timeout;
        loop {
            let calls = self.calls_of(method);
            if calls.len() >= count {
                return calls;
            }
            assert!(Instant::now() < until, "fake {}: {count} x {method} not called within {timeout:?}; calls: {:?}",
                self.kind.name(), self.calls().iter().map(|c| c.method.clone()).collect::<Vec<_>>());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Steers the running turn has not read yet (`Script::pending_steer`).
    pub fn pending_steers(&self) -> Vec<Value> {
        self.state.lock().unwrap().pending_steers.iter().map(|(_, item)| item.clone()).collect()
    }

    /// Every turn (oldest first) with its items.
    pub fn turns(&self) -> Vec<Value> {
        self.state.lock().unwrap().turns.clone()
    }

    /// Inputs in the inbox (ExternalInput and `claude/input/submit`).
    pub fn inbox(&self) -> Vec<InboxEntry> {
        self.state.lock().unwrap().inbox.clone()
    }

    pub fn wait_inbox(&self, count: usize, timeout: Duration) -> Vec<InboxEntry> {
        let until = Instant::now() + timeout;
        loop {
            let inbox = self.inbox();
            if inbox.len() >= count {
                return inbox;
            }
            assert!(Instant::now() < until, "fake {}: {count} inbox inputs not received within {timeout:?}; calls: {:?}",
                self.kind.name(), self.calls().iter().map(|c| c.method.clone()).collect::<Vec<_>>());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn active_turn(&self) -> Option<Value> {
        active(&self.state.lock().unwrap().turns).cloned()
    }

    /// Start a turn the daemon did not ask for (the agent is "busy").
    pub fn begin_turn(&self) -> String {
        let mut state = self.state.lock().unwrap();
        open_turn(&mut state, Vec::new(), false)
    }

    /// One model step of the active turn (r274.2 S27): the steers the turn
    /// has not read yet (`Script::pending_steer`) are read first, at this
    /// tool boundary (Codex drains `pending_input` at the top of a step and
    /// echoes the user item with its clientId; the Claude adapter offers its
    /// steer queue in the tool gap: `stageChanged offered`, then
    /// `contextPersisted` when the turn completes), then one tool item runs
    /// (`commandExecution` `item/started` + `item/completed`). Returns the
    /// tool item's id; `None` without an active turn.
    pub fn step(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        step_locked(&mut state, true)
    }

    /// One more tool call of the current model step: no step boundary, so
    /// nothing pending is read (a model step can call several tools).
    pub fn tool(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        step_locked(&mut state, false)
    }

    /// Complete the active turn (`completed` / `interrupted` / `failed`).
    pub fn complete(&self, status: &str) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        complete_locked(&mut state, status)
    }

    pub fn emit(&self, method: &str, params: Value) {
        broadcast(&mut self.state.lock().unwrap(), method, params);
    }

    /// Close every connection (the runtime observation link drops).
    pub fn disconnect_all(&self) {
        let mut state = self.state.lock().unwrap();
        for peer in state.peers.drain(..) {
            peer.alive.store(false, Ordering::Relaxed);
        }
    }
}

impl Drop for FakeRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.disconnect_all();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

#[derive(Clone)]
struct Ctx {
    kind: Kind,
    thread: String,
    workspace: String,
    state: Arc<Mutex<State>>,
}

/// Remove `params.beforeTurnId` and every newer turn (none may run); the
/// removed ids, newest first (`removedTurnIds` of both runtimes).
fn revert_turns(state: &mut State, params: &Value) -> Result<Vec<Value>, Value> {
    let Some(index) = state.turns.iter().position(|turn| turn["id"] == params["beforeTurnId"]) else {
        return Err(json!({"code": -32004, "message": "fixture: beforeTurnId is not a visible turn"}));
    };
    if active(&state.turns).is_some() {
        return Err(json!({"code": -32009, "message": "fixture: a turn is running"}));
    }
    let removed: Vec<Value> = state.turns.drain(index..).rev().map(|turn| turn["id"].clone()).collect();
    Ok(removed)
}

fn active(turns: &[Value]) -> Option<&Value> {
    turns.iter().rev().find(|t| t["status"] == "inProgress")
}

fn active_mut(turns: &mut [Value]) -> Option<&mut Value> {
    turns.iter_mut().rev().find(|t| t["status"] == "inProgress")
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn broadcast(state: &mut State, method: &str, params: Value) {
    let text = json!({"method": method, "params": params}).to_string();
    state.peers.retain(|p| p.alive.load(Ordering::Relaxed) && p.tx.send(text.clone()).is_ok());
}

fn entry_text(entry: &InboxEntry) -> String {
    entry.envelope.as_ref().map(|e| &e["message"]["text"]).unwrap_or(&entry.params["text"]).as_str().unwrap_or("").to_string()
}

/// The native status the script gives one inbox input: the first
/// `external_status_by_text` key its text contains, else `external_status`.
fn status_for(script: &Script, entry: &InboxEntry) -> NativeStatus {
    let text = entry_text(entry);
    script.external_status_by_text.iter().find(|(key, _)| text.contains(key.as_str()))
        .map(|(_, status)| status.clone()).unwrap_or_else(|| script.external_status.clone())
}

/// Whether the script holds an inbox input (not taken into a turn).
fn held(script: &Script, entry: &InboxEntry) -> bool {
    !matches!(status_for(script, entry), NativeStatus::Persisted | NativeStatus::Pending)
}

/// Start a turn with `items`; queued passive / after-turn inbox inputs ride
/// along (claimed). `auto`: the script's auto completion applies.
fn open_turn(state: &mut State, items: Vec<Value>, auto: bool) -> String {
    let thread = state.thread.clone();
    let id = format!("turn-{}", uuid::Uuid::new_v4().simple());
    let mut items = items;
    let script = state.script.clone();
    for entry in state.inbox.iter_mut().filter(|e| e.state == "queued" && !held(&script, e)) {
        entry.state = "processing".into();
        entry.turn = Some(id.clone());
        entry.attempt = Some(uuid::Uuid::new_v4().to_string());
        items.push(inbox_item(entry));
    }
    let turn = json!({"id": id, "status": "inProgress", "items": items, "startedAt": now_secs()});
    state.turns.push(turn.clone());
    broadcast(state, "turn/started", json!({"threadId": thread, "turn": turn}));
    broadcast(state, "thread/status/changed", json!({"threadId": thread, "status": {"type": "active", "activeFlags": []}}));
    for item in items {
        broadcast(state, "item/started", json!({"threadId": thread, "turnId": id, "item": item}));
        // The Claude adapter emits item/started + item/completed for every
        // item of the turn at its start (r274.2 §9.1 S28).
        if state.kind == Kind::Claude {
            broadcast(state, "item/completed", json!({"threadId": thread, "turnId": id, "item": item}));
        }
    }
    if auto {
        if let (Some(after), Some(shared)) = (state.script.auto_complete_after, state.me.upgrade()) {
            let turn_id = id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                let mut state = shared.lock().unwrap();
                if active(&state.turns).is_some_and(|t| t["id"] == turn_id) {
                    complete_locked(&mut state, "completed");
                }
            });
        }
    }
    id
}

fn step_locked(state: &mut State, boundary: bool) -> Option<String> {
    let (thread, kind) = (state.thread.clone(), state.kind);
    let (echo, claude_stages) = (state.script.echo_steer_item, state.script.claude_stages);
    let current = active(&state.turns)?["id"].as_str()?.to_string();
    let (read, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut state.pending_steers).into_iter().partition(|(turn, _)| boundary && *turn == current);
    state.pending_steers = rest;
    // Claude, at the tool boundary: the PostToolUse hook claims the queued
    // passive notes (`Engine.inject_notes`: their `external_event` items,
    // item/started + item/completed, before the frames go to the model).
    // Their receipt still comes only when the turn completes (claude-r34).
    if kind == Kind::Claude && boundary {
        let script = state.script.clone();
        let mut notes = Vec::new();
        for entry in state.inbox.iter_mut().filter(|e| e.state == "queued" && e.delivery == "passive" && !held(&script, e)) {
            entry.state = "processing".into();
            entry.turn = Some(current.clone());
            entry.attempt = Some(uuid::Uuid::new_v4().to_string());
            notes.push(inbox_item(entry));
        }
        for item in notes {
            active_mut(&mut state.turns)?["items"].as_array_mut()?.push(item.clone());
            broadcast(state, "item/started", json!({"threadId": thread, "turnId": current, "item": item}));
            broadcast(state, "item/completed", json!({"threadId": thread, "turnId": current, "item": item}));
        }
    }
    for (_, item) in read {
        let owner_client = item["clientId"].as_str().unwrap_or("").to_string();
        active_mut(&mut state.turns)?["items"].as_array_mut()?.push(item.clone());
        match kind {
            Kind::Codex if echo => {
                broadcast(state, "item/started", json!({"threadId": thread, "turnId": current, "item": item}));
                broadcast(state, "item/completed", json!({"threadId": thread, "turnId": current, "item": item}));
            }
            Kind::Claude if claude_stages => {
                // `busy_delivery.inject`: the steer's own user item
                // (`steer-<clientId>`), then `offered` in the same step.
                broadcast(state, "item/started", json!({"threadId": thread, "turnId": current, "item": item}));
                broadcast(state, "item/completed", json!({"threadId": thread, "turnId": current, "item": item}));
                broadcast(state, "claude/input/stageChanged", json!({"threadId": thread, "turnId": current,
                    "messageId": item["id"], "clientId": item["clientId"], "stage": "offered"}));
            }
            _ => {}
        }
        claim_owner_carries(state, &owner_client, &current);
    }
    let tool = uuid::Uuid::new_v4().to_string();
    let started = json!({"type": "commandExecution", "id": tool, "command": "fixture step", "status": "inProgress"});
    let completed = json!({"type": "commandExecution", "id": tool, "command": "fixture step", "status": "completed",
        "exitCode": 0, "aggregatedOutput": "ok"});
    active_mut(&mut state.turns)?["items"].as_array_mut()?.push(completed.clone());
    broadcast(state, "item/started", json!({"threadId": thread, "turnId": current, "item": started}));
    broadcast(state, "item/completed", json!({"threadId": thread, "turnId": current, "item": completed}));
    Some(tool)
}

fn complete_locked(state: &mut State, status: &str) -> Option<String> {
    let (thread, kind) = (state.thread.clone(), state.kind);
    let drop_completed = state.script.drop_turn_completed;
    let claude_stages = state.script.claude_stages;
    let final_message = state.script.final_message;
    let current = active(&state.turns)?["id"].as_str().unwrap().to_string();
    // Unread steers: a completed turn reads them, an interrupt drops them.
    let (read, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut state.pending_steers).into_iter().partition(|(turn, _)| *turn == current);
    state.pending_steers = rest;
    // Claude `claude_slip_steers`: they run as the next turn's input instead.
    let slip = kind == Kind::Claude && state.script.claude_slip_steers && status == "completed";
    let slipped: Vec<Value> = if slip { read.iter().map(|(_, item)| item.clone()).collect() } else { Vec::new() };
    if status == "completed" && !slip {
        for (_, item) in read {
            let client = item["clientId"].as_str().unwrap_or("").to_string();
            active_mut(&mut state.turns)?["items"].as_array_mut().unwrap().push(item);
            claim_owner_carries(state, &client, &current);
        }
    }
    let turn = active_mut(&mut state.turns)?;
    turn["status"] = status.into();
    turn["completedAt"] = json!(now_secs());
    let id = turn["id"].as_str().unwrap().to_string();
    if final_message && status == "completed" {
        let item = json!({"type": "agentMessage", "id": uuid::Uuid::new_v4().to_string(), "text": format!("fixture final message of {id}")});
        turn["items"].as_array_mut().unwrap().push(item);
    }
    let turn = turn.clone();
    if kind == Kind::Claude && claude_stages && status == "completed" {
        let (keep, persisted): (Vec<_>, Vec<_>) = std::mem::take(&mut state.pending_claude).into_iter()
            .partition(|(_, client)| slipped.iter().any(|item| item["clientId"] == client.as_str()));
        state.pending_claude = keep;
        for (message, client) in persisted {
            broadcast(state, "claude/input/contextPersisted",
                json!({"threadId": thread, "turnId": id, "messageId": message, "clientId": client}));
        }
    } else {
        state.pending_claude.clear();
    }
    // Inbox inputs of this turn: persisted with a receipt when completed.
    let mut changed = Vec::new();
    let ordinal = &mut state.ordinal;
    for entry in state.inbox.iter_mut().filter(|e| e.turn.as_deref() == Some(id.as_str()) && e.state == "processing") {
        entry.state = status.to_string();
        if status == "completed" {
            entry.persisted = true;
            *ordinal += 1;
            entry.ordinal = *ordinal;
        }
        changed.push(entry.id.clone());
    }
    if !drop_completed {
        broadcast(state, "turn/completed", json!({"threadId": thread, "turn": turn}));
        broadcast(state, "thread/status/changed", json!({"threadId": thread, "status": {"type": "idle"}}));
    }
    for message in changed {
        broadcast(state, "thread/externalInput/statusChanged", json!({"threadId": thread, "messageId": message}));
    }
    if slipped.is_empty() {
        schedule_inbox_turn(state);
    } else {
        // `Store.claim`: the next turn's input, a new user item with the clientId.
        let items = slipped.into_iter().map(|item| json!({"type": "userMessage", "id": uuid::Uuid::new_v4().to_string(),
            "clientId": item["clientId"], "content": item["content"]})).collect();
        open_turn(state, items, false);
    }
    Some(id)
}

/// A queued after-turn inbox input starts its own turn shortly after the
/// runtime became idle (owner input that arrives first takes it along).
fn startable(state: &State) -> bool {
    state.inbox.iter().any(|e| e.state == "queued" && e.delivery != "passive" && !held(&state.script, e))
}

fn schedule_inbox_turn(state: &State) {
    if !startable(state) {
        return;
    }
    let (Some(shared), delay) = (state.me.upgrade(), state.script.inbox_turn_delay) else { return };
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let mut state = shared.lock().unwrap();
        if active(&state.turns).is_none() && startable(&state) {
            open_turn(&mut state, Vec::new(), true);
        }
    });
}

fn snapshot(ctx: &Ctx, state: &State) -> Value {
    let mut thread = json!({"id": ctx.thread, "cwd": ctx.workspace, "turns": state.turns,
        "modelProvider": if ctx.kind == Kind::Claude { "cute-claude" } else { "openai" },
        "status": {"type": if active(&state.turns).is_some() { "active" } else { "idle" }}});
    if let Some(path) = state.script.history_path.as_ref().filter(|_| ctx.kind == Kind::Codex) {
        thread["path"] = json!(path);
        thread["historyMode"] = json!("paginated");
    }
    thread
}

fn serve(ctx: Ctx, stream: TcpStream, stop: Arc<AtomicBool>) {
    let _ = stream.set_nonblocking(false);
    let Ok(mut ws) = tungstenite::accept(stream) else { return };
    if ws.get_ref().set_nonblocking(true).is_err() {
        return;
    }
    let (tx, rx) = mpsc::channel::<String>();
    let alive = Arc::new(AtomicBool::new(true));
    {
        let mut state = ctx.state.lock().unwrap();
        state.connections += 1;
        state.peers.push(Peer { tx: tx.clone(), alive: alive.clone() });
    }
    // Delayed answers are sent from helper threads through `tx`.
    loop {
        if stop.load(Ordering::Relaxed) || !alive.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return;
        }
        while let Ok(text) = rx.try_recv() {
            match ws.send(Message::Text(text.into())) {
                Ok(()) => {}
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return alive.store(false, Ordering::Relaxed),
            }
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                if let Ok(request) = serde_json::from_str::<Value>(text.as_ref()) {
                    handle(&ctx, request, &tx);
                }
            }
            Ok(Message::Close(_)) => return alive.store(false, Ordering::Relaxed),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let _ = ws.flush();
                std::thread::sleep(Duration::from_millis(3));
            }
            Err(_) => return alive.store(false, Ordering::Relaxed),
        }
    }
}

fn reply(tx: &mpsc::Sender<String>, id: &Value, outcome: Result<Value, Value>, delay: Duration) {
    let text = match outcome {
        Ok(result) => json!({"id": id, "result": result}),
        Err(error) => json!({"id": id, "error": error}),
    }
    .to_string();
    if delay.is_zero() {
        let _ = tx.send(text);
    } else {
        let tx = tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let _ = tx.send(text);
        });
    }
}

fn first_text(params: &Value) -> String {
    params["input"][0]["text"].as_str().unwrap_or("").to_string()
}

fn capabilities(ctx: &Ctx, state: &State) -> Value {
    let script = &state.script;
    let mut caps = json!({
        "implementation": "cute-claude-app-server", "runtimeId": "fixture-claude", "version": "fixture",
        "backend": "claude-subscription-sdk", "persistentRuntime": true,
        "ensureThread": "claude/thread/ensure",
        "input": {"sameTurnSteering": script.claude_same_turn_steering, "views": true,
            "deliveryModes": ["after-turn", "passive"], "admissions": ["exclusive_turn"]},
    });
    if ctx.kind == Kind::Claude && script.claude_native_ingress {
        caps["externalInputVersion"] = json!(1);
        caps["externalInputVersions"] = json!([1, 2]);
        caps["externalInputDeliveries"] = if script.claude_soon { json!(["after_turn", "passive", "soon"]) } else { json!(["after_turn", "passive"]) };
    }
    caps
}

fn handle(ctx: &Ctx, request: Value, tx: &mpsc::Sender<String>) {
    let Some(id) = request.get("id").cloned() else { return };
    let method = request["method"].as_str().unwrap_or("").to_string();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let mut state = ctx.state.lock().unwrap();
    state.calls.push(Call { at: Instant::now(), method: method.clone(), params: params.clone() });
    let script = state.script.clone();
    // Claude: only the probe before the submit (the input is not in the inbox
    // yet); the status read after the submit is answered at once.
    let probe = (method == "thread/read" && params["includeTurns"] != true)
        || (method == "claude/input/status" && !state.inbox.iter().any(|e| params["messageId"] == e.id.as_str()));
    if probe && !script.probe_delay.is_zero() {
        drop(state);
        let (ctx, tx) = (ctx.clone(), tx.clone());
        std::thread::spawn(move || {
            std::thread::sleep(script.probe_delay);
            let state = ctx.state.lock().unwrap();
            let outcome = match method.as_str() {
                "thread/read" if state.script.history_unreadable => Err(json!({"code": -32603, "message": "fixture: history unreadable"})),
                "thread/read" => Ok(json!({"thread": snapshot(&ctx, &state)})),
                _ => local_status(&ctx, &state, &params),
            };
            drop(state);
            reply(&tx, &id, outcome, Duration::ZERO);
        });
        return;
    }
    let mut delay = Duration::ZERO;
    let outcome: Result<Value, Value> = match method.as_str() {
        // Codex: the ExternalInput surface the daemon's debug hooks use
        // (C3 bus, C4 asks: N1 exclusive admission, `soon`).
        "initialize" => {
            delay = script.initialize_delay;
            let mut response = if ctx.kind == Kind::Codex {
                json!({"userAgent": "input-ledger-e2e-fake-codex", "externalInputVersion": 1,
                    "externalInputControlVersion": 1, "externalInputDeliveries": ["after_turn", "passive", "soon"]})
            } else { json!({"userAgent": format!("input-ledger-e2e-fake-{}", ctx.kind.name())}) };
            if script.owner_input_carries { response["ownerInputCarries"] = json!(1); }
            Ok(response)
        }
        "thread/resume" | "thread/read" if script.history_unreadable && method == "thread/read" => {
            Err(json!({"code": -32603, "message": "fixture: history unreadable"}))
        }
        // Like the real Claude adapter (`protocol.py` `_thread_response`):
        // `thread/resume` has the thread status but no turns; the running
        // turn is only in `thread/turns/list`. Codex too with
        // `resume_without_turns` or `excludeTurns`.
        "thread/resume" if ctx.kind == Kind::Claude || script.resume_without_turns || params["excludeTurns"] == true => {
            let mut thread = snapshot(ctx, &state);
            thread["turns"] = json!([]);
            Ok(json!({"thread": thread}))
        }
        "thread/resume" | "thread/read" => Ok(json!({"thread": snapshot(ctx, &state)})),
        "thread/turns/list" if script.history_unreadable => Err(json!({"code": -32603, "message": "fixture: history unreadable"})),
        "thread/turns/list" => Ok(json!({"data": state.turns.iter().rev().cloned().collect::<Vec<_>>(), "nextCursor": null})),
        "thread/items/list" if script.history_unreadable => Err(json!({"code": -32603, "message": "fixture: history unreadable"})),
        // One page with every item of the turn (history.rs `read_turn`).
        "thread/items/list" => match state.turns.iter().find(|t| t["id"] == params["turnId"]) {
            Some(turn) => {
                let mut items: Vec<Value> = turn["items"].as_array().cloned().unwrap_or_default().into_iter()
                    .map(|item| json!({"turnId": turn["id"], "item": item})).collect();
                if params["sortDirection"] == "desc" {
                    items.reverse();
                }
                Ok(json!({"data": items, "nextCursor": null}))
            }
            None => Err(json!({"code": -32004, "message": "fixture: turn not found"})),
        },
        "claude/capabilities" => Ok(capabilities(ctx, &state)),
        "claude/runtime/status" => Ok(json!({
            "threadId": ctx.thread, "state": if active(&state.turns).is_some() { "working" } else { "waiting" },
            "sessionId": "fixture-native", "connectionId": state.connection_id,
        })),
        "claude/input/submit" if params.get("messageId").is_some_and(Value::is_string) => {
            delay = script.start_delay;
            inbox_submit(ctx, &mut state, &params)
        }
        "claude/input/status" => local_status(ctx, &state, &params),
        "turn/start" | "turn/steer" | "claude/input/submit" => {
            delay = if method == "turn/steer" { script.steer_delay } else { script.start_delay };
            start_or_steer(ctx, &mut state, &method, &params, &script)
        }
        "turn/interrupt" => {
            complete_locked(&mut state, "interrupted");
            Ok(json!({}))
        }
        "thread/externalInput/submit" => external_submit(ctx, &mut state, &params),
        "thread/externalInput/status" => Ok(external_status(ctx, &state, &params)),
        // S33 history revert: Codex `thread/revert` (paginated threads),
        // the Claude adapter's `claude/thread/inspect` / `claude/thread/revert`
        // (`session_ops.py`: the receipt of a committed revert).
        "thread/revert" | "claude/thread/revert" => match revert_turns(&mut state, &params) {
            Ok(_) if method == "thread/revert" => {
                let thread = ctx.thread.clone();
                broadcast(&mut state, "thread/reverted", json!({"threadId": thread}));
                Ok(json!({"thread": snapshot(ctx, &state), "turnsBackwardsCursor": null, "itemsBackwardsCursor": null}))
            }
            Ok(removed) => {
                let action = format!("fixture-revert-{}", uuid::Uuid::new_v4().simple());
                Ok(json!({"threadId": ctx.thread, "beforeTurnId": params["beforeTurnId"], "removedTurnIds": removed,
                    "sourceSessionId": "fixture-native", "backupPath": null, "rewindFilesRequested": false,
                    "filesRewound": false, "historyCommitted": true, "nativeSessionId": "fixture-native",
                    "runtime": {"threadId": ctx.thread, "state": "waiting", "connectionId": state.connection_id},
                    "actionId": action, "state": "completed", "receiptPath": format!("fixture/session-operations/{action}.json")}))
            }
            Err(error) => Err(error),
        },
        "claude/thread/inspect" => Ok(json!({"threadId": ctx.thread, "nativeSession": {"session_id": "fixture-native"}})),
        _ => Ok(json!({})),
    };
    drop(state);
    reply(tx, &id, outcome, delay);
}

/// Simulate the native carry contract, not an independent ExternalInput submit.
/// Validate the entire bundle first so a malformed item cannot partly start it.
fn register_owner_carries(ctx: &Ctx, state: &mut State, params: &Value, client: &str) -> Result<(), Value> {
    let Some(raw) = params.get("ownerCarries") else { return Ok(()) };
    let invalid = |message: String| json!({"code": -32602, "message": message});
    if !state.script.owner_input_carries { return Err(invalid("owner carries not enabled in this fake".into())); }
    let values = raw.as_array().ok_or_else(|| invalid("ownerCarries must be an array".into()))?;
    let mut parsed = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for value in values {
        let e: cutex::app_server::external_input::Envelope = serde_json::from_value(value.clone())
            .map_err(|error| invalid(error.to_string()))?;
        e.validate().map_err(|error| invalid(error.to_string()))?;
        if e.thread_id != ctx.thread || !ids.insert(e.message.id.clone())
            || state.inbox.iter().any(|entry| entry.id == e.message.id) {
            return Err(invalid("carry identity/binding conflict".into()));
        }
        parsed.push(e);
    }
    let carry_ids = parsed.iter().map(|e| e.message.id.clone()).collect();
    for e in parsed {
        let envelope = serde_json::to_value(&e).unwrap();
        state.inbox.push(InboxEntry {
            id: e.message.id, envelope: Some(envelope.clone()), params: envelope,
            semantic: e.semantic_sha256, delivery: "owner_carry".into(), state: "owner_pending".into(),
            turn: None, attempt: None, persisted: false, ordinal: 0,
            owner: e.owner_id, generation: json!(e.runtime_generation),
        });
    }
    state.pending_owner_carries.insert(client.into(), carry_ids);
    Ok(())
}

fn claim_owner_carries(state: &mut State, client: &str, turn: &str) {
    let Some(ids) = state.pending_owner_carries.remove(client) else { return };
    let mut items = Vec::new();
    for id in ids {
        let entry = state.inbox.iter_mut().find(|entry| entry.id == id).expect("registered carry");
        entry.state = "processing".into();
        entry.turn = Some(turn.into());
        entry.attempt = Some(uuid::Uuid::new_v4().to_string());
        items.push(inbox_item(entry));
    }
    let thread = state.thread.clone();
    for item in items {
        active_mut(&mut state.turns).expect("owner's active turn")["items"].as_array_mut().unwrap().push(item.clone());
        broadcast(state, "item/started", json!({"threadId": thread, "turnId": turn, "item": item}));
        broadcast(state, "item/completed", json!({"threadId": thread, "turnId": turn, "item": item}));
    }
}

fn start_or_steer(ctx: &Ctx, state: &mut State, method: &str, params: &Value, script: &Script) -> Result<Value, Value> {
    let marker = first_text(params);
    if let Some(error) = script.refuse.get(&marker) {
        return Err(error.clone());
    }
    let client = params["clientUserMessageId"].as_str().or(params["clientId"].as_str()).unwrap_or("").to_string();
    let steer = method == "turn/steer";
    if steer {
        if ctx.kind == Kind::Claude && !script.claude_same_turn_steering {
            return Err(json!({"code": -32009, "message": "steer unavailable"}));
        }
        let Some(turn) = active(&state.turns) else {
            return Err(json!({"code": -32009, "message": "no active turn"}));
        };
        if params["expectedTurnId"].as_str().is_some_and(|t| Some(t) != turn["id"].as_str()) {
            return Err(json!({"code": -32009, "message": "turn mismatch"}));
        }
    } else if active(&state.turns).is_some() {
        return Err(json!({"code": -32009, "message": "already active"}));
    }
    register_owner_carries(ctx, state, params, &client)?;
    // The Claude adapter names a steer's user item `steer-<clientId>`
    // (`cc/protocol.py _steer`, `busy_delivery.steer_item`).
    let claude_steer = steer && ctx.kind == Kind::Claude;
    let item_id = if claude_steer { format!("steer-{client}") } else { uuid::Uuid::new_v4().to_string() };
    let mut item = json!({"type": "userMessage", "id": item_id, "clientId": client, "content": params["input"]});
    if claude_steer {
        item["steered"] = json!(true);
    }
    let thread = ctx.thread.clone();
    if steer {
        let turn = active_mut(&mut state.turns).unwrap();
        let turn_id = turn["id"].as_str().unwrap().to_string();
        if script.pending_steer {
            state.pending_steers.push((turn_id.clone(), item));
            if ctx.kind == Kind::Claude && script.claude_stages {
                // Registered after the steer answer; never offered.
                state.pending_claude.push((item_id.clone(), client.clone()));
                if let Some(shared) = state.me.upgrade() {
                    let turn_id = turn_id.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(50));
                        let mut state = shared.lock().unwrap();
                        if state.pending_claude.iter().any(|(message, _)| *message == item_id) {
                            broadcast(&mut state, "claude/input/stageChanged", json!({"threadId": thread, "turnId": turn_id,
                                "messageId": item_id, "clientId": client, "stage": "registered"}));
                        }
                    });
                }
            }
            return Ok(json!({"turnId": turn_id}));
        }
        turn["items"].as_array_mut().unwrap().push(item.clone());
        match ctx.kind {
            Kind::Codex if script.echo_steer_item => {
                broadcast(state, "item/started", json!({"threadId": thread, "turnId": turn_id, "item": item}));
                broadcast(state, "item/completed", json!({"threadId": thread, "turnId": turn_id, "item": item}));
            }
            Kind::Claude if script.claude_stages => {
                // Like the adapter: registered right after the steer answer,
                // offered when the model is offered the input (a step later).
                state.pending_claude.push((item_id.clone(), client.clone()));
                if let Some(shared) = state.me.upgrade() {
                    let (turn_id, item_id, client) = (turn_id.clone(), item_id.clone(), client.clone());
                    let item = item.clone();
                    std::thread::spawn(move || {
                        for (stage, wait) in [("registered", 50), ("offered", 300)] {
                            std::thread::sleep(Duration::from_millis(wait));
                            let mut state = shared.lock().unwrap();
                            if !state.pending_claude.iter().any(|(message, _)| *message == item_id) {
                                return;
                            }
                            if stage == "offered" {
                                // The steer's own user item, in the same step.
                                broadcast(&mut state, "item/started", json!({"threadId": thread, "turnId": turn_id, "item": item}));
                                broadcast(&mut state, "item/completed", json!({"threadId": thread, "turnId": turn_id, "item": item}));
                                claim_owner_carries(&mut state, &client, &turn_id);
                            }
                            broadcast(&mut state, "claude/input/stageChanged", json!({"threadId": thread, "turnId": turn_id,
                                "messageId": item_id, "clientId": client, "stage": stage}));
                        }
                    });
                }
            }
            _ => {}
        }
        if ctx.kind == Kind::Codex || !script.claude_stages {
            claim_owner_carries(state, &client, &turn_id);
        }
        return Ok(json!({"turnId": turn_id}));
    }
    // open_turn appends queued carry items after the owner's separate user item.
    if let Some(ids) = state.pending_owner_carries.remove(&client) {
        for entry in &mut state.inbox {
            if ids.contains(&entry.id) { entry.state = "queued".into(); }
        }
    }
    let turn_id = open_turn(state, vec![item], true);
    let turn = state.turns.iter().rev().find(|t| t["id"] == turn_id.as_str()).cloned().unwrap();
    Ok(json!({"turn": turn}))
}

// ---------------------------------------------------------------------------
// Inbox: ExternalInput and the Claude adapter's local inbox

/// The model-visible item of an inbox input (Claude projects it as an
/// `external_event` function output with the message id as item id).
fn inbox_item(entry: &InboxEntry) -> Value {
    let (source, event_type, text, view) = match &entry.envelope {
        Some(envelope) => (envelope["message"]["source"].clone(), envelope["message"]["type"].clone(),
            envelope["message"]["text"].clone(), envelope.get("view").cloned().unwrap_or(Value::Null)),
        None => (entry.params["source"].clone(), entry.params["eventType"].clone(), entry.params["text"].clone(),
            entry.params.get("view").cloned().unwrap_or(Value::Null)),
    };
    json!({"type": "functionCallOutput", "name": "external_event", "id": entry.id,
        "output": json!({"source": source, "type": event_type, "text": text}).to_string(),
        "externalInputView": view})
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let ordered: std::collections::BTreeMap<_, _> = map.iter().map(|(k, v)| (k.clone(), sorted(v))).collect();
            serde_json::to_value(ordered).unwrap()
        }
        Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// The adapter's legacy inbox digest (asks_claude.rs `legacy_digest`).
fn legacy_digest(params: &Value) -> String {
    use sha2::Digest;
    let mut payload = json!({});
    for key in ["source", "eventType", "text", "metadata", "view"] {
        if let Some(value) = params.get(key) {
            payload[key] = value.clone();
        }
    }
    format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(&sorted(&payload)).unwrap()))
}

fn delivery_label(delivery: &str) -> &'static str {
    match delivery {
        "passive" => "passive",
        "soon" => "soon",
        _ => "after-turn",
    }
}

fn busy_refusal() -> Value {
    json!({"code": -32009, "message": "busy: an active turn", "data": {"externalInputError": "busy", "reason": "active_turn"}})
}

/// Place a new (or re-submitted, still queued) inbox input: soon joins a
/// running turn, after-turn starts a turn when idle, passive waits for one.
fn place(state: &mut State, index: usize, exclusive: bool) -> Result<(), Value> {
    if held(&state.script, &state.inbox[index]) || state.inbox[index].state != "queued" {
        return Ok(());
    }
    let busy = active(&state.turns).map(|t| t["id"].as_str().unwrap().to_string());
    let delivery = state.inbox[index].delivery.clone();
    match (busy, delivery.as_str()) {
        (Some(_), _) if exclusive => Err(busy_refusal()),
        (Some(turn), "soon") => {
            let thread = state.thread.clone();
            let entry = &mut state.inbox[index];
            entry.state = "processing".into();
            entry.turn = Some(turn.clone());
            entry.attempt = Some(uuid::Uuid::new_v4().to_string());
            let item = inbox_item(entry);
            if let Some(t) = active_mut(&mut state.turns) {
                t["items"].as_array_mut().unwrap().push(item.clone());
            }
            broadcast(state, "item/started", json!({"threadId": thread, "turnId": turn, "item": item}));
            Ok(())
        }
        (Some(_), _) | (None, "passive") => Ok(()),
        (None, _) => {
            open_turn(state, Vec::new(), true);
            Ok(())
        }
    }
}

fn external_submit(ctx: &Ctx, state: &mut State, params: &Value) -> Result<Value, Value> {
    let Some(message_id) = params["message"]["id"].as_str().map(str::to_string) else {
        return Err(json!({"code": -32602, "message": "fixture: message.id missing"}));
    };
    let exclusive = params["admission"] == "exclusive_turn";
    let known = state.inbox.iter().any(|e| e.id == message_id);
    let index = match state.inbox.iter().position(|e| e.id == message_id) {
        Some(index) => index,
        None => {
            let mut envelope = params.clone();
            if let Some(object) = envelope.as_object_mut() {
                object.remove("admission");
            }
            state.inbox.push(InboxEntry {
                id: message_id.clone(),
                envelope: Some(envelope),
                params: params.clone(),
                semantic: params["semanticSha256"].as_str().unwrap_or("").to_string(),
                delivery: params["message"]["delivery"].as_str().unwrap_or("after_turn").to_string(),
                state: "queued".into(),
                turn: None,
                attempt: None,
                persisted: false,
                ordinal: 0,
                owner: params["ownerId"].as_str().unwrap_or("").to_string(),
                generation: params["runtimeGeneration"].clone(),
            });
            state.inbox.len() - 1
        }
    };
    if known && !exclusive && active(&state.turns).is_none() {
        // A repeated submit of an input the runtime already holds is
        // idempotent: a queued after-turn input starts its own turn the way
        // the runtime starts it after a turn (`inbox_turn_delay`), not at
        // once (the C3 debug delivery re-submits a pending input every
        // sweep, which made the turn start depend on the sweep timing).
        schedule_inbox_turn(state);
    } else {
        place(state, index, exclusive)?;
    }
    let entry = state.inbox[index].clone();
    let mut response = json!({"version": 1, "ownerId": params["ownerId"], "threadId": ctx.thread,
        "runtimeGeneration": params["runtimeGeneration"], "statuses": [entry_status(&state.script, &entry)]});
    // N1 exclusive admission answers the reserved turn at once (the receipt follows).
    if exclusive {
        if let Some(turn) = &entry.turn {
            response["turnId"] = json!(turn);
        }
    }
    Ok(response)
}

fn receipt(entry: &InboxEntry) -> Value {
    let mut receipt = cutex::app_server::external_input::Receipt {
        schema: "codex.external-input-receipt.v1".into(),
        receipt_id: String::new(),
        owner_id: entry.owner.clone(),
        thread_id: entry.envelope.as_ref().and_then(|e| e["threadId"].as_str()).unwrap_or("").to_string(),
        message_id: entry.id.clone(),
        semantic_sha256: entry.semantic.clone(),
        response_item_id: entry.id.clone(),
        turn_id: entry.turn.clone().unwrap_or_default(),
        ordinal: entry.ordinal.max(1),
    };
    receipt.receipt_id = receipt.digest_id();
    serde_json::to_value(receipt).unwrap()
}

/// The RC8 status of one inbox input.
fn entry_status(script: &Script, entry: &InboxEntry) -> Value {
    let none = json!({"state": "none", "attemptId": null, "reason": null});
    let (delivery, receipt_value, processing) = if entry.persisted {
        ("context_persisted", receipt(entry), none)
    } else {
        match &status_for(script, entry) {
            NativeStatus::Held(reason) => ("pending", Value::Null, json!({"state": "held", "attemptId": entry.attempt, "reason": reason})),
            NativeStatus::RetryableError => ("retryable_error", Value::Null, none),
            NativeStatus::Conflict => ("conflict", Value::Null, none),
            NativeStatus::Persisted | NativeStatus::Pending => match entry.state.as_str() {
                "processing" => ("pending", Value::Null, json!({"state": "claimed", "attemptId": entry.attempt, "reason": null})),
                "interrupted" | "failed" => ("pending", Value::Null, json!({"state": "held",
                    "attemptId": entry.attempt.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()), "reason": "interrupted"})),
                _ => ("pending", Value::Null, json!({"state": "pending", "attemptId": null, "reason": null})),
            },
        }
    };
    json!({"messageId": entry.id, "semanticSha256": entry.semantic, "deliveryState": delivery,
        "receipt": receipt_value, "processing": processing})
}

fn external_status(ctx: &Ctx, state: &State, params: &Value) -> Value {
    let keys = params["messages"].as_array().cloned().unwrap_or_default();
    let statuses: Vec<Value> = keys
        .iter()
        .map(|key| match state.inbox.iter().find(|e| key["messageId"] == e.id.as_str()) {
            Some(entry) => entry_status(&state.script, entry),
            // Unknown here: what the script says, else never seen.
            None => {
                let (delivery, processing) = match &state.script.external_status {
                    NativeStatus::Held(reason) => ("pending", json!({"state": "held", "attemptId": null, "reason": reason})),
                    NativeStatus::RetryableError => ("retryable_error", json!({"state": "none", "attemptId": null, "reason": null})),
                    NativeStatus::Conflict => ("conflict", json!({"state": "none", "attemptId": null, "reason": null})),
                    NativeStatus::Pending => ("pending", json!({"state": "pending", "attemptId": null, "reason": null})),
                    NativeStatus::Persisted => ("unknown", json!({"state": "none", "attemptId": null, "reason": null})),
                };
                json!({"messageId": key["messageId"], "semanticSha256": key["semanticSha256"],
                    "deliveryState": delivery, "receipt": null, "processing": processing})
            }
        })
        .collect();
    json!({"version": 1, "ownerId": params["ownerId"], "threadId": ctx.thread,
        "runtimeGeneration": params["runtimeGeneration"], "statuses": statuses})
}

/// `claude/input/submit` with a message id: the adapter's durable local inbox
/// (bus messages on pre-r8 adapters, delegations and their results).
fn inbox_submit(ctx: &Ctx, state: &mut State, params: &Value) -> Result<Value, Value> {
    let message_id = params["messageId"].as_str().unwrap().to_string();
    if let Some(entry) = state.inbox.iter().find(|e| e.id == message_id) {
        let mut receipt = local_receipt(ctx, entry);
        receipt["duplicate"] = json!(true);
        return Ok(receipt);
    }
    let admission = params["admission"].as_str().unwrap_or("");
    let busy = active(&state.turns).is_some();
    if admission == "exclusive_turn" && busy {
        return Err(busy_refusal());
    }
    let delivery = match (admission, params["metadata"]["deliveryMode"].as_str()) {
        ("next_step", _) | (_, Some("soon")) => "soon",
        (_, Some("passive")) => "passive",
        _ => "after_turn",
    };
    state.inbox.push(InboxEntry {
        id: message_id.clone(),
        envelope: None,
        params: params.clone(),
        semantic: legacy_digest(params),
        delivery: delivery.into(),
        state: "queued".into(),
        turn: None,
        attempt: None,
        persisted: false,
        ordinal: 0,
        owner: params["metadata"]["cutexSessionId"].as_str().unwrap_or("").to_string(),
        generation: Value::Null,
    });
    let index = state.inbox.len() - 1;
    place(state, index, admission == "exclusive_turn")?;
    Ok(local_receipt(ctx, &state.inbox[index]))
}

fn local_receipt(ctx: &Ctx, entry: &InboxEntry) -> Value {
    let context_turn = if entry.persisted || entry.state == "processing" { entry.turn.clone() } else { None };
    json!({"threadId": ctx.thread, "messageId": entry.id, "durable": true, "receiptScope": "local-inbox",
        "state": entry.state, "turnId": entry.turn, "contextTurnId": context_turn,
        "contextPersisted": entry.persisted, "semanticSha256": match &entry.envelope {
            Some(_) => legacy_digest(&entry.params),
            None => entry.semantic.clone(),
        },
        "deliveryMode": delivery_label(&entry.delivery)})
}

fn local_status(ctx: &Ctx, state: &State, params: &Value) -> Result<Value, Value> {
    match state.inbox.iter().find(|e| params["messageId"] == e.id.as_str()) {
        Some(entry) => Ok(local_receipt(ctx, entry)),
        None => Err(json!({"code": -32004, "message": "input not found"})),
    }
}

// ---------------------------------------------------------------------------
// A minimal client, to test the fakes on their own.

pub struct Client {
    ws: WebSocket<TcpStream>,
    next: u64,
    pub notifications: Vec<Value>,
}

impl Client {
    pub fn connect(url: &str) -> Self {
        let addr = url.trim_start_matches("ws://");
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (ws, _) = tungstenite::client(format!("{url}/"), stream).unwrap();
        Self { ws, next: 0, notifications: Vec::new() }
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next += 1;
        let id = self.next;
        self.ws.send(Message::Text(json!({"id": id, "method": method, "params": params}).to_string().into())).unwrap();
        loop {
            let message = self.ws.read().unwrap();
            let Message::Text(text) = message else { continue };
            let value: Value = serde_json::from_str(text.as_ref()).unwrap();
            if value["id"] == id {
                return match value.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(value["result"].clone()),
                };
            }
            self.notifications.push(value);
        }
    }

    pub fn wait_notification(&mut self, method: &str) -> Value {
        if let Some(pos) = self.notifications.iter().position(|n| n["method"] == method) {
            return self.notifications.remove(pos);
        }
        loop {
            let Message::Text(text) = self.ws.read().unwrap() else { continue };
            let value: Value = serde_json::from_str(text.as_ref()).unwrap();
            if value["method"] == method {
                return value;
            }
            self.notifications.push(value);
        }
    }
}
