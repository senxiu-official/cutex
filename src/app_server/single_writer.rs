//! One durable writer per native event of an app-server runtime.
//!
//! Several cutex processes may hold a connection to the same runtime: the
//! Agent Bus service (`agent_create`, `member_runtime`, Agent Management
//! recovery) launches and connects runtimes, and Management adopts them
//! (`late_adoption`, startup adoption). The app-server fans every thread
//! notification out to all subscribed connections, so each process used to
//! append the same native event to the shared Management v2 journal and run
//! the same durable side effects (activity, usage, pending server requests,
//! asks, queue flushes).
//!
//! Handoff by emission time. Every native notification carries the
//! app-server's `emittedAtMs`, stamped once before fan-out, so all
//! connections see the same value for the same event. Management (the
//! *owner* role, one per home) picks a cut `C` when it connects:
//!
//! * another live process is registered as a writer of the same runtime
//!   occurrence: `C = now + HANDOFF_MARGIN_MS`; that process keeps every
//!   event emitted at or before `C`, Management takes every later one;
//! * nobody else writes: `C = i64::MIN` and Management takes everything.
//!
//! The lease (`event-owners.json` next to the journal) is written before `C`
//! and every *launcher* process (any other process) re-reads it at least
//! every [`CACHE_TTL`] (< margin), so a launcher always knows `C` before it
//! handles an event emitted after `C`. Each event therefore lands on exactly
//! one side, independent of how far either worker lags; nothing is compared
//! by content. Coalesced deltas are never merged across `C`
//! ([`ConnectionOwnership::split_after`]).
//!
//! Events without a stamp: a server request (approval) takes the stamp of
//! the last stamped notification of the same connection (both connections
//! receive the same order); a runtime without stamps at all falls back to
//! the local receive time, Management leaning [`UNTIMED_OWNER_BIAS_MS`]
//! towards an overlap rather than a gap. Connection-local events
//! (`Disconnected`, protocol violations) use the local receive time.
//!
//! A lease whose owner process is gone (Management stopped or crashed) no
//! longer applies: launchers write again until Management adopts anew.
//! Management releases its lease when its connection ends.
//!
//! Crash safety: each side keeps (bounded) the events the other side is
//! responsible for. When a launcher sees the owner's cut move up (owner gone
//! or released, or a later cut of a new owner), it journals the events of
//! the range it got back that the journal tail does not hold yet; when the
//! owner's cut passes while no launcher of that runtime is alive, it does
//! the same for the range before the cut. A writer handles its events in
//! stream order, so the tail check is "up to its last stamp" (non-delta
//! events by identity), and an uncertain case journals twice rather than
//! not at all.

use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::client::AppServerEvent;

/// How far in the future Management puts the cut when another process is
/// still writing: covers writing the lease, the launchers' cache and an
/// app-server event whose fan-out snapshot straddled Management's subscribe.
pub const HANDOFF_MARGIN_MS: i64 = 1_500;
/// Stampless stream events: Management counts them this much later, so a
/// receive-time race near the cut duplicates instead of losing an event.
pub const UNTIMED_OWNER_BIAS_MS: i64 = 250;
/// Launchers re-read the lease at least this often; must stay well below
/// [`HANDOFF_MARGIN_MS`].
pub const CACHE_TTL: Duration = Duration::from_millis(250);

const FILE_NAME: &str = "event-owners.json";
const LOCK_FILE_NAME: &str = "event-owners.lock";
const FILE_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Management serve: takes runtimes over from other processes.
    Owner,
    /// Every other process: writes until an owner holds a live lease.
    Launcher,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Creation time (RFC 3339) where the platform reports it; rejects a
    /// reused PID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
}

impl ProcessIdentity {
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            started_at: crate::platform::process::process_started_at(pid)
                .ok()
                .map(|at| at.to_rfc3339()),
        }
    }

    fn is_live(&self) -> bool {
        if !crate::platform::process::process_is_running(self.pid) {
            return false;
        }
        match &self.started_at {
            None => true,
            Some(expected) => crate::platform::process::process_started_at(self.pid)
                .is_ok_and(|at| &at.to_rfc3339() == expected),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnersFile {
    #[serde(default)]
    version: u8,
    /// Per cutex session: the owner of its current runtime occurrence.
    #[serde(default)]
    leases: BTreeMap<String, Lease>,
    /// Per cutex session: launcher connections that write its events.
    #[serde(default)]
    writers: BTreeMap<String, Vec<Writer>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Lease {
    runtime_generation: u64,
    owner: ProcessIdentity,
    /// Events emitted after this (Unix ms) belong to the owner.
    cut_at_ms: i64,
    token: String,
    since: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Writer {
    runtime_generation: u64,
    process: ProcessIdentity,
    token: String,
}

type Liveness = dyn Fn(&ProcessIdentity) -> bool + Send + Sync;

/// The shared lease file of one cutex home, as seen by one process.
pub struct Ledger {
    root: PathBuf,
    me: ProcessIdentity,
    role: Option<Role>,
    is_live: Box<Liveness>,
    cache: Mutex<Option<(Instant, BTreeMap<String, (u64, i64)>)>>,
    process_lock: Mutex<()>,
}

static OWNER_ROLE: AtomicBool = AtomicBool::new(false);
static PROCESS_LEDGER: OnceLock<Option<Arc<Ledger>>> = OnceLock::new();

/// Management serve: this process takes runtimes over (call before it
/// connects any runtime).
pub fn claim_owner_role() {
    OWNER_ROLE.store(true, Ordering::Release);
}

/// This process's ledger, or `None` when the runtime directory is unusable
/// (then every connection writes, as before).
pub fn process_ledger() -> Option<Arc<Ledger>> {
    PROCESS_LEDGER
        .get_or_init(|| {
            let root = crate::config::paths::runtime_dir().ok()?.join("management-v2");
            Some(Arc::new(Ledger {
                root,
                me: ProcessIdentity::current(),
                role: None,
                is_live: Box::new(ProcessIdentity::is_live),
                cache: Mutex::new(None),
                process_lock: Mutex::new(()),
            }))
        })
        .clone()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn emitted_at_ms(raw: &Value) -> Option<i64> {
    raw.get("emittedAtMs").and_then(Value::as_i64).filter(|ms| *ms > 0)
}

impl Ledger {
    /// A ledger with a fixed role and identity (tests simulate several
    /// processes on one file).
    pub fn new(
        root: impl Into<PathBuf>,
        me: ProcessIdentity,
        role: Role,
        is_live: impl Fn(&ProcessIdentity) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            root: root.into(),
            me,
            role: Some(role),
            is_live: Box::new(is_live),
            cache: Mutex::new(None),
            process_lock: Mutex::new(()),
        }
    }

    pub fn role(&self) -> Role {
        self.role.unwrap_or_else(|| {
            if OWNER_ROLE.load(Ordering::Acquire) {
                Role::Owner
            } else {
                Role::Launcher
            }
        })
    }

    /// Called before a connection to `generation` of `cutex_session_id` is
    /// made. A launcher registers as a writer at once, so an owner adopting
    /// meanwhile hands over instead of taking everything.
    pub fn open(self: &Arc<Self>, cutex_session_id: &str, generation: u64) -> ConnectionOwnership {
        let role = self.role();
        let mut connection = ConnectionOwnership {
            ledger: self.clone(),
            cutex_session_id: cutex_session_id.to_string(),
            generation,
            token: uuid::Uuid::new_v4().to_string(),
            role,
            cut: None,
            last_emitted: None,
            registered: false,
            leased: false,
            applied: None,
            retained: std::collections::VecDeque::new(),
            retained_bytes: 0,
            covered: None,
            settled: false,
            replay: Vec::new(),
        };
        if role == Role::Launcher {
            let writer = Writer {
                runtime_generation: generation,
                process: self.me.clone(),
                token: connection.token.clone(),
            };
            match self.mutate(|file| {
                file.writers
                    .entry(cutex_session_id.to_string())
                    .or_default()
                    .push(writer);
                Ok(())
            }) {
                Ok(()) => connection.registered = true,
                Err(error) => eprintln!(
                    "warning: event writer registration for {cutex_session_id} failed: {error:#}"
                ),
            }
        }
        connection
    }

    fn path(&self) -> PathBuf {
        self.root.join(FILE_NAME)
    }

    fn read(&self) -> OwnersFile {
        match fs::read(self.path()) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
            Err(_) => OwnersFile::default(),
        }
    }

    /// Read-modify-write under the cross-process lock; drops entries of
    /// processes that are gone.
    fn mutate<T>(&self, change: impl FnOnce(&mut OwnersFile) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let _guard = self
            .process_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("event owner lock poisoned"))?;
        fs::create_dir_all(&self.root)?;
        let lock = open_private(&self.root.join(LOCK_FILE_NAME))?;
        lock.lock()?;
        let result = (|| {
            let mut file = self.read();
            file.version = FILE_VERSION;
            file.leases.retain(|_, lease| (self.is_live)(&lease.owner));
            for writers in file.writers.values_mut() {
                writers.retain(|writer| (self.is_live)(&writer.process));
            }
            file.writers.retain(|_, writers| !writers.is_empty());
            let value = change(&mut file)?;
            if file.leases.is_empty() && file.writers.is_empty() {
                match fs::remove_file(self.path()) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            } else {
                crate::config::atomic::write_private_pretty_json_atomic(
                    &self.path(),
                    &file,
                    "event owners",
                )?;
            }
            Ok(value)
        })();
        let _ = lock.unlock();
        self.invalidate();
        result
    }

    fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    /// The live owner's cut for this runtime occurrence, as a launcher sees
    /// it (at most [`CACHE_TTL`] old).
    fn owner_cut(&self, cutex_session_id: &str, generation: u64) -> Option<i64> {
        let mut cache = self.cache.lock().ok()?;
        let fresh = cache
            .as_ref()
            .is_some_and(|(read_at, _)| read_at.elapsed() < CACHE_TTL);
        if !fresh {
            let leases = self
                .read()
                .leases
                .into_iter()
                .filter(|(_, lease)| lease.owner != self.me && (self.is_live)(&lease.owner))
                .map(|(id, lease)| (id, (lease.runtime_generation, lease.cut_at_ms)))
                .collect();
            *cache = Some((Instant::now(), leases));
        }
        cache
            .as_ref()
            .and_then(|(_, leases)| leases.get(cutex_session_id))
            .filter(|(lease_generation, _)| *lease_generation == generation)
            .map(|(_, cut)| *cut)
    }
}

/// Bounds of the events a connection keeps while another process is
/// responsible for them, to journal them itself if that process stops
/// before it did (crash, released connection).
const MAX_RETAINED_EVENTS: usize = 1_024;
const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
/// How much of the journal tail a takeover reads to learn what the previous
/// writer already journaled.
const JOURNAL_SCAN_BYTES: u64 = 16 * 1024 * 1024;
const JOURNAL_FILES: [&str; 2] = ["events.jsonl", "events.jsonl.1"];

struct Retained {
    key: i64,
    event: AppServerEvent,
    bytes: usize,
}

/// What another writer journaled for one session after some key: its last
/// key and the identities of its events (`journal_identity`).
#[derive(Debug, Default)]
struct Covered {
    /// Only keys above this were the other writer's.
    after: i64,
    hwm: Option<i64>,
    identities: std::collections::HashSet<String>,
}

impl Covered {
    /// Whether the other writer already journaled `event`. That writer
    /// handled its events in stream order, so everything up to its last key
    /// was handled; deltas are taken as covered there (it may have coalesced
    /// them differently), every other event must match by identity.
    fn covers(&self, event: &AppServerEvent, key: i64) -> bool {
        if key <= self.after || self.hwm.is_none_or(|hwm| key > hwm) {
            return false;
        }
        match event {
            AppServerEvent::Notification(notification)
                if super::delta_coalesce::DELTA_METHODS.contains(&notification.method.as_str()) =>
            {
                true
            }
            _ => event_identity(event).is_some_and(|id| self.identities.contains(&id)),
        }
    }
}

/// Identity of a native event across connections: method, emission stamp
/// and the item / turn id (else the params), robust to journal-side payload
/// trimming. Server requests: their native id.
fn message_identity(message: &Value) -> Option<String> {
    let method = message.get("method")?.as_str()?;
    if let Some(id) = message.get("id").filter(|id| !id.is_null()) {
        return Some(format!("request\u{0}{method}\u{0}{id}"));
    }
    let emitted = emitted_at_ms(message)?;
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let subject = params
        .get("itemId")
        .or_else(|| params.pointer("/item/id"))
        .or_else(|| params.pointer("/turn/id"))
        .map(Value::to_string)
        .unwrap_or_else(|| params.to_string());
    Some(format!("{method}\u{0}{emitted}\u{0}{subject}"))
}

fn event_identity(event: &AppServerEvent) -> Option<String> {
    match event {
        AppServerEvent::Notification(notification) => message_identity(&notification.raw),
        AppServerEvent::ServerRequest(request) => message_identity(&request.raw),
        _ => None,
    }
}

/// The native events journaled for `cutex_session_id` with a key above
/// `after`, from the tail of the Management v2 journal next to the lease.
fn journaled_after(root: &Path, cutex_session_id: &str, after: i64) -> Covered {
    use std::io::{Read, Seek, SeekFrom};
    let mut covered = Covered { after, ..Covered::default() };
    let mut budget = JOURNAL_SCAN_BYTES;
    for name in JOURNAL_FILES {
        if budget == 0 {
            break;
        }
        let Ok(mut file) = File::open(root.join(name)) else {
            continue;
        };
        let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
            continue;
        };
        let start = length.saturating_sub(budget);
        budget = budget.saturating_sub(length - start);
        let mut tail = Vec::new();
        if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut tail).is_err() {
            continue;
        }
        let mut lines = tail.split(|byte| *byte == b'\n');
        if start > 0 {
            lines.next(); // partial first line
        }
        for line in lines {
            // A line still being written fails to parse and is skipped.
            let Ok(envelope) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if envelope.get("cutexSessionId").and_then(Value::as_str) != Some(cutex_session_id) {
                continue;
            }
            let Some(message) = envelope.pointer("/native/message") else {
                continue;
            };
            let Some(key) = emitted_at_ms(message) else {
                continue;
            };
            if key <= after {
                continue;
            }
            covered.hwm = Some(covered.hwm.map_or(key, |hwm| hwm.max(key)));
            if let Some(identity) = message_identity(message) {
                covered.identities.insert(identity);
            }
        }
    }
    covered
}

/// The ownership state of one connection (one event worker).
pub struct ConnectionOwnership {
    ledger: Arc<Ledger>,
    cutex_session_id: String,
    generation: u64,
    token: String,
    role: Role,
    /// Owner only: set by [`Self::activate`].
    cut: Option<i64>,
    last_emitted: Option<i64>,
    registered: bool,
    leased: bool,
    /// Launcher: the owner cut in effect when last looked (`None` inside: no
    /// owner). Unset before activation.
    applied: Option<Option<i64>>,
    /// Events another writer is responsible for, kept until it is known to
    /// have journaled them (bounded).
    retained: std::collections::VecDeque<Retained>,
    retained_bytes: usize,
    /// After a takeover: what the previous writer journaled in the range.
    covered: Option<Covered>,
    /// Owner: the handoff was settled (cut passed).
    settled: bool,
    /// Events to journal before the next one (from a takeover).
    replay: Vec<AppServerEvent>,
}

impl ConnectionOwnership {
    /// Called once the connection is subscribed (thread bootstrap answered)
    /// and before its first event is handled. An owner picks its cut and
    /// publishes the lease; a launcher reads the lease.
    pub fn activate(&mut self) {
        self.activate_at(now_ms());
    }

    pub fn activate_at(&mut self, now: i64) {
        if self.role == Role::Launcher {
            self.ledger.invalidate();
            self.applied = Some(self.ledger.owner_cut(&self.cutex_session_id, self.generation));
            return;
        }
        let ledger = self.ledger.clone();
        let session = self.cutex_session_id.clone();
        let generation = self.generation;
        let token = self.token.clone();
        let result = ledger.mutate(|file| {
            let other_writer = file.writers.get(&session).is_some_and(|writers| {
                writers.iter().any(|writer| {
                    writer.runtime_generation == generation && writer.process != ledger.me
                })
            });
            let cut = if other_writer {
                now.saturating_add(HANDOFF_MARGIN_MS)
            } else {
                i64::MIN
            };
            file.leases.insert(
                session.clone(),
                Lease {
                    runtime_generation: generation,
                    owner: ledger.me.clone(),
                    cut_at_ms: cut,
                    token: token.clone(),
                    since: chrono::Utc::now().to_rfc3339(),
                },
            );
            Ok(cut)
        });
        match result {
            Ok(cut) => {
                self.cut = Some(cut);
                self.leased = true;
                self.settled = cut == i64::MIN;
            }
            Err(error) => {
                // Without a lease the launchers keep writing too: overlap,
                // never a gap.
                eprintln!("warning: event owner lease for {session} failed: {error:#}");
                self.cut = Some(i64::MIN);
                self.settled = true;
            }
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Owner: its cut once active. Launcher: the live owner's cut, if any.
    pub fn cut(&self) -> Option<i64> {
        match self.role {
            Role::Owner => self.cut,
            Role::Launcher => self.ledger.owner_cut(&self.cutex_session_id, self.generation),
        }
    }

    /// Deltas emitted at or before this must not be merged with later ones.
    pub fn split_after(&self) -> Option<i64> {
        self.cut().filter(|cut| *cut != i64::MIN)
    }

    /// Whether this process journals `event` and runs its durable effects.
    /// Call [`Self::take_replay`] afterwards: events to journal first.
    pub fn handles(&mut self, event: &AppServerEvent) -> bool {
        self.handles_at(event, now_ms())
    }

    pub fn handles_at(&mut self, event: &AppServerEvent, now: i64) -> bool {
        let local = matches!(
            event,
            AppServerEvent::ProtocolViolation { .. } | AppServerEvent::Disconnected { .. }
        );
        let key = self.key(event, now);
        match self.role {
            Role::Owner => {
                if key > self.cut.unwrap_or(i64::MIN) {
                    self.settle_handoff();
                    return true;
                }
                if !local {
                    self.retain(key, event);
                }
                false
            }
            Role::Launcher => {
                let cut = self.ledger.owner_cut(&self.cutex_session_id, self.generation);
                self.apply_cut(cut);
                if key <= cut.unwrap_or(i64::MAX) {
                    return !self.covered.as_ref().is_some_and(|covered| covered.covers(event, key));
                }
                if !local {
                    self.retain(key, event);
                }
                false
            }
        }
    }

    /// No event arrived for a while: notice an owner change or a passed cut.
    pub fn poll(&mut self) {
        self.poll_at(now_ms());
    }

    pub fn poll_at(&mut self, now: i64) {
        match self.role {
            Role::Owner => {
                if now > self.cut.unwrap_or(i64::MIN) {
                    self.settle_handoff();
                }
            }
            Role::Launcher => {
                let cut = self.ledger.owner_cut(&self.cutex_session_id, self.generation);
                self.apply_cut(cut);
            }
        }
    }

    /// Events this process must journal now, before the event just
    /// classified: the part of a range it took over that the previous
    /// writer did not journal.
    pub fn take_replay(&mut self) -> Vec<AppServerEvent> {
        std::mem::take(&mut self.replay)
    }

    fn key(&mut self, event: &AppServerEvent, now: i64) -> i64 {
        let bias = match self.role {
            Role::Owner => UNTIMED_OWNER_BIAS_MS,
            Role::Launcher => 0,
        };
        match event {
            AppServerEvent::Notification(notification) => match emitted_at_ms(&notification.raw) {
                Some(ms) => {
                    self.last_emitted = Some(ms);
                    ms
                }
                None => self.last_emitted.unwrap_or(now.saturating_add(bias)),
            },
            AppServerEvent::ServerRequest(request) => emitted_at_ms(&request.raw)
                .or(self.last_emitted)
                .unwrap_or(now.saturating_add(bias)),
            AppServerEvent::ProtocolViolation { .. } | AppServerEvent::Disconnected { .. } => now,
        }
    }

    fn retain(&mut self, key: i64, event: &AppServerEvent) {
        let bytes = match event {
            AppServerEvent::Notification(notification) => notification.raw.to_string().len(),
            AppServerEvent::ServerRequest(request) => request.raw.to_string().len(),
            _ => 0,
        };
        self.retained.push_back(Retained { key, event: event.clone(), bytes });
        self.retained_bytes += bytes;
        while self.retained.len() > MAX_RETAINED_EVENTS || self.retained_bytes > MAX_RETAINED_BYTES {
            let Some(dropped) = self.retained.pop_front() else {
                break;
            };
            self.retained_bytes -= dropped.bytes;
        }
    }

    /// Launcher: the owner's cut moved. When it moved up (the owner is gone,
    /// released the runtime, or a new owner starts later), the range between
    /// the old and the new cut is this connection's again: journal what the
    /// previous owner had not, and skip what it had.
    fn apply_cut(&mut self, cut: Option<i64>) {
        let Some(previous) = self.applied.replace(cut) else {
            return;
        };
        let old = previous.unwrap_or(i64::MAX);
        let new = cut.unwrap_or(i64::MAX);
        if new <= old {
            return;
        }
        let covered = journaled_after(&self.ledger.root, &self.cutex_session_id, old);
        self.take_over_retained(new, &covered);
        self.covered = Some(covered);
    }

    /// Owner: the cut passed. Events up to it were the launcher's; if no
    /// launcher of this runtime is alive any more, journal those it did not.
    fn settle_handoff(&mut self) {
        if self.settled {
            return;
        }
        self.settled = true;
        let Some(first) = self.retained.front().map(|retained| retained.key) else {
            return;
        };
        let file = self.ledger.read();
        let launcher_alive = file.writers.get(&self.cutex_session_id).is_some_and(|writers| {
            writers.iter().any(|writer| {
                writer.runtime_generation == self.generation
                    && writer.process != self.ledger.me
                    && (self.ledger.is_live)(&writer.process)
            })
        });
        if launcher_alive {
            self.retained.clear();
            self.retained_bytes = 0;
            return;
        }
        let covered = journaled_after(&self.ledger.root, &self.cutex_session_id, first.saturating_sub(1));
        self.take_over_retained(i64::MAX, &covered);
    }

    fn take_over_retained(&mut self, up_to: i64, covered: &Covered) {
        let mut kept = std::collections::VecDeque::new();
        let mut kept_bytes = 0;
        for retained in std::mem::take(&mut self.retained) {
            if retained.key > up_to {
                kept_bytes += retained.bytes;
                kept.push_back(retained);
            } else if !covered.covers(&retained.event, retained.key) {
                self.replay.push(retained.event);
            }
        }
        self.retained = kept;
        self.retained_bytes = kept_bytes;
    }
}

impl Drop for ConnectionOwnership {
    fn drop(&mut self) {
        if !self.registered && !self.leased {
            return;
        }
        let session = self.cutex_session_id.clone();
        let token = self.token.clone();
        let result = self.ledger.mutate(|file| {
            if file.leases.get(&session).is_some_and(|lease| lease.token == token) {
                file.leases.remove(&session);
            }
            if let Some(writers) = file.writers.get_mut(&session) {
                writers.retain(|writer| writer.token != token);
                if writers.is_empty() {
                    file.writers.remove(&session);
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            eprintln!("warning: event owner release for {session} failed: {error:#}");
        }
    }
}

fn open_private(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

#[cfg(test)]
#[path = "single_writer_tests.rs"]
mod tests;
