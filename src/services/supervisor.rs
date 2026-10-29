//! Daemon-side services supervisor: health probes, process supervision for
//! `managed` services, bounded log rings, and change events.
//!
//! One background thread ticks every second (or when kicked): it reloads the
//! registry when its `revision` moved (agents write it through the Agent Bus),
//! reconciles processes, probes health every
//! 15 s (every 2 s while something is starting) with bounded concurrency, and
//! persists runtime facts to `state.json` for other readers.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;
use serde_json::Value;

use super::health::compute_health;
use super::health::probe;
use super::health::ProcessFacts;
use super::health::RuntimeSnapshot;
use super::health::ServiceHealth;
use super::process::ProcessTree;
use super::store::now_rfc3339;
use super::store::service_view;
use super::store::Registry;
use super::store::ServiceRecord;
use super::store::ServiceRuntime;
use super::store::ServiceStore;
use super::store::ServicesError;
use super::store::ServicesResult;
use super::store::ServicesSettings;

pub const LOG_RING_BYTES: usize = 256 * 1024;
const LOG_FILE_MAX_BYTES: u64 = 5 * 1024 * 1024;
const TICK: Duration = Duration::from_secs(1);
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(15);
const STARTING_HEALTH_INTERVAL: Duration = Duration::from_secs(2);
const PROBE_CONCURRENCY: usize = 8;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A process that stayed up this long resets the crash backoff.
const BACKOFF_RESET_UPTIME: Duration = Duration::from_secs(60);
const LAST_SEEN_PERSIST_INTERVAL: Duration = Duration::from_secs(60);
const LOG_SUBSCRIBER_CAPACITY: usize = 512;
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);
const OPEN_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// A login-link command (`loginLinkCommand`) gets this long before its tree is killed.
const LOGIN_LINK_TIMEOUT: Duration = Duration::from_secs(15);

/// A change clients should refresh on (`cutex/services/changed`).
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceEvent {
    /// `registered` | `updated` | `removed` | `health` | `started` | `exited` | `stopped` | `settings`.
    pub reason: &'static str,
    pub slug: Option<String>,
    /// Current client view (absent for `removed` / `settings`).
    pub service: Option<Value>,
}

impl ServiceEvent {
    pub fn params(&self) -> Value {
        json!({
            "reason": self.reason,
            "slug": self.slug,
            "service": self.service,
            "hostId": crate::platform::host::current_host_name(),
        })
    }
}

pub type EventSink = Box<dyn Fn(ServiceEvent) + Send + Sync>;

#[derive(Clone)]
pub enum LogEvent {
    /// `(end offset after this chunk, text)`.
    Output(u64, Arc<String>),
    State(Value),
}

#[derive(Default)]
struct LogRingState {
    bytes: VecDeque<u8>,
    end_offset: u64,
    subscribers: Vec<mpsc::SyncSender<LogEvent>>,
    file: Option<File>,
    file_path: Option<PathBuf>,
    file_bytes: u64,
}

/// Bounded in-memory log for one service, mirrored to its log file.
#[derive(Default)]
pub struct LogRing {
    state: Mutex<LogRingState>,
}

impl LogRing {
    fn open_file(&self, path: PathBuf) {
        let mut state = self.state.lock().expect("log ring");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_FILE_MAX_BYTES) {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        state.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        state.file_bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        state.file_path = Some(path);
    }

    pub fn push(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut state = self.state.lock().expect("log ring");
        state.bytes.extend(text.as_bytes());
        let overflow = state.bytes.len().saturating_sub(LOG_RING_BYTES);
        if overflow > 0 {
            state.bytes.drain(..overflow);
        }
        state.end_offset += text.len() as u64;
        let mut rotate = false;
        if let Some(file) = state.file.as_mut() {
            let _ = file.write_all(text.as_bytes());
            rotate = true;
        }
        if rotate {
            state.file_bytes += text.len() as u64;
            if state.file_bytes > LOG_FILE_MAX_BYTES {
                if let Some(path) = state.file_path.clone() {
                    state.file = None;
                    let _ = std::fs::rename(&path, path.with_extension("log.1"));
                    state.file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .ok();
                    state.file_bytes = 0;
                }
            }
        }
        let event = LogEvent::Output(state.end_offset, Arc::new(text));
        state
            .subscribers
            .retain(|subscriber| subscriber.try_send(event.clone()).is_ok());
    }

    fn note(&self, line: &str) {
        self.push(format!("[cutex] {line}\n").as_bytes());
    }

    fn publish_state(&self, value: Value) {
        let mut state = self.state.lock().expect("log ring");
        let event = LogEvent::State(value);
        state
            .subscribers
            .retain(|subscriber| subscriber.try_send(event.clone()).is_ok());
    }

    /// The last `lines` lines held in memory.
    pub fn tail(&self, lines: usize) -> (String, u64) {
        let state = self.state.lock().expect("log ring");
        let bytes: Vec<u8> = state.bytes.iter().copied().collect();
        (
            tail_lines(&String::from_utf8_lossy(&bytes), lines),
            state.end_offset,
        )
    }

    fn subscribe(&self, lines: usize) -> (mpsc::Receiver<LogEvent>, String, u64) {
        let mut state = self.state.lock().expect("log ring");
        let (sender, receiver) = mpsc::sync_channel(LOG_SUBSCRIBER_CAPACITY);
        state.subscribers.push(sender);
        let bytes: Vec<u8> = state.bytes.iter().copied().collect();
        (
            receiver,
            tail_lines(&String::from_utf8_lossy(&bytes), lines),
            state.end_offset,
        )
    }
}

/// The last `lines` lines of `text` (a trailing partial line counts).
pub fn tail_lines(text: &str, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let mut start = trimmed.len();
    let mut seen = 0;
    for (index, byte) in trimmed.bytes().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen == lines {
                start = index + 1;
                break;
            }
        }
        start = index;
    }
    text[start..].to_string()
}

struct Running {
    tree: Arc<ProcessTree>,
    generation: u64,
    spawned_at: Instant,
    reached: bool,
}

struct Entry {
    snapshot: RuntimeSnapshot,
    running: Option<Running>,
    fingerprint: String,
    backoff: Duration,
    next_restart: Option<Instant>,
    logs: Arc<LogRing>,
    last_probe_ok: bool,
    last_seen_persisted: Option<Instant>,
    max_restarts: Option<u32>,
    crash_looped: bool,
}

impl Entry {
    fn new(record: &ServiceRecord, persisted: Option<RuntimeSnapshot>) -> Self {
        let mut snapshot = persisted.unwrap_or(RuntimeSnapshot {
            last_traffic_at: None,
            health: ServiceHealth::Unreachable,
            last_seen_at: None,
            last_probe_at: None,
            pid: None,
            started_at: None,
            restarts: 0,
            last_exit_code: None,
            last_exit_at: None,
            next_restart_at: None,
        });
        let crash_looped = record.lifetime == super::store::ServiceLifetime::Ephemeral
            && snapshot.health == ServiceHealth::CrashLooped;
        snapshot.pid = None;
        snapshot.started_at = None;
        snapshot.next_restart_at = None;
        if record.lifetime == super::store::ServiceLifetime::Persistent {
            snapshot.restarts = 0;
        }
        snapshot.health = if crash_looped {
            ServiceHealth::CrashLooped
        } else if record.runtime.supervised() {
            ServiceHealth::Stopped
        } else {
            snapshot.health
        };
        Self {
            snapshot,
            running: None,
            fingerprint: record.launch_fingerprint(),
            backoff: BACKOFF_MIN,
            next_restart: None,
            logs: Arc::new(LogRing::default()),
            last_probe_ok: false,
            last_seen_persisted: None,
            max_restarts: None,
            crash_looped,
        }
    }

    fn process_facts(&self, record: &ServiceRecord) -> ProcessFacts {
        match &self.running {
            Some(running) => ProcessFacts::Running {
                since_spawn: running.spawned_at.elapsed(),
                reached_since_spawn: running.reached,
            },
            None if self.next_restart.is_some() && wants_running(record) => {
                ProcessFacts::RestartPending
            }
            None => ProcessFacts::Down,
        }
    }
}

fn wants_running(record: &ServiceRecord) -> bool {
    match record.runtime {
        ServiceRuntime::External | ServiceRuntime::Static => false,
        ServiceRuntime::Managed => !record.stopped && !record.archived,
    }
}

#[derive(Default)]
struct Inner {
    revision: Option<u64>,
    settings: ServicesSettings,
    records: BTreeMap<String, ServiceRecord>,
    entries: HashMap<String, Entry>,
    generation: u64,
    state_dirty: bool,
    last_health: Option<Instant>,
    shutdown: bool,
    /// Slugs whose `restartRequestedAt` changed since the last reload
    /// (`service_restart`, possibly written by the Agent Bus process).
    pending_restarts: Vec<String>,
}

pub struct Supervisor {
    store: ServiceStore,
    inner: Mutex<Inner>,
    wake: Condvar,
    sink: EventSink,
    default_cwd: PathBuf,
    lifecycle: Arc<super::lifecycle::Lifecycle>,
}

impl Supervisor {
    pub fn new(store: ServiceStore, sink: EventSink) -> Arc<Self> {
        let default_cwd = crate::config::paths::home_dir().unwrap_or_else(std::env::temp_dir);
        let lifecycle = super::lifecycle::for_store(&store);
        Arc::new(Self {
            lifecycle,
            store,
            inner: Mutex::new(Inner::default()),
            wake: Condvar::new(),
            sink,
            default_cwd,
        })
    }

    pub fn store(&self) -> &ServiceStore {
        &self.store
    }

    /// Load the registry, reap leftovers from a previous daemon, start managed
    /// services and run the background loop.
    pub fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.store.dir())?;
        self.store.mutate(|_| Ok(()))?;
        reap_previous_processes(&self.store);
        self.reclaim();
        let mut events = Vec::new();
        {
            let mut inner = self.inner.lock().expect("services supervisor");
            self.reload_locked(&mut inner, &mut events, true);
            self.reconcile_locked(&mut inner, &mut events);
            self.persist_locked(&mut inner);
        }
        self.emit(events);
        let supervisor = self.clone();
        std::thread::Builder::new()
            .name("cutex-services".into())
            .spawn(move || supervisor.run_loop())?;
        Ok(())
    }

    /// Stop the loop and every supervised process (tests / shutdown).
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().expect("services supervisor");
        inner.shutdown = true;
        for entry in inner.entries.values_mut() {
            if let Some(running) = entry.running.take() {
                running.tree.kill();
            }
        }
        self.wake.notify_all();
    }

    pub fn kick(&self) {
        self.wake.notify_all();
    }

    fn emit(&self, events: Vec<ServiceEvent>) {
        for event in events {
            (self.sink)(event);
        }
    }

    fn run_loop(self: Arc<Self>) {
        loop {
            {
                let inner = self.inner.lock().expect("services supervisor");
                if inner.shutdown {
                    return;
                }
                let (inner, _) = self
                    .wake
                    .wait_timeout(inner, TICK)
                    .expect("services supervisor");
                if inner.shutdown {
                    return;
                }
            }
            self.tick();
        }
    }

    /// One supervision pass (also called directly by tests).
    pub fn tick(self: &Arc<Self>) {
        self.reclaim();
        let mut events = Vec::new();
        let due = {
            let mut inner = self.inner.lock().expect("services supervisor");
            self.reload_locked(&mut inner, &mut events, false);
            self.reconcile_locked(&mut inner, &mut events);
            let starting = inner
                .entries
                .values()
                .any(|entry| entry.snapshot.health == ServiceHealth::Starting);
            let interval = if starting {
                STARTING_HEALTH_INTERVAL
            } else {
                HEALTH_INTERVAL
            };
            inner
                .last_health
                .is_none_or(|last| last.elapsed() >= interval)
        };
        self.emit(events);
        self.run_pending_restarts();
        if due {
            self.probe_all();
        }
        let mut inner = self.inner.lock().expect("services supervisor");
        self.persist_locked(&mut inner);
    }

    /// Restart the services an owning agent asked to restart
    /// (`ServiceStore::request_restart`) through the same path as the owner's
    /// `POST /v2/services/{slug}/restart`. A service the owner stopped (or that
    /// is no longer managed) since the request stays as it is.
    fn run_pending_restarts(self: &Arc<Self>) {
        let pending = {
            let mut inner = self.inner.lock().expect("services supervisor");
            std::mem::take(&mut inner.pending_restarts)
        };
        for slug in pending {
            match self.store.get(&slug) {
                Ok(record) if record.runtime == ServiceRuntime::Managed && !record.stopped => {
                    if let Ok(logs) = self.ring(&slug) {
                        logs.note("restart requested by the owning agent");
                    }
                    if let Err(error) = self.restart_service(&slug) {
                        eprintln!("services: agent restart of {slug} failed: {error}");
                    }
                }
                Ok(_) => {
                    if let Ok(logs) = self.ring(&slug) {
                        logs.note("agent restart request ignored: service stopped or not managed");
                    }
                }
                Err(_) => {}
            }
        }
    }

    fn reload_locked(&self, inner: &mut Inner, events: &mut Vec<ServiceEvent>, initial: bool) {
        let registry: Registry = match self.store.load() {
            Ok(registry) => registry,
            Err(error) => {
                eprintln!("services: registry unavailable: {error}");
                return;
            }
        };
        if inner.revision == Some(registry.revision) {
            return;
        }
        inner.revision = Some(registry.revision);
        if inner.settings != registry.settings && !initial {
            events.push(ServiceEvent {
                reason: "settings",
                slug: None,
                service: None,
            });
        }
        inner.settings = registry.settings.clone();
        let persisted = if initial {
            self.store.load_state()
        } else {
            BTreeMap::new()
        };
        let removed: Vec<String> = inner
            .records
            .keys()
            .filter(|slug| !registry.services.contains_key(*slug))
            .cloned()
            .collect();
        for slug in removed {
            inner.records.remove(&slug);
            if let Some(mut entry) = inner.entries.remove(&slug) {
                if let Some(running) = entry.running.take() {
                    entry.logs.note("service unregistered; stopping");
                    running.tree.kill();
                }
            }
            super::traffic::forget(&slug);
            inner.state_dirty = true;
            events.push(ServiceEvent {
                reason: if registry.tombstones.contains_key(&slug) {
                    "reclaimed"
                } else {
                    "removed"
                },
                slug: Some(slug),
                service: None,
            });
        }
        for (slug, record) in registry.services {
            let previous = inner.records.insert(slug.clone(), record.clone());
            if previous
                .as_ref()
                .is_some_and(|previous| previous.created_at != record.created_at)
            {
                // Unregistered (or reclaimed) and registered again between two
                // passes: a new service under the old slug. Nothing of the old
                // one carries over (process, restarts, health, traffic).
                if let Some(mut entry) = inner.entries.remove(&slug) {
                    if let Some(running) = entry.running.take() {
                        entry.logs.note("service registered again; stopping the previous one");
                        running.tree.kill();
                    }
                }
                super::traffic::forget(&slug);
            }
            let entry = inner
                .entries
                .entry(slug.clone())
                .or_insert_with(|| Entry::new(&record, persisted.get(&slug).cloned()));
            entry.max_restarts = (record.lifetime == super::store::ServiceLifetime::Ephemeral)
                .then_some(inner.settings.ephemeral_max_restarts);
            if record.lifetime == super::store::ServiceLifetime::Persistent {
                entry.crash_looped = false;
            }
            if !record.runtime.supervised() {
                entry.next_restart = None;
                if entry.snapshot.health == ServiceHealth::Stopped {
                    entry.snapshot.health = ServiceHealth::Unreachable;
                }
            }
            if !initial
                && record.restart_requested_at.is_some()
                && previous.as_ref().is_some_and(|previous| {
                    previous.restart_requested_at != record.restart_requested_at
                })
                && !inner.pending_restarts.contains(&slug)
            {
                inner.pending_restarts.push(slug.clone());
            }
            let reason = match &previous {
                None if initial => None,
                None => Some("registered"),
                Some(previous) if *previous != record => Some("updated"),
                Some(_) => None,
            };
            if previous.is_some_and(|previous| previous.runtime != record.runtime) {
                // A runtime switch forces fresh health (e.g. external -> managed).
                inner.last_health = None;
            }
            if record.runtime == ServiceRuntime::Managed && record.stopped {
                inner.entries.get_mut(&slug).unwrap().next_restart = None;
            }
            if let Some(reason) = reason {
                inner.state_dirty = true;
                inner.last_health = None;
                let entry = &inner.entries[&slug];
                events.push(ServiceEvent {
                    reason,
                    slug: Some(slug.clone()),
                    service: Some(service_view(&record, Some(&entry.snapshot))),
                });
            }
        }
    }

    fn reconcile_locked(self: &Arc<Self>, inner: &mut Inner, events: &mut Vec<ServiceEvent>) {
        let slugs: Vec<String> = inner.records.keys().cloned().collect();
        for slug in slugs {
            let record = inner.records[&slug].clone();
            let generation = inner.generation + 1;
            let entry = inner.entries.get_mut(&slug).expect("entry per record");
            let fingerprint = record.launch_fingerprint();
            let want = wants_running(&record) && !entry.crash_looped;
            if let Some(running) = &entry.running {
                let stale = fingerprint != entry.fingerprint;
                if !want || stale {
                    entry.logs.note(if stale {
                        "configuration changed; restarting"
                    } else {
                        "stopping"
                    });
                    running.tree.kill();
                    entry.running = None;
                    entry.snapshot.pid = None;
                    entry.snapshot.started_at = None;
                    entry.next_restart = None;
                    entry.backoff = BACKOFF_MIN;
                    entry.snapshot.health = if want {
                        ServiceHealth::Starting
                    } else {
                        ServiceHealth::Stopped
                    };
                    entry.logs.publish_state(json!({"event": "stopped"}));
                    inner.state_dirty = true;
                    events.push(ServiceEvent {
                        reason: "stopped",
                        slug: Some(slug.clone()),
                        service: Some(service_view(&record, Some(&entry.snapshot))),
                    });
                }
            }
            entry.fingerprint = fingerprint;
            if !want && entry.running.is_none() && record.runtime.supervised() {
                entry.next_restart = None;
                entry.snapshot.next_restart_at = None;
                if entry.snapshot.health != ServiceHealth::Stopped && !entry.crash_looped {
                    entry.snapshot.health = ServiceHealth::Stopped;
                    inner.state_dirty = true;
                    events.push(ServiceEvent {
                        reason: "health",
                        slug: Some(slug.clone()),
                        service: Some(service_view(&record, Some(&entry.snapshot))),
                    });
                }
            }
            if want
                && entry.running.is_none()
                && entry.next_restart.is_none_or(|at| Instant::now() >= at)
            {
                inner.generation = generation;
                self.spawn_locked(&record, entry, generation, events);
                inner.state_dirty = true;
                inner.last_health = None;
            }
        }
    }

    fn spawn_locked(
        self: &Arc<Self>,
        record: &ServiceRecord,
        entry: &mut Entry,
        generation: u64,
        events: &mut Vec<ServiceEvent>,
    ) {
        if entry.logs.state.lock().expect("log ring").file.is_none() {
            entry.logs.open_file(self.store.log_path(&record.slug));
        }
        let spawned = super::process::build_command(record, &self.default_cwd)
            .and_then(super::process::spawn);
        let (mut child, tree) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                entry.logs.note(&format!("failed to start: {error:#}"));
                self.schedule_restart(record, entry);
                if !entry.crash_looped {
                    entry.snapshot.health = ServiceHealth::Unreachable;
                }
                events.push(ServiceEvent {
                    reason: "exited",
                    slug: Some(record.slug.clone()),
                    service: Some(service_view(record, Some(&entry.snapshot))),
                });
                return;
            }
        };
        let tree = Arc::new(tree);
        entry.logs.note(&format!(
            "started pid {} on port {}",
            tree.pid(),
            record.port
        ));
        entry
            .logs
            .publish_state(json!({"event": "started", "pid": tree.pid()}));
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, entry.logs.clone(), &record.slug);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, entry.logs.clone(), &record.slug);
        }
        entry.running = Some(Running {
            tree: tree.clone(),
            generation,
            spawned_at: Instant::now(),
            reached: false,
        });
        entry.next_restart = None;
        entry.snapshot.pid = Some(tree.pid());
        entry.snapshot.started_at = Some(now_rfc3339());
        entry.snapshot.next_restart_at = None;
        entry.snapshot.health = ServiceHealth::Starting;
        events.push(ServiceEvent {
            reason: "started",
            slug: Some(record.slug.clone()),
            service: Some(service_view(record, Some(&entry.snapshot))),
        });
        let supervisor = self.clone();
        let slug = record.slug.clone();
        let _ = std::thread::Builder::new()
            .name(format!("cutex-service-wait-{slug}"))
            .spawn(move || {
                let code = child.wait().ok().and_then(|status| status.code());
                tree.mark_exited();
                // Children of an exited root die with it.
                tree.kill();
                supervisor.on_exit(&slug, generation, code);
            });
    }

    fn schedule_restart(&self, record: &ServiceRecord, entry: &mut Entry) {
        if record.runtime != ServiceRuntime::Managed || record.stopped {
            entry.next_restart = None;
            entry.snapshot.next_restart_at = None;
            return;
        }
        if entry
            .max_restarts
            .is_some_and(|max| entry.snapshot.restarts >= max)
        {
            entry.crash_looped = true;
            entry.snapshot.health = ServiceHealth::CrashLooped;
            entry.next_restart = None;
            entry.snapshot.next_restart_at = None;
            entry
                .logs
                .note("restart budget exhausted; owner start/open is required");
            return;
        }
        let delay = entry.backoff;
        entry.backoff = (entry.backoff * 2).min(BACKOFF_MAX);
        entry.next_restart = Some(Instant::now() + delay);
        entry.snapshot.restarts += 1;
        entry.snapshot.next_restart_at = Some(
            (chrono::Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default())
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        entry
            .logs
            .note(&format!("restarting in {} s", delay.as_secs()));
    }

    fn on_exit(&self, slug: &str, generation: u64, code: Option<i32>) {
        let mut events = Vec::new();
        {
            let mut guard = self.inner.lock().expect("services supervisor");
            let inner = &mut *guard;
            let Some(record) = inner.records.get(slug).cloned() else {
                return;
            };
            let Some(entry) = inner.entries.get_mut(slug) else {
                return;
            };
            let Some(running) = entry
                .running
                .as_ref()
                .filter(|running| running.generation == generation)
            else {
                return;
            };
            let uptime = running.spawned_at.elapsed();
            entry.running = None;
            entry.snapshot.pid = None;
            entry.snapshot.last_exit_code = code;
            entry.snapshot.last_exit_at = Some(now_rfc3339());
            entry.logs.note(&format!(
                "exited with code {}",
                code.map_or_else(|| "unknown".to_string(), |code| code.to_string())
            ));
            entry
                .logs
                .publish_state(json!({"event": "exited", "exitCode": code}));
            if uptime >= BACKOFF_RESET_UPTIME {
                entry.backoff = BACKOFF_MIN;
            }
            self.schedule_restart(&record, entry);
            if !entry.crash_looped {
                entry.snapshot.health =
                    compute_health(record.runtime, entry.process_facts(&record), false);
            }
            inner.state_dirty = true;
            let entry = &inner.entries[slug];
            events.push(ServiceEvent {
                reason: "exited",
                slug: Some(slug.to_string()),
                service: Some(service_view(&record, Some(&entry.snapshot))),
            });
        }
        self.emit(events);
        self.kick();
    }

    /// Probe every service that can be up (bounded concurrency, outside the lock).
    pub fn probe_all(&self) {
        let targets: Vec<(String, u16, Option<String>, Option<String>)> = {
            let mut inner = self.inner.lock().expect("services supervisor");
            inner.last_health = Some(Instant::now());
            inner
                .records
                .values()
                .filter(|record| {
                    !record.runtime.supervised()
                        || inner
                            .entries
                            .get(&record.slug)
                            .is_some_and(|entry| entry.running.is_some())
                })
                .map(|record| {
                    (
                        record.slug.clone(),
                        record.port,
                        record.path.clone(),
                        if record.runtime == ServiceRuntime::Static {
                            record.root.clone()
                        } else {
                            None
                        },
                    )
                })
                .collect()
        };
        let mut results: HashMap<String, bool> = HashMap::new();
        for chunk in targets.chunks(PROBE_CONCURRENCY) {
            std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|(slug, port, path, root)| {
                        scope.spawn(move || {
                            (
                                slug.clone(),
                                root.as_deref().map_or_else(
                                    || probe(*port, path.as_deref()),
                                    |p| std::path::Path::new(p).exists(),
                                ),
                            )
                        })
                    })
                    .collect();
                for handle in handles {
                    if let Ok((slug, ok)) = handle.join() {
                        results.insert(slug, ok);
                    }
                }
            });
        }
        let mut events = Vec::new();
        {
            let mut guard = self.inner.lock().expect("services supervisor");
            let inner = &mut *guard;
            let now = now_rfc3339();
            let slugs: Vec<String> = inner.records.keys().cloned().collect();
            for slug in slugs {
                let record = inner.records[&slug].clone();
                let Some(entry) = inner.entries.get_mut(&slug) else {
                    continue;
                };
                let probe_ok = results.get(&slug).copied().unwrap_or(false);
                if results.contains_key(&slug) {
                    entry.snapshot.last_probe_at = Some(now.clone());
                }
                if probe_ok {
                    entry.snapshot.last_seen_at = Some(now.clone());
                    if let Some(running) = entry.running.as_mut() {
                        running.reached = true;
                    }
                    if entry
                        .last_seen_persisted
                        .is_none_or(|at| at.elapsed() >= LAST_SEEN_PERSIST_INTERVAL)
                    {
                        entry.last_seen_persisted = Some(Instant::now());
                        inner.state_dirty = true;
                    }
                }
                let entry = inner.entries.get_mut(&slug).unwrap();
                entry.last_probe_ok = probe_ok;
                let health = if entry.crash_looped {
                    ServiceHealth::CrashLooped
                } else {
                    compute_health(record.runtime, entry.process_facts(&record), probe_ok)
                };
                if health != entry.snapshot.health {
                    entry.snapshot.health = health;
                    inner.state_dirty = true;
                    let entry = &inner.entries[&slug];
                    events.push(ServiceEvent {
                        reason: "health",
                        slug: Some(slug.clone()),
                        service: Some(service_view(&record, Some(&entry.snapshot))),
                    });
                }
            }
        }
        self.emit(events);
    }

    fn persist_locked(&self, inner: &mut Inner) {
        for (slug, entry) in &mut inner.entries {
            if let Some(at) = super::traffic::take_persisted_timestamp(slug) {
                if entry.snapshot.last_traffic_at.as_ref() != Some(&at) {
                    entry.snapshot.last_traffic_at = Some(at);
                    inner.state_dirty = true;
                }
            }
        }
        if !inner.state_dirty {
            return;
        }
        let state: BTreeMap<String, RuntimeSnapshot> = inner
            .entries
            .iter()
            .map(|(slug, entry)| (slug.clone(), entry.snapshot.clone()))
            .collect();
        match self.store.save_state(&state) {
            Ok(()) => inner.state_dirty = false,
            Err(error) => eprintln!("services: could not save state: {error:#}"),
        }
    }

    /// Current runtime facts (fresher than `state.json`).
    pub fn snapshots(&self) -> BTreeMap<String, RuntimeSnapshot> {
        let inner = self.inner.lock().expect("services supervisor");
        inner
            .entries
            .iter()
            .map(|(slug, entry)| (slug.clone(), entry.snapshot.clone()))
            .collect()
    }

    /// Pull a registry change made through this process in right away.
    pub fn sync(self: &Arc<Self>) {
        let mut events = Vec::new();
        {
            let mut inner = self.inner.lock().expect("services supervisor");
            self.reload_locked(&mut inner, &mut events, false);
            self.reconcile_locked(&mut inner, &mut events);
            self.persist_locked(&mut inner);
        }
        self.emit(events);
        self.run_pending_restarts();
        self.kick();
    }

    pub fn view(&self, slug: &str) -> ServicesResult<Value> {
        let inner = self.inner.lock().expect("services supervisor");
        let record = inner
            .records
            .get(slug)
            .ok_or_else(|| ServicesError::not_found(slug))?;
        let mut view = service_view(record, inner.entries.get(slug).map(|entry| &entry.snapshot));
        let traffic = super::traffic::snapshot_for(record);
        self.lifecycle.augment_view(
            record,
            &inner.settings,
            traffic.last_traffic_at.as_deref(),
            traffic.active_connections,
            &mut view,
        );
        Ok(view)
    }

    fn reclaim(&self) {
        let now = chrono::Utc::now();
        // The registry lock makes the predicate and delete atomic against lifetime upgrades.
        // Lock order is supervisor -> registry, like reconciliation; leases never take either lock.
        let mut inner = self.inner.lock().expect("services supervisor");
        let result = self.store.mutate(|registry| {
            // Only the owners' archived state is needed. Read it from the shared
            // session-store snapshot (no whole-store copy), and only when some
            // service has an owner: an empty or ownerless registry does no
            // session-store work at all. The snapshot's own locks never take the
            // supervisor or registry lock, so reading it here keeps the order.
            let archived_owners = owners_archived(registry.services.values());
            let archived_projects = projects_archived(registry.services.values());
            let victims: Vec<_> = registry
                .services
                .values()
                .filter_map(|record| {
                    let traffic = super::traffic::snapshot_for(record);
                    let archived = if record.owner_scope.is_session() {
                        record.owner_cutex_session_id.as_ref()
                            .is_some_and(|owner| archived_owners.contains(owner.as_str()))
                    } else {
                        record.project_id.as_ref()
                            .is_some_and(|project| archived_projects.contains(project.as_str()))
                    };
                    self.lifecycle
                        .reason(
                            record,
                            &registry.settings,
                            traffic.last_traffic_at.as_deref(),
                            traffic.active_connections,
                            archived,
                            now,
                        )
                        .map(|reason| (record.clone(), reason))
                })
                .collect();
            for (record, reason) in &victims {
                if let Some(entry) = inner.entries.get_mut(&record.slug) {
                    if let Some(running) = entry.running.take() {
                        running.tree.kill();
                    }
                }
                registry.services.remove(&record.slug);
                registry.tombstones.insert(
                    record.slug.clone(),
                    super::lifecycle::tombstone(&record, reason, now),
                );
            }
            let cutoff = now - chrono::Duration::days(registry.settings.tombstone_days.into());
            registry.tombstones.retain(|_, tomb| {
                super::lifecycle::parse_time(&tomb.reclaimed_at).is_some_and(|at| at > cutoff)
            });
            Ok(victims)
        });
        let mut events = Vec::new();
        match result {
            Ok(victims) => {
                for (record, _) in victims {
                    self.lifecycle.forget(&record.slug);
                    inner.records.remove(&record.slug);
                    inner.entries.remove(&record.slug);
                    super::traffic::forget(&record.slug);
                    inner.state_dirty = true;
                    events.push(ServiceEvent {
                        reason: "reclaimed",
                        slug: Some(record.slug),
                        service: None,
                    });
                }
            }
            Err(error) => eprintln!("services: reclamation failed: {error}"),
        }
        drop(inner);
        self.emit(events);
    }

    fn require_supervised(&self, slug: &str) -> ServicesResult<ServiceRecord> {
        let record = self.store.get(slug)?;
        if record.archived {
            return Err(ServicesError::new(
                409,
                "service_archived",
                format!("service `{slug}` is archived; restore it before starting it"),
            ));
        }
        if !record.runtime.supervised() {
            return Err(ServicesError::new(
                409,
                "not_supervised",
                format!(
                    "`{slug}` is a {} service; cutex does not run a process for it (delete it to remove it)",
                    record.runtime.label()
                ),
            ));
        }
        Ok(record)
    }

    /// `POST /v2/services/{slug}/start`.
    pub fn start_service(self: &Arc<Self>, slug: &str) -> ServicesResult<Value> {
        self.require_supervised(slug)?;
        self.store.set_stopped(slug, false)?;
        {
            let mut inner = self.inner.lock().expect("services supervisor");
            let mut events = Vec::new();
            self.reload_locked(&mut inner, &mut events, false);
            if let Some(entry) = inner.entries.get_mut(slug) {
                entry.next_restart = None;
                entry.backoff = BACKOFF_MIN;
                entry.crash_looped = false;
                entry.snapshot.restarts = 0;
                entry.snapshot.health = ServiceHealth::Stopped;
            }
            self.reconcile_locked(&mut inner, &mut events);
            self.persist_locked(&mut inner);
            drop(inner);
            self.emit(events);
        }
        self.view(slug)
    }

    /// `POST /v2/services/{slug}/stop`.
    pub fn stop_service(self: &Arc<Self>, slug: &str) -> ServicesResult<Value> {
        self.require_supervised(slug)?;
        self.store.set_stopped(slug, true)?;
        {
            let mut inner = self.inner.lock().expect("services supervisor");
            let mut events = Vec::new();
            self.reload_locked(&mut inner, &mut events, false);
            if let Some(entry) = inner.entries.get_mut(slug) {
                entry.next_restart = None;
            }
            self.reconcile_locked(&mut inner, &mut events);
            self.persist_locked(&mut inner);
            drop(inner);
            self.emit(events);
        }
        self.view(slug)
    }

    /// `POST /v2/services/{slug}/restart`: kill (if running) and start again.
    pub fn restart_service(self: &Arc<Self>, slug: &str) -> ServicesResult<Value> {
        let record = self.require_supervised(slug)?;
        let killed = {
            let mut inner = self.inner.lock().expect("services supervisor");
            inner.entries.get_mut(slug).is_some_and(|entry| {
                entry.running.take().is_some_and(|running| {
                    entry.logs.note("restart requested");
                    running.tree.kill();
                    entry.snapshot.pid = None;
                    true
                })
            })
        };
        if killed {
            // Let the old tree release the port before the new one binds it.
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && probe(record.port, None) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.start_service(slug)
    }

    /// `POST /v2/services/{slug}/open`: start a stopped (or not yet running)
    /// managed service, clearing its `stopped` flag, wait up to 15 s for it to
    /// answer, and return its URLs. External services are returned as is.
    /// The login-link path of a running managed service with a `loginLinkCommand`
    /// (`process::login_link_path`); `None` when it is not running.
    pub fn login_link(
        &self,
        record: &crate::services::store::ServiceRecord,
    ) -> Option<Result<String, super::process::LoginLinkError>> {
        let running = {
            let inner = self.inner.lock().expect("services supervisor");
            inner
                .entries
                .get(&record.slug)
                .is_some_and(|entry| entry.running.is_some())
        };
        running
            .then(|| super::process::login_link_path(record, &self.default_cwd, LOGIN_LINK_TIMEOUT))
    }

    pub fn open_service(self: &Arc<Self>, slug: &str) -> ServicesResult<Value> {
        let record = self.store.get(slug)?;
        if record.archived {
            return Err(ServicesError::new(
                409,
                "service_archived",
                format!("service `{slug}` is archived; restore it before opening it"),
            ));
        }
        if record.runtime == ServiceRuntime::Managed {
            let running = {
                let inner = self.inner.lock().expect("services supervisor");
                inner
                    .entries
                    .get(slug)
                    .is_some_and(|entry| entry.running.is_some())
            };
            if record.stopped || !running {
                self.start_service(slug)?;
            }
            let deadline = Instant::now() + OPEN_READY_TIMEOUT;
            loop {
                let ready = probe(record.port, record.path.as_deref());
                if ready || Instant::now() >= deadline {
                    if ready {
                        self.probe_all();
                    }
                    break;
                }
                let alive = {
                    let inner = self.inner.lock().expect("services supervisor");
                    inner
                        .entries
                        .get(slug)
                        .is_some_and(|entry| entry.running.is_some())
                };
                if !alive {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        let view = self.view(slug)?;
        Ok(json!({
            "slug": slug,
            "url": view["url"],
            "localUrl": view["localUrl"],
            "health": view["health"],
            "service": view,
        }))
    }

    fn ring(&self, slug: &str) -> ServicesResult<Arc<LogRing>> {
        let inner = self.inner.lock().expect("services supervisor");
        inner
            .entries
            .get(slug)
            .map(|entry| entry.logs.clone())
            .ok_or_else(|| ServicesError::not_found(slug))
    }

    /// `GET /v2/services/{slug}/logs?tail=`: memory ring, else the log file.
    pub fn logs(&self, slug: &str, tail: usize) -> ServicesResult<Value> {
        let ring = self.ring(slug)?;
        let (mut text, offset) = ring.tail(tail);
        let mut source = "memory";
        if offset == 0 {
            text = read_log_file_tail(&self.store, slug, tail);
            source = "file";
        }
        Ok(json!({"slug": slug, "text": text, "offset": offset, "source": source}))
    }

    /// `GET /v2/services/{slug}/logs/stream?tail=` (SSE):
    /// `ready {slug, offset}`, `log {text, replay?}` (id = end offset),
    /// `state {event: started|exited|stopped, pid?, exitCode?}`.
    pub fn stream_logs(
        &self,
        stream: &mut TcpStream,
        slug: &str,
        tail: usize,
    ) -> anyhow::Result<()> {
        let ring = match self.ring(slug) {
            Ok(ring) => ring,
            Err(error) => return write_error(stream, &error),
        };
        let (receiver, replay, offset) = ring.subscribe(tail);
        let replay = if offset == 0 {
            read_log_file_tail(&self.store, slug, tail)
        } else {
            replay
        };
        stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
        let _ = (|| -> anyhow::Result<()> {
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n",
            )?;
            write_sse(
                stream,
                "ready",
                None,
                &json!({"slug": slug, "offset": offset}),
            )?;
            if !replay.is_empty() {
                write_sse(
                    stream,
                    "log",
                    Some(offset),
                    &json!({"text": replay, "replay": true}),
                )?;
            }
            let mut last_write = Instant::now();
            let mut watch = crate::management::listeners::RevocationWatch::for_stream(stream)?;
            loop {
                if watch.disconnected()? { return Ok(()); }
                match receiver.recv_timeout(crate::management::listeners::STREAM_TICK) {
                    Ok(LogEvent::Output(end, text)) => {
                        write_sse(stream, "log", Some(end), &json!({"text": text.as_str()}))?;
                        last_write = Instant::now();
                    }
                    Ok(LogEvent::State(value)) => {
                        write_sse(stream, "state", None, &value)?;
                        last_write = Instant::now();
                    },
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if last_write.elapsed() >= SSE_KEEPALIVE {
                            stream.write_all(b": keepalive\n\n")?;
                            stream.flush()?;
                            last_write = Instant::now();
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
        })();
        let _ = stream.shutdown(std::net::Shutdown::Both);
        Ok(())
    }
}

fn write_sse(
    stream: &mut TcpStream,
    event: &str,
    id: Option<u64>,
    data: &Value,
) -> anyhow::Result<()> {
    if let Some(id) = id {
        write!(stream, "id: {id}\n")?;
    }
    write!(
        stream,
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data)?
    )?;
    stream.flush()?;
    Ok(())
}

pub(crate) fn write_error(stream: &mut TcpStream, error: &ServicesError) -> anyhow::Result<()> {
    let reason = match error.status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Service Unavailable",
    };
    crate::http::server::write_json_response(
        stream,
        error.status,
        reason,
        &json!({"error": {"code": error.code, "message": error.message, "retryable": error.status == 503}}),
    )
}

fn read_log_file_tail(store: &ServiceStore, slug: &str, lines: usize) -> String {
    let path = store.log_path(slug);
    let Ok(mut file) = File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_RING_BYTES as u64);
    use std::io::Seek;
    if file.seek(std::io::SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    tail_lines(&String::from_utf8_lossy(&bytes), lines)
}

fn spawn_reader(mut reader: impl Read + Send + 'static, ring: Arc<LogRing>, slug: &str) {
    let _ = std::thread::Builder::new()
        .name(format!("cutex-service-log-{slug}"))
        .spawn(move || {
            let mut buffer = vec![0_u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => ring.push(&buffer[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
}

/// The owners (of `records`) whose Agent is gone or archived. Reads the shared
/// session-store snapshot only when some record has an owner; an unreadable
/// store counts as "not archived" (nothing is reclaimed for it this tick).
fn owners_archived<'a>(
    records: impl Iterator<Item = &'a ServiceRecord>,
) -> std::collections::BTreeSet<String> {
    // Project services are not reclaimed with their registrant.
    let owners: std::collections::BTreeSet<&str> = records
        .filter(|record| record.owner_scope.is_session())
        .filter_map(|record| record.owner_cutex_session_id.as_deref())
        .collect();
    if owners.is_empty() {
        return Default::default();
    }
    crate::session::store::with_cutex_session_store(|store| {
        owners
            .iter()
            .filter(|owner| store.sessions.get(**owner).is_none_or(|r| r.is_retired()))
            .map(|owner| owner.to_string())
            .collect()
    })
    .unwrap_or_default()
}

/// Project services follow project archive/removal, never registrant archive.
/// Missing legacy project-state records and unreadable snapshots fail closed:
/// neither is positive evidence that the project was archived.
fn projects_archived<'a>(
    records: impl Iterator<Item = &'a ServiceRecord>,
) -> std::collections::BTreeSet<String> {
    let projects: std::collections::BTreeSet<&str> = records
        .filter(|record| !record.owner_scope.is_session())
        .filter_map(|record| record.project_id.as_deref())
        .collect();
    if projects.is_empty() {
        return Default::default();
    }
    crate::memory::load_agent_management_snapshot()
        .map(|snapshot| {
            snapshot.project_states.values()
                .filter(|state| projects.contains(state.project_id.as_str()))
                .filter(|state| state.lifecycle != crate::agent_management::ProjectLifecycle::Active)
                .map(|state| state.project_id.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Unix: kill surviving process groups only when the recorded process matches.
/// Windows job objects already took them down when the old daemon exited.
fn reap_previous_processes(store: &ServiceStore) {
    #[cfg(unix)]
    for snapshot in store.load_state().values() {
        let (Some(pid), Some(started_at)) = (snapshot.pid, snapshot.started_at.as_deref()) else {
            continue;
        };
        let Ok(recorded) = chrono::DateTime::parse_from_rfc3339(started_at) else {
            continue;
        };
        let Ok(actual) = crate::platform::process::process_started_at(pid) else {
            continue;
        };
        if (actual - recorded.with_timezone(&chrono::Utc))
            .num_seconds()
            .abs()
            <= 5
        {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = store;
}

static GLOBAL: OnceLock<Arc<Supervisor>> = OnceLock::new();

/// The daemon's supervisor, once [`start_global`] ran.
pub fn global() -> Option<Arc<Supervisor>> {
    GLOBAL.get().cloned()
}

/// Start the daemon's supervisor on the default store (idempotent).
pub fn start_global(sink: EventSink) -> anyhow::Result<Arc<Supervisor>> {
    if let Some(existing) = GLOBAL.get() {
        return Ok(existing.clone());
    }
    let store = ServiceStore::open_default()?;
    std::fs::create_dir_all(store.dir())?;
    #[cfg(windows)]
    if let Err(error) = crate::platform::private_fs::secure_directory(store.dir()) {
        eprintln!(
            "services: could not secure {}: {error:?}",
            store.dir().display()
        );
    }
    let supervisor = Supervisor::new(store, sink);
    if GLOBAL.set(supervisor.clone()).is_err() {
        return Ok(GLOBAL.get().expect("set").clone());
    }
    supervisor.start()?;
    Ok(supervisor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::store::Caller;
    use crate::services::store::RegisterRequest;
    use crate::services::store::ServiceCommand;
    use std::net::TcpListener;

    fn temp_store() -> ServiceStore {
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-sup-{}",
            uuid::Uuid::new_v4().simple()
        ));
        ServiceStore::new(dir, "testhost")
    }

    /// `Supervisor::new` captures `home_dir()` as the default process cwd. A
    /// parallel test's `TestHome` points `HOME` at a temp dir it deletes on
    /// drop, so a supervisor built inside that window spawned its services in
    /// a missing cwd (`failed to start: No such file or directory`). Build it
    /// under the same `HOME` lock.
    fn recording_supervisor(
        store: ServiceStore,
    ) -> (Arc<Supervisor>, Arc<Mutex<Vec<ServiceEvent>>>) {
        let _home_lock = crate::config::paths::test_home_env_lock();
        recording_supervisor_unlocked(store)
    }

    /// For tests that already hold the `HOME` lock through `TestHome`.
    fn recording_supervisor_unlocked(
        store: ServiceStore,
    ) -> (Arc<Supervisor>, Arc<Mutex<Vec<ServiceEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let supervisor = Supervisor::new(
            store,
            Box::new(move |event| sink_events.lock().unwrap().push(event)),
        );
        (supervisor, events)
    }

    /// Stop accepting on `listener` for good. A plain `drop` is not enough
    /// while parallel tests spawn processes: a child between fork and exec
    /// holds a copy of every fd (CLOEXEC closes it only at exec), so the
    /// socket kept listening and a probe right after `drop` still connected.
    /// `shutdown` acts on the socket itself, i.e. on every copy.
    fn stop_listening(listener: TcpListener) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: a valid, owned socket fd; dropped right below.
            unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) };
        }
        // Windows: std sockets are not inheritable, so the drop closes it.
        drop(listener);
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// PERF-01: the per-second pass copies no session store, and with no
    /// owned service it does not read the store at all; owner archival still
    /// reclaims (archived and missing owners), live owners keep theirs.
    #[test]
    fn reclaim_reads_only_owner_state_without_copying_the_session_store() {
        use crate::session::model::{CutexSessionArchiveState, CutexSessionRecord, CutexSessionStore};
        use crate::session::store::whole_store_copies_on_this_thread;
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let record = |id: &str, retired: bool| {
            let mut record = CutexSessionRecord::new_at(
                id.into(),
                None,
                crate::platform::host::current_host_name(),
                "/tmp".into(),
                None,
                "2026-10-15T00:00:00Z".into(),
            )
            .unwrap();
            if retired {
                record.archive_state = CutexSessionArchiveState::Retired;
            }
            record
        };
        let mut sessions = CutexSessionStore::default();
        for (id, retired) in [("cutex.live", false), ("cutex.archived", true)] {
            sessions.sessions.insert(id.into(), record(id, retired));
        }
        crate::session::store::save_cutex_session_store(&sessions).unwrap();

        // Empty registry: no session-store copy (the old pass copied it every tick).
        let empty = temp_store();
        let (supervisor, _) = recording_supervisor_unlocked(empty);
        let before = whole_store_copies_on_this_thread();
        for _ in 0..5 {
            supervisor.tick();
        }
        assert_eq!(whole_store_copies_on_this_thread(), before);

        let store = temp_store();
        let listeners: Vec<_> = (0..3).map(|_| TcpListener::bind(("127.0.0.1", 0)).unwrap()).collect();
        for ((slug, owner), listener) in [
            ("live-owner", "cutex.live"),
            ("archived-owner", "cutex.archived"),
            ("missing-owner", "cutex.missing"),
        ]
        .into_iter()
        .zip(&listeners)
        {
            let port = listener.local_addr().unwrap().port();
            store
                .register(
                    RegisterRequest {
                        slug: slug.into(),
                        port: Some(port),
                        lifetime: Some("ephemeral".into()),
                        owner_cutex_session_id: Some(owner.into()),
                        ..Default::default()
                    },
                    &Caller::Owner,
                )
                .unwrap();
        }
        let (supervisor, _) = recording_supervisor_unlocked(store.clone());
        let before = whole_store_copies_on_this_thread();
        supervisor.tick();
        assert_eq!(whole_store_copies_on_this_thread(), before);
        assert!(store.get("live-owner").is_ok());
        for slug in ["archived-owner", "missing-owner"] {
            assert_eq!(
                store.get_tombstone(slug).unwrap().unwrap().reason,
                "owner_archived",
                "{slug}"
            );
        }
    }

    #[test]
    fn project_reclaim_follows_project_lifecycle_not_registrant() {
        use crate::agent_management::{AgentManagementStore, ProjectId, ProjectStateRecord};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let projects = AgentManagementStore::open_default().unwrap();
        projects.with_state(true, |mut snapshot| {
            for (project, lifecycle) in [("active", "active"), ("archived", "archived"), ("removed", "removed")] {
                let state: ProjectStateRecord = serde_json::from_value(json!({
                    "project_id": project, "lifecycle": lifecycle, "revision": 1,
                    "created_at": "2026-10-28T00:00:00Z", "updated_at": "2026-10-28T00:00:00Z"
                })).unwrap();
                snapshot.project_states.insert(ProjectId::new(project).unwrap(), state);
            }
            Ok((snapshot, (), true))
        }).unwrap();
        let store = temp_store();
        store.mutate(|registry| {
            for project in ["active", "archived", "removed", "legacy"] {
                let record: ServiceRecord = serde_json::from_value(json!({
                    "slug": project, "displayName": project, "host": "testhost", "port": 1,
                    "runtime": "external", "lifetime": "ephemeral", "pinned": true,
                    "ownerScope": "project", "projectId": project,
                    "ownerCutexSessionId": "cutex.missing",
                    "createdAt": chrono::Utc::now().to_rfc3339(),
                    "updatedAt": chrono::Utc::now().to_rfc3339()
                })).unwrap();
                registry.services.insert(project.into(), record);
            }
            Ok(())
        }).unwrap();
        let (supervisor, _) = recording_supervisor_unlocked(store.clone());
        supervisor.reclaim();
        for project in ["active", "legacy"] {
            assert!(store.get(project).is_ok(), "missing registrant is not the project's archive");
        }
        for project in ["archived", "removed"] {
            let tombstone = store.get_tombstone(project).unwrap().unwrap();
            assert_eq!(tombstone.reason, "project_archived");
            crate::management::v2::schema_export::assert_valid(
                "services", "tombstone", &serde_json::to_value(&tombstone).unwrap(),
            );
        }
    }

    #[test]
    fn tail_lines_takes_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(tail_lines("a\nb\n", 10), "a\nb\n");
        assert_eq!(tail_lines("abc", 1), "abc");
        assert_eq!(tail_lines("a\nb", 0), "");
    }

    #[test]
    fn ephemeral_ttl_reclaims_external_without_killing_listener_and_expires_tombstones() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let store = temp_store();
        store
            .register(
                RegisterRequest {
                    slug: "external".into(),
                    port: Some(port),
                    lifetime: Some("ephemeral".into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        store
            .mutate(|registry| {
                registry.services.get_mut("external").unwrap().created_at =
                    (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
                Ok(())
            })
            .unwrap();
        let (supervisor, events) = recording_supervisor(store.clone());
        supervisor.tick();
        assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
        assert_eq!(store.get("external").unwrap_err().status, 410);
        assert_eq!(
            store.get_tombstone("external").unwrap().unwrap().reason,
            "ttl"
        );
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.reason == "reclaimed"));
        store
            .mutate(|registry| {
                registry
                    .tombstones
                    .get_mut("external")
                    .unwrap()
                    .reclaimed_at = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
                Ok(())
            })
            .unwrap();
        supervisor.tick();
        assert_eq!(store.get("external").unwrap_err().status, 404);
        std::fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn ephemeral_restart_budget_and_owner_start_reset() {
        let store = temp_store();
        let (record, _) = store
            .register(
                RegisterRequest {
                    slug: "crash".into(),
                    runtime: Some("managed".into()),
                    command: Some(ServiceCommand::Argv(vec![
                        "cutex-nonexistent-task10-program".into(),
                    ])),
                    lifetime: Some("ephemeral".into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        store
            .update_settings(super::super::store::SettingsPatch {
                ephemeral_max_restarts: Some(0),
                ..Default::default()
            })
            .unwrap();
        let (supervisor, _) = recording_supervisor(store.clone());
        supervisor.tick();
        assert_eq!(
            supervisor.snapshots()["crash"].health,
            ServiceHealth::CrashLooped
        );
        assert_eq!(supervisor.snapshots()["crash"].restarts, 0);
        // Explicit start resets the exhausted state and tries the command once.
        let view = supervisor.start_service("crash").unwrap();
        assert_eq!(view["health"], "crashLooped");
        let mut entry = Entry::new(&record, None);
        entry.max_restarts = Some(2);
        supervisor.schedule_restart(&record, &mut entry);
        supervisor.schedule_restart(&record, &mut entry);
        assert_eq!(entry.snapshot.restarts, 2);
        assert!(!entry.crash_looped);
        supervisor.schedule_restart(&record, &mut entry);
        assert!(entry.crash_looped);
        assert!(entry.next_restart.is_none());
        std::fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn log_ring_is_bounded() {
        let ring = LogRing::default();
        let chunk = vec![b'x'; 100 * 1024];
        for _ in 0..5 {
            ring.push(&chunk);
        }
        let state = ring.state.lock().unwrap();
        assert_eq!(state.bytes.len(), LOG_RING_BYTES);
        assert_eq!(state.end_offset, 500 * 1024);
    }

    #[test]
    fn external_service_health_flips_and_emits_events() {
        let store = temp_store();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        store
            .register(
                RegisterRequest {
                    slug: "ext".into(),
                    port: Some(port),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let (supervisor, events) = recording_supervisor(store.clone());
        {
            let mut inner = supervisor.inner.lock().unwrap();
            let mut pending = Vec::new();
            supervisor.reload_locked(&mut inner, &mut pending, true);
        }
        supervisor.probe_all();
        assert_eq!(
            supervisor.snapshots()["ext"].health,
            ServiceHealth::Reachable
        );
        assert!(supervisor.snapshots()["ext"].last_seen_at.is_some());
        stop_listening(listener);
        supervisor.probe_all();
        assert_eq!(
            supervisor.snapshots()["ext"].health,
            ServiceHealth::Unreachable
        );
        let reasons: Vec<_> = events.lock().unwrap().iter().map(|e| e.reason).collect();
        assert_eq!(reasons, vec!["health", "health"]);
        // A registration from another process (Agent Bus) is picked up.
        store
            .register(
                RegisterRequest {
                    slug: "second".into(),
                    port: Some(port.wrapping_add(1).max(1)),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        supervisor.sync();
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.reason == "registered"));
        store.remove("second", &Caller::Owner).unwrap();
        supervisor.sync();
        assert!(events.lock().unwrap().iter().any(|e| e.reason == "removed"));
        supervisor.tick();
        assert!(store.load_state().contains_key("ext"));
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[cfg(unix)]
    #[test]
    fn login_link_runs_only_while_the_service_runs() {
        let store = temp_store();
        let (record, _) = store
            .register(
                RegisterRequest {
                    slug: "login".into(),
                    runtime: Some("managed".into()),
                    command: Some(ServiceCommand::Shell("sleep 30".into())),
                    login_link_command: Some(ServiceCommand::Shell(
                        "echo \"http://localhost:$PORT/?nonce=n1\"".into(),
                    )),
                    port: Some(45991),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let (supervisor, _) = recording_supervisor(store.clone());
        assert!(supervisor.login_link(&record).is_none(), "not running yet");
        supervisor.start_service("login").unwrap();
        assert_eq!(
            supervisor.login_link(&record).unwrap().as_deref(),
            Ok("/?nonce=n1")
        );
        supervisor.stop_service("login").unwrap();
        assert!(supervisor.login_link(&record).is_none());
        let _ = std::fs::remove_dir_all(store.dir());
    }

    fn server_command() -> ServiceCommand {
        // A tiny HTTP server that prints a line; node is not guaranteed on CI
        // boxes, so the test skips when it is missing.
        ServiceCommand::Argv(vec![
            "node".into(),
            "-e".into(),
            "console.log('svc-up');require('http').createServer((q,s)=>s.end('ok')).listen(+process.env.PORT,'127.0.0.1')".into(),
        ])
    }

    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn managed_lifecycle_and_open_starts_stopped() {
        if !node_available() {
            eprintln!("node not found; skipping");
            return;
        }
        let store = temp_store();
        let (managed, _) = store
            .register(
                RegisterRequest {
                    slug: "managed".into(),
                    runtime: Some("managed".into()),
                    command: Some(server_command()),
                    path: Some("/".into()),
                    port: Some(
                        crate::services::store::allocate_port(
                            &ServicesSettings {
                                port_range_start: 45100,
                                port_range_end: 45900,
                                ..Default::default()
                            },
                            &Default::default(),
                            crate::services::store::os_port_free,
                        )
                        .unwrap(),
                    ),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let (supervisor, events) = recording_supervisor(store.clone());
        supervisor.start().unwrap();
        assert!(
            wait_until(Duration::from_secs(20), || {
                supervisor.probe_all();
                supervisor.snapshots()["managed"].health == ServiceHealth::Reachable
            }),
            "managed service becomes reachable"
        );
        assert!(wait_until(Duration::from_secs(5), || supervisor
            .logs("managed", 50)
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("svc-up")));
        assert!(store.log_path("managed").is_file());
        // Stop persists `stopped` and keeps it down.
        let view = supervisor.stop_service("managed").unwrap();
        assert_eq!(view["health"], "stopped");
        assert!(store.get("managed").unwrap().stopped);
        assert!(wait_until(Duration::from_secs(10), || {
            !crate::services::health::probe(managed.port, None)
        }));
        supervisor.tick();
        assert_eq!(
            supervisor.snapshots()["managed"].health,
            ServiceHealth::Stopped
        );
        // Start again, then a crash (kill the tree behind its back) restarts it.
        supervisor.start_service("managed").unwrap();
        assert!(wait_until(Duration::from_secs(20), || {
            supervisor.probe_all();
            supervisor.snapshots()["managed"].health == ServiceHealth::Reachable
        }));
        let tree = supervisor.inner.lock().unwrap().entries["managed"]
            .running
            .as_ref()
            .unwrap()
            .tree
            .clone();
        tree.kill();
        assert!(wait_until(Duration::from_secs(20), || {
            supervisor.tick();
            supervisor.snapshots()["managed"].restarts >= 1
                && supervisor.snapshots()["managed"].pid.is_some()
        }));
        assert!(events.lock().unwrap().iter().any(|e| e.reason == "exited"));
        supervisor.stop_service("managed").unwrap();
        assert!(wait_until(Duration::from_secs(10), || {
            !crate::services::health::probe(managed.port, None)
        }));

        // Open on a stopped managed service starts it and clears `stopped`.
        assert_eq!(supervisor.view("managed").unwrap()["health"], "stopped");
        let opened = supervisor.open_service("managed").unwrap();
        assert_eq!(opened["health"], "reachable");
        assert_eq!(
            opened["localUrl"],
            format!("http://127.0.0.1:{}/", managed.port)
        );
        assert!(!store.get("managed").unwrap().stopped);
        // Unregistering a running service stops it.
        store.remove("managed", &Caller::Owner).unwrap();
        supervisor.sync();
        assert!(wait_until(Duration::from_secs(10), || {
            !crate::services::health::probe(managed.port, None)
        }));
        supervisor.shutdown();
        let _ = std::fs::remove_dir_all(store.dir());
    }

    /// `service_restart`: a new `restartRequestedAt` written by another
    /// process (the Agent Bus) restarts the running process once through
    /// `restart_service`; a request the owner overrode with a stop is dropped.
    #[test]
    fn agent_restart_request_restarts_once_and_respects_owner_stop() {
        if !node_available() {
            eprintln!("node not found; skipping");
            return;
        }
        let store = temp_store();
        let agent = Caller::Agent {
            cutex_session_id: "cutex.00000000-0000-0000-0000-0000000000a1".into(),
            project_id: None,
        };
        let (managed, _) = store
            .register(
                RegisterRequest {
                    slug: "own".into(),
                    runtime: Some("managed".into()),
                    // The fixture session does not exist: keep an ephemeral
                    // service from being reclaimed as owner-archived.
                    lifetime: Some("persistent".into()),
                    command: Some(server_command()),
                    path: Some("/".into()),
                    port: Some(
                        crate::services::store::allocate_port(
                            &ServicesSettings {
                                port_range_start: 45100,
                                port_range_end: 45900,
                                ..Default::default()
                            },
                            &Default::default(),
                            crate::services::store::os_port_free,
                        )
                        .unwrap(),
                    ),
                    ..Default::default()
                },
                &agent,
            )
            .unwrap();
        let (supervisor, _) = recording_supervisor(store.clone());
        supervisor.start().unwrap();
        let reachable = |supervisor: &Arc<Supervisor>| {
            wait_until(Duration::from_secs(20), || {
                supervisor.probe_all();
                supervisor.snapshots()["own"].health == ServiceHealth::Reachable
            })
        };
        assert!(reachable(&supervisor), "managed service becomes reachable");
        let first_pid = supervisor.snapshots()["own"].pid.unwrap();

        store.request_restart("own", &agent).unwrap();
        supervisor.tick();
        assert!(reachable(&supervisor), "reachable again after the restart");
        let second_pid = supervisor.snapshots()["own"].pid.unwrap();
        assert_ne!(first_pid, second_pid, "a new process runs");
        // The same request is not replayed by later passes.
        supervisor.tick();
        supervisor.tick();
        assert_eq!(supervisor.snapshots()["own"].pid, Some(second_pid));
        assert!(supervisor.logs("own", 200).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("restart requested by the owning agent"));

        // Stopped by the owner after the request: it stays down.
        store.request_restart("own", &agent).unwrap();
        store.set_stopped("own", true).unwrap();
        supervisor.tick();
        assert!(wait_until(Duration::from_secs(10), || {
            !crate::services::health::probe(managed.port, None)
        }));
        assert!(store.get("own").unwrap().stopped);
        assert_eq!(supervisor.snapshots()["own"].pid, None);
        supervisor.shutdown();
        let _ = std::fs::remove_dir_all(store.dir());
    }
}
