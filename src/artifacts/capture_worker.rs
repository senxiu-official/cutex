//! One workspace scanner, separate from the ordered turn-window recorder.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{scan_project, ArtifactHost, ArtifactStore, Budget, ManagementHost, Utc};

const MAX_PENDING: usize = 128;
const RECOVERY_PAGE: usize = 32;
const NOTICE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
enum Task {
    Project(String),
    Recover { since_ms: i64 },
}
#[derive(Default)]
struct State {
    pending: VecDeque<(String, i64)>,
    recovery_since: Option<i64>,
    closed: bool,
}
struct Queue {
    state: Mutex<State>,
    changed: Condvar,
    capacity: usize,
}
impl Queue {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self { state: Mutex::new(State::default()), changed: Condvar::new(), capacity }
    }
    fn submit(&self, project: String, at_ms: i64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed { return; }
        if let Some(since) = &mut state.recovery_since { *since = (*since).min(at_ms); }
        if let Some((_, since)) = state.pending.iter_mut().find(|(pending, _)| *pending == project) {
            *since = (*since).min(at_ms);
            return;
        }
        if state.pending.len() < self.capacity {
            state.pending.push_back((project, at_ms));
        } else {
            // Every request's window was persisted before submit. A single lower
            // bound replaces overflow identifiers; recovery streams project IDs
            // from those windows. Include pending requests before clearing them.
            let since = state.pending.iter().map(|(_, at)| *at).fold(at_ms, i64::min);
            state.recovery_since = Some(state.recovery_since.map_or(since, |old| old.min(since)));
        }
        self.changed.notify_one();
    }
    fn take(&self) -> Option<Task> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(since_ms) = state.recovery_since.take() {
                state.pending.clear();
                return Some(Task::Recover { since_ms });
            }
            if let Some((project, _)) = state.pending.pop_front() { return Some(Task::Project(project)); }
            if state.closed { return None; }
            state = self.changed.wait(state).unwrap_or_else(|error| error.into_inner());
        }
    }
    fn retry_recovery(&self, since_ms: i64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed { return; }
        state.recovery_since = Some(state.recovery_since.map_or(since_ms, |old| old.min(since_ms)));
        self.changed.notify_one();
    }
    fn close(&self) {
        self.state.lock().unwrap_or_else(|error| error.into_inner()).closed = true;
        self.changed.notify_one();
    }
}

pub(super) struct Worker { queue: Arc<Queue> }
impl Worker {
    pub(super) fn start() -> std::io::Result<Self> {
        let queue = Arc::new(Queue::new(MAX_PENDING));
        let receiver = Arc::clone(&queue);
        std::thread::Builder::new().name("cutex-artifact-scan".into()).spawn(move || run(&receiver))?;
        Ok(Self { queue })
    }
    pub(super) fn schedule(&self, project: String, at_ms: i64) { self.queue.submit(project, at_ms); }
}
impl Drop for Worker { fn drop(&mut self) { self.queue.close(); } }

struct Notice { at: Instant, suppressed: usize }
#[derive(Default)]
struct BudgetNotices { projects: HashMap<String, Notice> }
impl BudgetNotices {
    fn reached(&mut self, project: &str, now: Instant) -> Option<usize> {
        if let Some(notice) = self.projects.get_mut(project) {
            if now.saturating_duration_since(notice.at) < NOTICE_INTERVAL {
                notice.suppressed = notice.suppressed.saturating_add(1);
                return None;
            }
            let suppressed = notice.suppressed;
            *notice = Notice { at: now, suppressed: 0 };
            return Some(suppressed);
        }
        if self.projects.len() >= MAX_PENDING {
            if let Some(oldest) = self.projects.iter().min_by_key(|(_, notice)| notice.at).map(|(id, _)| id.clone()) {
                self.projects.remove(&oldest);
            }
        }
        self.projects.insert(project.into(), Notice { at: now, suppressed: 0 });
        Some(0)
    }
}

fn scan(host: &dyn ArtifactHost, store: &ArtifactStore, project: &str, notices: &mut BudgetNotices) {
    match scan_project(host, store, project, Utc::now(), Budget::default()) {
        Ok(report) if report.budget_hit => {
            if let Some(suppressed) = notices.reached(project, Instant::now()) {
                eprintln!("artifacts: auto capture of project {project} stopped at the scan budget ({} entries / {} s per phase) after {} entries; the rest waits for the next turn end ({suppressed} similar budget notices suppressed)",
                    Budget::default().max_entries, Budget::default().max_time.as_secs(), report.visited);
            }
        }
        Ok(_) => {}
        Err(error) => eprintln!("artifacts: auto capture of project {project}: {error:#}"),
    }
}
fn recover(host: &dyn ArtifactHost, store: &ArtifactStore, since_ms: i64, notices: &mut BudgetNotices) -> anyhow::Result<()> {
    let mut after = None;
    loop {
        let projects = store.capture_projects_since(since_ms, after.as_deref(), RECOVERY_PAGE)?;
        if projects.is_empty() { return Ok(()); }
        for project in &projects { scan(host, store, project, notices); }
        after = projects.into_iter().last();
    }
}
fn run(queue: &Queue) {
    let host = ManagementHost;
    let mut notices = BudgetNotices::default();
    while let Some(task) = queue.take() {
        let store = match host.store() {
            Ok(store) => store,
            Err(error) => {
                eprintln!("artifacts: auto capture scan store unavailable: {error:#}");
                match task {
                    Task::Recover { since_ms } => queue.retry_recovery(since_ms),
                    Task::Project(project) => queue.submit(project, i64::MIN),
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        match task {
            Task::Project(project) => scan(&host, &store, &project, &mut notices),
            Task::Recover { since_ms } => if let Err(error) = recover(&host, &store, since_ms, &mut notices) {
                eprintln!("artifacts: auto capture recovery: {error:#}");
                queue.retry_recovery(since_ms);
                std::thread::sleep(Duration::from_secs(1));
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{record, AgentFacts, TurnEvent};
    use crate::artifacts::host::tests::TestHost;
    use std::sync::mpsc;

    #[test]
    fn queue_coalesces_projects_and_overflow_keeps_a_durable_recovery_bound() {
        let queue = Queue::new(2);
        queue.submit("a".into(), 100);
        queue.submit("a".into(), 90);
        queue.submit("b".into(), 200);
        queue.submit("c".into(), 300);
        queue.submit("d".into(), 80); // Clock adjustment or older boundary: preserve the minimum.
        assert_eq!(queue.state.lock().unwrap().pending.len(), 2);
        assert_eq!(queue.take(), Some(Task::Recover { since_ms: 80 }));
        queue.submit("late".into(), 400);
        assert_eq!(queue.take(), Some(Task::Project("late".into())));
        queue.close();
        assert_eq!(queue.take(), None);
    }

    #[test]
    fn stalled_scanner_does_not_hold_turn_window_recording_or_grow_pending_projects() {
        let host = TestHost::new("capture-worker-stall");
        let store = host.store().unwrap();
        let queue = Arc::new(Queue::new(2));
        let worker = Worker { queue: Arc::clone(&queue) };
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let scanner = std::thread::spawn(move || {
            assert_eq!(queue.take(), Some(Task::Project("blocked".into())));
            started_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            queue.take()
        });
        worker.schedule("blocked".into(), 1);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for index in 0..8 {
            let project = format!("p{index}");
            let facts = AgentFacts { project: Some(project.clone()), ..Default::default() };
            let mut event = TurnEvent { session: format!("s{index}"), runtime_generation: 1, started: true,
                turn_id: Some("turn".into()), thread_id: None, at_ms: 100 };
            record(&store, &event, &facts).unwrap();
            event.started = false; event.at_ms = 200;
            assert_eq!(record(&store, &event, &facts).unwrap(), Some(project.clone()));
            worker.schedule(project.clone(), event.at_ms);
            assert_eq!(store.project_windows(&project, 0).unwrap()[0].ended_at_ms, Some(200));
        }
        assert_eq!(worker.queue.state.lock().unwrap().pending.len(), 2);
        resume_tx.send(()).unwrap();
        assert_eq!(scanner.join().unwrap(), Some(Task::Recover { since_ms: 200 }));
        // Every overflow project remains recoverable from persisted windows,
        // independent of the currently active session or scan thread.
        let first = store.capture_projects_since(200, None, 3).unwrap();
        assert_eq!(first, ["p0", "p1", "p2"]);
        let second = store.capture_projects_since(200, first.last().map(String::as_str), 3).unwrap();
        assert_eq!(second, ["p3", "p4", "p5"]);
        let last = store.capture_projects_since(200, second.last().map(String::as_str), 3).unwrap();
        assert_eq!(last, ["p6", "p7"]);
        assert!(store.capture_projects_since(201, None, 3).unwrap().is_empty());
    }

    #[test]
    fn recovery_pages_completed_windows_only_and_deduplicates_projects() {
        let host = TestHost::new("capture-worker-pages");
        let store = host.store().unwrap();
        for (session, project, end) in [("s1", "a", Some(100)), ("s2", "a", Some(110)), ("s3", "b", Some(120)), ("s4", "open", None)] {
            let facts = AgentFacts { project: Some(project.into()), ..Default::default() };
            let mut event = TurnEvent { session: session.into(), runtime_generation: 1, started: true, turn_id: Some("t".into()), thread_id: None, at_ms: 1 };
            record(&store, &event, &facts).unwrap();
            if let Some(end) = end { event.started = false; event.at_ms = end; record(&store, &event, &facts).unwrap(); }
        }
        assert_eq!(store.capture_projects_since(100, None, 1).unwrap(), ["a"]);
        assert_eq!(store.capture_projects_since(100, Some("a"), 1).unwrap(), ["b"]);
        assert!(store.capture_projects_since(100, Some("b"), 1).unwrap().is_empty());
    }

    #[test]
    fn orphan_completion_is_recoverable_without_inventing_an_attribution_window() {
        let host = TestHost::new("capture-worker-orphan");
        let store = host.store().unwrap();
        let event = TurnEvent { session: "orphan".into(), runtime_generation: 1, started: false,
            turn_id: Some("already-running".into()), thread_id: None, at_ms: 100 };
        let facts = AgentFacts { project: Some("p".into()), ..Default::default() };
        assert_eq!(record(&store, &event, &facts).unwrap(), Some("p".into()));
        assert!(store.project_windows("p", 0).unwrap().is_empty());
        assert_eq!(store.capture_projects_since(100, None, 32).unwrap(), ["p"]);
        assert!(store.capture_projects_since(101, None, 32).unwrap().is_empty());
    }

    #[test]
    fn recovery_retry_preserves_older_requests_when_new_overflow_arrives() {
        let queue = Queue::new(1);
        queue.submit("first".into(), 100);
        queue.submit("overflow".into(), 200);
        assert_eq!(queue.take(), Some(Task::Recover { since_ms: 100 }));
        queue.submit("new".into(), 300);
        queue.submit("another".into(), 400);
        queue.retry_recovery(100);
        assert_eq!(queue.take(), Some(Task::Recover { since_ms: 100 }));
        queue.close();
        queue.retry_recovery(100);
        assert_eq!(queue.take(), None);
    }

    #[test]
    fn budget_notices_are_per_project_aggregated_and_bounded() {
        let mut notices = BudgetNotices::default();
        let start = Instant::now();
        assert_eq!(notices.reached("p", start), Some(0));
        for _ in 0..75 { assert_eq!(notices.reached("p", start + Duration::from_secs(1)), None); }
        assert_eq!(notices.reached("other", start), Some(0));
        assert_eq!(notices.reached("p", start + NOTICE_INTERVAL), Some(75));
        for index in 0..(MAX_PENDING + 10) {
            notices.reached(&format!("project-{index}"), start + Duration::from_secs(index as u64));
            assert!(notices.projects.len() <= MAX_PENDING);
        }
    }
}
