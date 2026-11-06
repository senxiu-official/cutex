//! Best-effort *observation* ingress. Commands still use strict repository
//! append: accepting an observation into RAM is never a durable acknowledgment.
use super::model::{EventEnvelope, PendingEvent};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

const MAX_EVENTS: usize = 512;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
type Completion = Box<dyn FnOnce(Option<EventEnvelope>) -> anyhow::Result<()> + Send>;
type Append = dyn Fn(PendingEvent) -> anyhow::Result<Option<EventEnvelope>> + Send + Sync;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    Durable,
    Buffered,
    Omitted,
}

struct Entry {
    ticket: u64,
    pending: PendingEvent,
    completion: Completion,
    bytes: usize,
}

#[derive(Default)]
struct State {
    next_ticket: u64,
    in_flight: usize,
    entries: VecDeque<Entry>,
    bytes: usize,
    unavailable: Option<serde_json::Value>,
    omitted: u64,
    loss_notice: Option<PendingEvent>,
}

/// One queue per process, shared by all connected runtimes. Both count and
/// retained payload cost are bounded. A full queue omits new observations,
/// reports the loss, and keeps the transport alive; it never claims durability.
pub struct RuntimeJournal {
    path: PathBuf,
    state: Mutex<State>,
    append: Box<Append>,
}

pub(crate) fn blocked_io(error: &anyhow::Error) -> Option<i32> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .find_map(|io| {
            if matches!(io.raw_os_error(), Some(225 | 226)) {
                return io.raw_os_error();
            }
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                return Some(io.raw_os_error().unwrap_or(5));
            }
            None
        })
}

impl RuntimeJournal {
    pub(crate) fn new(
        path: PathBuf,
        append: impl Fn(PendingEvent) -> anyhow::Result<Option<EventEnvelope>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            path,
            state: Mutex::new(State::default()),
            append: Box::new(append),
        }
    }

    fn record_failure(&self, state: &mut State, error: &anyhow::Error) {
        let code = blocked_io(error);
        let condition = if code.is_some() {
            "event_journal_blocked"
        } else {
            "event_journal_unavailable"
        };
        if state
            .unavailable
            .as_ref()
            .and_then(|v| v["condition"].as_str())
            != Some(condition)
        {
            eprintln!("{condition}: {}; runtime connections remain open; observations in memory are NOT durable: {error:#}", self.path.display());
        }
        state.unavailable = Some(serde_json::json!({
            "condition":condition, "file":self.path, "osError":code,
            "reason":format!("{error:#}").chars().take(512).collect::<String>(),
            "nextAction":"Inspect antivirus quarantine/file permissions or disk availability; restore journal access. Do not delete journal/state files."
        }));
    }

    pub fn submit(
        &self,
        pending: PendingEvent,
        retained_bytes: usize,
        completion: Completion,
    ) -> anyhow::Result<Acceptance> {
        pending.validate()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        // Only the empty, healthy fast path attempts IO on a receive worker.
        // During an outage producers only enqueue; retries have a fixed cadence.
        if state.entries.is_empty() && state.unavailable.is_none() {
            // Preserve the repository's normal cross-runtime group commit.
            // No queue mutex is held across healthy IO. The retry worker waits
            // for these calls to settle before draining a failed prefix.
            state.in_flight += 1;
            drop(state);
            let outcome = (self.append)(pending.clone());
            state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.in_flight -= 1;
            match outcome {
                Ok(envelope) => {
                    drop(state);
                    completion(envelope)?;
                    return Ok(Acceptance::Durable);
                }
                Err(error) => self.record_failure(&mut state, &error),
            }
        }
        if state.entries.len() >= MAX_EVENTS
            || state.bytes.saturating_add(retained_bytes) > MAX_BYTES
        {
            state.omitted = state.omitted.saturating_add(1);
            if state.omitted == 1 {
                eprintln!("event_journal_unavailable: {}: bounded observation queue full; observation omitted without durable acknowledgment; resync from rollout required", self.path.display());
            }
            if state.loss_notice.is_none() {
                state.loss_notice = Some(PendingEvent {
                    cutex_session_id: pending.cutex_session_id.clone(),
                    host_id: pending.host_id.clone(),
                    source: super::model::EventSource::Cutex,
                    schema: None,
                    correlation: super::model::EventCorrelation::default(),
                    native: None,
                    cutex: Some(super::model::CutexMessage {
                        method: "cutex/stream/reset".into(),
                        params: serde_json::json!({"previousStreamId":null,"previousLastSequence":0,
                            "reason":"persistence_failure","lostEventCount":null,
                            "resyncRequired":true,"detectedAt":chrono::Utc::now().to_rfc3339()}),
                    }),
                });
            }
            return Ok(Acceptance::Omitted);
        }
        state.bytes += retained_bytes;
        // Two healthy appends can fail in reverse completion order. Keep their
        // original admission order ahead of observations accepted meanwhile.
        let position = state.entries.partition_point(|entry| entry.ticket < ticket);
        state.entries.insert(
            position,
            Entry {
                ticket,
                pending,
                completion,
                bytes: retained_bytes,
            },
        );
        Ok(Acceptance::Buffered)
    }

    /// Retry a bounded number in ingress order. Remove only after append has
    /// confirmed fsync (or the stop observer has durably registered its work).
    /// Never retry a completion: that could repeat business side effects.
    pub(crate) fn tick(&self) -> bool {
        for _ in 0..32 {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.in_flight != 0 {
                return false;
            }
            let Some(entry) = state.entries.front() else {
                if let Some(mut notice) = state.loss_notice.clone() {
                    notice.cutex.as_mut().unwrap().params["lostEventCount"] = state.omitted.into();
                    match (self.append)(notice) {
                        Ok(_) => {
                            state.loss_notice = None;
                        }
                        Err(error) => {
                            self.record_failure(&mut state, &error);
                            return false;
                        }
                    }
                }
                if state.unavailable.take().is_some() {
                    eprintln!(
                        "event_journal_recovered: {}; buffered observations drained; omitted={}",
                        self.path.display(),
                        state.omitted
                    );
                }
                return false;
            };
            let envelope = match (self.append)(entry.pending.clone()) {
                Ok(envelope) => envelope,
                Err(error) => {
                    self.record_failure(&mut state, &error);
                    return false;
                }
            };
            let entry = state.entries.pop_front().expect("front exists");
            state.bytes = state.bytes.saturating_sub(entry.bytes);
            // Keep this process's callbacks in the same order as appends.
            // Producers cannot execute a newer fast-path callback until the
            // backlog's unavailable flag has been cleared after callbacks.
            drop(state);
            if let Err(error) = (entry.completion)(envelope) {
                eprintln!(
                    "event journal durable observation follow-up failed (not retried): {error:#}"
                );
            }
        }
        true
    }

    pub fn health(&self) -> Option<serde_json::Value> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.unavailable.is_none() && state.omitted == 0 {
            return None;
        }
        let mut detail = state.unavailable.clone().unwrap_or_else(|| serde_json::json!({
            "condition":"event_journal_observations_lost", "file":self.path,
            "nextAction":"Resync affected conversations from rollout; omitted observations were never durable."
        }));
        detail["bufferedEvents"] = state.entries.len().into();
        detail["bufferedBytes"] = state.bytes.into();
        detail["omittedEvents"] = state.omitted.into();
        detail["bufferIsDurable"] = false.into();
        Some(detail)
    }
}

static JOURNAL: OnceLock<Arc<RuntimeJournal>> = OnceLock::new();
static START: Mutex<()> = Mutex::new(());

fn journal() -> anyhow::Result<&'static Arc<RuntimeJournal>> {
    if let Some(journal) = JOURNAL.get() {
        return Ok(journal);
    }
    let _start = START
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(journal) = JOURNAL.get() {
        return Ok(journal);
    }
    // Resolve a path even when opening the actual journal is currently blocked.
    let path = crate::config::paths::runtime_dir()?.join("management-v2/events.jsonl");
    let journal = Arc::new(RuntimeJournal::new(
        path,
        super::native_events::append_observed_event,
    ));
    let worker = journal.clone();
    std::thread::Builder::new()
        .name("runtime-event-journal-retry".into())
        .spawn(move || loop {
            if worker.tick() {
                // Once writes recover, drain in bounded batches without a
                // 32-events/second ceiling that could perpetuate overflow.
                std::thread::yield_now();
            } else {
                std::thread::sleep(RETRY_INTERVAL);
            }
        })?;
    let _ = JOURNAL.set(journal);
    Ok(JOURNAL.get().expect("journal initialized"))
}

/// The caller accounts for the pending payload AND captured context/event.
/// No envelope/cursor is returned for a volatile acceptance.
pub fn submit(
    pending: PendingEvent,
    retained_bytes: usize,
    completion: impl FnOnce(Option<EventEnvelope>) -> anyhow::Result<()> + Send + 'static,
) -> anyhow::Result<Acceptance> {
    journal()?.submit(pending, retained_bytes, Box::new(completion))
}

pub fn health() -> Option<serde_json::Value> {
    if let Some(detail) = JOURNAL.get().and_then(|journal| journal.health()) {
        return Some(detail);
    }
    // An agent-bus process may own the affected events. A read-only probe lets
    // management report an AV/access block even without a local producer.
    let path = crate::config::paths::runtime_dir()
        .ok()?
        .join("management-v2/events.jsonl");
    if let Err(error) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        let error = anyhow::Error::new(error);
        if let Some(code) = blocked_io(&error) {
            return Some(
                serde_json::json!({"condition":"event_journal_blocked","file":path,
                "osError":code,"reason":error.to_string(),"bufferIsDurable":false,
                "nextAction":"Inspect antivirus quarantine/file permissions; restore journal access without deleting journal/state files."}),
            );
        }
    }
    None
}

/// Conservative accounting for JSON storage (including String/Value overhead)
/// and the captured original native event/context used by the durable callback.
pub fn retained_cost(pending: &PendingEvent) -> usize {
    let mut bytes = pending
        .cutex_session_id
        .len()
        .saturating_add(pending.host_id.len());
    if let Some(native) = &pending.native {
        bytes = bytes.saturating_add(super::event_blobs::encoded_len(&native.message));
    }
    if let Some(cutex) = &pending.cutex {
        bytes = bytes
            .saturating_add(cutex.method.len())
            .saturating_add(super::event_blobs::encoded_len(&cutex.params));
    }
    if let Some(schema) = &pending.schema {
        bytes = bytes.saturating_add(serde_json::to_vec(schema).map_or(MAX_BYTES, |v| v.len()));
    }
    bytes.saturating_mul(16).saturating_add(64 * 1024)
}

#[cfg(test)]
#[path = "runtime_journal_tests.rs"]
mod tests;
