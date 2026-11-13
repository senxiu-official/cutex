//! Durable replay, bounded ingress: notices only wake a cursor-based consumer
//! of the already-durable Management journal. No disk I/O on the producer.
use super::*;
use crate::management::v2::{model::EventEnvelope, repository::{EventRepository, ReplayError, ReplayQuery}};
use serde::{Deserialize, Serialize};
use std::sync::{mpsc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

const PAGE: usize = 256;
const GAP_KEY: &str = "captureEventReplayGap";
const FAILURE_KEY: &str = "captureEventReplayFailure";
const MAX_EVENT_ATTEMPTS: u64 = 3;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static START_CURSOR: OnceLock<Checkpoint> = OnceLock::new();

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    stream_id: Option<String>,
    cursor: Option<String>,
}
impl Checkpoint {
    fn at(event: &EventEnvelope) -> Self {
        Self { stream_id: Some(event.stream_id.clone()), cursor: Some(event.cursor.clone()) }
    }
}
#[derive(Clone)]
struct Notice {
    event: TurnEvent,
    checkpoint: Checkpoint,
}
fn notice(envelope: &EventEnvelope) -> Result<Option<Notice>, chrono::ParseError> {
    let Some(native) = &envelope.native else { return Ok(None); };
    if native.kind != crate::management::v2::model::NativeMessageKind::Notification { return Ok(None); }
    let method = native.message["method"].as_str().unwrap_or_default();
    if !matches!(method, "turn/started" | "turn/completed") { return Ok(None); }
    let at = DateTime::parse_from_rfc3339(&envelope.received_at)?.timestamp_millis();
    let event = TurnEvent::from_notification(&envelope.cutex_session_id,
        envelope.correlation.runtime_generation.unwrap_or(0), method, native.message.get("params"), at)
        .expect("turn notification was filtered above");
    Ok(Some(Notice { event, checkpoint: Checkpoint::at(envelope) }))
}
fn loss_notice(envelope: &EventEnvelope) -> bool {
    envelope.cutex.as_ref().is_some_and(|message| message.method == "cutex/stream/reset"
        && (message.params["resyncRequired"] == true || message.params["reason"] == "persistence_failure"))
}
fn mark_event_gap(store: &ArtifactStore, envelope: &EventEnvelope, reason: &str) -> anyhow::Result<()> {
    store.set_meta(GAP_KEY, Some(&serde_json::json!({
        "at": Utc::now(), "eventId": envelope.event_id, "session": envelope.cutex_session_id,
        "checkpoint": Checkpoint::at(envelope), "reason": reason,
    }).to_string()))?;
    eprintln!("artifacts: capture replay gap recorded at {GAP_KEY}, event {}: {reason}", envelope.event_id);
    Ok(())
}
fn failed_decode(store: &ArtifactStore, envelope: &EventEnvelope, error: &chrono::ParseError) -> anyhow::Result<bool> {
    let previous: serde_json::Value = store.meta(FAILURE_KEY)?.and_then(|value| serde_json::from_str(&value).ok()).unwrap_or_default();
    let attempts = if previous["eventId"] == envelope.event_id {
        previous["attempts"].as_u64().unwrap_or(0).saturating_add(1)
    } else { 1 };
    store.set_meta(FAILURE_KEY, Some(&serde_json::json!({
        "eventId": envelope.event_id, "attempts": attempts, "error": error.to_string(),
    }).to_string()))?;
    if attempts < MAX_EVENT_ATTEMPTS { return Ok(false); }
    mark_event_gap(store, envelope, &format!("event failed {attempts} times: {error:#}"))?;
    // Persist the explicit gap before advancing. A crash between these writes
    // can repeat the marker, but cannot silently acknowledge an omitted event.
    store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&Checkpoint::at(envelope))?))?;
    Ok(true)
}

/// Call only after the Management journal has committed this envelope.
pub fn on_durable_event(envelope: &EventEnvelope) {
    // Only the Management server owns capture consumption. Other CLI
    // processes may append to this same journal without starting consumers.
    if !ACTIVE.load(Ordering::Acquire) { return; }
    if !loss_notice(envelope) && !envelope.native.as_ref().is_some_and(|native| native.kind == crate::management::v2::model::NativeMessageKind::Notification
        && matches!(native.message["method"].as_str(), Some("turn/started" | "turn/completed"))) { return; }
    if let Some(sender) = worker() {
        match sender.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => {} // Source remains in the journal.
            Err(mpsc::TrySendError::Disconnected(_)) => eprintln!("artifacts: capture worker unavailable; event {} of {} remains in Management journal for cursor replay", envelope.event_id, envelope.cutex_session_id),
        }
    }
}
/// Resume committed capture cursors at daemon startup, even with no new turns.
pub fn start() {
    // Freeze the first-install lower bound before enabling producer hints.
    // Using the first callback as a seed could skip an earlier writer's turn.
    START_CURSOR.get_or_init(|| match crate::management::v2::repository::management_v2_repository().and_then(|repository| repository.checkpoint()) {
        Ok(checkpoint) => Checkpoint { stream_id: Some(checkpoint.stream_id), cursor: checkpoint.cursor },
        Err(error) => {
            eprintln!("artifacts: initial capture boundary unavailable; will replay retained journal instead of skipping data: {error:#}");
            Checkpoint::default()
        }
    });
    ACTIVE.store(true, Ordering::Release);
    let _ = worker();
}
fn worker() -> Option<&'static mpsc::SyncSender<()>> {
    static WORKER: OnceLock<Option<mpsc::SyncSender<()>>> = OnceLock::new();
    WORKER.get_or_init(|| {
        if crate::config::paths::in_test_harness() { return None; }
        let (sender, receiver) = mpsc::sync_channel(1);
        match std::thread::Builder::new().name("cutex-artifact-capture".into()).spawn(move || run(receiver)) {
            Ok(_) => Some(sender),
            Err(error) => { eprintln!("artifacts: auto capture worker not started: {error}; durable turns remain in Management journal"); None }
        }
    }).as_ref()
}

fn apply(store: &ArtifactStore, event: &Notice, enabled: bool, facts: &AgentFacts) -> anyhow::Result<Option<String>> {
    let checkpoint = serde_json::to_string(&event.checkpoint)?;
    if enabled {
        store.record_capture_event(&event.event, facts, Some(&checkpoint))
    } else {
        store.set_meta(REPLAY_CURSOR_KEY, Some(&checkpoint))?;
        Ok(None)
    }
}

fn page(
    repository: &EventRepository,
    store: &ArtifactStore,
    enabled: bool,
    facts: impl FnMut(&str) -> anyhow::Result<AgentFacts>,
    schedule: impl FnMut(&str, i64),
) -> anyhow::Result<bool> {
    page_decoded(repository, store, enabled, facts, schedule, notice)
}
fn page_decoded(
    repository: &EventRepository,
    store: &ArtifactStore,
    enabled: bool,
    mut facts: impl FnMut(&str) -> anyhow::Result<AgentFacts>,
    mut schedule: impl FnMut(&str, i64),
    mut decode: impl FnMut(&EventEnvelope) -> Result<Option<Notice>, chrono::ParseError>,
) -> anyhow::Result<bool> {
    let saved = store.meta(REPLAY_CURSOR_KEY)?.ok_or_else(|| anyhow::anyhow!("capture replay has no initial cursor"))?;
    let checkpoint: Checkpoint = serde_json::from_str(&saved)?;
    let latest = repository.checkpoint()?;
    if checkpoint.stream_id.as_deref() == Some(latest.stream_id.as_str()) && checkpoint.cursor == latest.cursor {
        return Ok(false); // Idle checkpoint probe, no journal page scan.
    }
    let query = ReplayQuery { after: checkpoint.cursor.clone(), stream_id: checkpoint.stream_id.clone(), limit: PAGE, cutex_session_id: None };
    let page = match repository.page(query) {
        Ok(page) => page,
        Err(error @ (ReplayError::CursorExpired { .. } | ReplayError::StreamChanged { .. })) => {
            // A retention/stream gap is a durable, explicit degraded marker.
            // Never silently present the new cursor as fully caught up.
            store.set_meta(GAP_KEY, Some(&serde_json::json!({"at":Utc::now(),"previous":checkpoint,"reason":error.to_string()}).to_string()))?;
            store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&Checkpoint::default())?))?;
            anyhow::bail!("artifact capture replay gap recorded at {GAP_KEY}: {error}");
        }
        Err(error) => return Err(error.into()),
    };
    for envelope in &page.events {
        if loss_notice(envelope) {
            mark_event_gap(store, envelope, &format!("upstream observation loss: {}", serde_json::to_string(&envelope.cutex)?))?;
        }
        let event = match decode(envelope) {
            Ok(event) => event,
            Err(error) => {
                if !failed_decode(store, envelope, &error)? { return Err(error.into()); }
                continue;
            }
        };
        if let Some(event) = event {
            // Attribution and persistence failures may be transient. They
            // never increment the permanent-decode counter or skip a turn.
            let facts = if enabled { facts(&event.event.session)? } else { AgentFacts::default() };
            if let Some(project) = apply(store, &event, enabled, &facts)? {
                schedule(&project, event.event.at_ms);
            }
        }
    }
    if let Some(last) = page.events.last() {
        // Turn mutations committed their own cursor atomically. This advances
        // past unrelated events only after every turn in this page succeeded.
        store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&Checkpoint::at(last))?))?;
    }
    Ok(page.has_more)
}

#[derive(Default)]
struct RetryBackoff { failures: u32, until: Option<std::time::Instant> }
impl RetryBackoff {
    fn failed(&mut self, now: std::time::Instant) {
        self.failures = self.failures.saturating_add(1);
        let seconds = 5u64.saturating_mul(1u64 << self.failures.saturating_sub(1).min(6)).min(300);
        self.until = Some(now + Duration::from_secs(seconds));
    }
    fn remaining(&self, now: std::time::Instant) -> Option<Duration> {
        self.until.and_then(|until| until.checked_duration_since(now)).filter(|left| !left.is_zero())
    }
}
fn run(receiver: mpsc::Receiver<()>) {
    let host = ManagementHost;
    let scans = scan_worker::Worker::start().map_err(|error| eprintln!("artifacts: capture scanner unavailable: {error:#}")).ok();
    let mut more = true;
    let mut last_prune_ms = 0;
    let mut backoff = RetryBackoff::default();
    loop {
        if let Some(left) = backoff.remaining(std::time::Instant::now()) {
            if matches!(receiver.recv_timeout(left), Err(mpsc::RecvTimeoutError::Disconnected)) { return; }
            continue; // New hints must not bypass outage backoff.
        }
        let result = (|| -> anyhow::Result<()> {
            let store = host.store()?;
            let repository = crate::management::v2::repository::management_v2_repository()?;
            let enabled = host.settings().auto_capture.enabled;
            // Do not rebuild the directory on idle checkpoint probes. Load it
            // lazily once for this pass, and retain the cursor on read failure
            // rather than misclassifying a member as having no project.
            let mut context = None;
            let mut facts = |session: &str| -> anyhow::Result<AgentFacts> {
                if context.is_none() {
                    let directory = crate::asks::directory::Directory::load()
                        .map_err(|error| anyhow::anyhow!("capture attribution directory unavailable: {error:?}"))?;
                    let asks = crate::asks::store::AskStore::open_default()?.peek()?;
                    context = Some((directory, asks));
                }
                let (directory, asks) = context.as_ref().unwrap();
                Ok(facts_of(&host, Some(directory), asks.as_ref(), session))
            };
            let schedule = |project: &str, at| { if let Some(scans) = &scans { scans.schedule(project.to_string(), at); } };
            if store.meta(REPLAY_CURSOR_KEY)?.is_none() {
                let initial = START_CURSOR.get().cloned().unwrap_or_default();
                store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&initial)?))?;
            }
            more = false;
            for _ in 0..4 {
                more = page(repository, &store, enabled, &mut facts, schedule)?;
                if !more { break; }
            }
            let now_ms = Utc::now().timestamp_millis();
            if now_ms - last_prune_ms >= PRUNE_EVERY_MS {
                store.prune_windows(now_ms - WINDOW_RETENTION_MS)?;
                last_prune_ms = now_ms;
            }
            Ok(())
        })();
        let retry = result.is_err();
        if let Err(error) = result {
            eprintln!("artifacts: capture deferred; durable cursor retained: {error:#}");
            more = false;
            backoff.failed(std::time::Instant::now());
        } else {
            backoff = RetryBackoff::default();
        }
        let received = if more || retry {
            receiver.recv_timeout(if more { Duration::ZERO } else { backoff.remaining(std::time::Instant::now()).unwrap_or_default() })
        } else {
            // Other CLI processes can append while this process has no live
            // SSE subscribers/follower. A cheap checkpoint probe recovers those
            // turns too; unchanged journals are not continuously scanned.
            receiver.recv_timeout(Duration::from_secs(5))
        };
        match received {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::model::*;
    use serde_json::json;

    fn append(repository: &EventRepository, started: bool, turn: usize) -> EventEnvelope {
        repository.append(PendingEvent {
            cutex_session_id: "cutex.capture".into(), host_id: "fixture".into(), source: EventSource::AppServer,
            schema: Some(AppServerSchema { protocol: "codex-app-server".into(), major_version: 2, version: "fixture".into(), sha256: "a".repeat(64), channel: AppServerSchemaChannel::Experimental, capabilities: json!({}), extensions: vec![] }),
            correlation: EventCorrelation { runtime_generation: Some(1), ..Default::default() },
            native: Some(NativeMessage { kind: NativeMessageKind::Notification, message: json!({"method":if started {"turn/started"} else {"turn/completed"},"params":{"threadId":"root","turn":{"id":format!("t{turn}")}}}) }),
            cutex: None,
        }).unwrap()
    }
    fn facts() -> AgentFacts { AgentFacts { project: Some("p".into()), thread_id: Some("root".into()), ..Default::default() } }

    #[test]
    fn a_full_hint_channel_and_restart_recover_all_durable_turns() {
        let (store, root) = crate::artifacts::store::tests::temp_store("capture-replay");
        let repository = EventRepository::open(root.join("journal"), "fixture").unwrap();
        let first = notice(&append(&repository, true, 0)).unwrap().unwrap();
        apply(&store, &first, true, &facts()).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut coalesced = 0;
        for turn in 0..40 {
            let event = notice(&append(&repository, false, turn)).unwrap().unwrap();
            if sender.try_send(event).is_err() { coalesced += 1; }
            if turn < 39 {
                let event = notice(&append(&repository, true, turn + 1)).unwrap().unwrap();
                if sender.try_send(event).is_err() { coalesced += 1; }
            }
        }
        assert!(coalesced > 70);
        assert_eq!(receiver.try_iter().count(), 1);
        let mut scans = Vec::new();
        while page(&repository, &store, true, |_| Ok(facts()), |project, _| scans.push(project.to_string())).unwrap() {}
        let windows = store.project_windows("p", 0).unwrap();
        assert_eq!(windows.len(), 40);
        assert!(windows.iter().all(|window| window.ended_at_ms.is_some()));
        assert_eq!(scans.len(), 40);
        let saved = store.meta(REPLAY_CURSOR_KEY).unwrap();
        let restored = ArtifactStore::at(&root);
        assert!(!page(&repository, &restored, true, |_| Ok(facts()), |_, _| panic!("already processed turn")).unwrap());
        assert_eq!(restored.meta(REPLAY_CURSOR_KEY).unwrap(), saved);
        append(&repository, true, 40);
        append(&repository, false, 40);
        // No in-memory hint survives: daemon startup still replays its cursor.
        assert!(!page(&repository, &restored, true, |_| Ok(facts()), |_, _| {}).unwrap());
        assert_eq!(restored.project_windows("p", 0).unwrap().len(), 41);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_stream_gaps_are_marked_before_restarting_at_retained_events() {
        let (store, root) = crate::artifacts::store::tests::temp_store("capture-gap");
        let repository = EventRepository::open(root.join("journal"), "fixture").unwrap();
        append(&repository, true, 0);
        append(&repository, false, 0);
        store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&Checkpoint { stream_id: Some("old-stream".into()), cursor: None }).unwrap())).unwrap();
        assert!(page(&repository, &store, true, |_| Ok(facts()), |_, _| {}).is_err());
        assert!(store.meta(GAP_KEY).unwrap().unwrap().contains("old-stream"));
        assert!(!page(&repository, &store, true, |_| Ok(facts()), |_, _| {}).unwrap());
        assert_eq!(store.project_windows("p", 0).unwrap().len(), 1);
        assert!(store.meta(GAP_KEY).unwrap().is_some(), "catch-up must not erase the degraded marker");
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_capture_advances_without_backfilling_windows_on_enable() {
        let (store, root) = crate::artifacts::store::tests::temp_store("capture-disabled");
        let repository = EventRepository::open(root.join("journal"), "fixture").unwrap();
        let first = notice(&append(&repository, true, 0)).unwrap().unwrap();
        apply(&store, &first, false, &facts()).unwrap();
        append(&repository, false, 0);
        page(&repository, &store, false, |_| Ok(facts()), |_, _| panic!("disabled scan")).unwrap();
        page(&repository, &store, true, |_| Ok(facts()), |_, _| panic!("historical disabled turn")).unwrap();
        assert!(store.project_windows("p", 0).unwrap().is_empty());
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_boundary_covers_writers_even_without_an_ingress_hint() {
        let (store, root) = crate::artifacts::store::tests::temp_store("capture-start-boundary");
        let repository = EventRepository::open(root.join("journal"), "fixture").unwrap();
        append(&repository, true, 0); // Before enabling capture: no historical backfill.
        append(&repository, false, 0);
        let floor = repository.checkpoint().unwrap();
        store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&Checkpoint { stream_id: Some(floor.stream_id), cursor: floor.cursor }).unwrap())).unwrap();
        append(&repository, true, 1);
        append(&repository, false, 1); // Simulate another process: no callback/hint.
        page(&repository, &store, true, |_| Ok(facts()), |_, _| {}).unwrap();
        let windows = store.project_windows("p", 0).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].turn_id.as_deref(), Some("t1"));
        assert!(windows[0].ended_at_ms.is_some());
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upstream_runtime_observation_loss_is_a_persistent_capture_gap() {
        let (store, root) = crate::artifacts::store::tests::temp_store("capture-upstream-loss");
        let repository = EventRepository::open(root.join("journal"), "fixture").unwrap();
        store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&Checkpoint::default()).unwrap())).unwrap();
        repository.append(PendingEvent {
            cutex_session_id: "cutex.capture".into(), host_id: "fixture".into(), source: EventSource::Cutex,
            schema: None, correlation: EventCorrelation::default(), native: None,
            cutex: Some(CutexMessage { method: "cutex/stream/reset".into(), params: json!({
                "previousStreamId":null, "previousLastSequence":0,
                "reason":"persistence_failure", "resyncRequired":true, "lostEventCount":4,
                "detectedAt":Utc::now().to_rfc3339(),
            }) }),
        }).unwrap();
        append(&repository, true, 1);
        append(&repository, false, 1);
        page(&repository, &store, true, |_| Ok(facts()), |_, _| {}).unwrap();
        let marker = store.meta(GAP_KEY).unwrap().unwrap();
        assert!(marker.contains("persistence_failure"));
        assert!(marker.contains("lostEventCount"));
        assert_eq!(store.project_windows("p", 0).unwrap().len(), 1);
        assert!(!page(&repository, &store, true, |_| Ok(facts()), |_, _| {}).unwrap());
        assert_eq!(store.meta(GAP_KEY).unwrap().as_deref(), Some(marker.as_str()));
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn poison_event_is_marked_after_three_attempts_and_later_turns_continue() {
        let (store, root) = crate::artifacts::store::tests::temp_store("capture-poison");
        let repository = EventRepository::open(root.join("journal"), "fixture").unwrap();
        store.set_meta(REPLAY_CURSOR_KEY, Some(&serde_json::to_string(&Checkpoint::default()).unwrap())).unwrap();
        append(&repository, true, 0);
        append(&repository, true, 1);
        append(&repository, false, 1);
        let decode = |event: &EventEnvelope| {
            if event.native.as_ref().unwrap().message.pointer("/params/turn/id").and_then(|id| id.as_str()) == Some("t0") {
                return Err(DateTime::parse_from_rfc3339("invalid timestamp").unwrap_err());
            }
            notice(event)
        };
        for _ in 0..2 {
            assert!(page_decoded(&repository, &store, true, |_| Ok(facts()), |_, _| {}, decode).is_err());
        }
        // The attempt count survives opening the store again.
        let restored = ArtifactStore::at(&root);
        page_decoded(&repository, &restored, true, |_| Ok(facts()), |_, _| {}, decode).unwrap();
        assert!(restored.meta(GAP_KEY).unwrap().unwrap().contains("failed 3 times"));
        assert_eq!(restored.project_windows("p", 0).unwrap().len(), 1);
        assert!(!page(&repository, &restored, true, |_| panic!("already consumed"), |_, _| {}).unwrap());
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_attribution_metadata_keeps_the_event_replayable() {
        let (store, root) = crate::artifacts::store::tests::temp_store("capture-metadata-retry");
        let repository = EventRepository::open(root.join("journal"), "fixture").unwrap();
        let initial = serde_json::to_string(&Checkpoint::default()).unwrap();
        store.set_meta(REPLAY_CURSOR_KEY, Some(&initial)).unwrap();
        append(&repository, true, 0);
        append(&repository, false, 0);
        for attempt in 0..10 {
            assert!(page(&repository, &store, true, |_| {
                if attempt % 2 == 0 { anyhow::bail!("directory temporarily unavailable"); }
                Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None).into())
            }, |_, _| {}).is_err());
            assert_eq!(store.meta(REPLAY_CURSOR_KEY).unwrap().as_deref(), Some(initial.as_str()));
            assert!(store.meta(FAILURE_KEY).unwrap().is_none());
            assert!(store.meta(GAP_KEY).unwrap().is_none());
        }
        assert_eq!(store.meta(REPLAY_CURSOR_KEY).unwrap().as_deref(), Some(initial.as_str()));
        assert!(store.project_windows("p", 0).unwrap().is_empty());
        page(&repository, &store, true, |_| Ok(facts()), |_, _| {}).unwrap();
        assert_eq!(store.project_windows("p", 0).unwrap().len(), 1);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn outage_backoff_grows_to_minutes_and_a_hint_does_not_reset_it() {
        let mut backoff = RetryBackoff::default();
        let mut now = std::time::Instant::now();
        for seconds in [5, 10, 20, 40, 80, 160, 300, 300] {
            backoff.failed(now);
            assert_eq!(backoff.remaining(now), Some(Duration::from_secs(seconds)));
            assert_eq!(backoff.remaining(now + Duration::from_secs(1)), Some(Duration::from_secs(seconds - 1)));
            now += Duration::from_secs(seconds);
            assert!(backoff.remaining(now).is_none());
        }
    }
}
