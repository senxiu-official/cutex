//! Process-local, bounded-cardinality control-path diagnostics. Never accept
//! request ids, session ids, payloads, error text, URLs or caller labels.
//! Stages are sequential: repeated visits are summed for one request, then
//! recorded once in that stage's histogram. Snapshots are approximate during
//! concurrent updates; no global lock or per-request heap allocation is used.
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock,
};
use std::time::Instant;

const SLOW_NS: u64 = 250_000_000;
const LOG_INTERVAL_NS: u64 = 1_000_000_000;
const UPPER_NS: [u64; 9] = [
    1_000_000,
    5_000_000,
    10_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    5_000_000_000,
];
const BUCKETS: usize = UPPER_NS.len() + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Path {
    InputSubmit,
    InputHistory,
    InputNative,
    InputQueue,
    BusFanout,
    UserInput,
    ManagementRead,
    AsksAdmission,
    AsksDispatch,
    AsksStoreRead,
    AsksStoreWrite,
    ControlOther,
    McpAgentDelegate,
    McpAgentCreate,
    McpAgentList,
    McpAgentTell,
    McpDelegationAwait,
    McpDelegationClose,
    McpDelegationProgress,
    McpDelegationRead,
    McpDelegationReopen,
    McpDelegationResume,
    McpDelegationStop,
    McpDelegationWait,
    McpQuestionAnswer,
    McpResolveIds,
    Other,
    EventAppend,
    EventFanout,
    LivePublish,
    LiveObserve,
    NativeEventSend,
    EventRepositoryLock,
    EventReplayLock,
}
const PATHS: [Path; 34] = [
    Path::InputSubmit,
    Path::InputHistory,
    Path::InputNative,
    Path::InputQueue,
    Path::BusFanout,
    Path::UserInput,
    Path::ManagementRead,
    Path::AsksAdmission,
    Path::AsksDispatch,
    Path::AsksStoreRead,
    Path::AsksStoreWrite,
    Path::ControlOther,
    Path::McpAgentDelegate,
    Path::McpAgentCreate,
    Path::McpAgentList,
    Path::McpAgentTell,
    Path::McpDelegationAwait,
    Path::McpDelegationClose,
    Path::McpDelegationProgress,
    Path::McpDelegationRead,
    Path::McpDelegationReopen,
    Path::McpDelegationResume,
    Path::McpDelegationStop,
    Path::McpDelegationWait,
    Path::McpQuestionAnswer,
    Path::McpResolveIds,
    Path::Other,
    Path::EventAppend,
    Path::EventFanout,
    Path::LivePublish,
    Path::LiveObserve,
    Path::NativeEventSend,
    Path::EventRepositoryLock,
    Path::EventReplayLock,
];
impl Path {
    pub fn from_tool(name: &str) -> Self {
        match name {
            "agent_delegate" => Self::McpAgentDelegate,
            "agent_create" => Self::McpAgentCreate,
            "agent_list" => Self::McpAgentList,
            "agent_tell" => Self::McpAgentTell,
            "delegation_await" => Self::McpDelegationAwait,
            "delegation_close" => Self::McpDelegationClose,
            "delegation_progress" => Self::McpDelegationProgress,
            "delegation_read" => Self::McpDelegationRead,
            "delegation_reopen" => Self::McpDelegationReopen,
            "delegation_resume" => Self::McpDelegationResume,
            "delegation_stop" => Self::McpDelegationStop,
            "delegation_wait" => Self::McpDelegationWait,
            "question_answer" => Self::McpQuestionAnswer,
            _ => Self::Other,
        }
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::InputSubmit => "input.submit",
            Self::InputHistory => "input.history",
            Self::InputNative => "input.native",
            Self::InputQueue => "input.queue",
            Self::BusFanout => "bus.fanout",
            Self::UserInput => "user_input",
            Self::ManagementRead => "management_read",
            Self::AsksAdmission => "asks.admission",
            Self::AsksDispatch => "asks.dispatch",
            Self::AsksStoreRead => "asks.store_read",
            Self::AsksStoreWrite => "asks.store_write",
            Self::ControlOther => "control_other",
            Self::McpAgentDelegate => "mcp.agent_delegate",
            Self::McpAgentCreate => "mcp.agent_create",
            Self::McpAgentList => "mcp.agent_list",
            Self::McpAgentTell => "mcp.agent_tell",
            Self::McpDelegationAwait => "mcp.delegation_await",
            Self::McpDelegationClose => "mcp.delegation_close",
            Self::McpDelegationProgress => "mcp.delegation_progress",
            Self::McpDelegationRead => "mcp.delegation_read",
            Self::McpDelegationReopen => "mcp.delegation_reopen",
            Self::McpDelegationResume => "mcp.delegation_resume",
            Self::McpDelegationStop => "mcp.delegation_stop",
            Self::McpDelegationWait => "mcp.delegation_wait",
            Self::McpQuestionAnswer => "mcp.question_answer",
            Self::McpResolveIds => "mcp.resolve_ids",
            Self::Other => "other",
            Self::EventAppend => "management.event_append",
            Self::EventFanout => "management.event_fanout",
            Self::LivePublish => "management.live_publish",
            Self::LiveObserve => "management.live_observe",
            Self::NativeEventSend => "native.event_send",
            Self::EventRepositoryLock => "management.repository_lock",
            Self::EventReplayLock => "management.replay_lock",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Stage {
    Tool,
    Validation,
    ContextRead,
    LockWait,
    StoreRead,
    StoreWrite,
    Dispatch,
    Fanout,
    ResponseWrite,
    ApprovalWait,
    QueueWait,
}
const STAGES: [Stage; 11] = [
    Stage::Tool,
    Stage::Validation,
    Stage::ContextRead,
    Stage::LockWait,
    Stage::StoreRead,
    Stage::StoreWrite,
    Stage::Dispatch,
    Stage::Fanout,
    Stage::ResponseWrite,
    Stage::ApprovalWait,
    Stage::QueueWait,
];
impl Stage {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Validation => "validation",
            Self::ContextRead => "context_read",
            Self::LockWait => "lock_wait",
            Self::StoreRead => "store_read",
            Self::StoreWrite => "store_write",
            Self::Dispatch => "dispatch",
            Self::Fanout => "fanout",
            Self::ResponseWrite => "response_write",
            Self::ApprovalWait => "approval_wait",
            Self::QueueWait => "queue_wait",
        }
    }
}

struct Histogram {
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}
impl Histogram {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    fn record(&self, ns: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .sum_ns
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |sum| {
                Some(sum.saturating_add(ns))
            });
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        let bucket = UPPER_NS
            .iter()
            .position(|upper| ns <= *upper)
            .unwrap_or(UPPER_NS.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }
    fn snapshot(&self) -> Value {
        json!({"count":self.count.load(Ordering::Relaxed),"sumNs":self.sum_ns.load(Ordering::Relaxed),"maxNs":self.max_ns.load(Ordering::Relaxed),
            "buckets":self.buckets.each_ref().map(|bucket| bucket.load(Ordering::Relaxed))})
    }
}
struct Counters {
    started: AtomicU64,
    total: Histogram,
    stages: [Histogram; STAGES.len()],
    active_stages: [AtomicU64; STAGES.len()],
    status_classes: [AtomicU64; 7],
    slow: AtomicU64,
    suppressed: AtomicU64,
    next_log_ns: AtomicU64,
}
impl Counters {
    fn new() -> Self {
        Self {
            started: AtomicU64::new(0),
            total: Histogram::new(),
            stages: std::array::from_fn(|_| Histogram::new()),
            active_stages: std::array::from_fn(|_| AtomicU64::new(0)),
            status_classes: std::array::from_fn(|_| AtomicU64::new(0)),
            slow: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            next_log_ns: AtomicU64::new(0),
        }
    }
}
struct Registry {
    origin: Instant,
    paths: [Counters; PATHS.len()],
}
impl Registry {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            paths: std::array::from_fn(|_| Counters::new()),
        }
    }
    fn record(
        &self,
        path: Path,
        status: u16,
        total: u64,
        stages: &[u64; STAGES.len()],
        seen: &[bool; STAGES.len()],
        now_ns: u64,
    ) -> bool {
        let counters = &self.paths[path as usize];
        counters.total.record(total);
        for stage in STAGES {
            if seen[stage as usize] {
                counters.stages[stage as usize].record(stages[stage as usize]);
            }
        }
        let class = if status == 0 {
            0
        } else if (100..600).contains(&status) {
            usize::from(status / 100)
        } else {
            6
        };
        counters.status_classes[class].fetch_add(1, Ordering::Relaxed);
        if total < SLOW_NS {
            return false;
        }
        counters.slow.fetch_add(1, Ordering::Relaxed);
        let next = counters.next_log_ns.load(Ordering::Relaxed);
        if now_ns >= next
            && counters
                .next_log_ns
                .compare_exchange(
                    next,
                    now_ns.saturating_add(LOG_INTERVAL_NS),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            return true;
        }
        counters.suppressed.fetch_add(1, Ordering::Relaxed);
        false
    }
    fn snapshot(&self) -> Value {
        let mut paths = serde_json::Map::new();
        for path in PATHS {
            let counters = &self.paths[path as usize];
            let started = counters.started.load(Ordering::Relaxed);
            if started == 0 {
                continue;
            }
            let mut stages = serde_json::Map::new();
            for stage in STAGES {
                let histogram = &counters.stages[stage as usize];
                let active = counters.active_stages[stage as usize].load(Ordering::Relaxed);
                if active != 0 || histogram.count.load(Ordering::Relaxed) != 0 {
                    let mut value = histogram.snapshot();
                    value["inFlight"] = json!(active);
                    stages.insert(stage.name().into(), value);
                }
            }
            paths.insert(path.name().into(), json!({"started":started,"inFlight":started.saturating_sub(counters.total.count.load(Ordering::Relaxed)),
                "total":counters.total.snapshot(),"stages":stages,"statusClasses":counters.status_classes.each_ref().map(|count| count.load(Ordering::Relaxed)),
                "slow":counters.slow.load(Ordering::Relaxed),"slowLogsSuppressed":counters.suppressed.load(Ordering::Relaxed)}));
        }
        json!({"schema":1,"scope":"process","approximate":true,"slowThresholdMs":250,"slowLogIntervalMs":1000,
            "bucketUpperMs":[1,5,10,50,100,250,500,1000,5000,null],"statusClassLabels":["unset","1xx","2xx","3xx","4xx","5xx","other"],"paths":paths})
    }
}
static REGISTRY: OnceLock<Registry> = OnceLock::new();
fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}
fn nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

pub struct Trace {
    registry: &'static Registry,
    path: Path,
    started: Instant,
    last: Instant,
    current: Stage,
    elapsed: [u64; STAGES.len()],
    seen: [bool; STAGES.len()],
    status: u16,
    emit_slow_log: bool,
}
impl Trace {
    pub fn new(path: Path) -> Self {
        Self::with_registry(path, registry())
    }
    /// Nested hot-path metrics must not synchronously write stderr while the
    /// repository or live coordinator is held. Outer request traces still log.
    pub fn metrics_only(path: Path) -> Self {
        let mut trace = Self::new(path);
        trace.emit_slow_log = false;
        trace
    }
    fn with_registry(path: Path, registry: &'static Registry) -> Self {
        registry.paths[path as usize]
            .started
            .fetch_add(1, Ordering::Relaxed);
        registry.paths[path as usize].active_stages[Stage::Tool as usize]
            .fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut seen = [false; STAGES.len()];
        seen[Stage::Tool as usize] = true;
        Self {
            registry,
            path,
            started: now,
            last: now,
            current: Stage::Tool,
            elapsed: [0; STAGES.len()],
            seen,
            status: 0,
            emit_slow_log: true,
        }
    }
    pub fn stage(&mut self, stage: Stage) {
        if stage == self.current {
            return;
        }
        self.finish_stage();
        let counters = &self.registry.paths[self.path as usize];
        counters.active_stages[self.current as usize].fetch_sub(1, Ordering::Relaxed);
        counters.active_stages[stage as usize].fetch_add(1, Ordering::Relaxed);
        self.current = stage;
        self.seen[stage as usize] = true;
    }
    pub fn status(&mut self, status: u16) {
        self.status = status;
    }
    fn slow_record(&self, total: u64) -> Value {
        let mut stages = serde_json::Map::new();
        for stage in STAGES {
            if self.seen[stage as usize] {
                stages.insert(stage.name().into(), json!(self.elapsed[stage as usize]));
            }
        }
        // Wall clock identifies when the completed slow sample was emitted;
        // durations remain monotonic. Only fixed enum labels are serialized.
        json!({"schema":1,"timestamp":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "path":self.path.name(),"status":self.status,"totalNs":total,"stagesNs":stages})
    }
    fn finish_stage(&mut self) {
        let now = Instant::now();
        let at = self.current as usize;
        self.elapsed[at] = self.elapsed[at].saturating_add(nanos(now.duration_since(self.last)));
        self.last = now;
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        self.finish_stage();
        self.registry.paths[self.path as usize].active_stages[self.current as usize]
            .fetch_sub(1, Ordering::Relaxed);
        let total = nanos(self.started.elapsed());
        if self.registry.record(
            self.path,
            self.status,
            total,
            &self.elapsed,
            &self.seen,
            nanos(self.registry.origin.elapsed()),
        ) && self.emit_slow_log {
            // Numeric status is supplied by the caller (MCP uses logical
            // success/refusal/error classes); no original error is logged.
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "cutex-control-slow {}",
                self.slow_record(total)
            );
        }
    }
}

/// Only this process is included: an MCP adapter and the daemon intentionally
/// have separate registries. No filesystem, environment, token or RPC reads.
pub fn snapshot() -> Value {
    let mut value = registry().snapshot();
    value["scheduler"] = crate::asks::scheduler::diagnostics();
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_tool_labels_are_bounded_and_never_reach_diagnostics() {
        for name in [
            "delegation_arbitrary-secret",
            "agent_list/session-secret",
            "Bearer credential",
            "",
        ] {
            assert_eq!(Path::from_tool(name), Path::Other);
        }
        assert_eq!(Path::from_tool("agent_delegate"), Path::McpAgentDelegate);
        let local = Box::leak(Box::new(Registry::new()));
        {
            let mut trace = Trace::with_registry(Path::from_tool("Bearer credential"), local);
            trace.status(403);
        }
        let snapshot = local.snapshot().to_string();
        assert!(!snapshot.contains("credential") && !snapshot.contains("Bearer"));
        assert_eq!(local.snapshot()["paths"]["other"]["statusClasses"][4], 1);
    }
    #[test]
    fn drop_records_one_request_and_repeated_stage_visits_once() {
        let local = Box::leak(Box::new(Registry::new()));
        let mut trace = Trace::with_registry(Path::UserInput, local);
        assert_eq!(local.snapshot()["paths"]["user_input"]["inFlight"], 1);
        trace.stage(Stage::Validation);
        trace.stage(Stage::StoreRead);
        trace.stage(Stage::Validation);
        trace.status(200);
        assert_eq!(
            local.snapshot()["paths"]["user_input"]["stages"]["validation"]["inFlight"],
            1
        );
        drop(trace);
        let path = &local.snapshot()["paths"]["user_input"];
        assert_eq!(path["inFlight"], 0);
        assert_eq!(path["total"]["count"], 1);
        assert_eq!(path["stages"]["validation"]["count"], 1);
        assert_eq!(path["stages"]["validation"]["inFlight"], 0);
        assert_eq!(path["stages"]["store_read"]["count"], 1);
        assert_eq!(path["statusClasses"][2], 1);
    }
    #[test]
    fn histogram_boundaries_and_slow_log_rate_limit_are_exact() {
        let local = Registry::new();
        let stages = [0; STAGES.len()];
        let seen = [false; STAGES.len()];
        let path = Path::McpAgentTell;
        for ns in [0, 1_000_000, 1_000_001, 5_000_000_001] {
            local.record(path, 200, ns, &stages, &seen, 0);
        }
        let counters = &local.paths[path as usize];
        assert_eq!(counters.total.buckets[0].load(Ordering::Relaxed), 2);
        assert_eq!(counters.total.buckets[1].load(Ordering::Relaxed), 1);
        assert_eq!(counters.total.buckets[9].load(Ordering::Relaxed), 1);
        // The >5s sample above logged at monotonic t=0.
        assert!(!local.record(path, 500, SLOW_NS, &stages, &seen, LOG_INTERVAL_NS - 1));
        assert!(local.record(path, 500, SLOW_NS, &stages, &seen, LOG_INTERVAL_NS));
        assert_eq!(counters.slow.load(Ordering::Relaxed), 3);
        assert_eq!(counters.suppressed.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn concurrent_requests_keep_exact_counts_and_status_classes() {
        let local = std::sync::Arc::new(Registry::new());
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let local = local.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        local.paths[Path::McpAgentList as usize]
                            .started
                            .fetch_add(1, Ordering::Relaxed);
                        local.record(
                            Path::McpAgentList,
                            if thread % 2 == 0 { 200 } else { 500 },
                            10_000_000,
                            &[0; STAGES.len()],
                            &[false; STAGES.len()],
                            0,
                        );
                    }
                });
            }
        });
        let path = &local.snapshot()["paths"]["mcp.agent_list"];
        assert_eq!(path["started"], 8000);
        assert_eq!(path["inFlight"], 0);
        assert_eq!(path["total"]["count"], 8000);
        assert_eq!(path["total"]["sumNs"], 80_000_000_000_u64);
        assert_eq!(path["statusClasses"][2], 4000);
        assert_eq!(path["statusClasses"][5], 4000);
    }
    #[test]
    fn slow_record_has_utc_time_and_only_fixed_labels() {
        let local = Box::leak(Box::new(Registry::new()));
        let mut trace = Trace::with_registry(Path::from_tool("Bearer secret /session/user-body"), local);
        trace.status(503);
        trace.stage(Stage::LockWait);
        trace.finish_stage();
        let before = chrono::Utc::now().timestamp_millis();
        let record = trace.slow_record(300_000_000);
        let after = chrono::Utc::now().timestamp_millis();
        let timestamp = record["timestamp"].as_str().unwrap();
        assert!(timestamp.ends_with('Z'));
        let parsed = chrono::DateTime::parse_from_rfc3339(timestamp).unwrap().timestamp_millis();
        assert!((before..=after).contains(&parsed));
        assert_eq!(record["schema"], 1);
        assert_eq!(record["path"], "other");
        assert_eq!(record["status"], 503);
        assert_eq!(record["totalNs"], 300_000_000);
        assert!(record["stagesNs"]["lock_wait"].is_u64());
        assert_eq!(record.as_object().unwrap().len(), 6);
        assert!(!record.to_string().contains("secret"));
        assert!(!record.to_string().contains("user-body"));
        assert!(!record.to_string().contains("Bearer"));
    }

}
