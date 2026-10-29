//! Reserved control capacity on the existing listener. Heavy reads have a
//! bounded FIFO wait of their own; control requests never join that queue.
use crate::http::server::SimpleHttpRequest;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const CONTROL_CONNECTION_RESERVE: usize = 32;
pub const HEAVY_READ_LIMIT: usize = 2;
pub const HEAVY_READ_QUEUE_LIMIT: usize = 64;
pub const HEAVY_READ_WAIT: Duration = Duration::from_secs(3);
pub const STREAM_LIMIT: usize = 128;
pub const FILE_TRANSFER_LIMIT: usize = 2;
static TRANSFERS: AtomicUsize = AtomicUsize::new(0);
static HEAVY: HeavyPool = HeavyPool::new(HEAVY_READ_LIMIT, HEAVY_READ_QUEUE_LIMIT);
static STREAMS: AtomicUsize = AtomicUsize::new(0);
static REFUSED: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkClass {
    Control,
    HeavyRead,
    Stream,
    FileTransfer,
}

pub fn classify(request: &SimpleHttpRequest) -> WorkClass {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if request.method == "GET" && super::file_transfer::is_path(path) {
        return WorkClass::FileTransfer;
    }
    // Use the workspace parser for both session and project SSE watches.
    let fs_watch = request.method == "GET" && path.ends_with("/fs/watch")
        && matches!(super::workspace::parse_route(path),
        Some(super::workspace::WorkspaceRoute::Scoped(_, super::workspace::ScopedRoute::FsWatch)));
    // Side-chat detail GET can wait up to 25 seconds (query or JSON body).
    // Classify the exact route, not query text: collection reads remain heavy,
    // and a caller cannot bypass admission by moving waitMs into the body.
    let side_wait = matches!(super::side_chats::parse_path(path), Some((_, Some(_), None)));
    // Browser submit waits for a desktop receipt (including duplicate submits).
    // Result, cancel and grant routes must retain control capacity so waits can
    // complete even when this bounded waiting pool is full.
    if request.method == "POST" && path == "/v2/browser/commands" {
        return WorkClass::Stream;
    }
    if request.method == "GET" && (path.ends_with("/stream") || path == "/v2/summary/events" || fs_watch || side_wait) {
        return WorkClass::Stream;
    }
    if request.method == "GET"
        && (path == "/v2/sessions"
            || path.starts_with("/v2/sessions/")
            || path == "/v2/summary"
            || path == "/v2/events"
            || path.starts_with("/v2/artifacts")
            || path.starts_with("/v2/documents")
            || path == "/v2/storage"
            || path.starts_with("/v2/storage/"))
    {
        return WorkClass::HeavyRead;
    }
    if request.method == "POST"
        && matches!(path, "/v2/storage/refresh" | "/v2/storage/cleanups/preview")
    {
        return WorkClass::HeavyRead;
    }
    if request.method == "POST" && path.ends_with("/requests") && request.body.len() <= 64 * 1024 {
        if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) {
            if matches!(
                body.get("method").and_then(|v| v.as_str()),
                Some("cutex/session/get" | "cutex/focus/get")
            ) {
                return WorkClass::HeavyRead;
            }
            if matches!(
                body.pointer("/native/message/method")
                    .and_then(|v| v.as_str()),
                Some("thread/read" | "thread/list" | "thread/items/list" | "thread/loaded/list")
            ) {
                return WorkClass::HeavyRead;
            }
        }
    }
    WorkClass::Control
}

/// The cause belongs to this admission attempt, never inferred from counters
/// which other requests can change concurrently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    HeavyReadQueueFull,
    HeavyReadWaitTimeout,
    ControlConnectionReserve,
    StreamLimit,
    FileTransferLimit,
}
impl RefusalReason {
    fn name(self) -> &'static str {
        match self {
            Self::HeavyReadQueueFull => "heavy_read_queue_full",
            Self::HeavyReadWaitTimeout => "heavy_read_wait_timeout",
            Self::ControlConnectionReserve => "control_connection_reserve",
            Self::StreamLimit => "stream_limit",
            Self::FileTransferLimit => "file_transfer_limit",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::HeavyReadQueueFull => "heavy-read wait queue is full; retry shortly",
            Self::HeavyReadWaitTimeout => "heavy-read admission wait budget expired; retry shortly",
            Self::ControlConnectionReserve => "bulk connection limit reached; remaining connections are reserved for control; retry shortly",
            Self::StreamLimit => "stream capacity is exhausted; retry shortly",
            Self::FileTransferLimit => "file transfer capacity is exhausted; retry shortly",
        }
    }
}

#[derive(Debug)]
pub struct AdmissionRefusal {
    pub reason: RefusalReason,
    heavy: (usize, usize),
    connections_open: usize,
    bulk_connections: usize,
    streams_active: usize,
}
impl AdmissionRefusal {
    fn new(reason: RefusalReason, heavy: (usize, usize)) -> Self {
        // Heavy counts are captured under the decision's lock for queue
        // refusals. Other pools are sampled, not one atomic global snapshot.
        Self {
            reason, heavy,
            connections_open: crate::management::listeners::open_connections(),
            bulk_connections: crate::management::listeners::bulk_connections(),
            streams_active: STREAMS.load(Ordering::Relaxed),
        }
    }
    pub fn details(&self) -> serde_json::Value {
        // Allocate JSON only after releasing the admission lock.
        let capacity = serde_json::json!({
            "approximate": true,
            "heavyReadsActive": self.heavy.0, "heavyReadsWaiting": self.heavy.1,
            "heavyReadLimit": HEAVY_READ_LIMIT, "heavyReadQueueLimit": HEAVY_READ_QUEUE_LIMIT,
            "heavyReadWaitMs": HEAVY_READ_WAIT.as_millis(),
            "connectionsOpen": self.connections_open,
            "bulkConnections": self.bulk_connections,
            "connectionLimit": crate::management::listeners::MAX_CONNECTIONS,
            "bulkConnectionLimit": crate::management::listeners::MAX_CONNECTIONS - CONTROL_CONNECTION_RESERVE,
            "controlConnectionReserve": CONTROL_CONNECTION_RESERVE,
            "streamsActive": self.streams_active, "streamLimit": STREAM_LIMIT,
            "fileTransfersActive": TRANSFERS.load(Ordering::Relaxed), "fileTransferLimit": FILE_TRANSFER_LIMIT,
        });
        serde_json::json!({"reason": self.reason.name(), "retryAfterMs": 250, "capacity": capacity})
    }
}

pub struct Permit {
    counter: Option<&'static AtomicUsize>,
    heavy: Option<&'static HeavyPool>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(pool) = self.heavy {
            let mut state = pool
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active -= 1;
            drop(state);
            pool.ready.notify_all();
        }
        if let Some(counter) = self.counter {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct HeavyState {
    active: usize,
    tickets: VecDeque<u64>,
    next_ticket: u64,
}
struct HeavyPool {
    limit: usize,
    queue_limit: usize,
    state: Mutex<HeavyState>,
    ready: Condvar,
    queued: AtomicU64,
    wait_ns: AtomicU64,
    timed_out: AtomicU64,
    queue_full: AtomicU64,
}
impl HeavyPool {
    const fn new(limit: usize, queue_limit: usize) -> Self {
        Self {
            limit,
            queue_limit,
            state: Mutex::new(HeavyState {
                active: 0,
                tickets: VecDeque::new(),
                next_ticket: 0,
            }),
            ready: Condvar::new(),
            queued: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            queue_full: AtomicU64::new(0),
        }
    }

    fn counts(&self) -> (usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.active, state.tickets.len())
    }

    fn acquire(&'static self, budget: Duration) -> Result<Permit, AdmissionRefusal> {
        let started = Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active < self.limit && state.tickets.is_empty() {
            state.active += 1;
            return Ok(Permit {
                counter: None,
                heavy: Some(self),
            });
        }
        if state.tickets.len() >= self.queue_limit {
            self.queue_full.fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionRefusal::new(RefusalReason::HeavyReadQueueFull,
                (state.active, state.tickets.len())));
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.tickets.push_back(ticket);
        self.queued.fetch_add(1, Ordering::Relaxed);
        loop {
            let elapsed = started.elapsed();
            if elapsed >= budget {
                // Remove timed-out heads as well as followers, so a timeout
                // never leaves a hole that blocks the next eligible reader.
                let position = state
                    .tickets
                    .iter()
                    .position(|value| *value == ticket)
                    .expect("queued heavy read keeps its ticket");
                state.tickets.remove(position);
                self.timed_out.fetch_add(1, Ordering::Relaxed);
                self.wait_ns.fetch_add(
                    elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                let refusal = AdmissionRefusal::new(RefusalReason::HeavyReadWaitTimeout,
                    (state.active, state.tickets.len()));
                drop(state);
                self.ready.notify_all();
                return Err(refusal);
            }
            if state.active < self.limit && state.tickets.front() == Some(&ticket) {
                state.tickets.pop_front();
                state.active += 1;
                self.wait_ns.fetch_add(
                    elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                return Ok(Permit {
                    counter: None,
                    heavy: Some(self),
                });
            }
            let (next, _) = self
                .ready
                .wait_timeout(state, budget.saturating_sub(elapsed))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }
}
fn take(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < limit).then_some(n + 1)
        })
        .is_ok()
}

/// Old clients need no retry support for ordinary bursts: heavy reads wait
/// here on their own bounded lane. The listener reserves control connections
/// before admission, so these waiting threads cannot consume that reserve.
pub fn admit(class: WorkClass) -> Result<Permit, AdmissionRefusal> {
    if class == WorkClass::HeavyRead {
        let permit = HEAVY.acquire(HEAVY_READ_WAIT);
        if permit.is_err() {
            REFUSED.fetch_add(1, Ordering::Relaxed);
        }
        return permit;
    }
    let pool = match class {
        WorkClass::Control => None,
        WorkClass::HeavyRead => unreachable!("heavy reads use their own waiting lane"),
        WorkClass::Stream => Some((&STREAMS, STREAM_LIMIT)),
        WorkClass::FileTransfer => Some((&TRANSFERS, FILE_TRANSFER_LIMIT)),
    };
    if let Some((counter, limit)) = pool {
        if !take(counter, limit) {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            let reason = if class == WorkClass::FileTransfer {
                RefusalReason::FileTransferLimit
            } else {
                RefusalReason::StreamLimit
            };
            return Err(AdmissionRefusal::new(reason, HEAVY.counts()));
        }
        Ok(Permit {
            counter: Some(counter),
            heavy: None,
        })
    } else {
        Ok(Permit {
            counter: None,
            heavy: None,
        })
    }
}

/// Shared listener admission, also used by isolated HTTP compatibility tests.
/// The connection is charged to bulk before waiting, preserving the control
/// reserve even when every queued reader has its own blocked HTTP thread.
pub fn admit_request(
    request: &SimpleHttpRequest,
    slot: Option<&mut crate::management::listeners::ConnectionSlot>,
) -> Result<Permit, AdmissionRefusal> {
    let class = classify(request);
    admit_with_bulk(class, || slot.is_none_or(|slot| slot.restrict_to_bulk()))
}

fn admit_with_bulk(class: WorkClass, restrict: impl FnOnce() -> bool) -> Result<Permit, AdmissionRefusal> {
    if class != WorkClass::Control && !restrict() {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(AdmissionRefusal::new(RefusalReason::ControlConnectionReserve, HEAVY.counts()));
    }
    admit(class)
}

pub fn snapshot() -> serde_json::Value {
    let (active, waiting) = HEAVY.counts();
    serde_json::json!({ "controlConnectionReserve": CONTROL_CONNECTION_RESERVE, "heavyReadLimit": HEAVY_READ_LIMIT,
        "connectionsOpen": crate::management::listeners::open_connections(),
        "bulkConnections": crate::management::listeners::bulk_connections(),
        "connectionLimit": crate::management::listeners::MAX_CONNECTIONS,
        "heavyReadsActive": active, "heavyReadsWaiting": waiting,
        "heavyReadQueueLimit": HEAVY_READ_QUEUE_LIMIT, "heavyReadWaitMs": HEAVY_READ_WAIT.as_millis(),
        "heavyReadsQueuedTotal": HEAVY.queued.load(Ordering::Relaxed),
        "heavyReadWaitNsTotal": HEAVY.wait_ns.load(Ordering::Relaxed),
        "heavyReadWaitTimeouts": HEAVY.timed_out.load(Ordering::Relaxed),
        "heavyReadQueueFull": HEAVY.queue_full.load(Ordering::Relaxed), "streamLimit": STREAM_LIMIT,
        "streamsActive": STREAMS.load(Ordering::Relaxed), "fileTransfersActive": TRANSFERS.load(Ordering::Relaxed),
        "fileTransferLimit": FILE_TRANSFER_LIMIT, "refused": REFUSED.load(Ordering::Relaxed) })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_transfers_have_dedicated_capacity_and_preserve_control_reads_and_watches() {
        for path in [
            "/v2/fs/transfer",
            "/v2/fs/transfer/chunk?offset=0",
            "/v2/projects/p/fs/transfer",
            "/v2/sessions/s/fs/transfer/chunk",
        ] {
            assert_eq!(classify(&request("GET", path, "")), WorkClass::FileTransfer);
            assert_ne!(classify(&request("POST", path, "")), WorkClass::FileTransfer);
        }
        let transfers: Vec<_> = (0..FILE_TRANSFER_LIMIT)
            .map(|_| admit(WorkClass::FileTransfer).unwrap())
            .collect();
        assert_eq!(
            admit(WorkClass::FileTransfer).err().unwrap().reason,
            RefusalReason::FileTransferLimit
        );
        let control = admit(WorkClass::Control).unwrap();
        let read = admit(WorkClass::HeavyRead).unwrap();
        let watch = admit(WorkClass::Stream).unwrap();
        drop(transfers);
        let again = admit(WorkClass::FileTransfer).unwrap();
        drop((again, control, read, watch));
    }
    fn request(method: &str, path: &str, body: &str) -> SimpleHttpRequest {
        SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        }
    }
    #[test]
    fn costly_routes_cannot_take_control_capacity() {
        for path in [
            "/v2/sessions",
            "/v2/sessions/cutex.a/bootstrap",
            "/v2/sessions/cutex.a/history?limit=100",
            "/v2/summary",
            "/v2/storage",
            "/v2/events",
        ] {
            assert_eq!(
                classify(&request("GET", path, "")),
                WorkClass::HeavyRead,
                "{path}"
            );
        }
        assert_eq!(
            classify(&request("GET", "/v2/events/stream", "")),
            WorkClass::Stream
        );
        assert_eq!(
            classify(&request("GET", "/v2/summary/events", "")),
            WorkClass::Stream
        );
        assert_eq!(
            classify(&request(
                "POST",
                "/v2/sessions/cutex.a/app-server/requests",
                r#"{"requestId":"r","native":{"message":{"id":1,"method":"thread/read","params":{}}}}"#
            )),
            WorkClass::HeavyRead
        );
        assert_eq!(
            classify(&request("POST", "/v2/storage/cleanups/preview", "{}")),
            WorkClass::HeavyRead
        );
        assert_eq!(
            classify(&request(
                "POST",
                "/v2/sessions/cutex.a/cutex/requests",
                r#"{"method":"cutex/session/get","params":{}}"#
            )),
            WorkClass::HeavyRead
        );
        for method in [
            "cutex/userInput/submit",
            "cutex/turn/stop",
            "cutex/asks/send",
        ] {
            assert_eq!(
                classify(&request(
                    "POST",
                    "/v2/sessions/cutex.a/cutex/requests",
                    &format!(r#"{{"method":"{method}"}}"#)
                )),
                WorkClass::Control
            );
        }
    }
    #[test]
    fn every_management_stream_uses_only_stream_capacity() {
        // Production SSE inventory: event replay/live, summary, project events,
        // workspace watches (both scopes), terminal output, browser commands, logs.
        for path in [
            "/v2/events/stream", "/v2/events/stream?cutexSessionId=cutex.a&liveDeltas=1",
            "/v2/summary/events", "/v2/projects/p/events/stream",
            "/v2/sessions/cutex.a/fs/watch", "/v2/projects/p/fs/watch",
            "/v2/sessions/cutex%2Ea/fs/watch?x=1", "/v2/projects/p%20a/fs/watch?x=1",
            "/v2/terminals/t-1/stream?since=4",
            "/v2/browser/devices/device/commands/stream", "/v2/services/web/logs/stream?tail=20",
        ] {
            assert_eq!(classify(&request("GET", path, "")), WorkClass::Stream, "{path}");
            assert_ne!(classify(&request("POST", path, "")), WorkClass::Stream, "{path}");
        }
        for path in ["/v2/sessions/a/fs/watch/extra", "/v2/sessions/a/fs/file?path=watch", "/v2/sessions/a/fs/dir"] {
            assert_eq!(classify(&request("GET", path, "")), WorkClass::HeavyRead, "{path}");
        }
        assert_eq!(classify(&request("GET", "/v2/projects/p/fs/file", "")), WorkClass::Control);
    }

    #[test]
    fn side_chat_wait_route_cannot_occupy_heavy_reads() {
        let path = "/v2/sessions/a/side-chats/b";
        for query in ["", "?afterRevision=1&waitMs=25000", "?waitMs=0", "?waitMs=invalid"] {
            assert_eq!(classify(&request("GET", &format!("{path}{query}"), "")), WorkClass::Stream);
        }
        assert_eq!(classify(&request("GET", path, r#"{"afterRevision":1,"waitMs":25000}"#)), WorkClass::Stream);
        for path in [
            "/v2/sessions/a/side-chats?waitMs=25000",
            "/v2/sessions/a/side-chats/b/messages?waitMs=25000",
            "/v2/sessions/a/history?waitMs=25000",
            "/v2/sessions/a/side-chats/b/extra/more",
        ] {
            assert_eq!(classify(&request("GET", path, "")), WorkClass::HeavyRead, "{path}");
        }
        for method in ["POST", "DELETE"] {
            assert_eq!(classify(&request(method, path, "")), WorkClass::Control);
        }
    }

    #[test]
    fn browser_waits_are_bounded_but_completion_routes_keep_control() {
        assert_eq!(classify(&request("POST", "/v2/browser/commands", "{}")), WorkClass::Stream);
        for (method, path) in [
            ("GET", "/v2/browser/commands/c"),
            ("POST", "/v2/browser/commands/c/cancel"),
            ("POST", "/v2/browser/devices/d/commands/c/result"),
            ("POST", "/v2/browser/leases/grant"),
            ("GET", "/v2/browser/commands"),
        ] {
            assert_eq!(classify(&request(method, path, "{}")), WorkClass::Control, "{method} {path}");
        }
    }

    #[test]
    fn bounded_pool_is_race_safe() {
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let joins: Vec<_> = (0..16)
            .map(|_| {
                let count = count.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    take(&count, 2)
                })
            })
            .collect();
        assert_eq!(
            joins
                .into_iter()
                .map(|j| usize::from(j.join().unwrap()))
                .sum::<usize>(),
            2
        );
        assert_eq!(count.load(Ordering::Acquire), 2);
    }

    #[test]
    fn saturated_heavy_reads_leave_control_and_stream_capacity_available() {
        let permits: Vec<_> = (0..HEAVY_READ_LIMIT)
            .map(|_| admit(WorkClass::HeavyRead).expect("heavy read slot"))
            .collect();
        let reader = std::thread::spawn(|| {
            admit(WorkClass::HeavyRead).expect("burst reader waits instead of failing")
        });
        wait_for_queue(&HEAVY, 1);
        let control = admit(WorkClass::Control).expect("control must remain admitted");
        let stream = admit(WorkClass::Stream).expect("stream has its own bounded pool");
        drop((permits, control, stream));
        drop(reader.join().unwrap());
        assert!(
            admit(WorkClass::HeavyRead).is_ok(),
            "completed work returns capacity"
        );
    }

    fn wait_for_queue(pool: &HeavyPool, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while pool.counts().1 != count {
            assert!(
                Instant::now() < deadline,
                "waiter must enter the real admission queue"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn queued_readers_are_fifo_and_return_capacity() {
        let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(1, 8)));
        let held = pool.acquire(Duration::ZERO).unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let mut workers = Vec::new();
        for index in 0..8 {
            let send = send.clone();
            workers.push(std::thread::spawn(move || {
                let _permit = pool
                    .acquire(Duration::from_secs(2))
                    .expect("ordinary burst must drain");
                send.send(index).unwrap();
            }));
            wait_for_queue(pool, index + 1);
        }
        drop(held);
        for index in 0..8 {
            assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), index);
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(pool.counts(), (0, 0));
        assert_eq!(pool.timed_out.load(Ordering::Relaxed), 0);
        assert_eq!(pool.queue_full.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn timed_out_head_does_not_block_the_next_reader() {
        let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(1, 2)));
        let held = pool.acquire(Duration::ZERO).unwrap();
        let first = std::thread::spawn(move || {
            let started = Instant::now();
            let refusal = pool.acquire(Duration::from_millis(500)).err().expect("head times out");
            assert_eq!(refusal.reason, RefusalReason::HeavyReadWaitTimeout);
            assert_eq!(refusal.details()["capacity"]["heavyReadsWaiting"], 1);
            assert!(started.elapsed() >= Duration::from_millis(500));
        });
        wait_for_queue(pool, 1);
        let second = std::thread::spawn(move || {
            pool.acquire(Duration::from_secs(2))
                .expect("follower survives head timeout")
        });
        wait_for_queue(pool, 2);
        first.join().unwrap();
        assert_eq!(pool.counts(), (1, 1));
        drop(held);
        drop(second.join().unwrap());
        assert_eq!(pool.counts(), (0, 0));
        assert_eq!(pool.timed_out.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn full_wait_queue_refuses_without_allocating_another_waiter() {
        let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(1, 1)));
        let held = pool.acquire(Duration::ZERO).unwrap();
        let reader = std::thread::spawn(move || pool.acquire(Duration::from_secs(2)).unwrap());
        wait_for_queue(pool, 1);
        let refusal = pool.acquire(Duration::from_secs(1)).err().expect("queue full");
        assert_eq!(refusal.reason, RefusalReason::HeavyReadQueueFull);
        assert_eq!(refusal.details()["capacity"]["heavyReadsActive"], 1);
        assert_eq!(refusal.details()["capacity"]["heavyReadsWaiting"], 1);
        assert_eq!(pool.counts(), (1, 1));
        assert_eq!(pool.queue_full.load(Ordering::Relaxed), 1);
        drop(held);
        drop(reader.join().unwrap());
        assert_eq!(pool.counts(), (0, 0));
    }
    #[test]
    fn simultaneous_full_queue_refusals_keep_their_own_reason_after_a_timeout() {
        let pool: &'static HeavyPool = Box::leak(Box::new(HeavyPool::new(1, 1)));
        let held = pool.acquire(Duration::ZERO).unwrap();
        let waiter = std::thread::spawn(move || pool.acquire(Duration::from_secs(60)));
        wait_for_queue(pool, 1);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let workers: Vec<_> = (0..8).map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                pool.acquire(Duration::ZERO).err().expect("full queue")
            })
        }).collect();
        barrier.wait();
        let refusals: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        drop(held);
        drop(waiter.join().unwrap().expect("release the full queue after workers finish"));
        let held = pool.acquire(Duration::ZERO).unwrap();
        assert_eq!(pool.acquire(Duration::ZERO).err().unwrap().reason, RefusalReason::HeavyReadWaitTimeout);
        drop(held);
        assert_eq!(pool.counts(), (0, 0));
        for refusal in refusals {
            assert_eq!(refusal.reason, RefusalReason::HeavyReadQueueFull);
            assert_eq!(refusal.details()["capacity"]["heavyReadsWaiting"], 1);
        }
        assert_eq!(pool.queue_full.load(Ordering::Relaxed), 8);
        assert_eq!(pool.timed_out.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bulk_boundary_is_reported_without_admitting_work_or_restricting_control() {
        for class in [WorkClass::HeavyRead, WorkClass::Stream, WorkClass::FileTransfer] {
            let refusal = admit_with_bulk(class, || false).err().expect("bulk reserve");
            assert_eq!(refusal.reason, RefusalReason::ControlConnectionReserve);
            assert_eq!(refusal.details()["reason"], "control_connection_reserve");
            assert_eq!(refusal.details()["retryAfterMs"], 250);
            assert_eq!(refusal.details()["capacity"]["controlConnectionReserve"], 32);
        }
        let control = admit_with_bulk(WorkClass::Control, || panic!("control must bypass bulk"));
        assert!(control.is_ok());
    }

    #[test]
    fn real_connection_and_stream_pools_report_and_recover_in_isolated_process() {
        // Exhaust real process-global pools only in a child running this one
        // test, so unrelated parallel HTTP tests retain their capacity.
        const CHILD: &str = "CUTEX_TEST_ADMISSION_REFUSAL_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "management::v2::control_capacity::tests::real_connection_and_stream_pools_report_and_recover_in_isolated_process", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
            return;
        }
        use crate::management::listeners::{ConnectionSlot, MAX_CONNECTIONS};
        let streams: Vec<_> = (0..STREAM_LIMIT).map(|_| admit(WorkClass::Stream).unwrap()).collect();
        let refused = admit_request(&request("GET", "/v2/events/stream", ""), None).err().unwrap();
        assert_eq!(refused.reason, RefusalReason::StreamLimit);
        assert_eq!(refused.details()["capacity"]["streamsActive"], STREAM_LIMIT);
        let refused = admit_request(&request("POST", "/v2/browser/commands", "{}"), None).err().unwrap();
        assert_eq!(refused.reason, RefusalReason::StreamLimit);
        for path in ["/v2/browser/commands/c/cancel", "/v2/browser/devices/d/commands/c/result", "/v2/browser/leases/grant"] {
            drop(admit_request(&request("POST", path, "{}"), None).expect("wait completion retains reserved control capacity"));
        }
        drop(streams);
        drop(admit_request(&request("GET", "/v2/events/stream", ""), None).unwrap());
        let mut bulk: Vec<_> = (0..MAX_CONNECTIONS - CONTROL_CONNECTION_RESERVE).map(|_| {
            let mut slot = ConnectionSlot::acquire().unwrap();
            assert!(slot.restrict_to_bulk());
            slot
        }).collect();
        let mut reserved = ConnectionSlot::acquire().expect("control reserve remains available");
        let refused = admit_request(&request("GET", "/v2/sessions", ""), Some(&mut reserved)).err().unwrap();
        assert_eq!(refused.reason, RefusalReason::ControlConnectionReserve);
        assert_eq!(refused.details()["capacity"]["bulkConnections"], MAX_CONNECTIONS - CONTROL_CONNECTION_RESERVE);
        drop(admit_request(&request("POST", "/v2/sessions/a/cutex/requests", r#"{"method":"cutex/turn/stop"}"#), Some(&mut reserved)).unwrap());
        drop(bulk.pop());
        drop(admit_request(&request("GET", "/v2/sessions", ""), Some(&mut reserved)).unwrap());
        drop((bulk, reserved));
        assert_eq!(crate::management::listeners::bulk_connections(), 0);
        assert_eq!(crate::management::listeners::open_connections(), 0);
        assert_eq!(STREAMS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn refusal_http_contract_keeps_retry_and_never_echoes_request_contents() {
        use std::io::Read;
        for reason in [RefusalReason::HeavyReadQueueFull, RefusalReason::HeavyReadWaitTimeout,
            RefusalReason::ControlConnectionReserve, RefusalReason::StreamLimit, RefusalReason::FileTransferLimit] {
            let refusal = AdmissionRefusal::new(reason, (2, 3));
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let client = std::thread::spawn(move || {
                let mut socket = std::net::TcpStream::connect(address).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut response = String::new();
                socket.read_to_string(&mut response).unwrap();
                response
            });
            let (mut socket, _) = listener.accept().unwrap();
            crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Requested);
            crate::management::server::write_capacity_refusal(&mut socket, &refusal).unwrap();
            drop(socket);
            let response = client.join().unwrap();
            assert!(response.starts_with("HTTP/1.1 503 "));
            assert!(response.contains("Connection: close\r\n"));
            let body: serde_json::Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["error"]["code"], "heavy_read_capacity_exceeded");
            assert_eq!(body["error"]["retryable"], true);
            assert_eq!(body["error"]["message"], reason.message());
            assert_eq!(body["error"]["details"], refusal.details());
            assert_eq!(body["error"]["details"]["reason"], reason.name());
            assert_eq!(body["error"]["details"]["retryAfterMs"], 250);
        }
        // The actual request-admission path receives these values, but the
        // refusal representation can only contain enum labels and counters.
        let mut secret = request("GET", "/v2/sessions/private-session/history?secret=query", "private-body");
        secret.headers.insert("authorization".into(), "Bearer private-token".into());
        let refusal = admit_with_bulk(classify(&secret), || false).err().unwrap();
        let output = format!("{}{}", refusal.reason.message(), refusal.details());
        for value in ["private-session", "secret=query", "private-body", "private-token", "Bearer"] {
            assert!(!output.contains(value));
        }
        let capacity = refusal.details()["capacity"].clone();
        assert_eq!(capacity.as_object().unwrap().len(), 15);
        assert!(capacity.as_object().unwrap().iter().all(|(key, value)|
            key == "approximate" && value == true || value.is_u64()));
    }

}
