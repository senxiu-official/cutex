use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;
use crate::platform::host::current_host_name;

use super::model::EventCheckpoint;
use super::model::EventEnvelope;
use super::model::EventPage;
use super::model::EventPageScope;
use super::model::EventStreamBoundary;
use super::model::EventStreamMetadata;
use super::model::PendingEvent;
use super::model::CONTRACT_VERSION;
use super::model::MAX_SAFE_SEQUENCE;

const STATE_FILE_NAME: &str = "stream-state.json";
const EVENTS_FILE_NAME: &str = "events.jsonl";
const LOCK_FILE_NAME: &str = "repository.lock";
const ORPHANED_DIR_NAME: &str = "orphaned";
const PENDING_RESET_FILE_NAME: &str = "pending-stream-reset.json";

static MANAGEMENT_V2_REPOSITORY: super::process_repository::ProcessRepository<EventRepository> =
    super::process_repository::ProcessRepository::new();
static DIAGNOSTIC_REPOSITORY: Mutex<Option<&'static EventRepository>> = Mutex::new(None);

pub fn management_v2_repository() -> anyhow::Result<&'static EventRepository> {
    let repository = MANAGEMENT_V2_REPOSITORY.get_or_open(
        || Ok(runtime_dir()?.join("management-v2")),
        |dir| EventRepository::open(dir, current_host_name()),
    )?;
    *DIAGNOSTIC_REPOSITORY.lock().unwrap_or_else(|e| e.into_inner()) = Some(repository);
    Ok(repository)
}

/// Never opens storage or waits for a repository operation to finish.
pub(crate) fn queue_diagnostics() -> serde_json::Value {
    let repository = match DIAGNOSTIC_REPOSITORY.try_lock() {
        Ok(slot) => *slot,
        Err(_) => return serde_json::json!({"available":false}),
    };
    repository.map_or_else(|| serde_json::json!({"available":false}), EventRepository::queue_diagnostics)
}

/// Retention of the event journal (owner rule: the stream keeps at least
/// 24 hours). A sealed file is removed once its newest event is
/// [`RETENTION_AGE`] old, except the newest [`MIN_SEALED_FILES`]; the
/// `retained_files` ceiling bounds the bytes whatever the age.
#[derive(Debug, Clone, Copy)]
pub struct RepositoryOptions {
    pub rotate_bytes: u64,
    /// Ceiling on sealed files (`events.jsonl.1` ..= `.N`).
    pub retained_files: usize,
}

impl Default for RepositoryOptions {
    fn default() -> Self {
        // 32 MiB × (127 + active) = 4 GiB at most; a day at the busiest
        // host seen (host-a, ~128 MiB an hour) is ~3 GiB.
        Self {
            rotate_bytes: 32 * 1024 * 1024,
            retained_files: 127,
        }
    }
}

pub const EVENT_MIN_RETENTION_HOURS: u64 = 24;
const RETENTION_AGE: std::time::Duration = std::time::Duration::from_secs(EVENT_MIN_RETENTION_HOURS * 60 * 60);

/// Sealed files kept whatever their age (the old fixed retention).
const MIN_SEALED_FILES: usize = 3;

/// Newest journal files (active included) whose cursors are all indexed in
/// memory: the old 128 MiB window, so open and live paging cost what they
/// did. Older ("cold") files are only boundary-checked at open; a cursor in
/// one is found by a byte search, and each served page indexes its next
/// cursor so a catch-up pages on without searching again.
const HOT_FILES: usize = 4;

impl RepositoryOptions {
    fn slot_count(&self) -> usize {
        self.retained_files + 1
    }

    /// First slot index (in [`EventRepository::read_paths`] order) that is hot.
    fn hot_start(&self) -> usize {
        self.slot_count().saturating_sub(HOT_FILES)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct StreamState {
    version: u8,
    stream_id: String,
    next_sequence: u64,
}

impl StreamState {
    fn new() -> Self {
        Self {
            version: 1,
            stream_id: Uuid::new_v4().to_string(),
            next_sequence: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StreamRecoveryReset {
    pub previous_stream_id: Option<String>,
    pub previous_last_sequence: Option<u64>,
    pub stream_id: String,
    pub reason: String,
    pub detected_at: String,
    pub diagnostic: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PendingStreamReset {
    reset: StreamRecoveryReset,
    #[serde(default)]
    emitted_session_ids: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct ReplayQuery {
    pub stream_id: Option<String>,
    pub after: Option<String>,
    pub limit: usize,
    pub cutex_session_id: Option<String>,
}

impl Default for ReplayQuery {
    fn default() -> Self {
        Self {
            stream_id: None,
            after: None,
            limit: 100,
            cutex_session_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedBoundary {
    pub sequence: u64,
    pub cursor: String,
}

#[derive(Debug)]
pub enum ReplayError {
    InvalidQuery(String),
    ConflictingCursor,
    StreamChanged {
        requested_stream_id: String,
        current_stream_id: String,
    },
    CursorExpired {
        stream_id: String,
        cursor: String,
        earliest: Option<RetainedBoundary>,
        latest: Option<RetainedBoundary>,
    },
    Repository(anyhow::Error),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuery(message) => formatter.write_str(message),
            Self::ConflictingCursor => {
                formatter.write_str("after and Last-Event-ID identify different cursors")
            }
            Self::StreamChanged {
                requested_stream_id,
                current_stream_id,
            } => write!(
                formatter,
                "requested stream {requested_stream_id} differs from current stream {current_stream_id}"
            ),
            Self::CursorExpired { cursor, .. } => {
                write!(formatter, "cursor is no longer retained: {cursor}")
            }
            Self::Repository(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<anyhow::Error> for ReplayError {
    fn from(error: anyhow::Error) -> Self {
        Self::Repository(error)
    }
}

pub struct EventSubscription {
    pub page: EventPage,
    pub receiver: EventReceiver,
    /// Set when the subscriber was dropped because its queue was full; the
    /// receiver then drains and disconnects.
    pub lagged: Arc<AtomicBool>,
    pub live_deltas: Option<super::live_deltas::LiveSubscription>,
    pub(crate) wake: Arc<super::live_deltas::StreamWake>,
    _live: Arc<()>,
}

type Subscriber = (mpsc::SyncSender<QueuedEvent>, Arc<AtomicBool>, Weak<()>, Arc<super::live_deltas::StreamWake>);

#[derive(Default)]
struct SubscriptionMetrics {
    events: AtomicU64,
    bytes: AtomicU64,
}

/// Charge follows the channel item, including after its sender is removed on lag.
/// No charge survives dequeue, failed enqueue, or receiver destruction.
struct QueuedEvent {
    event: Option<EventEnvelope>,
    bytes: u64,
    metrics: Arc<SubscriptionMetrics>,
}
impl QueuedEvent {
    fn new(event: EventEnvelope, bytes: usize, metrics: &Arc<SubscriptionMetrics>) -> Self {
        metrics.events.fetch_add(1, Ordering::Relaxed);
        metrics.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        Self { event: Some(event), bytes: bytes as u64, metrics: metrics.clone() }
    }
    fn into_event(mut self) -> EventEnvelope { self.event.take().expect("queued event") }
}
impl Drop for QueuedEvent {
    fn drop(&mut self) {
        self.metrics.events.fetch_sub(1, Ordering::Relaxed);
        self.metrics.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

pub struct EventReceiver(mpsc::Receiver<QueuedEvent>);
impl EventReceiver {
    pub fn try_recv(&self) -> Result<EventEnvelope, mpsc::TryRecvError> {
        self.0.try_recv().map(QueuedEvent::into_event)
    }
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Result<EventEnvelope, mpsc::RecvTimeoutError> {
        self.0.recv_timeout(timeout).map(QueuedEvent::into_event)
    }
    pub fn try_iter(&self) -> impl Iterator<Item = EventEnvelope> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}

pub struct EventRepository {
    root: PathBuf,
    host_id: String,
    options: RepositoryOptions,
    process_lock: Mutex<()>,
    subscribers: Mutex<Vec<Subscriber>>,
    subscription_metrics: Arc<SubscriptionMetrics>,
    /// Highest journal event offered to this process's subscribers. Guarded by
    /// the repository lock as well as this mutex, like local publication.
    live_checkpoint: Mutex<Option<EventCheckpoint>>,
    live_deltas: super::live_deltas::LiveHub,
    recovery_reset: Mutex<Option<PendingStreamReset>>,
    replay_index: Mutex<ReplayIndex>,
    stats: AppendStats,
    commits: Mutex<CommitQueue>,
    committed: std::sync::Condvar,
    checkpoint: Mutex<StreamCheckpoint>,
}

/// Group commit (PERF-07): appends that arrive while a batch is being made
/// durable wait here and go out together in the next batch, one fsync per
/// batch. A batch is written in arrival order under the repository lock,
/// so per-stream order is the order `append` calls entered the queue.
#[derive(Default)]
struct CommitQueue {
    next_ticket: u64,
    waiting: Vec<(u64, PendingEvent)>,
    leader: bool,
    active_batch_items: usize,
    done: HashMap<u64, anyhow::Result<EventEnvelope>>,
}

/// The newest stream position this process made durable, and when the
/// stream state file last recorded it. `stream-state.json` is a checkpoint:
/// its `nextSequence` may lag the journal tail (written at most every
/// [`STATE_CHECKPOINT_INTERVAL`], on rotation and on every stream change),
/// never lead it. The in-memory floor keeps the old per-event guarantee
/// inside one process: a journal tail that moves backwards is a lost
/// journal (`state_lost`), not a place to reuse sequences.
#[derive(Default)]
struct StreamCheckpoint {
    stream_id: Option<String>,
    next_sequence: u64,
    written_next_sequence: u64,
    written_at: Option<std::time::Instant>,
}

const STATE_CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Lines of one batch for the active journal file, written and synced once.
struct BatchWriter {
    path: PathBuf,
    active_len: u64,
    buffer: Vec<u8>,
}

impl BatchWriter {
    fn buffered_end(&self) -> u64 {
        self.active_len.saturating_add(self.buffer.len() as u64)
    }
}

/// Write-path counters of one repository instance (diagnostics, benches).
#[derive(Default)]
pub(crate) struct AppendStats {
    pub(crate) events: std::sync::atomic::AtomicU64,
    pub(crate) batches: std::sync::atomic::AtomicU64,
    pub(crate) journal_syncs: std::sync::atomic::AtomicU64,
    pub(crate) state_writes: std::sync::atomic::AtomicU64,
    pub(crate) lock_wait_ns: std::sync::atomic::AtomicU64,
    pub(crate) lock_hold_ns: std::sync::atomic::AtomicU64,
}

impl AppendStats {
    fn add(counter: &std::sync::atomic::AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::Relaxed);
    }
}

struct RepositorySnapshot {
    state: StreamState,
    earliest: Option<EventStreamBoundary>,
    latest: Option<EventStreamBoundary>,
}

#[derive(Default)]
struct ReplayIndex {
    stream_id: Option<String>,
    files: Vec<JournalFileStamp>,
    cursors: HashMap<String, ReplayLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalFileStamp {
    length: u64,
    first: Option<RetainedBoundary>,
    last: Option<RetainedBoundary>,
}

#[derive(Debug, Clone, Copy)]
struct ReplayLocation {
    file_index: usize,
    next_offset: u64,
    sequence: u64,
}

struct JournalScan {
    stream_id: Option<String>,
    last: Option<RetainedBoundary>,
    records: Vec<EventEnvelope>,
}

impl EventRepository {
    pub fn open(root: impl Into<PathBuf>, host_id: impl Into<String>) -> anyhow::Result<Self> {
        Self::open_with_options(root, host_id, RepositoryOptions::default())
    }

    pub fn open_with_options(
        root: impl Into<PathBuf>,
        host_id: impl Into<String>,
        options: RepositoryOptions,
    ) -> anyhow::Result<Self> {
        if options.rotate_bytes == 0 {
            anyhow::bail!("management v2 rotate_bytes must be positive");
        }
        if options.retained_files == 0 {
            anyhow::bail!("management v2 retained_files must be positive");
        }
        let host_id = host_id.into();
        if host_id.is_empty() {
            anyhow::bail!("management v2 repository hostId must not be empty");
        }
        let repository = Self {
            root: root.into(),
            host_id,
            options,
            process_lock: Mutex::new(()),
            subscribers: Mutex::new(Vec::new()),
            subscription_metrics: Arc::new(SubscriptionMetrics::default()),
            live_checkpoint: Mutex::new(None),
            live_deltas: super::live_deltas::LiveHub::new(),
            recovery_reset: Mutex::new(None),
            replay_index: Mutex::new(ReplayIndex::default()),
            stats: AppendStats::default(),
            commits: Mutex::new(CommitQueue::default()),
            committed: std::sync::Condvar::new(),
            checkpoint: Mutex::new(StreamCheckpoint::default()),
        };
        repository.prepare_root()?;
        repository.with_repository_lock(|repository| {
            let (state, reset) = repository.load_or_recover_state_locked()?;
            if reset.is_some() {
                repository.record_recovery_reset(reset)?;
            } else {
                repository.restore_pending_recovery_reset_locked(&state)?;
            }
            Ok(())
        })?;
        Ok(repository)
    }

    /// True means recognized as replaceable text traffic, including when it was
    /// discarded for validation/budget reasons. Never fall back to append.
    pub fn publish_live(&self, pending: PendingEvent) -> bool {
        let mut timing = super::control_timing::Trace::metrics_only(super::control_timing::Path::LivePublish);
        timing.stage(super::control_timing::Stage::Fanout);
        if pending.host_id != self.host_id {
            return super::live_deltas::is_live_pending(&pending);
        }
        self.live_deltas.publish(pending)
    }

    pub fn subscribe_live(&self, session: Option<String>) -> Result<super::live_deltas::LiveSubscription,super::live_deltas::LiveCapacityError> {
        self.live_deltas.subscribe(session)
    }

    pub(crate) fn enable_live(&self, subscription:&mut EventSubscription, session:Option<String>)->Result<(),super::live_deltas::LiveCapacityError> {
        subscription.live_deltas=Some(self.live_deltas.subscribe_woken(session,Some(subscription.wake.clone()))?);
        Ok(())
    }

    pub fn invalidate_live(&self, session: &str, generation: u64) {
        self.live_deltas.invalidate(session, generation);
    }

    pub fn live_delta_stats(&self) -> super::live_deltas::LiveStats {
        self.live_deltas.stats()
    }

    fn queue_diagnostics(&self) -> serde_json::Value {
        // Short, independent try-lock snapshots: diagnostic reads must not join
        // the stalled operation they are intended to diagnose.
        let commits = match self.commits.try_lock() {
            Ok(queue) => serde_json::json!({"available":true,
                "waitingItems":queue.waiting.len(), "activeBatchItems":queue.active_batch_items,
                "completedResultsAwaitingCaller":queue.done.len()}),
            Err(_) => serde_json::json!({"available":false}),
        };
        let subscriptions = match self.subscribers.try_lock() {
            // Publication holds this lock through enqueue/rollback, so no
            // failed-send reservation is exposed. Receives remain concurrent.
            Ok(_guard) => serde_json::json!({"available":true,"approximate":true,
                "queuedEvents":self.subscription_metrics.events.load(Ordering::Relaxed),
                "queuedEncodedBytes":self.subscription_metrics.bytes.load(Ordering::Relaxed),
                "byteDefinition":"compact_json_envelope_excluding_newline_per_queued_copy",
                "excludesReplayAndDequeued":true}),
            Err(_) => serde_json::json!({"available":false}),
        };
        serde_json::json!({"available":true,"commits":commits,"durableSubscriptions":subscriptions,
            "liveSubscriptions":self.live_deltas.queue_diagnostics()})
    }

    /// Appends one event and returns it once it is durable (fsynced) and
    /// published to this process's live subscribers. Concurrent appends are
    /// group-committed: see [`CommitQueue`].
    pub fn append(&self, mut pending: PendingEvent) -> anyhow::Result<EventEnvelope> {
        // Fixed-cardinality diagnostics include queue/commit wait and synchronous
        // publication. Fanout is separately timed and is nested, not additive.
        let mut timing = super::control_timing::Trace::metrics_only(super::control_timing::Path::EventAppend);
        timing.stage(super::control_timing::Stage::StoreWrite);
        if super::live_deltas::is_live_pending(&pending) {
            anyhow::bail!("text deltas are live-only; use publish_live instead of durable append");
        }
        pending.validate()?;
        let blobs = super::event_blobs::EventBlobStore::at(self.root.join("event-blobs"));
        if let Some(native) = pending.native.as_mut() {
            super::native_errors::annotate_notification(&mut native.message);
            blobs.normalize(&pending.cutex_session_id, &mut native.message)?;
            crate::session::fork::annotate_native_rejection(
                &pending.cutex_session_id,
                &mut native.message,
            );
        }
        if let Some(cutex) = pending.cutex.as_mut() {
            blobs.normalize_cutex(&pending.cutex_session_id, &mut cutex.params)?;
        }
        pending.validate()?;
        if pending.host_id != self.host_id {
            anyhow::bail!(
                "event hostId {} differs from repository hostId {}",
                pending.host_id,
                self.host_id
            );
        }
        timing.stage(super::control_timing::Stage::QueueWait);
        let mut queue = self.lock_commits();
        let ticket = queue.next_ticket;
        queue.next_ticket = queue.next_ticket.wrapping_add(1);
        queue.waiting.push((ticket, pending));
        loop {
            if let Some(result) = queue.done.remove(&ticket) {
                return result;
            }
            if !queue.leader {
                // Lead one batch (which holds this ticket), then hand over.
                queue.leader = true;
                let batch = std::mem::take(&mut queue.waiting);
                queue.active_batch_items = batch.len();
                drop(queue);
                let mut leadership = BatchLeadership {
                    repository: self,
                    tickets: batch.iter().map(|(ticket, _)| *ticket).collect(),
                    results: Vec::new(),
                };
                timing.stage(super::control_timing::Stage::StoreWrite);
                leadership.results = self.commit_batch(batch);
                drop(leadership);
                timing.stage(super::control_timing::Stage::QueueWait);
                queue = self.lock_commits();
                continue;
            }
            queue = self
                .committed
                .wait(queue)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn lock_commits(&self) -> std::sync::MutexGuard<'_, CommitQueue> {
        self.commits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes one batch: one lock, one tail read, one fsync (two when the
    /// batch crosses a rotation), at most one state checkpoint. Events are
    /// published in sequence order after they are durable, still under the
    /// lock, so a subscriber never sees a later event first. An error that
    /// hits the batch as a whole fails every event of it; like a failed
    /// single append before, an event that failed may still be in the
    /// journal (written, sync not confirmed), never acknowledged.
    fn commit_batch(
        &self,
        batch: Vec<(u64, PendingEvent)>,
    ) -> Vec<(u64, anyhow::Result<EventEnvelope>)> {
        let tickets = batch.iter().map(|(ticket, _)| *ticket).collect::<Vec<_>>();
        AppendStats::add(&self.stats.batches, 1);
        AppendStats::add(&self.stats.events, batch.len() as u64);
        let outcome = self.with_repository_lock(|repository| {
            let (mut state, reset) = repository.load_append_state_locked()?;
            repository.record_recovery_reset(reset)?;
            // Another process may have committed earlier events since our last
            // append. Deliver those first; a local event must never overtake them.
            repository.catch_up_live_locked(&state)?;
            // Reading an unseen corrupt record can recover/reset the stream.
            // Do not stage this append with the pre-recovery stream/sequence.
            let recovered = repository.live_checkpoint.lock().unwrap_or_else(|e| e.into_inner())
                .as_ref().is_some_and(|checkpoint| checkpoint.stream_id != state.stream_id);
            if recovered {
                let (current, reset) = repository.load_append_state_locked()?;
                repository.record_recovery_reset(reset)?;
                state = current;
            }
            let stream_id = state.stream_id.clone();
            let mut writer = repository.batch_writer_locked()?;
            let mut rotated_any = false;
            let mut reset_sessions = HashSet::new();
            let mut staged = Vec::new();
            let mut results = Vec::with_capacity(batch.len());
            for (ticket, pending) in batch {
                let session = pending.cutex_session_id.clone();
                let reset = if reset_sessions.contains(&session) {
                    None
                } else {
                    repository.pending_reset_for_session(&session, &stream_id)?
                };
                if let Some(reset) = reset {
                    let (envelope, encoded) =
                        repository.stage_locked(&mut state, reset_event(&repository.host_id, &session, &reset))?;
                    let (start, next, rotated) = repository.buffer_line_locked(&mut writer, &encoded)?;
                    rotated_any |= rotated;
                    staged.push((envelope, start, next, rotated, encoded.len().saturating_sub(1)));
                    reset_sessions.insert(session);
                }
                match repository.stage_locked(&mut state, pending) {
                    Ok((envelope, encoded)) => {
                        let (start, next, rotated) =
                            repository.buffer_line_locked(&mut writer, &encoded)?;
                        rotated_any |= rotated;
                        results.push((ticket, Ok(staged.len())));
                        staged.push((envelope, start, next, rotated, encoded.len().saturating_sub(1)));
                    }
                    // A per-event refusal (validation, sequence range)
                    // consumes no sequence and fails only that append.
                    Err(error) => results.push((ticket, Err(error))),
                }
            }
            repository.flush_batch_locked(&mut writer)?;
            if let Some((last, _, _, _, _)) = staged.last() {
                debug_assert_eq!(state.next_sequence, last.sequence.saturating_add(1));
                // The journal is durable and authoritative; a checkpoint that
                // could not be written only leaves the state file lagging.
                if let Err(error) = repository.checkpoint_locked(&state, rotated_any) {
                    eprintln!("warning: management v2 stream checkpoint not written: {error:#}");
                }
            }
            for (envelope, start, next, rotated, _) in &staged {
                repository.record_indexed_append(envelope, *rotated, *start, *next);
            }
            if !reset_sessions.is_empty() {
                repository.mark_resets_emitted(reset_sessions)?;
            }
            for (envelope, _, _, _, bytes) in &staged {
                repository.publish(envelope, Some(*bytes));
            }
            Ok((results, staged))
        });
        match outcome {
            Ok((results, staged)) => {
                let mut envelopes = staged
                    .into_iter()
                    .map(|(envelope, _, _, _, _)| Some(envelope))
                    .collect::<Vec<_>>();
                results
                    .into_iter()
                    .map(|(ticket, slot)| {
                        (ticket, slot.map(|position| {
                            envelopes[position].take().expect("one result per staged event")
                        }))
                    })
                    .collect()
            }
            Err(error) => {
                if let Ok(mut index) = self.replay_index.lock() {
                    *index = ReplayIndex::default();
                }
                let message = format!("{error:#}");
                tickets
                    .into_iter()
                    .map(|ticket| (ticket, Err(anyhow::anyhow!("{message}"))))
                    .collect()
            }
        }
    }

    pub fn materialize_recovery_reset(
        &self,
        cutex_session_ids: &[String],
        complete: bool,
    ) -> anyhow::Result<Vec<EventEnvelope>> {
        if !self.has_pending_recovery_reset()? {
            return Ok(Vec::new());
        }
        let mut session_ids = cutex_session_ids
            .iter()
            .filter(|session_id| !session_id.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        session_ids.sort();
        session_ids.dedup();
        let envelopes = self.with_repository_lock(|repository| {
            let (mut state, reset) = repository.load_or_recover_state_locked()?;
            repository.record_recovery_reset(reset)?;
            repository.reconcile_emitted_reset_sessions_locked()?;
            let mut envelopes = Vec::new();
            for session_id in session_ids {
                if let Some(envelope) =
                    repository.append_reset_for_session_locked(&mut state, &session_id)?
                {
                    envelopes.push(envelope);
                }
            }
            if complete {
                repository.clear_pending_recovery_reset_locked()?;
            }
            // Serialize reset publication with journal following and append.
            // The follower must not race a publication outside this lock.
            repository.catch_up_live_locked(&state)?;
            Ok(envelopes)
        })?;
        Ok(envelopes)
    }

    pub fn checkpoint(&self) -> anyhow::Result<EventCheckpoint> {
        self.with_repository_lock(|repository| {
            let (snapshot, reset) = repository.load_snapshot_locked()?;
            repository.record_recovery_reset(reset)?;
            Ok(checkpoint_for_snapshot(&snapshot))
        })
    }

    pub fn stream_metadata(&self) -> anyhow::Result<EventStreamMetadata> {
        self.with_repository_lock(|repository| {
            let (snapshot, reset) = repository.load_snapshot_locked()?;
            repository.record_recovery_reset(reset)?;
            Ok(EventStreamMetadata {
                stream_id: snapshot.state.stream_id,
                earliest: snapshot.earliest,
                latest: snapshot.latest,
            })
        })
    }

    pub fn page(&self, query: ReplayQuery) -> Result<EventPage, ReplayError> {
        self.with_replay_lock(|repository| repository.page_locked(query))
    }

    pub fn has_live_subscribers(&self) -> bool {
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return false;
        };
        subscribers.retain(|(_, lagged, live, _)| live.strong_count() > 0 && !lagged.load(Ordering::SeqCst));
        !subscribers.is_empty()
    }

    pub fn page_and_subscribe(
        &self,
        query: ReplayQuery,
        capacity: usize,
    ) -> Result<EventSubscription, ReplayError> {
        if capacity == 0 {
            return Err(ReplayError::InvalidQuery(
                "subscriber capacity must be positive".to_string(),
            ));
        }
        self.with_replay_lock(|repository| {
            let (state, reset) = repository.load_append_state_locked()?;
            repository.record_recovery_reset(reset)?;
            repository.catch_up_live_locked(&state)?;
            let page = repository.page_locked(query)?;
            for event in &page.events { repository.live_deltas.seed_anchor(event); }
            // An initial/replacement subscription starts at this snapshot's
            // boundary. Existing retained events belong to its replay page.
            if !repository.has_live_subscribers() {
                *repository.live_checkpoint.lock().unwrap_or_else(|e| e.into_inner()) = Some(page.checkpoint.clone());
            }
            let (sender, receiver) = mpsc::sync_channel(capacity);
            let lagged = Arc::new(AtomicBool::new(false));
            let live = Arc::new(());
            let wake = Arc::new(super::live_deltas::StreamWake::default());
            repository
                .subscribers
                .lock()
                .map_err(|_| {
                    ReplayError::Repository(anyhow::anyhow!(
                        "management v2 subscriber lock was poisoned"
                    ))
                })?
                .push((sender, lagged.clone(), Arc::downgrade(&live), wake.clone()));
            Ok(EventSubscription {
                page,
                receiver: EventReceiver(receiver),
                lagged,
                live_deltas: None,
                wake,
                _live: live,
            })
        })
    }

    /// Management's journal follower. No append, side effects, model calls or
    /// synthetic events: offer already durable external events to live readers.
    pub fn follow_shared_journal(&self) -> anyhow::Result<usize> {
        if !self.has_live_subscribers() { return Ok(0); }
        self.with_repository_lock(|repository| {
            let (state, reset) = repository.load_append_state_locked()?;
            repository.record_recovery_reset(reset)?;
            repository.catch_up_live_locked(&state)
        })
    }

    fn catch_up_live_locked(&self, state: &StreamState) -> anyhow::Result<usize> {
        if !self.has_live_subscribers() { return Ok(0); }
        let Some(mut checkpoint) = self.live_checkpoint.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return Ok(0);
        };
        if checkpoint.stream_id == state.stream_id && checkpoint.sequence.saturating_add(1) == state.next_sequence {
            return Ok(0);
        }
        let mut count = 0;
        loop {
            let query = ReplayQuery { stream_id: Some(checkpoint.stream_id.clone()), after: checkpoint.cursor.clone(), limit: 1000, cutex_session_id: None };
            let page = match self.page_locked(query) {
                Ok(page) => page,
                // Let the normal SSE pump see an event in the new stream and
                // emit its existing stream_changed frame (never invent a cursor).
                Err(ReplayError::StreamChanged { .. }) => self.page_locked(ReplayQuery { limit: 1000, ..ReplayQuery::default() })?,
                Err(ReplayError::CursorExpired { .. }) => {
                    // Retention overtook the follower. Reconnect/resync rather
                    // than silently skip the missing prefix for any reader.
                    let mut subscribers = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
                    for (_, lagged, _, wake) in subscribers.iter() { lagged.store(true, Ordering::SeqCst); wake.notify(); }
                    subscribers.clear();
                    *self.live_checkpoint.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    return Ok(count);
                }
                Err(error) => return Err(error.into()),
            };
            for envelope in &page.events {
                self.publish(envelope, None);
                checkpoint = EventCheckpoint { stream_id: envelope.stream_id.clone(), sequence: envelope.sequence, cursor: Some(envelope.cursor.clone()) };
                count += 1;
            }
            if !self.has_live_subscribers() {
                *self.live_checkpoint.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Ok(count);
            }
            if !page.has_more {
                *self.live_checkpoint.lock().unwrap_or_else(|e| e.into_inner()) = Some(page.checkpoint);
                return Ok(count);
            }
        }
    }

    pub fn recovery_reset(&self) -> anyhow::Result<Option<StreamRecoveryReset>> {
        Ok(self
            .recovery_reset
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 recovery-reset lock was poisoned"))?
            .as_ref()
            .map(|pending| pending.reset.clone()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn has_pending_recovery_reset(&self) -> anyhow::Result<bool> {
        Ok(self
            .recovery_reset
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 recovery-reset lock was poisoned"))?
            .is_some())
    }

    fn page_locked(&self, query: ReplayQuery) -> Result<EventPage, ReplayError> {
        if !(1..=1000).contains(&query.limit) {
            return Err(ReplayError::InvalidQuery(
                "event page limit must be between 1 and 1000".to_string(),
            ));
        }
        let initial_cursor = query
            .after
            .as_deref()
            .is_none_or(|value| value.is_empty() || value == "0");
        if !initial_cursor && query.stream_id.is_none() {
            return Err(ReplayError::InvalidQuery(
                "streamId is required with a nonzero after cursor".to_string(),
            ));
        }
        let mut touched_cold = false;
        match self.page_locked_once(&query, initial_cursor, &mut touched_cold) {
            Err(ReplayError::Repository(_)) => {
                // A failure in a cold file (only boundary-checked at open)
                // needs the full check to decide; hot files are always full.
                let (_, reset) = self
                    .load_or_recover_state_with_locked(touched_cold)
                    .map_err(ReplayError::from)?;
                self.record_recovery_reset(reset)
                    .map_err(ReplayError::from)?;
                self.page_locked_once(&query, initial_cursor, &mut touched_cold)
            }
            result => result,
        }
    }

    fn page_locked_once(
        &self,
        query: &ReplayQuery,
        initial_cursor: bool,
        touched_cold: &mut bool,
    ) -> Result<EventPage, ReplayError> {
        let (snapshot, reset) = self.load_snapshot_locked().map_err(ReplayError::from)?;
        self.record_recovery_reset(reset)
            .map_err(ReplayError::from)?;
        if let Some(requested) = query.stream_id.as_deref() {
            if requested != snapshot.state.stream_id {
                return Err(ReplayError::StreamChanged {
                    requested_stream_id: requested.to_string(),
                    current_stream_id: snapshot.state.stream_id,
                });
            }
        }
        let stream_id = snapshot.state.stream_id.clone();
        self.ensure_replay_index_locked(&stream_id)
            .map_err(ReplayError::from)?;
        let start = {
            let index = self.replay_index.lock().map_err(|_| {
                ReplayError::Repository(anyhow::anyhow!(
                    "management v2 replay-index lock was poisoned"
                ))
            })?;
            if initial_cursor {
                index
                    .files
                    .iter()
                    .position(|file| file.first.is_some())
                    .map(|file_index| {
                        let sequence = index.files[file_index]
                            .first
                            .as_ref()
                            .expect("selected nonempty replay-index file")
                            .sequence;
                        ReplayLocation {
                            file_index,
                            next_offset: 0,
                            sequence: sequence.saturating_sub(1),
                        }
                    })
            } else {
                index
                    .cursors
                    .get(query.after.as_deref().unwrap_or_default())
                    .copied()
            }
        };
        let start = match start {
            None if !initial_cursor => {
                let cursor = query.after.as_deref().unwrap_or_default();
                *touched_cold = true;
                let found = self
                    .find_cold_cursor_locked(cursor, &stream_id)
                    .map_err(ReplayError::from)?
                    .ok_or_else(|| ReplayError::CursorExpired {
                        stream_id: stream_id.clone(),
                        cursor: cursor.to_string(),
                        earliest: snapshot.earliest.as_ref().map(retained_boundary),
                        latest: snapshot.latest.as_ref().map(retained_boundary),
                    })?;
                Some(found)
            }
            start => start,
        };
        if start.is_some_and(|start| start.file_index < self.options.hot_start()) {
            *touched_cold = true;
        }
        if let (false, Some(start)) = (initial_cursor, start) {
            self.validate_indexed_cursor_locked(
                query.after.as_deref().unwrap_or_default(),
                start,
                &stream_id,
            )
            .map_err(ReplayError::from)?;
        }
        let mut scanned = match start {
            Some(start) => self
                .read_indexed_records_locked(start, query.limit.saturating_add(1), &stream_id)
                .map_err(ReplayError::from)?,
            None => Vec::new(),
        };
        let has_more = scanned.len() > query.limit;
        if has_more {
            scanned.truncate(query.limit);
        }
        if let Some((event, location)) = scanned.last() {
            // Cold cursors are not indexed: keep the one this page hands out
            // so the next page of a catch-up needs no search.
            if location.file_index < self.options.hot_start() {
                if let Ok(mut index) = self.replay_index.lock() {
                    index.cursors.entry(event.cursor.clone()).or_insert(*location);
                }
            }
        }
        let next_cursor = scanned
            .last()
            .map(|(event, _)| event.cursor.clone())
            .or_else(|| (!initial_cursor).then(|| query.after.clone()).flatten());
        let scanned_count = scanned.len();
        let events = scanned
            .into_iter()
            .map(|(event, _)| event)
            .filter(|event| {
                query
                    .cutex_session_id
                    .as_deref()
                    .is_none_or(|session_id| event.cutex_session_id == session_id)
            })
            .collect();
        Ok(EventPage {
            contract_version: CONTRACT_VERSION,
            host_id: self.host_id.clone(),
            stream_id,
            scope: EventPageScope {
                cutex_session_id: query.cutex_session_id.clone(),
            },
            events,
            next_cursor,
            checkpoint: checkpoint_for_snapshot(&snapshot),
            scanned_count,
            has_more,
        })
    }

    /// Assigns the next sequence to `pending` and encodes its journal line.
    fn stage_locked(
        &self,
        state: &mut StreamState,
        pending: PendingEvent,
    ) -> anyhow::Result<(EventEnvelope, Vec<u8>)> {
        if state.next_sequence > MAX_SAFE_SEQUENCE {
            anyhow::bail!("management v2 event sequence exhausted the JSON-safe range");
        }
        let envelope = EventEnvelope {
            contract_version: CONTRACT_VERSION,
            event_id: Uuid::new_v4().to_string(),
            cursor: format!("c2:{}", Uuid::new_v4()),
            stream_id: state.stream_id.clone(),
            sequence: state.next_sequence,
            received_at: Utc::now().to_rfc3339(),
            cutex_session_id: pending.cutex_session_id,
            host_id: pending.host_id,
            source: pending.source,
            sensitivity: "owner".to_string(),
            schema: pending.schema,
            correlation: pending.correlation,
            native: pending.native,
            cutex: pending.cutex,
        };
        envelope.validate()?;
        anyhow::ensure!(super::event_blobs::encoded_fits(&envelope, super::event_blobs::MAX_EVENT_BYTES - 1),
            "management event metadata exceeds event size limit");
        let mut encoded =
            serde_json::to_vec(&envelope).context("Failed to serialize management v2 event")?;
        encoded.push(b'\n');
        state.next_sequence = state.next_sequence.saturating_add(1);
        Ok((envelope, encoded))
    }

    /// One event outside the group commit (stream-reset materialization).
    fn append_pending_locked(
        &self,
        state: &mut StreamState,
        pending: PendingEvent,
    ) -> anyhow::Result<EventEnvelope> {
        let mut writer = self.batch_writer_locked()?;
        let (envelope, encoded) = self.stage_locked(state, pending)?;
        let (start_offset, next_offset, rotated) = self.buffer_line_locked(&mut writer, &encoded)?;
        self.flush_batch_locked(&mut writer)?;
        self.checkpoint_locked(state, true)?;
        self.record_indexed_append(&envelope, rotated, start_offset, next_offset);
        Ok(envelope)
    }

    fn batch_writer_locked(&self) -> anyhow::Result<BatchWriter> {
        let path = self.root.join(EVENTS_FILE_NAME);
        let active_len = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to stat management v2 log: {}", path.display())
                })
            }
        };
        Ok(BatchWriter {
            path,
            active_len,
            buffer: Vec::new(),
        })
    }

    /// Adds one line to the batch: `(start, next, rotated)` offsets in the
    /// active file. A line that would overflow a nonempty active file first
    /// makes the lines before it durable, then rotates.
    fn buffer_line_locked(
        &self,
        writer: &mut BatchWriter,
        encoded: &[u8],
    ) -> anyhow::Result<(u64, u64, bool)> {
        let end = writer.buffered_end();
        let rotated = end > 0
            && end.saturating_add(encoded.len() as u64) > self.options.rotate_bytes;
        if rotated {
            self.flush_batch_locked(writer)?;
            self.rotate_files_locked()?;
            writer.active_len = 0;
        }
        let start = writer.buffered_end();
        writer.buffer.extend_from_slice(encoded);
        Ok((start, writer.buffered_end(), rotated))
    }

    /// Writes the buffered lines with one `sync_data`.
    fn flush_batch_locked(&self, writer: &mut BatchWriter) -> anyhow::Result<()> {
        if writer.buffer.is_empty() {
            return Ok(());
        }
        let path = &writer.path;
        let mut file = open_private_file(path, true)?;
        file.write_all(&writer.buffer)
            .with_context(|| format!("Failed to append management v2 event: {}", path.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush management v2 event: {}", path.display()))?;
        file.sync_data()
            .with_context(|| format!("Failed to sync management v2 event: {}", path.display()))?;
        AppendStats::add(&self.stats.journal_syncs, 1);
        writer.active_len = writer.buffered_end();
        writer.buffer.clear();
        Ok(())
    }

    /// Records the durable stream position (in memory always; on disk when
    /// `force`, on a stream change or every [`STATE_CHECKPOINT_INTERVAL`]).
    fn checkpoint_locked(&self, state: &StreamState, force: bool) -> anyhow::Result<()> {
        let mut checkpoint = self
            .checkpoint
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 checkpoint lock was poisoned"))?;
        let same_stream = checkpoint.stream_id.as_deref() == Some(state.stream_id.as_str());
        let due = force
            || !same_stream
            || checkpoint
                .written_at
                .is_none_or(|at| at.elapsed() >= STATE_CHECKPOINT_INTERVAL);
        if due && !(same_stream && checkpoint.written_next_sequence == state.next_sequence) {
            self.write_state_locked(state)?;
            checkpoint.written_next_sequence = state.next_sequence;
            checkpoint.written_at = Some(std::time::Instant::now());
        } else if !same_stream {
            checkpoint.written_next_sequence = 0;
        }
        checkpoint.stream_id = Some(state.stream_id.clone());
        checkpoint.next_sequence = state.next_sequence;
        Ok(())
    }

    /// The highest `nextSequence` this process knows to be durable for
    /// `stream_id` (journal synced), raising a lagging state checkpoint.
    fn known_next_sequence(&self, stream_id: &str, from_state: u64) -> u64 {
        self.checkpoint
            .lock()
            .ok()
            .filter(|checkpoint| checkpoint.stream_id.as_deref() == Some(stream_id))
            .map_or(from_state, |checkpoint| checkpoint.next_sequence.max(from_state))
    }

    fn pending_reset_for_session(
        &self,
        cutex_session_id: &str,
        stream_id: &str,
    ) -> anyhow::Result<Option<StreamRecoveryReset>> {
        let pending = self
            .recovery_reset
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 recovery-reset lock was poisoned"))?;
        let reset = pending.as_ref().and_then(|pending| {
            (!pending.emitted_session_ids.contains(cutex_session_id))
                .then(|| pending.reset.clone())
        });
        if reset.as_ref().is_some_and(|reset| reset.stream_id != stream_id) {
            anyhow::bail!("pending stream reset does not match the active stream");
        }
        Ok(reset)
    }

    /// After the reset events are durable (never before: a crash would
    /// otherwise mark a session reset whose event was lost).
    fn mark_resets_emitted(&self, sessions: HashSet<String>) -> anyhow::Result<()> {
        let mut pending = self
            .recovery_reset
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 recovery-reset lock was poisoned"))?;
        if let Some(pending) = pending.as_mut() {
            pending.emitted_session_ids.extend(sessions);
            self.persist_pending_recovery_reset_locked(pending)?;
        }
        Ok(())
    }

    fn append_reset_for_session_locked(
        &self,
        state: &mut StreamState,
        cutex_session_id: &str,
    ) -> anyhow::Result<Option<EventEnvelope>> {
        let Some(reset) = self.pending_reset_for_session(cutex_session_id, &state.stream_id)? else {
            return Ok(None);
        };
        let envelope = self.append_pending_locked(
            state,
            reset_event(&self.host_id, cutex_session_id, &reset),
        )?;
        self.mark_resets_emitted(HashSet::from([cutex_session_id.to_string()]))?;
        Ok(Some(envelope))
    }

    fn reconcile_emitted_reset_sessions_locked(&self) -> anyhow::Result<()> {
        if !self.has_pending_recovery_reset()? {
            return Ok(());
        }
        let emitted = self
            .read_records_locked()?
            .into_iter()
            .filter(|event| {
                event
                    .cutex
                    .as_ref()
                    .is_some_and(|message| message.method == "cutex/stream/reset")
            })
            .map(|event| event.cutex_session_id)
            .collect::<HashSet<_>>();
        let mut pending = self
            .recovery_reset
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 recovery-reset lock was poisoned"))?;
        if let Some(pending) = pending.as_mut() {
            pending.emitted_session_ids.extend(emitted);
            self.persist_pending_recovery_reset_locked(pending)?;
        }
        Ok(())
    }

    fn prepare_root(&self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.root).with_context(|| {
            format!(
                "Failed to create management v2 repository: {}",
                self.root.display()
            )
        })?;
        secure_directory(&self.root)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> &AppendStats {
        &self.stats
    }

    fn with_repository_lock<T>(
        &self,
        action: impl FnOnce(&Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut timing = super::control_timing::Trace::metrics_only(super::control_timing::Path::EventRepositoryLock);
        timing.stage(super::control_timing::Stage::LockWait);
        let waiting = std::time::Instant::now();
        let _process_guard = self
            .process_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 process lock was poisoned"))?;
        timing.stage(super::control_timing::Stage::ContextRead);
        let lock_path = self.root.join(LOCK_FILE_NAME);
        let file = open_private_file(&lock_path, false)?;
        timing.stage(super::control_timing::Stage::LockWait);
        file.lock().with_context(|| {
            format!(
                "Failed to lock management v2 repository: {}",
                lock_path.display()
            )
        })?;
        let holding = std::time::Instant::now();
        AppendStats::add(&self.stats.lock_wait_ns, (holding - waiting).as_nanos() as u64);
        timing.stage(super::control_timing::Stage::Dispatch);
        let result = action(self);
        AppendStats::add(&self.stats.lock_hold_ns, holding.elapsed().as_nanos() as u64);
        if let Err(error) = file.unlock() {
            if result.is_ok() {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to unlock management v2 repository: {}",
                        lock_path.display()
                    )
                });
            }
        }
        result
    }

    fn with_replay_lock<T>(
        &self,
        action: impl FnOnce(&Self) -> Result<T, ReplayError>,
    ) -> Result<T, ReplayError> {
        let mut timing = super::control_timing::Trace::metrics_only(super::control_timing::Path::EventReplayLock);
        timing.stage(super::control_timing::Stage::LockWait);
        let _process_guard = self.process_lock.lock().map_err(|_| {
            ReplayError::Repository(anyhow::anyhow!("management v2 process lock was poisoned"))
        })?;
        timing.stage(super::control_timing::Stage::ContextRead);
        let lock_path = self.root.join(LOCK_FILE_NAME);
        let file = open_private_file(&lock_path, false).map_err(ReplayError::from)?;
        timing.stage(super::control_timing::Stage::LockWait);
        file.lock().map_err(|error| {
            ReplayError::Repository(anyhow::Error::new(error).context(format!(
                "Failed to lock management v2 repository: {}",
                lock_path.display()
            )))
        })?;
        timing.stage(super::control_timing::Stage::Dispatch);
        let result = action(self);
        if let Err(error) = file.unlock() {
            if result.is_ok() {
                return Err(ReplayError::Repository(anyhow::Error::new(error).context(
                    format!(
                        "Failed to unlock management v2 repository: {}",
                        lock_path.display()
                    ),
                )));
            }
        }
        result
    }

    fn load_append_state_locked(
        &self,
    ) -> anyhow::Result<(StreamState, Option<StreamRecoveryReset>)> {
        if let Ok(Some(state)) = self.try_load_append_state_locked() {
            return Ok((state, None));
        }
        self.load_or_recover_state_locked()
    }

    fn load_snapshot_locked(
        &self,
    ) -> anyhow::Result<(RepositorySnapshot, Option<StreamRecoveryReset>)> {
        if let Ok(Some(snapshot)) = self.try_load_snapshot_locked() {
            return Ok((snapshot, None));
        }
        let (state, reset) = self.load_or_recover_state_locked()?;
        let snapshot = self
            .try_snapshot_for_state_locked(state)?
            .context("recovered management v2 state has inconsistent retained boundaries")?;
        Ok((snapshot, reset))
    }

    fn try_load_snapshot_locked(&self) -> anyhow::Result<Option<RepositorySnapshot>> {
        let state = match self.try_read_state_locked()? {
            Some(state) => state,
            None => return Ok(None),
        };
        self.try_snapshot_for_state_locked(state)
    }

    fn try_snapshot_for_state_locked(
        &self,
        mut state: StreamState,
    ) -> anyhow::Result<Option<RepositorySnapshot>> {
        let earliest = self.read_first_record_locked()?;
        let latest = self.read_last_record_locked()?;
        // The state file is a checkpoint: it may lag the journal, never lead.
        let floor = self.known_next_sequence(&state.stream_id, state.next_sequence);
        match (earliest, latest) {
            (None, None) if floor == 1 => Ok(Some(RepositorySnapshot {
                state,
                earliest: None,
                latest: None,
            })),
            (Some(earliest), Some(latest))
                if earliest.stream_id == state.stream_id
                    && latest.stream_id == state.stream_id
                    && earliest.sequence <= latest.sequence
                    && floor <= latest.sequence.saturating_add(1) =>
            {
                state.next_sequence = latest.sequence.saturating_add(1);
                Ok(Some(RepositorySnapshot {
                    state,
                    earliest: Some(EventStreamBoundary {
                        sequence: earliest.sequence,
                        cursor: earliest.cursor,
                    }),
                    latest: Some(EventStreamBoundary {
                        sequence: latest.sequence,
                        cursor: latest.cursor,
                    }),
                }))
            }
            _ => Ok(None),
        }
    }

    fn try_read_state_locked(&self) -> anyhow::Result<Option<StreamState>> {
        let state_path = self.root.join(STATE_FILE_NAME);
        let bytes = match fs::read(&state_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to read stream state: {}", state_path.display())
                })
            }
        };
        Ok(match serde_json::from_slice::<StreamState>(&bytes) {
            Ok(state) if state.version == 1 => Some(state),
            Ok(_) | Err(_) => None,
        })
    }

    /// The append position from the state checkpoint and the journal tail
    /// (one small file read and one tail read), or `None` when they do not
    /// agree and the full recovery has to decide.
    fn try_load_append_state_locked(&self) -> anyhow::Result<Option<StreamState>> {
        let mut state = match self.try_read_state_locked()? {
            Some(state) => state,
            None => return Ok(None),
        };
        let floor = self.known_next_sequence(&state.stream_id, state.next_sequence);
        let Some(last) = self.read_last_record_locked()? else {
            return Ok((floor == 1).then_some(state));
        };
        let expected_next = last.sequence.saturating_add(1);
        if state.stream_id != last.stream_id || floor > expected_next {
            return Ok(None);
        }
        state.next_sequence = expected_next;
        Ok(Some(state))
    }

    fn load_or_recover_state_locked(
        &self,
    ) -> anyhow::Result<(StreamState, Option<StreamRecoveryReset>)> {
        self.load_or_recover_state_with_locked(false)
    }

    /// `validate_cold` also parses every cold record (after a failed replay,
    /// so a corrupt old file resets the stream instead of failing each page).
    fn load_or_recover_state_with_locked(
        &self,
        validate_cold: bool,
    ) -> anyhow::Result<(StreamState, Option<StreamRecoveryReset>)> {
        let state_path = self.root.join(STATE_FILE_NAME);
        let mut state_parse_error = None;
        let loaded_state = match fs::read(&state_path) {
            Ok(bytes) => match serde_json::from_slice::<StreamState>(&bytes) {
                Ok(state) => Some(state),
                Err(error) => {
                    state_parse_error = Some(error);
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to read stream state: {}", state_path.display())
                })
            }
        };
        // A journal tail behind what this process already made durable is a
        // lost journal, however far the on-disk checkpoint lags.
        let loaded_state = loaded_state.map(|mut state| {
            state.next_sequence = self.known_next_sequence(&state.stream_id, state.next_sequence);
            state
        });
        let scan = match self.scan_records_locked(false, validate_cold) {
            Ok(scan) => scan,
            Err(error) => {
                return self.reset_uncertain_stream_locked(
                    loaded_state.as_ref(),
                    None,
                    "protocol_violation",
                    format!("failed to read committed journal: {error:#}"),
                );
            }
        };
        if let Some(error) = state_parse_error {
            let prior = scan.last.as_ref().map(|event| StreamState {
                version: 1,
                stream_id: scan
                    .stream_id
                    .clone()
                    .expect("nonempty journal scan has a stream id"),
                next_sequence: event.sequence.saturating_add(1),
            });
            return self.reset_uncertain_stream_locked(
                prior.as_ref(),
                scan.last.as_ref().map(|event| event.sequence),
                "protocol_violation",
                format!("invalid stream state: {error}"),
            );
        }
        if scan.last.is_none() {
            if let Some(state) = loaded_state {
                if state.version == 1 && state.next_sequence == 1 {
                    return Ok((state, None));
                }
                return self.reset_uncertain_stream_locked(
                    Some(&state),
                    None,
                    "state_lost",
                    "stream state exists without its committed journal".to_string(),
                );
            }
            let state = StreamState::new();
            self.write_state_locked(&state)?;
            return Ok((state, None));
        }

        let last = scan.last.as_ref().expect("non-empty journal scan");
        let journal_stream_id = scan
            .stream_id
            .as_ref()
            .expect("non-empty journal scan has a stream id");
        let mut state = match loaded_state {
            Some(state) => state,
            None => StreamState {
                version: 1,
                stream_id: journal_stream_id.clone(),
                next_sequence: last.sequence.saturating_add(1),
            },
        };
        if state.version != 1 || state.stream_id != *journal_stream_id {
            return self.reset_uncertain_stream_locked(
                Some(&state),
                Some(last.sequence),
                "protocol_violation",
                "stream state and journal disagree".to_string(),
            );
        }
        let recovered_next = last.sequence.saturating_add(1);
        if state.next_sequence > recovered_next {
            return self.reset_uncertain_stream_locked(
                Some(&state),
                Some(last.sequence),
                "state_lost",
                "stream state advances beyond the committed journal".to_string(),
            );
        }
        if state.next_sequence != recovered_next {
            state.next_sequence = recovered_next;
            self.write_state_locked(&state)?;
        }
        Ok((state, None))
    }

    fn reset_uncertain_stream_locked(
        &self,
        prior_state: Option<&StreamState>,
        last_sequence: Option<u64>,
        reason: &str,
        diagnostic: String,
    ) -> anyhow::Result<(StreamState, Option<StreamRecoveryReset>)> {
        let orphaned = self.root.join(ORPHANED_DIR_NAME);
        fs::create_dir_all(&orphaned).with_context(|| {
            format!(
                "Failed to create orphaned stream dir: {}",
                orphaned.display()
            )
        })?;
        secure_directory(&orphaned)?;
        let quarantine_id = Uuid::new_v4();
        for (index, path) in self.read_paths().into_iter().enumerate() {
            if !path.exists() {
                continue;
            }
            let destination = orphaned.join(format!("{quarantine_id}-{index}.jsonl"));
            fs::rename(&path, &destination).with_context(|| {
                format!(
                    "Failed to quarantine uncertain stream {} to {}",
                    path.display(),
                    destination.display()
                )
            })?;
        }
        *self
            .replay_index
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 replay-index lock was poisoned"))? =
            ReplayIndex::default();
        let state = StreamState::new();
        self.write_state_locked(&state)?;
        let reset = StreamRecoveryReset {
            previous_stream_id: prior_state.map(|state| state.stream_id.clone()),
            previous_last_sequence: last_sequence,
            stream_id: state.stream_id.clone(),
            reason: reason.to_string(),
            detected_at: Utc::now().to_rfc3339(),
            diagnostic,
        };
        Ok((state, Some(reset)))
    }

    fn record_recovery_reset(&self, reset: Option<StreamRecoveryReset>) -> anyhow::Result<()> {
        if let Some(reset) = reset {
            let pending = PendingStreamReset {
                reset,
                emitted_session_ids: HashSet::new(),
            };
            self.persist_pending_recovery_reset_locked(&pending)?;
            *self
                .recovery_reset
                .lock()
                .map_err(|_| anyhow::anyhow!("management v2 recovery-reset lock was poisoned"))? =
                Some(pending);
        }
        Ok(())
    }

    fn restore_pending_recovery_reset_locked(&self, state: &StreamState) -> anyhow::Result<()> {
        let path = self.root.join(PENDING_RESET_FILE_NAME);
        let pending = match fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<PendingStreamReset>(&bytes).with_context(|| {
                    format!(
                        "Failed to parse pending management v2 stream reset: {}",
                        path.display()
                    )
                })?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to read pending management v2 stream reset: {}",
                        path.display()
                    )
                })
            }
        };
        match pending {
            Some(pending) if pending.reset.stream_id == state.stream_id => {
                *self.recovery_reset.lock().map_err(|_| {
                    anyhow::anyhow!("management v2 recovery-reset lock was poisoned")
                })? = Some(pending);
                self.reconcile_emitted_reset_sessions_locked()?;
            }
            Some(_) => {
                remove_file_if_present(&path)?;
            }
            None => {}
        }
        Ok(())
    }

    fn persist_pending_recovery_reset_locked(
        &self,
        pending: &PendingStreamReset,
    ) -> anyhow::Result<()> {
        write_private_pretty_json_atomic(
            &self.root.join(PENDING_RESET_FILE_NAME),
            pending,
            "pending management v2 stream reset",
        )
    }

    fn clear_pending_recovery_reset_locked(&self) -> anyhow::Result<()> {
        *self
            .recovery_reset
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 recovery-reset lock was poisoned"))? = None;
        remove_file_if_present(&self.root.join(PENDING_RESET_FILE_NAME))
    }

    fn read_records_locked(&self) -> anyhow::Result<Vec<EventEnvelope>> {
        Ok(self.scan_records_locked(true, true)?.records)
    }

    /// Reads the retained journal and rebuilds the replay index. Hot files are
    /// parsed and indexed whole; cold ones only boundary-checked unless
    /// `collect_records` or `validate_cold` (repair after a failed replay).
    fn scan_records_locked(
        &self,
        collect_records: bool,
        validate_cold: bool,
    ) -> anyhow::Result<JournalScan> {
        let hot_start = self.options.hot_start();
        let mut records = Vec::new();
        let mut files = Vec::with_capacity(self.options.retained_files + 1);
        let mut cursors = HashMap::new();
        let mut stream_id = None;
        let mut last_record = None;
        let mut expected_sequence = None;
        for (file_index, path) in self.read_paths().into_iter().enumerate() {
            if file_index < hot_start && !(collect_records || validate_cold) {
                let stamp = cold_file_stamp(&path, &mut stream_id, &mut expected_sequence)?;
                if stamp.last.is_some() {
                    last_record = stamp.last.clone();
                }
                files.push(stamp);
                continue;
            }
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    files.push(JournalFileStamp {
                        length: 0,
                        first: None,
                        last: None,
                    });
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to open management v2 log: {}", path.display())
                    })
                }
            };
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            let mut offset = 0_u64;
            let mut first = None;
            let mut last = None;
            loop {
                line.clear();
                let read = reader.read_line(&mut line).with_context(|| {
                    format!("Failed to read management v2 log: {}", path.display())
                })?;
                if read == 0 {
                    break;
                }
                if !line.ends_with('\n') {
                    anyhow::bail!("management v2 log has a partial tail: {}", path.display());
                }
                let event: EventEnvelope =
                    serde_json::from_str(line.trim_end()).with_context(|| {
                        format!("Invalid management v2 event in {}", path.display())
                    })?;
                event.validate().with_context(|| {
                    format!("Invalid management v2 envelope in {}", path.display())
                })?;
                match (stream_id.as_deref(), expected_sequence) {
                    (Some(expected_stream_id), Some(expected)) => {
                        if event.stream_id != expected_stream_id {
                            anyhow::bail!("retained management v2 log mixes stream ids");
                        }
                        if event.sequence != expected {
                            anyhow::bail!(
                                "retained management v2 sequence gap: expected {expected}, found {}",
                                event.sequence
                            );
                        }
                    }
                    (None, None) => stream_id = Some(event.stream_id.clone()),
                    _ => unreachable!("journal scan stream and sequence state diverged"),
                }
                expected_sequence = Some(event.sequence.saturating_add(1));
                offset = offset.saturating_add(read as u64);
                let boundary = boundary_for_event(&event);
                first.get_or_insert_with(|| boundary.clone());
                last = Some(boundary.clone());
                last_record = Some(boundary);
                if file_index >= hot_start {
                    cursors
                        .entry(event.cursor.clone())
                        .or_insert(ReplayLocation {
                            file_index,
                            next_offset: offset,
                            sequence: event.sequence,
                        });
                }
                if collect_records {
                    records.push(event);
                }
            }
            files.push(JournalFileStamp {
                length: offset,
                first,
                last,
            });
        }
        *self
            .replay_index
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 replay-index lock was poisoned"))? =
            ReplayIndex {
                stream_id: stream_id.clone(),
                files,
                cursors,
            };
        Ok(JournalScan {
            stream_id,
            last: last_record,
            records,
        })
    }

    fn read_first_record_locked(&self) -> anyhow::Result<Option<EventEnvelope>> {
        // One directory listing instead of failed opens of every empty slot
        // below the oldest retained file (this runs for every page).
        let present = match fs::read_dir(&self.root) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                .collect::<HashSet<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to list management v2 log: {}", self.root.display())
                })
            }
        };
        let paths = self.read_paths().into_iter().filter(|path| {
            path.file_name().is_some_and(|name| present.contains(name))
        });
        for path in paths {
            if let Some(encoded) = read_first_complete_line(&path)? {
                return parse_record(&path, &encoded).map(Some);
            }
        }
        Ok(None)
    }

    fn read_last_record_locked(&self) -> anyhow::Result<Option<EventEnvelope>> {
        let paths = std::iter::once(self.root.join(EVENTS_FILE_NAME))
            .chain((1..=self.options.retained_files).map(|index| self.rotated_path(index)));
        for path in paths {
            let Some(encoded) = read_last_complete_line(&path)? else {
                continue;
            };
            return parse_record(&path, &encoded).map(Some);
        }
        Ok(None)
    }

    /// Stamps of every journal slot. Cold files change only by a rotation,
    /// which always changes a hot sealed file, so while those match the
    /// index the cold stamps are taken from it without touching the disk.
    fn current_stamps_locked(&self, paths: &[PathBuf]) -> anyhow::Result<Vec<JournalFileStamp>> {
        let hot_start = self.options.hot_start().min(paths.len());
        let mut stamps = paths[hot_start..]
            .iter()
            .map(|path| file_stamp(path))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if hot_start == 0 {
            return Ok(stamps);
        }
        let indexed_cold = self
            .replay_index
            .lock()
            .ok()
            .filter(|index| {
                index.files.len() == paths.len()
                    && index.files[hot_start..paths.len() - 1] == stamps[..stamps.len() - 1]
            })
            .map(|index| index.files[..hot_start].to_vec());
        let mut cold = match indexed_cold {
            Some(cold) => cold,
            None => paths[..hot_start]
                .iter()
                .map(|path| file_stamp(path))
                .collect::<anyhow::Result<Vec<_>>>()?,
        };
        cold.append(&mut stamps);
        Ok(cold)
    }

    fn ensure_replay_index_locked(&self, stream_id: &str) -> anyhow::Result<()> {
        let paths = self.read_paths();
        let current_files = self.current_stamps_locked(&paths)?;
        {
            let mut index = self
                .replay_index
                .lock()
                .map_err(|_| anyhow::anyhow!("management v2 replay-index lock was poisoned"))?;
            if index.stream_id.as_deref() == Some(stream_id) && index.files == current_files {
                return Ok(());
            }
            if index.stream_id.as_deref() == Some(stream_id)
                && self.try_extend_replay_index_locked(
                    &mut index,
                    &paths,
                    &current_files,
                    stream_id,
                )?
            {
                return Ok(());
            }
        }

        self.scan_records_locked(false, false)?;
        let mut index = self
            .replay_index
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 replay-index lock was poisoned"))?;
        match index.stream_id.as_deref() {
            Some(indexed_stream_id) if indexed_stream_id != stream_id => {
                anyhow::bail!(
                    "management v2 replay index stream {indexed_stream_id} differs from active stream {stream_id}"
                );
            }
            Some(_) => {}
            None => index.stream_id = Some(stream_id.to_string()),
        }
        Ok(())
    }

    fn try_extend_replay_index_locked(
        &self,
        index: &mut ReplayIndex,
        paths: &[PathBuf],
        current_files: &[JournalFileStamp],
        stream_id: &str,
    ) -> anyhow::Result<bool> {
        if index.files.len() != current_files.len() || current_files.is_empty() {
            return Ok(false);
        }
        let active_index = current_files.len() - 1;
        if index.files[..active_index] != current_files[..active_index] {
            return Ok(false);
        }
        let previous = &index.files[active_index];
        let current = &current_files[active_index];
        if current.length <= previous.length
            || (previous.length > 0 && previous.first != current.first)
        {
            return Ok(false);
        }
        if previous.length > 0 {
            let Some(encoded) =
                read_complete_line_ending_at(&paths[active_index], previous.length)?
            else {
                return Ok(false);
            };
            let previous_tail = parse_record(&paths[active_index], &encoded)?;
            if Some(boundary_for_event(&previous_tail)) != previous.last {
                return Ok(false);
            }
        }

        let mut file = File::open(&paths[active_index]).with_context(|| {
            format!(
                "Failed to open management v2 log: {}",
                paths[active_index].display()
            )
        })?;
        file.seek(SeekFrom::Start(previous.length))
            .with_context(|| {
                format!(
                    "Failed to seek management v2 log: {}",
                    paths[active_index].display()
                )
            })?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut offset = previous.length;
        let mut expected_sequence = index
            .files
            .iter()
            .rev()
            .find_map(|file| file.last.as_ref().map(|boundary| boundary.sequence))
            .map_or(1, |sequence| sequence.saturating_add(1));
        loop {
            line.clear();
            let read = reader.read_line(&mut line).with_context(|| {
                format!(
                    "Failed to read management v2 log: {}",
                    paths[active_index].display()
                )
            })?;
            if read == 0 {
                break;
            }
            if !line.ends_with('\n') {
                anyhow::bail!(
                    "management v2 log has a partial tail: {}",
                    paths[active_index].display()
                );
            }
            let event = parse_record(&paths[active_index], line.trim_end().as_bytes())?;
            if event.stream_id != stream_id || event.sequence != expected_sequence {
                anyhow::bail!(
                    "management v2 incremental replay index expected stream {stream_id} sequence {expected_sequence}, found stream {} sequence {}",
                    event.stream_id,
                    event.sequence
                );
            }
            offset = offset.saturating_add(read as u64);
            index
                .cursors
                .entry(event.cursor.clone())
                .or_insert(ReplayLocation {
                    file_index: active_index,
                    next_offset: offset,
                    sequence: event.sequence,
                });
            expected_sequence = expected_sequence.saturating_add(1);
        }
        if offset != current.length {
            anyhow::bail!(
                "management v2 incremental replay index stopped at {offset}, expected {}",
                current.length
            );
        }
        index.files[active_index] = current.clone();
        Ok(true)
    }

    fn read_indexed_records_locked(
        &self,
        start: ReplayLocation,
        limit: usize,
        stream_id: &str,
    ) -> anyhow::Result<Vec<(EventEnvelope, ReplayLocation)>> {
        let paths = self.read_paths();
        if start.file_index >= paths.len() {
            anyhow::bail!(
                "management v2 replay index file {} is outside the retained journal",
                start.file_index
            );
        }
        let mut records = Vec::with_capacity(limit);
        let mut expected_sequence = start.sequence.saturating_add(1);
        for (file_index, path) in paths.iter().enumerate().skip(start.file_index) {
            let file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to open management v2 log: {}", path.display())
                    })
                }
            };
            let mut reader = BufReader::new(file);
            let mut offset = if file_index == start.file_index { start.next_offset } else { 0 };
            if file_index == start.file_index {
                reader
                    .seek(SeekFrom::Start(start.next_offset))
                    .with_context(|| {
                        format!("Failed to seek management v2 log: {}", path.display())
                    })?;
            }
            let mut line = String::new();
            loop {
                line.clear();
                let read = reader.read_line(&mut line).with_context(|| {
                    format!("Failed to read management v2 log: {}", path.display())
                })?;
                if read == 0 {
                    break;
                }
                if !line.ends_with('\n') {
                    anyhow::bail!("management v2 log has a partial tail: {}", path.display());
                }
                let mut event = parse_record(path, line.trim_end().as_bytes())?;
                if event.stream_id != stream_id || event.sequence != expected_sequence {
                    anyhow::bail!(
                        "management v2 bounded replay expected stream {stream_id} sequence {expected_sequence}, found stream {} sequence {}",
                        event.stream_id,
                        event.sequence
                    );
                }
                expected_sequence = expected_sequence.saturating_add(1);
                offset = offset.saturating_add(read as u64);
                let location = ReplayLocation { file_index, next_offset: offset, sequence: event.sequence };
                let blobs = super::event_blobs::EventBlobStore::at(self.root.join("event-blobs"));
                if let Some(native) = &mut event.native {
                    super::native_errors::annotate_notification(&mut native.message);
                    blobs.normalize(&event.cutex_session_id, &mut native.message)?;
                }
                if let Some(cutex) = &mut event.cutex { blobs.normalize_cutex(&event.cutex_session_id, &mut cutex.params)?; }
                records.push((event, location));
                if records.len() == limit {
                    return Ok(records);
                }
            }
        }
        Ok(records)
    }

    fn validate_indexed_cursor_locked(
        &self,
        cursor: &str,
        location: ReplayLocation,
        stream_id: &str,
    ) -> anyhow::Result<()> {
        let paths = self.read_paths();
        let path = paths.get(location.file_index).with_context(|| {
            format!(
                "management v2 replay index file {} is outside the retained journal",
                location.file_index
            )
        })?;
        let encoded =
            read_complete_line_ending_at(path, location.next_offset)?.with_context(|| {
                format!(
                "management v2 replay index cursor {cursor} has no complete record at offset {}",
                location.next_offset
            )
            })?;
        let event = parse_record(path, &encoded)?;
        if event.stream_id != stream_id
            || event.cursor != cursor
            || event.sequence != location.sequence
        {
            anyhow::bail!(
                "management v2 replay index cursor {cursor} expected stream {stream_id} sequence {}, found cursor {} stream {} sequence {}",
                location.sequence,
                event.cursor,
                event.stream_id,
                event.sequence
            );
        }
        Ok(())
    }

    /// A cursor older than the hot index: searched in the cold files, newest
    /// first, and indexed when found. `None` means it is not retained.
    fn find_cold_cursor_locked(
        &self,
        cursor: &str,
        stream_id: &str,
    ) -> anyhow::Result<Option<ReplayLocation>> {
        let hot_start = self.options.hot_start();
        if hot_start == 0 || cursor.is_empty() {
            return Ok(None);
        }
        let search = ByteSearch::new(format!("\"cursor\":{}", serde_json::to_string(cursor)?).into_bytes());
        let paths = self.read_paths();
        for file_index in (0..hot_start).rev() {
            let Some((event, next_offset)) = find_cursor_in_file(&paths[file_index], &search, cursor)?
            else {
                continue;
            };
            if event.stream_id != stream_id {
                anyhow::bail!(
                    "management v2 cold cursor {cursor} belongs to stream {}, not {stream_id}",
                    event.stream_id
                );
            }
            let location = ReplayLocation { file_index, next_offset, sequence: event.sequence };
            if let Ok(mut index) = self.replay_index.lock() {
                index.cursors.insert(cursor.to_string(), location);
            }
            return Ok(Some(location));
        }
        Ok(None)
    }

    fn record_indexed_append(
        &self,
        envelope: &EventEnvelope,
        rotated: bool,
        start_offset: u64,
        next_offset: u64,
    ) {
        let Ok(mut index) = self.replay_index.lock() else {
            return;
        };
        if rotated {
            let file_count = self.options.retained_files + 1;
            if index.files.len() != file_count
                || index.stream_id.as_deref() != Some(envelope.stream_id.as_str())
                || start_offset != 0
            {
                *index = ReplayIndex::default();
                return;
            }
            let boundary = boundary_for_event(envelope);
            let mut shifted_files = index.files[1..].to_vec();
            shifted_files.push(JournalFileStamp {
                length: next_offset,
                first: Some(boundary.clone()),
                last: Some(boundary),
            });
            let paths = self.read_paths();
            let hot_start = self.options.hot_start();
            // Cold slots: only their lengths (the rotation may have pruned
            // expired ones, which are then empty); hot slots: full stamps.
            let actual_files = paths
                .iter()
                .enumerate()
                .map(|(slot, path)| {
                    if slot >= hot_start {
                        return file_stamp(path);
                    }
                    let length = match fs::metadata(path) {
                        Ok(metadata) => metadata.len(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                        Err(error) => return Err(error.into()),
                    };
                    Ok(match &shifted_files[slot] {
                        shifted if length == shifted.length => shifted.clone(),
                        _ => JournalFileStamp { length, first: None, last: None },
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>();
            let Ok(actual_files) = actual_files else {
                *index = ReplayIndex::default();
                return;
            };
            for (shifted, actual) in shifted_files.iter_mut().zip(&actual_files).take(hot_start) {
                if actual.length == 0 {
                    *shifted = actual.clone();
                }
            }
            // Later lines of the same batch may already follow this one in
            // the new active file; they are indexed by the calls after this.
            let active = file_count - 1;
            let active_matches = actual_files.get(active).is_some_and(|actual| {
                actual.first == shifted_files[active].first && actual.length >= next_offset
            });
            if actual_files.len() != file_count
                || shifted_files[..active] != actual_files[..active]
                || !active_matches
            {
                *index = ReplayIndex::default();
                return;
            }
            // Cursors of files that turned cold leave the index (a cold
            // cursor is found by search), as do those of the dropped file.
            index.cursors.retain(|_, location| {
                if location.file_index == 0 || location.file_index - 1 < hot_start {
                    false
                } else {
                    location.file_index -= 1;
                    true
                }
            });
            index.files = shifted_files;
            index
                .cursors
                .entry(envelope.cursor.clone())
                .or_insert(ReplayLocation {
                    file_index: file_count - 1,
                    next_offset,
                    sequence: envelope.sequence,
                });
            return;
        }
        let file_count = self.options.retained_files + 1;
        if index.files.len() != file_count {
            return;
        }
        if index.stream_id.is_none()
            && start_offset == 0
            && index.files.iter().all(|file| file.length == 0)
        {
            index.stream_id = Some(envelope.stream_id.clone());
        }
        if index.stream_id.as_deref() != Some(envelope.stream_id.as_str()) {
            return;
        }
        let expected_sequence = index
            .files
            .iter()
            .rev()
            .find_map(|file| file.last.as_ref().map(|boundary| boundary.sequence))
            .map_or(1, |sequence| sequence.saturating_add(1));
        let active_index = file_count - 1;
        if index.files[active_index].length != start_offset
            || envelope.sequence != expected_sequence
        {
            return;
        }
        let boundary = boundary_for_event(envelope);
        let active = &mut index.files[active_index];
        active.length = next_offset;
        active.first.get_or_insert_with(|| boundary.clone());
        active.last = Some(boundary);
        index
            .cursors
            .entry(envelope.cursor.clone())
            .or_insert(ReplayLocation {
                file_index: active_index,
                next_offset,
                sequence: envelope.sequence,
            });
    }

    fn read_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(self.options.retained_files + 1);
        for index in (1..=self.options.retained_files).rev() {
            paths.push(self.rotated_path(index));
        }
        paths.push(self.root.join(EVENTS_FILE_NAME));
        paths
    }

    fn rotate_files_locked(&self) -> anyhow::Result<()> {
        let active = self.root.join(EVENTS_FILE_NAME);
        let oldest = self.rotated_path(self.options.retained_files);
        remove_file_if_present(&oldest)?;
        for index in (1..self.options.retained_files).rev() {
            let from = self.rotated_path(index);
            if !from.exists() {
                continue;
            }
            let to = self.rotated_path(index + 1);
            remove_file_if_present(&to)?;
            fs::rename(&from, &to).with_context(|| {
                format!(
                    "Failed to rotate management v2 log {} to {}",
                    from.display(),
                    to.display()
                )
            })?;
        }
        let first = self.rotated_path(1);
        remove_file_if_present(&first)?;
        fs::rename(&active, &first).with_context(|| {
            format!(
                "Failed to rotate management v2 log {} to {}",
                active.display(),
                first.display()
            )
        })?;
        self.prune_expired_files_locked(Utc::now())
    }

    /// Removes the oldest sealed files whose newest event is at least
    /// [`RETENTION_AGE`] old, keeping the newest [`MIN_SEALED_FILES`]. Stops
    /// at the first file still inside the window (or one it cannot date).
    fn prune_expired_files_locked(&self, now: chrono::DateTime<Utc>) -> anyhow::Result<()> {
        let Ok(age) = chrono::Duration::from_std(RETENTION_AGE) else {
            return Ok(());
        };
        let cutoff = now - age;
        for index in (MIN_SEALED_FILES + 1..=self.options.retained_files).rev() {
            let path = self.rotated_path(index);
            let newest = match read_last_complete_line(&path) {
                Ok(Some(encoded)) => parse_record(&path, &encoded).ok(),
                Ok(None) => continue,
                Err(_) => None,
            };
            let expired = newest
                .and_then(|event| chrono::DateTime::parse_from_rfc3339(&event.received_at).ok())
                .is_some_and(|received| received <= cutoff);
            if !expired {
                break;
            }
            remove_file_if_present(&path)?;
        }
        Ok(())
    }

    fn rotated_path(&self, index: usize) -> PathBuf {
        self.root.join(format!("{EVENTS_FILE_NAME}.{index}"))
    }

    fn write_state_locked(&self, state: &StreamState) -> anyhow::Result<()> {
        AppendStats::add(&self.stats.state_writes, 1);
        write_private_pretty_json_atomic(
            &self.root.join(STATE_FILE_NAME),
            state,
            "management v2 stream state",
        )
    }

    fn publish(&self, envelope: &EventEnvelope, encoded_bytes: Option<usize>) {
        let mut timing = super::control_timing::Trace::metrics_only(super::control_timing::Path::EventFanout);
        timing.stage(super::control_timing::Stage::Fanout);
        self.live_deltas.observe(envelope);
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return;
        };
        // Local appends reuse the journal encoding. Only external follower
        // events need a no-allocation counting writer, once per fanout.
        let bytes = if subscribers.is_empty() { 0 } else {
            encoded_bytes.unwrap_or_else(|| super::event_blobs::encoded_len(envelope))
        };
        subscribers.retain(|(sender, lagged, live, wake)| {
            if live.strong_count() == 0 { return false; }
            match sender.try_send(QueuedEvent::new(envelope.clone(), bytes, &self.subscription_metrics)) {
                Ok(()) => { wake.notify(); true },
                Err(mpsc::TrySendError::Full(_)) => {
                    lagged.store(true, Ordering::SeqCst);
                    wake.notify();
                    false
                }
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            }
        });
        *self.live_checkpoint.lock().unwrap_or_else(|e| e.into_inner()) = Some(EventCheckpoint {
            stream_id: envelope.stream_id.clone(), sequence: envelope.sequence, cursor: Some(envelope.cursor.clone()),
        });
    }
}

/// The leader's hand-over, also on unwind: publishes the batch results
/// (an aborted batch fails its appends instead of stranding them) and lets
/// the next waiter lead.
struct BatchLeadership<'a> {
    repository: &'a EventRepository,
    tickets: Vec<u64>,
    results: Vec<(u64, anyhow::Result<EventEnvelope>)>,
}

impl Drop for BatchLeadership<'_> {
    fn drop(&mut self) {
        let mut queue = self.repository.lock_commits();
        for (ticket, result) in self.results.drain(..) {
            queue.done.insert(ticket, result);
        }
        for ticket in &self.tickets {
            queue.done.entry(*ticket).or_insert_with(|| {
                Err(anyhow::anyhow!("management v2 append batch was aborted"))
            });
        }
        queue.leader = false;
        queue.active_batch_items = 0;
        drop(queue);
        self.repository.committed.notify_all();
    }
}

fn reset_event(host_id: &str, cutex_session_id: &str, reset: &StreamRecoveryReset) -> PendingEvent {
    PendingEvent {
        cutex_session_id: cutex_session_id.to_string(),
        host_id: host_id.to_string(),
        source: super::model::EventSource::Cutex,
        schema: None,
        correlation: super::model::EventCorrelation::default(),
        native: None,
        cutex: Some(super::model::CutexMessage {
            method: "cutex/stream/reset".to_string(),
            params: serde_json::json!({
                "previousStreamId": reset.previous_stream_id,
                "previousLastSequence": reset.previous_last_sequence.unwrap_or(0),
                "reason": reset.reason,
                "lostEventCount": null,
                "resyncRequired": true,
                "detectedAt": reset.detected_at,
            }),
        }),
    }
}

fn read_last_complete_line(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to open management v2 log: {}", path.display()))
        }
    };
    let length = file
        .metadata()
        .with_context(|| format!("Failed to stat management v2 log: {}", path.display()))?
        .len();
    if length == 0 {
        return Ok(None);
    }
    drop(file);
    read_complete_line_ending_at(path, length)
}

fn read_first_complete_line(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to open management v2 log: {}", path.display()))
        }
    };
    let mut reader = BufReader::new(file);
    let mut encoded = Vec::new();
    let read = reader
        .read_until(b'\n', &mut encoded)
        .with_context(|| format!("Failed to read management v2 log: {}", path.display()))?;
    if read == 0 {
        return Ok(None);
    }
    if encoded.last() != Some(&b'\n') {
        anyhow::bail!("management v2 log has a partial tail: {}", path.display());
    }
    encoded.pop();
    Ok(Some(encoded))
}

fn read_complete_line_ending_at(path: &Path, end: u64) -> anyhow::Result<Option<Vec<u8>>> {
    if end == 0 {
        return Ok(None);
    }
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open management v2 log: {}", path.display()))?;
    let length = file
        .metadata()
        .with_context(|| format!("Failed to stat management v2 log: {}", path.display()))?
        .len();
    if end > length {
        anyhow::bail!(
            "management v2 log shrank below indexed offset {end}: {}",
            path.display()
        );
    }

    file.seek(SeekFrom::Start(end - 1))
        .with_context(|| format!("Failed to seek management v2 log: {}", path.display()))?;
    let mut terminal = [0_u8; 1];
    file.read_exact(&mut terminal)
        .with_context(|| format!("Failed to read management v2 log: {}", path.display()))?;
    if terminal[0] != b'\n' {
        anyhow::bail!("management v2 log has a partial tail: {}", path.display());
    }

    let line_end = end - 1;
    let mut cursor = line_end;
    let mut scan = [0_u8; 8 * 1024];
    let line_start = loop {
        if cursor == 0 {
            break 0;
        }
        let chunk_start = cursor.saturating_sub(scan.len() as u64);
        let chunk_len = (cursor - chunk_start) as usize;
        file.seek(SeekFrom::Start(chunk_start))
            .with_context(|| format!("Failed to seek management v2 log: {}", path.display()))?;
        file.read_exact(&mut scan[..chunk_len])
            .with_context(|| format!("Failed to read management v2 log: {}", path.display()))?;
        if let Some(index) = scan[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
            break chunk_start + index as u64 + 1;
        }
        cursor = chunk_start;
    };

    let line_len = (line_end - line_start) as usize;
    let mut encoded = vec![0_u8; line_len];
    file.seek(SeekFrom::Start(line_start))
        .with_context(|| format!("Failed to seek management v2 log: {}", path.display()))?;
    file.read_exact(&mut encoded)
        .with_context(|| format!("Failed to read management v2 log: {}", path.display()))?;
    Ok(Some(encoded))
}

fn parse_record(path: &Path, encoded: &[u8]) -> anyhow::Result<EventEnvelope> {
    let event: EventEnvelope = serde_json::from_slice(encoded)
        .with_context(|| format!("Invalid management v2 event in {}", path.display()))?;
    event
        .validate()
        .with_context(|| format!("Invalid management v2 envelope in {}", path.display()))?;
    Ok(event)
}

/// Boundary check of a cold journal file at open: its first record continues
/// the stream and its last record follows it. The lines between are checked
/// when a replay reads them (a failure there runs the full check).
fn cold_file_stamp(
    path: &Path,
    stream_id: &mut Option<String>,
    expected_sequence: &mut Option<u64>,
) -> anyhow::Result<JournalFileStamp> {
    let length = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to stat management v2 log: {}", path.display()))
        }
    };
    if length == 0 {
        return Ok(JournalFileStamp { length, first: None, last: None });
    }
    let read = |encoded: Option<Vec<u8>>| -> anyhow::Result<EventEnvelope> {
        let encoded = encoded.with_context(|| {
            format!("management v2 log has no complete record: {}", path.display())
        })?;
        parse_record(path, &encoded)
    };
    let first = read(read_first_complete_line(path)?)?;
    let last = read(read_complete_line_ending_at(path, length)?)?;
    match (stream_id.as_deref(), *expected_sequence) {
        (Some(expected_stream_id), Some(expected)) => {
            if first.stream_id != expected_stream_id {
                anyhow::bail!("retained management v2 log mixes stream ids");
            }
            if first.sequence != expected {
                anyhow::bail!(
                    "retained management v2 sequence gap: expected {expected}, found {}",
                    first.sequence
                );
            }
        }
        _ => *stream_id = Some(first.stream_id.clone()),
    }
    if last.stream_id != first.stream_id || last.sequence < first.sequence {
        anyhow::bail!("management v2 log boundaries disagree: {}", path.display());
    }
    *expected_sequence = Some(last.sequence.saturating_add(1));
    Ok(JournalFileStamp {
        length,
        first: Some(boundary_for_event(&first)),
        last: Some(boundary_for_event(&last)),
    })
}

/// Horspool search for one needle over many buffers.
struct ByteSearch {
    needle: Vec<u8>,
    skip: [usize; 256],
}

impl ByteSearch {
    fn new(needle: Vec<u8>) -> Self {
        let mut skip = [needle.len().max(1); 256];
        for (position, byte) in needle.iter().enumerate().take(needle.len().saturating_sub(1)) {
            skip[*byte as usize] = needle.len() - 1 - position;
        }
        Self { needle, skip }
    }

    fn find(&self, haystack: &[u8], from: usize) -> Option<usize> {
        let length = self.needle.len();
        let last = *self.needle.last()?;
        let mut position = from;
        while position + length <= haystack.len() {
            let tail = haystack[position + length - 1];
            if tail == last && haystack[position..position + length] == self.needle[..] {
                return Some(position);
            }
            position += self.skip[tail as usize];
        }
        None
    }
}

/// The record whose `cursor` is `cursor` in one journal file:
/// `(record, offset after its line)`. Every byte hit is confirmed by parsing
/// its line, since a payload may quote the same text.
fn find_cursor_in_file(
    path: &Path,
    search: &ByteSearch,
    cursor: &str,
) -> anyhow::Result<Option<(EventEnvelope, u64)>> {
    const CHUNK: usize = 8 * 1024 * 1024;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to open management v2 log: {}", path.display()))
        }
    };
    let mut buffer = Vec::with_capacity(CHUNK);
    // File offset of `buffer[0]`.
    let mut base = 0_u64;
    loop {
        let kept = buffer.len();
        buffer.resize(kept + CHUNK, 0);
        let read = file
            .read(&mut buffer[kept..])
            .with_context(|| format!("Failed to read management v2 log: {}", path.display()))?;
        buffer.truncate(kept + read);
        // Only whole lines are searched; a partial one waits for the next read.
        let complete = match buffer.iter().rposition(|byte| *byte == b'\n') {
            Some(newline) => newline + 1,
            None if read == 0 => return Ok(None),
            None => continue,
        };
        let mut from = 0;
        while let Some(hit) = search.find(&buffer[..complete], from) {
            let start = buffer[..hit].iter().rposition(|byte| *byte == b'\n').map_or(0, |n| n + 1);
            let end = hit
                + buffer[hit..complete]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .expect("complete region ends with a newline");
            let event = parse_record(path, &buffer[start..end])?;
            if event.cursor == cursor {
                return Ok(Some((event, base + end as u64 + 1)));
            }
            from = end + 1;
        }
        if read == 0 {
            return Ok(None);
        }
        buffer.drain(..complete);
        base += complete as u64;
    }
}

fn file_stamp(path: &Path) -> anyhow::Result<JournalFileStamp> {
    let length = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to stat management v2 log: {}", path.display()))
        }
    };
    if length == 0 {
        return Ok(JournalFileStamp {
            length,
            first: None,
            last: None,
        });
    }
    let first = read_first_complete_line(path)?
        .map(|encoded| parse_record(path, &encoded))
        .transpose()?
        .map(|event| boundary_for_event(&event));
    let last = read_complete_line_ending_at(path, length)?
        .map(|encoded| parse_record(path, &encoded))
        .transpose()?
        .map(|event| boundary_for_event(&event));
    Ok(JournalFileStamp {
        length,
        first,
        last,
    })
}

fn checkpoint_for_snapshot(snapshot: &RepositorySnapshot) -> EventCheckpoint {
    EventCheckpoint {
        stream_id: snapshot.state.stream_id.clone(),
        sequence: snapshot
            .latest
            .as_ref()
            .map_or(0, |boundary| boundary.sequence),
        cursor: snapshot
            .latest
            .as_ref()
            .map(|boundary| boundary.cursor.clone()),
    }
}

fn boundary_for_event(event: &EventEnvelope) -> RetainedBoundary {
    RetainedBoundary {
        sequence: event.sequence,
        cursor: event.cursor.clone(),
    }
}

fn retained_boundary(boundary: &EventStreamBoundary) -> RetainedBoundary {
    RetainedBoundary {
        sequence: boundary.sequence,
        cursor: boundary.cursor.clone(),
    }
}

fn open_private_file(path: &Path, append: bool) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("Failed to open private file: {}", path.display()))?;
    secure_file(path)?;
    Ok(file)
}

fn remove_file_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to remove management v2 file: {}", path.display())),
    }
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Failed to secure directory: {}", path.display()))
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to secure file: {}", path.display()))
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use serde_json::json;

    use super::*;
    use crate::management::v2::model::CutexMessage;
    use crate::management::v2::model::EventCorrelation;
    use crate::management::v2::model::EventSource;

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cutex-management-v2-{label}-{}", Uuid::new_v4()))
    }

    fn pending(session: &str, value: usize) -> PendingEvent {
        PendingEvent {
            cutex_session_id: session.to_string(),
            host_id: "devbox".to_string(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation::default(),
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/test/event".to_string(),
                params: json!({ "value": value, "explicitNull": null }),
            }),
        }
    }

    #[test]
    fn subscription_diagnostics_follow_receive_lag_and_drop() {
        let root = test_root("subscription-diagnostics");
        let repo = EventRepository::open(&root, "devbox").unwrap();
        let sub = repo.page_and_subscribe(ReplayQuery::default(), 1).unwrap();
        let other = repo.page_and_subscribe(ReplayQuery::default(), 2).unwrap();
        let first = repo.append(pending("s", 1)).unwrap();
        let bytes = serde_json::to_vec(&first).unwrap().len() as u64;
        let snapshot = repo.queue_diagnostics();
        assert_eq!(snapshot["durableSubscriptions"]["queuedEvents"], 2);
        assert_eq!(snapshot["durableSubscriptions"]["queuedEncodedBytes"], bytes * 2);
        let second = repo.append(pending("s", 2)).unwrap();
        let second_bytes = serde_json::to_vec(&second).unwrap().len() as u64;
        assert!(sub.lagged.load(Ordering::SeqCst));
        // The lagged sender is gone, but its first event is still resident.
        assert_eq!(repo.queue_diagnostics()["durableSubscriptions"]["queuedEncodedBytes"], bytes * 2 + second_bytes);
        assert_eq!(sub.receiver.try_recv().unwrap(), first);
        assert_eq!(repo.queue_diagnostics()["durableSubscriptions"]["queuedEvents"], 2);
        drop(other); // request cancellation / unsubscribe drops unread channel items
        assert_eq!(repo.queue_diagnostics()["durableSubscriptions"]["queuedEncodedBytes"], 0);
        drop(sub);
        let guard = repo.subscribers.lock().unwrap();
        assert_eq!(repo.queue_diagnostics()["durableSubscriptions"]["available"], false);
        drop(guard);
        drop(repo);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "manual diagnostic overhead probe; prints timings, no timing assertions"]
    fn subscription_diagnostics_overhead_probe() {
        let root = test_root("diagnostics-overhead");
        let repo = EventRepository::open(&root, "devbox").unwrap();
        let mut event = repo.append(pending("s", 1)).unwrap();
        event.cutex.as_mut().unwrap().params = json!({"text":"x".repeat(240 * 1024)});
        let bytes = serde_json::to_vec(&event).unwrap().len();
        let started = std::time::Instant::now();
        for _ in 0..128 {
            assert_eq!(super::super::event_blobs::encoded_len(std::hint::black_box(&event)), bytes);
        }
        eprintln!("diagnostics_count_writer bytes={bytes} iterations=128 elapsed_us={}", started.elapsed().as_micros());
        let metrics = Arc::new(SubscriptionMetrics::default());
        let (raw_tx, raw_rx) = mpsc::sync_channel(1);
        let (tx, rx) = mpsc::sync_channel(1);
        // Interleave the two paths to reduce allocator/thermal order effects.
        let (mut raw_ns, mut measured_ns) = (0u128, 0u128);
        for _ in 0..1000 {
            let started = std::time::Instant::now();
            raw_tx.try_send(event.clone()).unwrap();
            std::hint::black_box(raw_rx.try_recv().unwrap());
            raw_ns += started.elapsed().as_nanos();
            let started = std::time::Instant::now();
            assert!(tx.try_send(QueuedEvent::new(event.clone(), bytes, &metrics)).is_ok());
            std::hint::black_box(rx.try_recv().unwrap().into_event());
            measured_ns += started.elapsed().as_nanos();
        }
        eprintln!("diagnostics_channel iterations=1000 raw_ns={raw_ns} measured_ns={measured_ns}");
        assert_eq!(metrics.bytes.load(Ordering::Relaxed), 0);
        drop(repo);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_diagnostics_reset_active_batch_on_unwind_and_failure() {
        let root = test_root("commit-diagnostics");
        let repo = EventRepository::open(&root, "devbox").unwrap();
        {
            let mut queue = repo.lock_commits();
            queue.leader = true;
            queue.active_batch_items = 1;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _leadership = BatchLeadership { repository: &repo, tickets: vec![42], results: vec![] };
            panic!("simulated batch cancellation");
        }));
        assert!(result.is_err());
        let snapshot = repo.queue_diagnostics();
        assert_eq!(snapshot["commits"]["activeBatchItems"], 0);
        assert_eq!(snapshot["commits"]["waitingItems"], 0);
        assert_eq!(snapshot["commits"]["completedResultsAwaitingCaller"], 1);
        assert!(repo.lock_commits().done.remove(&42).unwrap().is_err());
        assert!(repo.append(pending("s", 1)).is_ok());
        assert_eq!(repo.queue_diagnostics()["commits"]["completedResultsAwaitingCaller"], 0);
        drop(repo);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_text_is_not_durable_and_sse_frame_has_no_cursor() {
        use super::super::model::{AppServerSchema,AppServerSchemaChannel,NativeMessage,NativeMessageKind};
        struct StopAfterLive(Vec<u8>);
        impl std::io::Write for StopAfterLive {
            fn write(&mut self,bytes:&[u8])->std::io::Result<usize>{self.0.extend_from_slice(bytes);Ok(bytes.len())}
            fn flush(&mut self)->std::io::Result<()> {
                if String::from_utf8_lossy(&self.0).contains("event: live_delta\n") {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                } else { Ok(()) }
            }
        }
        let root=test_root("live-text-sse");
        let repo=EventRepository::open(&root,"devbox").unwrap();
        let first=repo.append(pending("cutex.a",1)).unwrap();
        let before=fs::read(root.join("events.jsonl")).unwrap();
        let mut sub=repo.page_and_subscribe(ReplayQuery::default(),16).unwrap();
        repo.enable_live(&mut sub,Some("cutex.a".into())).unwrap();
        let delta=PendingEvent{cutex_session_id:"cutex.a".into(),host_id:"devbox".into(),source:EventSource::AppServer,
            schema:Some(AppServerSchema{protocol:"codex-app-server".into(),major_version:2,version:"test".into(),sha256:"0".repeat(64),
                channel:AppServerSchemaChannel::Experimental,capabilities:json!({"features":{"fullTextItemTerminal":true}}),extensions:vec![]}),
            correlation:EventCorrelation{runtime_generation:Some(1),thread_id:Some("thread".into()),turn_id:Some("turn".into()),item_id:Some("item".into()),..Default::default()},
            native:Some(NativeMessage{kind:NativeMessageKind::Notification,message:json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"item","delta":"你🙂"}})}),cutex:None};
        let mut legacy=delta.clone();
        legacy.schema.as_mut().unwrap().capabilities=json!({"features":{"fullTextItemTerminal":false}});
        assert!(repo.append(delta.clone()).is_err());
        assert!(repo.publish_live(delta));
        assert_eq!(fs::read(root.join("events.jsonl")).unwrap(),before);
        let mut out=StopAfterLive(Vec::new());
        let result=super::super::server::stream_events(&mut out,sub,&ReplayQuery::default(),crate::management::listeners::RevocationWatch::new(None));
        assert!(result.is_err());
        let text=String::from_utf8(out.0).unwrap();
        assert_eq!(text.lines().filter(|l|l.starts_with("id: ")).count(),1);
        assert!(text.contains(&format!("id: {}",first.cursor)));
        let live=text.split("event: live_delta\ndata: ").nth(1).unwrap();
        let frame:serde_json::Value=serde_json::from_str(live.trim()).unwrap();
        assert_eq!(frame["params"]["delta"],"你🙂");
        assert_eq!(frame["anchor"]["sequence"],first.sequence);
        assert!(frame.get("sequence").is_none() && frame.get("cursor").is_none());
        assert_eq!(repo.page(ReplayQuery::default()).unwrap().events.len(),1);
        assert!(!repo.publish_live(legacy.clone()));
        let retained=repo.append(legacy).unwrap();
        assert_eq!(retained.native.unwrap().message["params"]["delta"],"你🙂");
        assert_eq!(repo.page(ReplayQuery::default()).unwrap().events.len(),2);
        drop(repo); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_journal_external_then_local_are_live_ordered_and_not_duplicated() {
        let root = test_root("shared-live-order");
        let management = EventRepository::open(&root, "devbox").unwrap();
        let agent_bus = EventRepository::open(&root, "devbox").unwrap();
        let live = management.page_and_subscribe(ReplayQuery::default(), 16).unwrap();
        let first = agent_bus.append(pending("cutex.a", 1)).unwrap();
        let second = management.append(pending("cutex.a", 2)).unwrap();
        assert_eq!(live.receiver.try_recv().unwrap(), first);
        assert_eq!(live.receiver.try_recv().unwrap(), second);
        assert_eq!(management.follow_shared_journal().unwrap(), 0);
        assert!(live.receiver.try_recv().is_err());
        let third = agent_bus.append(pending("cutex.a", 3)).unwrap();
        assert_eq!(management.follow_shared_journal().unwrap(), 1);
        assert_eq!(live.receiver.try_recv().unwrap(), third);
        assert_eq!(management.follow_shared_journal().unwrap(), 0);
        drop(live); drop(agent_bus); drop(management); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_journal_new_subscriber_replays_external_event_only_once() {
        let root = test_root("shared-live-subscribe");
        let management = EventRepository::open(&root, "devbox").unwrap();
        let other = EventRepository::open(&root, "devbox").unwrap();
        let old = management.page_and_subscribe(ReplayQuery::default(), 16).unwrap();
        let first = other.append(pending("cutex.a", 1)).unwrap();
        let new = management.page_and_subscribe(ReplayQuery::default(), 16).unwrap();
        assert_eq!(old.receiver.try_recv().unwrap(), first);
        assert_eq!(new.page.events, vec![first]);
        assert_eq!(management.follow_shared_journal().unwrap(), 0);
        assert!(new.receiver.try_recv().is_err());
        let next = other.append(pending("cutex.a", 2)).unwrap();
        management.follow_shared_journal().unwrap();
        assert_eq!(old.receiver.try_recv().unwrap(), next);
        assert_eq!(new.receiver.try_recv().unwrap(), next);
        drop(old); drop(new); drop(other); drop(management); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_journal_follows_external_rotation_and_disconnects_on_retention_gap() {
        let root = test_root("shared-live-rotation");
        let options = RepositoryOptions { rotate_bytes: 1, retained_files: 4 };
        let management = EventRepository::open_with_options(&root, "devbox", options).unwrap();
        let other = EventRepository::open_with_options(&root, "devbox", options).unwrap();
        let live = management.page_and_subscribe(ReplayQuery::default(), 16).unwrap();
        let first = other.append(pending("cutex.a", 1)).unwrap();
        management.follow_shared_journal().unwrap();
        assert_eq!(live.receiver.try_recv().unwrap(), first);
        for i in 2..5 { other.append(pending("cutex.a", i)).unwrap(); }
        assert_eq!(management.follow_shared_journal().unwrap(), 3);
        assert_eq!(live.receiver.try_iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![2,3,4]);
        for i in 5..15 { other.append(pending("cutex.a", i)).unwrap(); }
        management.follow_shared_journal().unwrap();
        assert!(live.lagged.load(Ordering::SeqCst));
        assert!(matches!(live.receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
        drop(live); drop(other); drop(management); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_journal_concurrent_writers_and_follower_preserve_one_sequence() {
        let root = test_root("shared-live-concurrent");
        let management = Arc::new(EventRepository::open(&root, "devbox").unwrap());
        let other = Arc::new(EventRepository::open(&root, "devbox").unwrap());
        let live = management.page_and_subscribe(ReplayQuery::default(), 128).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let follower = {
            let repository = management.clone(); let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    repository.follow_shared_journal().unwrap();
                    thread::sleep(std::time::Duration::from_millis(1));
                }
            })
        };
        let a = { let repository = management.clone(); thread::spawn(move || {
            for i in 0..30 { repository.append(pending("cutex.a", i)).unwrap(); }
        }) };
        let b = { let repository = other.clone(); thread::spawn(move || {
            for i in 30..60 { repository.append(pending("cutex.b", i)).unwrap(); }
        }) };
        a.join().unwrap(); b.join().unwrap(); stop.store(true, Ordering::SeqCst); follower.join().unwrap();
        management.follow_shared_journal().unwrap();
        assert_eq!(live.receiver.try_iter().map(|e| e.sequence).collect::<Vec<_>>(), (1..=60).collect::<Vec<_>>());
        drop(live); drop(other); drop(management); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_journal_recovery_while_following_does_not_lose_the_local_append() {
        let root = test_root("shared-live-recover-append");
        let management = EventRepository::open(&root, "devbox").unwrap();
        let other = EventRepository::open(&root, "devbox").unwrap();
        let live = management.page_and_subscribe(ReplayQuery::default(), 16).unwrap();
        let first = other.append(pending("cutex.a", 1)).unwrap();
        other.append(pending("cutex.a", 2)).unwrap();
        other.append(pending("cutex.a", 3)).unwrap();
        // The append fast path sees a valid head/tail. Only catch-up reaches
        // this broken middle record and discovers that recovery is needed.
        let journal = fs::read_to_string(root.join(EVENTS_FILE_NAME)).unwrap();
        let mut lines = journal.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        lines[1] = "{broken json}";
        fs::write(root.join(EVENTS_FILE_NAME), format!("{}\n", lines.join("\n"))).unwrap();
        let accepted = management.append(pending("cutex.a", 4)).expect("append uses the recovered stream");
        assert_ne!(accepted.stream_id, first.stream_id);
        assert!(live.receiver.try_iter().any(|event| event.cursor == accepted.cursor));
        assert_eq!(value_of(&accepted), 4);
        drop(live); drop(other); drop(management); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_events_are_small_in_journal_replay_and_live_subscribers() {
        use base64::Engine;
        let root = test_root("image-blobs");
        let repository = EventRepository::open(&root, "devbox").unwrap();
        let subscriber = repository.page_and_subscribe(ReplayQuery::default(),32).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(vec![19; 1024*1024]);
        for i in 0..24 {
            let mut p = pending("cutex.a", i);
            p.cutex.as_mut().unwrap().params = json!({"item":{"type":"imageGeneration","id":format!("image{i}"),"result":data}});
            let event = repository.append(p).unwrap();
            assert!(serde_json::to_vec(&event).unwrap().len() < 4096);
            let published = subscriber.receiver.try_recv().unwrap();
            assert_eq!(event,published);
        }
        let page = repository.page(ReplayQuery::default()).unwrap();
        assert_eq!(page.events.len(),24);
        assert!(fs::metadata(root.join("events.jsonl")).unwrap().len()<24*4096);
        let blob = &page.events[0].cutex.as_ref().unwrap().params["item"]["result"];
        assert_eq!(blob["type"],"cutexBlob");
        assert!(super::super::event_blobs::EventBlobStore::at(root.join("event-blobs"))
            .get("cutex.a",blob["blobId"].as_str().unwrap()).is_some());
        drop(subscriber); drop(repository);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_user_input_image_event_persists_blob_references_that_validate() {
        use base64::Engine;
        let root = test_root("user-input-image-blobs");
        let repository = EventRepository::open(&root, "devbox").unwrap();
        let data_url = format!("data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(vec![23;65_537]));
        let origin = json!({"kind":"other", "clientId":"desktop-test"});
        let input = json!([{"type":"image", "url":data_url, "detail":"auto"}]);
        let at = "2026-10-25T00:00:00Z";
        let queue_item = || json!({
            "queueId":"queue-1", "clientUserMessageId":"message-1", "origin":origin,
            "input":input, "position":0, "revision":1, "state":"queued",
            "createdAt":at, "updatedAt":at
        });
        let cases = [
            ("cutex/userInput/routeAccepted", json!({
                "managementRequestId":"request-1", "clientUserMessageId":"message-1",
                "origin":origin, "input":input, "strategy":"auto", "state":"routed_to_cutex",
                "acceptedAt":at
            })),
            ("cutex/userInput/queued", queue_item()),
            ("cutex/userInput/queueUpdated", queue_item()),
            ("cutex/userInput/submitted", json!({
                "managementRequestId":"request-1", "clientUserMessageId":"message-1",
                "origin":origin, "input":input, "disposition":"started",
                "nativeRequestId":"native-1", "nativeMethod":"turn/start",
                "appServerAccepted":true, "submittedAt":at
            })),
            ("cutex/userInput/failed", json!({
                "managementRequestId":"request-1", "clientUserMessageId":"message-1",
                "origin":origin, "input":input, "stage":"native_request",
                "error":{"source":"cutex", "code":"test_failure", "message":"fixture", "retryable":false},
                "failedAt":at
            })),
        ];
        for (method, params) in cases {
            let pending = PendingEvent {
                cutex_session_id:"cutex.a".into(), host_id:"devbox".into(), source:EventSource::Cutex,
                schema:None, correlation:EventCorrelation::default(), native:None,
                cutex:Some(CutexMessage { method:method.into(), params }),
            };
            let event = repository.append(pending).unwrap();
            let cutex = event.cutex.as_ref().unwrap();
            let blob = &cutex.params["input"][0]["url"];
            assert_eq!(blob["type"], "cutexBlob", "{method}");
            assert!(super::super::contract_validation::validate_cutex_event_message(
                &serde_json::to_value(cutex).unwrap()).is_ok(), "{method}");
            assert!(super::super::event_blobs::EventBlobStore::at(root.join("event-blobs"))
                .get("cutex.a",blob["blobId"].as_str().unwrap()).is_some());
        }
        drop(repository);
        let reopened = EventRepository::open(&root,"devbox").unwrap();
        let replay = reopened.page(ReplayQuery::default()).unwrap();
        assert_eq!(replay.events.len(),5);
        for event in &replay.events {
            event.validate().unwrap();
            assert_eq!(event.cutex.as_ref().unwrap().params["input"][0]["url"]["type"],"cutexBlob");
        }
        drop(reopened); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn strict_cutex_text_that_looks_like_media_stays_a_string() {
        use base64::Engine;
        let root = test_root("strict-cutex-data-url-text");
        let repository = EventRepository::open(&root,"devbox").unwrap();
        let content = format!("data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(vec![29;65_537]));
        let event = repository.append(PendingEvent {
            cutex_session_id:"cutex.a".into(), host_id:"devbox".into(), source:EventSource::Cutex,
            schema:None, correlation:EventCorrelation::default(), native:None,
            cutex:Some(CutexMessage {
                method:"cutex/agentBus/messageSent".into(),
                params:json!({
                    "messageId":"message-1", "fromCutexSessionId":"cutex.a",
                    "toCutexSessionId":"cutex.b", "deliveryMode":"after_turn",
                    "content":content, "sentAt":"2026-10-25T00:00:00Z"
                }),
            }),
        }).unwrap();
        assert!(event.cutex.as_ref().unwrap().params["content"].is_string());
        event.validate().unwrap();
        drop(repository); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn google_tool_schema_error_codes_survive_persistence_and_legacy_replay() {
        use super::super::model::{AppServerSchema, AppServerSchemaChannel, NativeMessage, NativeMessageKind};
        let root = test_root("google-tool-schema-code");
        let repository = EventRepository::open(&root, "devbox").unwrap();
        let native_error = json!({"message":"Google does not support this JSON Schema enum",
            "code":400, "codexErrorInfo":"other", "additionalDetails":"original detail"});
        let event = repository.append(PendingEvent {
            cutex_session_id:"cutex.a".into(), host_id:"devbox".into(), source:EventSource::AppServer,
            schema:Some(AppServerSchema {protocol:"codex-app-server".into(), major_version:2,
                version:"test".into(), sha256:"0".repeat(64), channel:AppServerSchemaChannel::Experimental,
                capabilities:json!({}), extensions:vec![]}),
            correlation:EventCorrelation {runtime_generation:Some(1), thread_id:Some("thread".into()),
                turn_id:Some("turn".into()), ..Default::default()},
            native:Some(NativeMessage {kind:NativeMessageKind::Notification,
                message:json!({"method":"turn/completed", "params":{"threadId":"thread",
                    "turn":{"id":"turn", "status":"failed", "error":native_error}}})}),
            cutex:None,
        }).unwrap();
        let error = &event.native.as_ref().unwrap().message["params"]["turn"]["error"];
        assert_eq!(error["cutexCode"], "tool_schema_rejected");
        for (key, value) in native_error.as_object().unwrap() { assert_eq!(&error[key], value); }
        let stored = fs::read_to_string(root.join("events.jsonl")).unwrap();
        let mut legacy: serde_json::Value = serde_json::from_str(stored.trim()).unwrap();
        assert_eq!(legacy["native"]["message"]["params"]["turn"]["error"]["cutexCode"], "tool_schema_rejected");
        legacy["native"]["message"]["params"]["turn"]["error"].as_object_mut().unwrap().remove("cutexCode");
        drop(repository);
        let legacy_line = format!("{}\n", serde_json::to_string(&legacy).unwrap());
        fs::write(root.join("events.jsonl"), &legacy_line).unwrap();
        let reopened = EventRepository::open(&root,"devbox").unwrap();
        let replay = reopened.page(ReplayQuery::default()).unwrap();
        assert_eq!(replay.events.len(),1);
        assert_eq!(replay.events[0].cursor,event.cursor);
        assert_eq!(replay.events[0].native.as_ref().unwrap().message["params"]["turn"]["error"]["cutexCode"], "tool_schema_rejected");
        assert_eq!(fs::read_to_string(root.join("events.jsonl")).unwrap(),legacy_line,
            "replay does not rewrite retained native history");
        drop(reopened); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn old_inline_events_replay_as_refs_without_rewriting_cursors_or_log() {
        use base64::Engine;
        let root=test_root("legacy-image-blobs");
        let repository=EventRepository::open(&root,"devbox").unwrap();
        let first=repository.append(pending("cutex.a",0)).unwrap();
        let mut old=serde_json::to_value(&first).unwrap();
        old["cutex"]["params"]=json!({"item":{"type":"imageGeneration","result":base64::engine::general_purpose::STANDARD.encode(vec![7;1024*1024])}});
        let line=format!("{}\n",serde_json::to_string(&old).unwrap());
        drop(repository);
        fs::write(root.join("events.jsonl"),&line).unwrap();
        let reopened=EventRepository::open(&root,"devbox").unwrap();
        let page=reopened.page(ReplayQuery::default()).unwrap();
        assert_eq!(page.events[0].cursor,first.cursor);
        assert_eq!(page.events[0].sequence,first.sequence);
        assert!(serde_json::to_vec(&page.events[0]).unwrap().len()<4096);
        assert_eq!(fs::read_to_string(root.join("events.jsonl")).unwrap(),line);
        drop(reopened); fs::remove_dir_all(root).unwrap();
    }

    /// A streaming delta as the event worker journals it (~350 bytes).
    fn delta_event(session: &str, value: usize) -> PendingEvent {
        PendingEvent {
            cutex_session_id: session.to_string(),
            host_id: "devbox".to_string(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation::default(),
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/test/delta".to_string(),
                params: json!({
                    "threadId": "00000000-0000-4000-8000-000000000001",
                    "turnId": "00000000-0000-4000-8000-000000000002",
                    "itemId": format!("msg-{value}"),
                    "delta": "a few streamed tokens of an answer ",
                }),
            }),
        }
    }

    /// PERF-07 measurement: `writers` threads append concurrently on a
    /// disk-backed directory (`CUTEX_BENCH_DIR`; tmpfs makes fsync free).
    /// `cargo test --lib bench_append_contention -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_append_contention() {
        let base = std::env::var_os("CUTEX_BENCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for (writers, per_writer) in [(1_usize, 1000_usize), (4, 400), (8, 250), (16, 125)] {
            let root = base.join(format!("cutex-v2-bench-{}", Uuid::new_v4()));
            let repository = Arc::new(EventRepository::open(&root, "devbox").expect("open"));
            let start = std::time::Instant::now();
            let latencies = (0..writers)
                .map(|writer| {
                    let repository = repository.clone();
                    thread::spawn(move || {
                        let session = format!("session-{writer}");
                        (0..per_writer)
                            .map(|value| {
                                let at = std::time::Instant::now();
                                repository.append(delta_event(&session, value)).expect("append");
                                at.elapsed()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|handle| handle.join().expect("writer"))
                .collect::<Vec<_>>();
            let elapsed = start.elapsed();
            let mut sorted = latencies.clone();
            sorted.sort();
            let stats = repository.stats();
            let get = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
            let events = get(&stats.events);
            let batches = get(&stats.batches).max(1);
            let seconds = elapsed.as_secs_f64();
            eprintln!(
                "BENCH writers={writers} events={events} {:.0} ev/s p50={:?} p95={:?} \
                 batches={batches} ({:.1} ev/batch) journal_syncs={} ({:.0}/s) state_writes={} ({:.0}/s) \
                 lock_hold/batch={:?} lock_wait/batch={:?}",
                events as f64 / seconds,
                sorted[sorted.len() / 2],
                sorted[sorted.len() * 95 / 100],
                events as f64 / batches as f64,
                get(&stats.journal_syncs),
                get(&stats.journal_syncs) as f64 / seconds,
                get(&stats.state_writes),
                get(&stats.state_writes) as f64 / seconds,
                std::time::Duration::from_nanos(get(&stats.lock_hold_ns) / batches),
                std::time::Duration::from_nanos(get(&stats.lock_wait_ns) / batches),
            );
            let page = repository
                .page(ReplayQuery { limit: 1000, ..ReplayQuery::default() })
                .expect("page");
            assert_eq!(page.events.first().map(|event| event.sequence), Some(1));
            drop(repository);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn reference_page(repository: &EventRepository, query: &ReplayQuery) -> EventPage {
        let state = repository
            .try_read_state_locked()
            .expect("read reference state")
            .expect("reference state");
        let records = repository
            .read_records_locked()
            .expect("read reference records");
        let initial_cursor = query
            .after
            .as_deref()
            .is_none_or(|value| value.is_empty() || value == "0");
        let start = if initial_cursor {
            0
        } else {
            records
                .iter()
                .position(|event| Some(event.cursor.as_str()) == query.after.as_deref())
                .expect("reference cursor")
                + 1
        };
        let end = start.saturating_add(query.limit).min(records.len());
        let scanned = &records[start..end];
        EventPage {
            contract_version: CONTRACT_VERSION,
            host_id: repository.host_id.clone(),
            stream_id: state.stream_id.clone(),
            scope: EventPageScope {
                cutex_session_id: query.cutex_session_id.clone(),
            },
            events: scanned
                .iter()
                .filter(|event| {
                    query
                        .cutex_session_id
                        .as_deref()
                        .is_none_or(|session_id| event.cutex_session_id == session_id)
                })
                .cloned()
                .collect(),
            next_cursor: scanned
                .last()
                .map(|event| event.cursor.clone())
                .or_else(|| (!initial_cursor).then(|| query.after.clone()).flatten()),
            checkpoint: match records.last() {
                Some(event) => EventCheckpoint {
                    stream_id: state.stream_id,
                    sequence: event.sequence,
                    cursor: Some(event.cursor.clone()),
                },
                None => EventCheckpoint {
                    stream_id: state.stream_id,
                    sequence: 0,
                    cursor: None,
                },
            },
            scanned_count: scanned.len(),
            has_more: end < records.len(),
        }
    }

    #[test]
    fn append_replay_and_zero_match_scans_are_monotonic() {
        let root = test_root("replay");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        for value in 0..5 {
            repository
                .append(pending("cutex.session-a", value))
                .expect("append event");
        }
        let first = repository
            .page(ReplayQuery {
                limit: 2,
                cutex_session_id: Some("cutex.missing".to_string()),
                ..ReplayQuery::default()
            })
            .expect("first page");
        assert!(first.events.is_empty());
        assert_eq!(first.scanned_count, 2);
        assert!(first.has_more);
        let second = repository
            .page(ReplayQuery {
                stream_id: Some(first.stream_id.clone()),
                after: first.next_cursor.clone(),
                limit: 3,
                cutex_session_id: Some("cutex.session-a".to_string()),
            })
            .expect("second page");
        assert_eq!(second.events.len(), 3);
        assert_eq!(second.events[0].sequence, 3);
        assert_eq!(second.checkpoint.sequence, 5);
        assert!(!second.has_more);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn bounded_pages_match_full_reader_for_initial_continuation_and_filtering() {
        let root = test_root("bounded-page-equivalence");
        let repository = EventRepository::open_with_options(
            &root,
            "devbox",
            RepositoryOptions {
                rotate_bytes: 1_400,
                retained_files: 3,
            },
        )
        .expect("open repository");
        for value in 0..20 {
            let session = if value % 2 == 0 {
                "cutex.session-a"
            } else {
                "cutex.session-b"
            };
            repository
                .append(pending(session, value))
                .expect("append event");
        }

        let initial = ReplayQuery {
            limit: 3,
            cutex_session_id: Some("cutex.missing".to_string()),
            ..ReplayQuery::default()
        };
        let expected_initial = reference_page(&repository, &initial);
        let actual_initial = repository.page(initial).expect("read initial page");
        assert_eq!(actual_initial, expected_initial);

        let continuation = ReplayQuery {
            stream_id: Some(actual_initial.stream_id.clone()),
            after: actual_initial.next_cursor.clone(),
            limit: 4,
            cutex_session_id: Some("cutex.session-a".to_string()),
        };
        let expected_continuation = reference_page(&repository, &continuation);
        let actual_continuation = repository
            .page(continuation)
            .expect("read continuation page");
        assert_eq!(actual_continuation, expected_continuation);

        let final_query = ReplayQuery {
            stream_id: Some(actual_continuation.stream_id.clone()),
            after: actual_continuation.checkpoint.cursor.clone(),
            limit: 10,
            cutex_session_id: None,
        };
        let expected_final = reference_page(&repository, &final_query);
        let actual_final = repository.page(final_query).expect("read final page");
        assert_eq!(actual_final, expected_final);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn bounded_page_discards_a_stale_cursor_location_and_rebuilds() {
        let root = test_root("bounded-page-stale-index");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        for value in 0..5 {
            repository
                .append(pending("cutex.session-a", value))
                .expect("append event");
        }
        let initial = repository
            .page(ReplayQuery {
                limit: 2,
                ..ReplayQuery::default()
            })
            .expect("build replay index");
        let first = &initial.events[0];
        {
            let mut index = repository.replay_index.lock().expect("lock replay index");
            let location = index
                .cursors
                .get_mut(&first.cursor)
                .expect("indexed first cursor");
            location.next_offset = 0;
        }

        let page = repository
            .page(ReplayQuery {
                stream_id: Some(initial.stream_id),
                after: Some(first.cursor.clone()),
                limit: 2,
                cutex_session_id: None,
            })
            .expect("rebuild stale replay index");
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].sequence, first.sequence + 1);
        assert!(repository.recovery_reset().unwrap().is_none());
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn bounded_page_discards_a_plausible_wrong_cursor_location_and_rebuilds() {
        let root = test_root("bounded-page-wrong-index");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        for value in 0..5 {
            repository
                .append(pending("cutex.session-a", value))
                .expect("append event");
        }
        let initial = repository
            .page(ReplayQuery {
                limit: 3,
                ..ReplayQuery::default()
            })
            .expect("build replay index");
        let first = initial.events[0].clone();
        let second = initial.events[1].clone();
        {
            let mut index = repository.replay_index.lock().expect("lock replay index");
            let wrong_location = *index
                .cursors
                .get(&second.cursor)
                .expect("indexed second cursor");
            index.cursors.insert(first.cursor.clone(), wrong_location);
        }

        let page = repository
            .page(ReplayQuery {
                stream_id: Some(initial.stream_id),
                after: Some(first.cursor),
                limit: 2,
                cutex_session_id: None,
            })
            .expect("rebuild plausible wrong replay index");
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].sequence, first.sequence + 1);
        assert!(repository.recovery_reset().unwrap().is_none());
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn bounded_page_refreshes_external_append_and_rotation() {
        let root = test_root("bounded-page-external");
        let options = RepositoryOptions {
            rotate_bytes: 1_400,
            retained_files: 3,
        };
        let reader = EventRepository::open_with_options(&root, "devbox", options)
            .expect("open reader repository");
        let writer = EventRepository::open_with_options(&root, "devbox", options)
            .expect("open writer repository");
        for value in 0..6 {
            writer
                .append(pending("cutex.session-a", value))
                .expect("append initial external event");
        }
        let initial = reader
            .page(ReplayQuery {
                limit: 100,
                ..ReplayQuery::default()
            })
            .expect("read initial external events");
        let retained_cursor = initial.events.last().expect("last initial event").clone();

        for value in 6..8 {
            writer
                .append(pending("cutex.session-a", value))
                .expect("append external continuation");
        }
        let continuation = reader
            .page(ReplayQuery {
                stream_id: Some(initial.stream_id.clone()),
                after: Some(retained_cursor.cursor.clone()),
                limit: 100,
                cutex_session_id: None,
            })
            .expect("read after external rotation");
        assert_eq!(
            continuation
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![retained_cursor.sequence + 1, retained_cursor.sequence + 2]
        );

        for value in 8..30 {
            writer
                .append(pending("cutex.session-a", value))
                .expect("append expiring external event");
        }
        let error = reader
            .page(ReplayQuery {
                stream_id: Some(initial.stream_id),
                after: Some(retained_cursor.cursor),
                limit: 100,
                cutex_session_id: None,
            })
            .expect_err("rotated cursor should expire");
        assert!(matches!(error, ReplayError::CursorExpired { .. }));
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn metadata_and_checkpoint_match_full_replay_after_rotation() {
        let root = test_root("metadata-rotation");
        let repository = EventRepository::open_with_options(
            &root,
            "devbox",
            RepositoryOptions {
                rotate_bytes: 700,
                retained_files: 2,
            },
        )
        .expect("open repository");
        for value in 0..8 {
            repository
                .append(pending("cutex.session-a", value))
                .expect("append rotated event");
        }

        let metadata = repository.stream_metadata().expect("read metadata");
        let checkpoint = repository.checkpoint().expect("read checkpoint");
        let page = repository
            .page(ReplayQuery {
                limit: 1000,
                ..ReplayQuery::default()
            })
            .expect("read retained events");
        let first = page.events.first().expect("retained first event");
        let last = page.events.last().expect("retained last event");

        assert_eq!(metadata.stream_id, page.stream_id);
        assert_eq!(
            metadata.earliest,
            Some(EventStreamBoundary {
                sequence: first.sequence,
                cursor: first.cursor.clone(),
            })
        );
        assert_eq!(
            metadata.latest,
            Some(EventStreamBoundary {
                sequence: last.sequence,
                cursor: last.cursor.clone(),
            })
        );
        assert_eq!(checkpoint, page.checkpoint);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn metadata_observes_external_append_and_rotation() {
        let root = test_root("metadata-external-rotation");
        let options = RepositoryOptions {
            rotate_bytes: 700,
            retained_files: 2,
        };
        let reader = EventRepository::open_with_options(&root, "devbox", options)
            .expect("open reader repository");
        let writer = EventRepository::open_with_options(&root, "devbox", options)
            .expect("open writer repository");

        let mut latest = None;
        for value in 0..8 {
            latest = Some(
                writer
                    .append(pending("cutex.session-a", value))
                    .expect("append external event"),
            );
        }
        let latest = latest.expect("latest external event");
        let metadata = reader.stream_metadata().expect("read external metadata");
        let page = reader
            .page(ReplayQuery {
                limit: 1000,
                ..ReplayQuery::default()
            })
            .expect("read externally rotated events");

        assert_eq!(metadata.stream_id, latest.stream_id);
        assert_eq!(metadata.latest.unwrap().sequence, latest.sequence);
        assert_eq!(
            metadata.earliest.unwrap().sequence,
            page.events.first().unwrap().sequence
        );
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn metadata_accepts_a_lagging_state_checkpoint() {
        let root = test_root("metadata-stale-state");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let latest = repository
            .append(pending("cutex.session-a", 1))
            .expect("append event");
        let state_path = root.join(STATE_FILE_NAME);
        let mut state: StreamState =
            serde_json::from_slice(&fs::read(&state_path).expect("read state"))
                .expect("parse state");
        state.next_sequence = latest.sequence;
        fs::write(
            &state_path,
            serde_json::to_vec_pretty(&state).expect("encode stale state"),
        )
        .expect("write stale state");

        let checkpoint = repository.checkpoint().expect("recover checkpoint");
        let recovered: StreamState =
            serde_json::from_slice(&fs::read(&state_path).expect("read recovered state"))
                .expect("parse recovered state");

        assert_eq!(checkpoint.stream_id, latest.stream_id);
        assert_eq!(checkpoint.sequence, latest.sequence);
        assert_eq!(checkpoint.cursor.as_deref(), Some(latest.cursor.as_str()));
        // `stream-state.json` is a checkpoint (may lag, never lead): a lag
        // is answered from the journal tail without a rescan or rewrite.
        assert_eq!(recovered.next_sequence, latest.sequence);
        assert!(repository.recovery_reset().unwrap().is_none());
        let next = repository.append(pending("cutex.session-a", 2)).expect("append after lag");
        assert_eq!(next.sequence, latest.sequence + 1);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn metadata_detects_a_partial_current_tail_immediately() {
        let root = test_root("metadata-partial-tail");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let previous = repository
            .append(pending("cutex.session-a", 1))
            .expect("append event");
        let mut file = OpenOptions::new()
            .append(true)
            .open(root.join(EVENTS_FILE_NAME))
            .expect("open journal");
        file.write_all(b"{\"partial\":true}")
            .expect("write partial tail");
        file.sync_all().expect("sync partial tail");

        let metadata = repository
            .stream_metadata()
            .expect("recover metadata after partial tail");
        let reset = repository
            .recovery_reset()
            .expect("read recovery reset")
            .expect("partial tail reset");

        assert_ne!(metadata.stream_id, previous.stream_id);
        assert_eq!(metadata.stream_id, reset.stream_id);
        assert!(metadata.earliest.is_none());
        assert!(metadata.latest.is_none());
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn old_middle_corruption_is_repaired_when_full_replay_reaches_it() {
        let root = test_root("metadata-middle-corruption");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        for value in 0..5 {
            repository
                .append(pending("cutex.session-a", value))
                .expect("append event");
        }
        let before = repository.stream_metadata().expect("read prior metadata");
        let journal_path = root.join(EVENTS_FILE_NAME);
        let mut lines = fs::read_to_string(&journal_path)
            .expect("read journal")
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        lines[2] = "{not-json".to_string();
        fs::write(&journal_path, format!("{}\n", lines.join("\n"))).expect("corrupt middle record");

        let fast = repository
            .stream_metadata()
            .expect("boundary metadata remains readable");
        assert_eq!(fast, before);
        assert!(repository.recovery_reset().unwrap().is_none());

        let page = repository
            .page(ReplayQuery::default())
            .expect("full replay repairs corrupt history");
        let reset = repository
            .recovery_reset()
            .expect("read recovery reset")
            .expect("middle corruption reset");
        assert_ne!(page.stream_id, before.stream_id);
        assert_eq!(page.stream_id, reset.stream_id);
        assert!(page.events.is_empty());
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn lost_state_write_recovers_from_committed_tail_without_duplicate_sequence() {
        let root = test_root("recover-state");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let first = repository
            .append(pending("cutex.session-a", 1))
            .expect("append first event");
        fs::remove_file(root.join(STATE_FILE_NAME)).expect("remove state");
        let reopened = EventRepository::open(&root, "devbox").expect("reopen repository");
        let second = reopened
            .append(pending("cutex.session-a", 2))
            .expect("append second event");
        assert_eq!(first.stream_id, second.stream_id);
        assert_eq!(second.sequence, first.sequence + 1);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn separate_repository_instances_observe_the_latest_committed_tail() {
        let root = test_root("separate-instances");
        let first_repository =
            EventRepository::open(&root, "devbox").expect("open first repository");
        let second_repository =
            EventRepository::open(&root, "devbox").expect("open second repository");

        let first = first_repository
            .append(pending("cutex.session-a", 1))
            .expect("append from first repository");
        let second = second_repository
            .append(pending("cutex.session-a", 2))
            .expect("append from second repository");
        let third = first_repository
            .append(pending("cutex.session-a", 3))
            .expect("append again from first repository");

        assert_eq!(second.stream_id, first.stream_id);
        assert_eq!(second.sequence, first.sequence + 1);
        assert_eq!(third.sequence, second.sequence + 1);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn stale_state_falls_back_to_full_tail_recovery_before_append() {
        let root = test_root("stale-state-append");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let first = repository
            .append(pending("cutex.session-a", 1))
            .expect("append first event");
        let state_path = root.join(STATE_FILE_NAME);
        let mut state: StreamState =
            serde_json::from_slice(&fs::read(&state_path).expect("read state"))
                .expect("parse state");
        state.next_sequence = first.sequence;
        fs::write(
            &state_path,
            serde_json::to_vec_pretty(&state).expect("encode state"),
        )
        .expect("write stale state");

        let second = repository
            .append(pending("cutex.session-a", 2))
            .expect("append after stale state");

        assert_eq!(second.stream_id, first.stream_id);
        assert_eq!(second.sequence, first.sequence + 1);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn partial_tail_during_append_uses_the_existing_stream_reset_path() {
        let root = test_root("partial-tail-append");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let first = repository
            .append(pending("cutex.session-a", 1))
            .expect("append first event");
        let mut file = OpenOptions::new()
            .append(true)
            .open(root.join(EVENTS_FILE_NAME))
            .expect("open journal");
        file.write_all(b"{\"partial\":true}")
            .expect("write partial tail");
        file.sync_all().expect("sync partial tail");

        let next = repository
            .append(pending("cutex.session-a", 2))
            .expect("append after partial tail");
        let reset = repository
            .recovery_reset()
            .expect("read reset")
            .expect("recovery reset");

        assert_eq!(
            reset.previous_stream_id.as_deref(),
            Some(first.stream_id.as_str())
        );
        assert_ne!(next.stream_id, first.stream_id);
        assert_eq!(next.sequence, 2);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn partial_tail_rotates_to_a_new_stream_and_reports_reset() {
        let root = test_root("partial-tail");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let first = repository
            .append(pending("cutex.session-a", 1))
            .expect("append first event");
        let mut file = OpenOptions::new()
            .append(true)
            .open(root.join(EVENTS_FILE_NAME))
            .expect("open journal");
        file.write_all(b"{\"partial\":true}")
            .expect("write partial tail");
        file.sync_all().expect("sync partial tail");
        let reopened = EventRepository::open(&root, "devbox").expect("reopen repository");
        let reset = reopened
            .recovery_reset()
            .expect("read reset")
            .expect("recovery reset");
        assert_eq!(
            reset.previous_stream_id.as_deref(),
            Some(first.stream_id.as_str())
        );
        assert_ne!(reset.stream_id, first.stream_id);
        let next = reopened
            .append(pending("cutex.session-a", 2))
            .expect("append after reset");
        assert_eq!(next.sequence, 2);
        assert_eq!(next.stream_id, reset.stream_id);
        let page = reopened
            .page(ReplayQuery {
                limit: 10,
                ..ReplayQuery::default()
            })
            .expect("read reset stream");
        assert_eq!(page.events[0].sequence, 1);
        assert_eq!(
            page.events[0]
                .cutex
                .as_ref()
                .map(|message| message.method.as_str()),
            Some("cutex/stream/reset")
        );
        assert_eq!(
            page.events[0].cutex.as_ref().unwrap().params["reason"],
            "protocol_violation"
        );
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn pending_reset_survives_reopen_until_all_active_sessions_are_materialized() {
        let root = test_root("pending-reset-reopen");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        repository
            .append(pending("cutex.session-a", 1))
            .expect("append prior event");
        let mut file = OpenOptions::new()
            .append(true)
            .open(root.join(EVENTS_FILE_NAME))
            .expect("open journal");
        file.write_all(b"{\"partial\":true}")
            .expect("write partial tail");
        file.sync_all().expect("sync partial tail");

        let recovered = EventRepository::open(&root, "devbox").expect("recover repository");
        let event_a = recovered
            .append(pending("cutex.session-a", 2))
            .expect("append session a after reset");
        assert_eq!(event_a.sequence, 2);
        drop(recovered);

        let reopened = EventRepository::open(&root, "devbox").expect("reopen reset stream");
        let reset_events = reopened
            .materialize_recovery_reset(
                &["cutex.session-a".to_string(), "cutex.session-b".to_string()],
                true,
            )
            .expect("materialize remaining reset events");
        assert_eq!(reset_events.len(), 1);
        assert_eq!(reset_events[0].cutex_session_id, "cutex.session-b");
        assert_eq!(reset_events[0].sequence, 3);
        assert!(reopened.recovery_reset().unwrap().is_none());

        let event_b = reopened
            .append(pending("cutex.session-b", 3))
            .expect("append session b after reset completion");
        assert_eq!(event_b.sequence, 4);
        let page = reopened
            .page(ReplayQuery {
                limit: 10,
                ..ReplayQuery::default()
            })
            .expect("read replacement stream");
        assert_eq!(
            page.events
                .iter()
                .filter(|event| {
                    event
                        .cutex
                        .as_ref()
                        .is_some_and(|message| message.method == "cutex/stream/reset")
                })
                .count(),
            2
        );
        fs::remove_dir_all(root).expect("remove test repository");
    }

    fn value_of(event: &EventEnvelope) -> u64 {
        event.cutex.as_ref().expect("cutex event").params["value"]
            .as_u64()
            .expect("value")
    }

    fn all_events(repository: &EventRepository) -> Vec<EventEnvelope> {
        let mut events = Vec::new();
        let mut query = ReplayQuery { limit: 1000, ..ReplayQuery::default() };
        loop {
            let page = repository.page(query.clone()).expect("page");
            let done = page.events.len() < query.limit;
            query.stream_id = Some(page.stream_id.clone());
            query.after = page.events.last().map(|event| event.cursor.clone());
            events.extend(page.events);
            if done || query.after.is_none() {
                return events;
            }
        }
    }

    /// Group commit keeps each writer's order, gives contiguous sequences
    /// across writers and two repository instances (separate queues, shared
    /// file lock, as two processes), crosses rotations inside batches, and
    /// publishes live events strictly in sequence order.
    #[test]
    fn group_commit_keeps_per_writer_order_across_concurrent_writers_and_instances() {
        let root = test_root("group-order");
        let options = RepositoryOptions { rotate_bytes: 64 * 1024, retained_files: 8 };
        let first = Arc::new(EventRepository::open_with_options(&root, "devbox", options).expect("open"));
        let second = Arc::new(EventRepository::open_with_options(&root, "devbox", options).expect("open"));
        let subscription = first
            .page_and_subscribe(ReplayQuery::default(), 10_000)
            .expect("subscribe");
        const WRITERS: usize = 12;
        const PER_WRITER: usize = 60;
        let workers = (0..WRITERS)
            .map(|writer| {
                let repository = if writer % 3 == 0 { second.clone() } else { first.clone() };
                thread::spawn(move || {
                    let session = format!("cutex.writer-{writer}");
                    (0..PER_WRITER)
                        .map(|value| repository.append(pending(&session, value)).expect("append").sequence)
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let acknowledged = workers
            .into_iter()
            .map(|worker| worker.join().expect("join"))
            .collect::<Vec<_>>();
        for sequences in &acknowledged {
            assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]), "a writer's acks out of order");
        }
        let events = all_events(&first);
        assert_eq!(
            events.iter().map(|event| event.sequence).collect::<Vec<_>>(),
            (1..=(WRITERS * PER_WRITER) as u64).collect::<Vec<_>>()
        );
        for writer in 0..WRITERS {
            let session = format!("cutex.writer-{writer}");
            let values = events
                .iter()
                .filter(|event| event.cutex_session_id == session)
                .map(value_of)
                .collect::<Vec<_>>();
            assert_eq!(values, (0..PER_WRITER as u64).collect::<Vec<_>>(), "{session} reordered");
        }
        assert!(first.root.join(format!("{EVENTS_FILE_NAME}.1")).exists(), "batches crossed a rotation");
        assert!(first.stats().events.load(Ordering::Relaxed) > first.stats().batches.load(Ordering::Relaxed),
            "concurrent appends shared batches");
        // The final tail may belong to the other instance. The follower now
        // pushes every writer's events to this already-open subscription.
        first.follow_shared_journal().expect("follow final external tail");
        let live = subscription.receiver.try_iter().map(|event| event.sequence).collect::<Vec<_>>();
        assert_eq!(live, (1..=(WRITERS * PER_WRITER) as u64).collect::<Vec<_>>(), "live sequence has a gap, duplicate, or reordering");
        drop(subscription);
        drop((first, second));
        fs::remove_dir_all(root).expect("remove test repository");
    }

    /// A crash while a batch was being written: the acknowledged events are
    /// durable; the unacknowledged tail is either kept whole (cut on a line
    /// boundary: no reset, sequences continue) or, when a line is torn,
    /// quarantined behind a stream reset as before. The stream state
    /// checkpoint lags but never leads, so it never forces a reset itself.
    #[test]
    fn crash_mid_batch_keeps_acknowledged_events_and_handles_the_torn_tail() {
        for torn in [false, true] {
            let root = test_root("crash-mid-batch");
            let repository = Arc::new(EventRepository::open(&root, "devbox").expect("open"));
            let acknowledged = (0..4)
                .map(|writer| {
                    let repository = repository.clone();
                    thread::spawn(move || {
                        (0..5)
                            .map(|value| repository.append(pending("cutex.session-a", writer * 5 + value)).expect("append"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|worker| worker.join().expect("join"))
                .collect::<Vec<_>>();
            assert_eq!(acknowledged.len(), 20);
            let stream_id = acknowledged[0].stream_id.clone();
            // The batch in flight at the crash: lines 21..=24 encoded exactly
            // as the writer does, only a prefix of them reached the disk.
            let batch = repository
                .with_repository_lock(|repository| {
                    let (mut state, _) = repository.load_append_state_locked()?;
                    (0..4)
                        .map(|value| repository.stage_locked(&mut state, pending("cutex.session-a", 100 + value)))
                        .collect::<anyhow::Result<Vec<_>>>()
                })
                .expect("stage in-flight batch");
            drop(repository);
            let mut bytes = batch[0].1.clone();
            bytes.extend_from_slice(&batch[1].1);
            if torn {
                bytes.extend_from_slice(&batch[2].1[..batch[2].1.len() / 2]);
            }
            let mut file = OpenOptions::new()
                .append(true)
                .open(root.join(EVENTS_FILE_NAME))
                .expect("open journal");
            file.write_all(&bytes).expect("write in-flight prefix");
            file.sync_all().expect("sync");
            drop(file);
            let state: StreamState = serde_json::from_slice(&fs::read(root.join(STATE_FILE_NAME)).unwrap()).unwrap();
            assert!(state.next_sequence <= 21, "the checkpoint never leads the journal");

            let reopened = EventRepository::open(&root, "devbox").expect("reopen after crash");
            let next = reopened.append(pending("cutex.session-a", 200)).expect("append after crash");
            let reset = reopened.recovery_reset().expect("reset state");
            if torn {
                let reset = reset.expect("a torn line resets the stream");
                assert_eq!(reset.previous_stream_id.as_deref(), Some(stream_id.as_str()));
                assert_ne!(next.stream_id, stream_id);
                // The acknowledged events are kept (quarantined), not lost.
                let orphaned = fs::read_dir(root.join(ORPHANED_DIR_NAME))
                    .expect("orphaned dir")
                    .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
                    .collect::<String>();
                for event in &acknowledged {
                    assert!(orphaned.contains(&event.cursor), "acknowledged event lost");
                }
            } else {
                assert!(reset.is_none());
                assert_eq!(next.stream_id, stream_id);
                assert_eq!(next.sequence, 23);
                let events = all_events(&reopened);
                assert_eq!(events.iter().map(|event| event.sequence).collect::<Vec<_>>(), (1..=23).collect::<Vec<_>>());
                for event in &acknowledged {
                    assert_eq!(events[event.sequence as usize - 1].cursor, event.cursor);
                }
                assert_eq!(events[20].cursor, batch[0].0.cursor);
            }
            drop(reopened);
            fs::remove_dir_all(root).expect("remove test repository");
        }
    }

    /// The state file may lag, but a journal tail that moves behind what
    /// this process already acknowledged is a lost journal: reset, never
    /// reuse a sequence.
    #[test]
    fn journal_behind_acknowledged_tail_resets_even_with_a_lagging_checkpoint() {
        let root = test_root("journal-behind");
        let repository = EventRepository::open(&root, "devbox").expect("open");
        let events = (0..3)
            .map(|value| repository.append(pending("cutex.session-a", value)).expect("append"))
            .collect::<Vec<_>>();
        let state: StreamState = serde_json::from_slice(&fs::read(root.join(STATE_FILE_NAME)).unwrap()).unwrap();
        assert!(state.next_sequence < 4, "checkpoint lags within the interval");
        let journal = fs::read(root.join(EVENTS_FILE_NAME)).unwrap();
        let keep = journal.iter().enumerate().filter(|(_, byte)| **byte == b'\n').nth(1).unwrap().0 + 1;
        fs::write(root.join(EVENTS_FILE_NAME), &journal[..keep]).unwrap();
        let next = repository.append(pending("cutex.session-a", 9)).expect("append");
        let reset = repository.recovery_reset().unwrap().expect("lost journal resets");
        assert_eq!(reset.reason, "state_lost");
        assert_ne!(next.stream_id, events[0].stream_id);
        drop(repository);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn concurrent_appends_allocate_unique_strict_sequences() {
        let root = test_root("concurrent");
        let repository = Arc::new(
            EventRepository::open_with_options(
                &root,
                "devbox",
                RepositoryOptions {
                    rotate_bytes: 1024 * 1024,
                    retained_files: 3,
                },
            )
            .expect("open repository"),
        );
        let mut workers = Vec::new();
        for worker in 0..4 {
            let repository = repository.clone();
            workers.push(thread::spawn(move || {
                for value in 0..25 {
                    repository
                        .append(pending("cutex.session-a", worker * 25 + value))
                        .expect("append concurrent event");
                }
            }));
        }
        for worker in workers {
            worker.join().expect("join worker");
        }
        let page = repository
            .page(ReplayQuery {
                limit: 1000,
                ..ReplayQuery::default()
            })
            .expect("read all events");
        assert_eq!(page.events.len(), 100);
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=100).collect::<Vec<_>>()
        );
        drop(repository);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn rotation_preserves_stream_and_expires_only_removed_exact_cursors() {
        let root = test_root("rotation");
        let repository = EventRepository::open_with_options(
            &root,
            "devbox",
            RepositoryOptions {
                rotate_bytes: 700,
                retained_files: 1,
            },
        )
        .expect("open repository");
        let first = repository
            .append(pending("cutex.session-a", 1))
            .expect("append first event");
        let mut latest = first.clone();
        let mut retained = first.clone();
        for value in 2..=8 {
            latest = repository
                .append(pending("cutex.session-a", value))
                .expect("append rotated event");
            if value == 7 {
                retained = latest.clone();
            }
        }
        assert_eq!(latest.stream_id, first.stream_id);
        assert_eq!(latest.sequence, 8);
        let continuation = repository
            .page(ReplayQuery {
                stream_id: Some(retained.stream_id.clone()),
                after: Some(retained.cursor),
                limit: 100,
                cutex_session_id: None,
            })
            .expect("recent cursor remains valid across local rotation");
        assert_eq!(continuation.events.len(), 1);
        assert_eq!(continuation.events[0].sequence, latest.sequence);
        let error = repository
            .page(ReplayQuery {
                stream_id: Some(first.stream_id.clone()),
                after: Some(first.cursor.clone()),
                limit: 100,
                cutex_session_id: None,
            })
            .expect_err("first cursor should have expired");
        let ReplayError::CursorExpired {
            earliest, latest, ..
        } = error
        else {
            panic!("expected cursor_expired");
        };
        assert!(earliest.expect("earliest boundary").sequence > first.sequence);
        assert_eq!(latest.expect("latest boundary").sequence, 8);
        let changed = repository
            .page(ReplayQuery {
                stream_id: Some(Uuid::new_v4().to_string()),
                after: Some(first.cursor),
                limit: 100,
                cutex_session_id: None,
            })
            .expect_err("foreign stream should fail");
        assert!(matches!(changed, ReplayError::StreamChanged { .. }));
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn live_subscribers_exclude_dropped_lagged_and_failed_subscriptions() {
        let root = test_root("live-subscriptions");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        assert!(!repository.has_live_subscribers());
        let subscription = repository.page_and_subscribe(ReplayQuery::default(), 1).unwrap();
        let retained_lagged = subscription.lagged.clone();
        assert!(repository.has_live_subscribers());
        drop(subscription);
        assert!(!repository.has_live_subscribers(), "drop needs no subsequent append");
        assert!(!retained_lagged.load(Ordering::SeqCst));

        assert!(repository.page_and_subscribe(ReplayQuery::default(), 0).is_err());
        assert!(!repository.has_live_subscribers());
        let query = ReplayQuery { stream_id: Some(Uuid::new_v4().to_string()), ..ReplayQuery::default() };
        assert!(repository.page_and_subscribe(query, 1).is_err());
        assert!(!repository.has_live_subscribers(), "failed replay must remove its registration");

        let lagging = repository.page_and_subscribe(ReplayQuery::default(), 1).unwrap();
        let healthy = repository.page_and_subscribe(ReplayQuery::default(), 4).unwrap();
        repository.append(pending("cutex.session-a", 1)).unwrap();
        repository.append(pending("cutex.session-a", 2)).unwrap();
        assert!(lagging.lagged.load(Ordering::SeqCst));
        assert!(repository.has_live_subscribers(), "healthy subscription remains active");
        drop(healthy);
        assert!(!repository.has_live_subscribers(), "lagged receiver is only draining");
        drop(lagging);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn full_subscriber_is_marked_lagged_and_stream_sends_lagged_frame() {
        let root = test_root("lagged-subscription");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let subscription = repository
            .page_and_subscribe(
                ReplayQuery {
                    limit: 100,
                    ..ReplayQuery::default()
                },
                2,
            )
            .expect("open subscription");
        for sequence in 1..=3 {
            repository
                .append(pending("cutex.session-a", sequence))
                .expect("append live event");
        }
        assert!(subscription.lagged.load(Ordering::SeqCst));
        let mut out = Vec::new();
        super::super::server::stream_events(
            &mut out,
            subscription,
            &ReplayQuery::default(),
            crate::management::listeners::RevocationWatch::new(None),
        )
        .expect("stream ends");
        let text = String::from_utf8(out).unwrap();
        // The two queued events are delivered, then the lagged frame names
        // the last delivered cursor.
        assert_eq!(text.matches("event: cutex_management_event_v2").count(), 2);
        let frame = text
            .split("event: cutex_management_stream_error\ndata: ")
            .nth(1)
            .expect("lagged frame");
        let value: serde_json::Value =
            serde_json::from_str(frame.trim()).expect("lagged frame JSON");
        assert_eq!(value["error"]["code"], "lagged");
        assert_eq!(value["error"]["retryable"], true);
        let last_id = text
            .lines()
            .filter_map(|line| line.strip_prefix("id: "))
            .last()
            .unwrap();
        assert_eq!(value["error"]["details"]["lastDeliveredCursor"], last_id);
        // A subscriber that keeps up is not marked.
        let calm = repository
            .page_and_subscribe(ReplayQuery { limit: 100, ..ReplayQuery::default() }, 8)
            .expect("open subscription");
        repository
            .append(pending("cutex.session-a", 4))
            .expect("append");
        assert!(!calm.lagged.load(Ordering::SeqCst));
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn revoking_the_device_token_closes_its_event_stream() {
        use crate::management::client_tokens::{
            authenticate_management_request_with, ClientTokenStore,
        };
        let root = test_root("revoked-stream");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let store = ClientTokenStore::at(root.join("management-clients"));
        let (issued, _) = store.issue_device("Pixel", "Pixel", "android").unwrap();
        let mut request = crate::http::server::SimpleHttpRequest {
            method: "GET".into(),
            path: "/v2/events/stream".into(),
            headers: HashMap::from([(
                "authorization".to_string(),
                format!("Bearer {}", issued.token),
            )]),
            body: Vec::new(),
        };
        let verified =
            authenticate_management_request_with(&mut request, Some("root-secret"), &store)
                .expect("device token verifies");
        let subscription = repository
            .page_and_subscribe(ReplayQuery { limit: 100, ..ReplayQuery::default() }, 16)
            .expect("open subscription");
        let validator_store = store.clone();
        let watch = crate::management::listeners::RevocationWatch::with_validator(
            Some(verified),
            move |token| token.still_valid_in(&validator_store),
        );
        let revoker = {
            let store = store.clone();
            let token_id = issued.token_id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                assert!(store.revoke(&token_id).unwrap());
            })
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        super::super::server::stream_events(&mut out, subscription, &ReplayQuery::default(), watch)
            .expect("stream ends");
        revoker.join().unwrap();
        // In-process revoke is seen within one stream tick.
        assert!(started.elapsed() < std::time::Duration::from_millis(1500));
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("event: cutex/device/revoked\ndata: "),
            "{text}"
        );
        assert!(text.contains(&issued.token_id));
        assert!(!text.contains("\nid: "));
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn page_and_subscription_have_no_local_replay_live_gap() {
        let root = test_root("subscription");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        repository
            .append(pending("cutex.session-a", 1))
            .expect("append replay event");
        let subscription = repository
            .page_and_subscribe(
                ReplayQuery {
                    limit: 100,
                    ..ReplayQuery::default()
                },
                4,
            )
            .expect("open subscription");
        assert_eq!(subscription.page.events.len(), 1);
        let live = repository
            .append(pending("cutex.session-a", 2))
            .expect("append live event");
        let received = subscription
            .receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("receive live event");
        assert_eq!(received.event_id, live.event_id);
        assert_eq!(received.sequence, subscription.page.checkpoint.sequence + 1);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn external_append_requires_replay_for_an_existing_subscription() {
        let root = test_root("external-subscription-replay");
        let reader = EventRepository::open(&root, "devbox").expect("open reader repository");
        let writer = EventRepository::open(&root, "devbox").expect("open writer repository");
        reader
            .append(pending("cutex.session-a", 1))
            .expect("append replay event");
        let subscription = reader
            .page_and_subscribe(
                ReplayQuery {
                    limit: 100,
                    ..ReplayQuery::default()
                },
                4,
            )
            .expect("open subscription");
        let external = writer
            .append(pending("cutex.session-a", 2))
            .expect("append through external repository");
        assert!(matches!(
            subscription
                .receiver
                .recv_timeout(std::time::Duration::from_millis(25)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        let replay = reader
            .page(ReplayQuery {
                stream_id: Some(subscription.page.stream_id),
                after: subscription.page.next_cursor,
                limit: 100,
                cutex_session_id: None,
            })
            .expect("replay external append");
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].event_id, external.event_id);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn corrupt_state_rotates_instead_of_guessing_continuity() {
        let root = test_root("corrupt-state");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        let first = repository
            .append(pending("cutex.session-a", 1))
            .expect("append first event");
        fs::write(root.join(STATE_FILE_NAME), b"{not-json\n").expect("corrupt state");
        let reopened = EventRepository::open(&root, "devbox").expect("reopen repository");
        let reset = reopened
            .recovery_reset()
            .expect("read reset")
            .expect("recovery reset");
        assert_eq!(
            reset.previous_stream_id.as_deref(),
            Some(first.stream_id.as_str())
        );
        assert_ne!(reset.stream_id, first.stream_id);
        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[cfg(unix)]
    #[test]
    fn repository_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("permissions");
        let repository = EventRepository::open(&root, "devbox").expect("open repository");
        repository
            .append(pending("cutex.session-a", 1))
            .expect("append event");
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for path in [
            root.join(LOCK_FILE_NAME),
            root.join(STATE_FILE_NAME),
            root.join(EVENTS_FILE_NAME),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).expect("remove test repository");
    }

    fn journal_files(root: &Path) -> Vec<String> {
        let mut names = fs::read_dir(root)
            .expect("list repository")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(EVENTS_FILE_NAME))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Rewrites the `receivedAt` of every record in one journal file.
    fn age_journal_file(path: &Path, at: chrono::DateTime<Utc>) {
        const KEY: &str = "\"receivedAt\":\"";
        let stamp = at.to_rfc3339();
        let aged = fs::read_to_string(path)
            .expect("read journal file")
            .lines()
            .map(|line| {
                let start = line.find(KEY).expect("receivedAt") + KEY.len();
                let end = start + line[start..].find('"').expect("receivedAt end");
                format!("{}{stamp}{}\n", &line[..start], &line[end..])
            })
            .collect::<String>();
        fs::write(path, aged).expect("age journal file");
    }

    fn page_after(
        repository: &EventRepository,
        stream_id: &str,
        cursor: &str,
        limit: usize,
    ) -> Result<EventPage, ReplayError> {
        repository.page(ReplayQuery {
            stream_id: Some(stream_id.to_string()),
            after: Some(cursor.to_string()),
            limit,
            cutex_session_id: None,
        })
    }

    fn sequences(page: &EventPage) -> Vec<u64> {
        page.events.iter().map(|event| event.sequence).collect()
    }

    /// Files past the hot window are only boundary-checked at open and not
    /// indexed; a cursor in one is found by search, the cursor a page hands
    /// out is indexed, and another instance's rotation is followed.
    #[test]
    fn cold_files_are_not_indexed_and_their_cursors_are_found_by_search() {
        let root = test_root("cold-cursors");
        // One record per file; 9 slots, the oldest 5 cold.
        let options = RepositoryOptions { rotate_bytes: 300, retained_files: 8 };
        assert_eq!(options.hot_start(), 5);
        let writer =
            EventRepository::open_with_options(&root, "devbox", options).expect("open writer");
        let appended = (0..14)
            .map(|value| writer.append(pending("cutex.session-a", value)).expect("append"))
            .collect::<Vec<_>>();
        assert_eq!(journal_files(&root).len(), 9);
        let stream_id = appended[0].stream_id.clone();

        let reader =
            EventRepository::open_with_options(&root, "devbox", options).expect("open reader");
        assert!(reader.recovery_reset().unwrap().is_none());
        {
            let index = reader.replay_index.lock().unwrap();
            let indexed = |event: &EventEnvelope| index.cursors.contains_key(&event.cursor);
            assert!(appended[..10].iter().all(|event| !indexed(event)), "cold cursors are not indexed");
            assert!(appended[10..].iter().all(indexed), "hot cursors are");
            assert_eq!(index.files[0].first.as_ref().unwrap().sequence, 6, "cold stamps come from boundaries");
        }

        // Sequence 7 sits in a cold file.
        let page = page_after(&reader, &stream_id, &appended[6].cursor, 2).expect("cold cursor page");
        assert_eq!(sequences(&page), vec![8, 9]);
        assert!(page.has_more);
        let next = page.next_cursor.clone().unwrap();
        assert!(
            reader.replay_index.lock().unwrap().cursors.contains_key(&next),
            "a handed-out cold cursor is indexed"
        );
        let page = page_after(&reader, &stream_id, &next, 100).expect("continue from cold");
        assert_eq!(sequences(&page), (10..=14).collect::<Vec<_>>());
        assert!(!page.has_more);

        // Rotated out by the ceiling, or never existed: expired, not an error.
        for cursor in [appended[4].cursor.as_str(), "c2:00000000-0000-4000-8000-000000000000"] {
            assert!(matches!(
                page_after(&reader, &stream_id, cursor, 10),
                Err(ReplayError::CursorExpired { .. })
            ));
        }

        // Another instance rotates twice; a cold cursor still resolves.
        for value in 14..16 {
            writer.append(pending("cutex.session-a", value)).expect("external append");
        }
        let page =
            page_after(&reader, &stream_id, &appended[8].cursor, 3).expect("after external rotation");
        assert_eq!(sequences(&page), vec![10, 11, 12]);
        let query = ReplayQuery {
            stream_id: Some(stream_id.clone()),
            after: Some(appended[8].cursor.clone()),
            limit: 100,
            cutex_session_id: None,
        };
        assert_eq!(reader.page(query.clone()).unwrap(), reference_page(&reader, &query));
        assert!(reader.recovery_reset().unwrap().is_none());
        fs::remove_dir_all(root).expect("remove test repository");
    }

    /// Owner rule: the stream keeps at least 24 hours. A rotation removes the
    /// oldest sealed files whose newest event is a day old, never one inside
    /// the window, and always keeps the newest three sealed files.
    #[test]
    fn rotation_prunes_only_files_older_than_a_day() {
        let root = test_root("retention");
        let options = RepositoryOptions { rotate_bytes: 300, retained_files: 8 };
        let repository =
            EventRepository::open_with_options(&root, "devbox", options).expect("open");
        let appended = (1..=7)
            .map(|value| repository.append(pending("cutex.session-a", value)).expect("append"))
            .collect::<Vec<_>>();
        let stream_id = appended[0].stream_id.clone();
        // events.jsonl.6 holds sequence 1, .5 sequence 2, .4 sequence 3.
        let now = Utc::now();
        let day = chrono::Duration::hours(24);
        age_journal_file(&root.join("events.jsonl.6"), now - day - chrono::Duration::hours(1));
        age_journal_file(&root.join("events.jsonl.5"), now - day - chrono::Duration::minutes(1));
        age_journal_file(&root.join("events.jsonl.4"), now - day + chrono::Duration::minutes(10));
        drop(repository);

        let repository =
            EventRepository::open_with_options(&root, "devbox", options).expect("reopen");
        assert_eq!(
            sequences(&repository.page(ReplayQuery::default()).unwrap()),
            (1..=7).collect::<Vec<_>>()
        );
        repository.append(pending("cutex.session-a", 8)).expect("rotating append");
        assert_eq!(
            journal_files(&root),
            [
                "events.jsonl",
                "events.jsonl.1",
                "events.jsonl.2",
                "events.jsonl.3",
                "events.jsonl.4",
                "events.jsonl.5"
            ]
        );
        assert!(
            repository.replay_index.lock().unwrap().stream_id.is_some(),
            "the index followed the pruning rotation"
        );
        let page = repository.page(ReplayQuery::default()).expect("initial page");
        assert_eq!(sequences(&page), (3..=8).collect::<Vec<_>>(), "the file inside the day stays");
        assert_eq!(page, reference_page(&repository, &ReplayQuery::default()));
        assert!(matches!(
            page_after(&repository, &stream_id, &appended[0].cursor, 10),
            Err(ReplayError::CursorExpired { earliest: Some(RetainedBoundary { sequence: 3, .. }), .. })
        ));
        assert_eq!(
            sequences(&page_after(&repository, &stream_id, &appended[2].cursor, 10).unwrap()),
            (4..=8).collect::<Vec<_>>()
        );

        // Everything sealed is old: the newest three sealed files remain.
        for index in 1..=5 {
            age_journal_file(&root.join(format!("events.jsonl.{index}")), now - day * 2);
        }
        drop(repository);
        let repository =
            EventRepository::open_with_options(&root, "devbox", options).expect("reopen");
        repository.append(pending("cutex.session-a", 9)).expect("rotating append");
        assert_eq!(
            journal_files(&root),
            ["events.jsonl", "events.jsonl.1", "events.jsonl.2", "events.jsonl.3"]
        );
        assert_eq!(
            sequences(&repository.page(ReplayQuery::default()).unwrap()),
            (6..=9).collect::<Vec<_>>()
        );
        assert!(repository.recovery_reset().unwrap().is_none());
        fs::remove_dir_all(root).expect("remove test repository");
    }

    /// A cold file is only boundary-checked at open, so a corrupt record in
    /// its middle is found when a replay reaches it; the full check then
    /// resets the stream as a corrupt hot file always did.
    #[test]
    fn corrupt_cold_record_resets_the_stream_when_a_replay_reaches_it() {
        let probe_root = test_root("cold-corruption-probe");
        let probe = EventRepository::open(&probe_root, "devbox").expect("open probe");
        probe.append(pending("cutex.session-a", 10)).expect("probe append");
        let line = fs::metadata(probe_root.join(EVENTS_FILE_NAME)).unwrap().len();
        drop(probe);
        fs::remove_dir_all(probe_root).ok();

        let root = test_root("cold-corruption");
        // Three records per file.
        let options = RepositoryOptions { rotate_bytes: line * 3 + line / 2, retained_files: 8 };
        let repository =
            EventRepository::open_with_options(&root, "devbox", options).expect("open");
        for value in 10..37 {
            repository.append(pending("cutex.session-a", value)).expect("append");
        }
        let before = repository.stream_metadata().unwrap();
        let cold = root.join("events.jsonl.7");
        let mut lines =
            fs::read_to_string(&cold).unwrap().lines().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        lines[1] = "{not-json".to_string();
        fs::write(&cold, format!("{}\n", lines.join("\n"))).unwrap();
        drop(repository);

        let repository =
            EventRepository::open_with_options(&root, "devbox", options).expect("reopen");
        assert!(repository.recovery_reset().unwrap().is_none(), "boundaries are intact");
        let page = repository.page(ReplayQuery::default()).expect("replay repairs");
        let reset = repository.recovery_reset().unwrap().expect("corruption reset");
        assert_ne!(page.stream_id, before.stream_id);
        assert_eq!(page.stream_id, reset.stream_id);
        fs::remove_dir_all(root).expect("remove test repository");
    }
}
