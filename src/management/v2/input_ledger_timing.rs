//! Input-ledger write timing for `/v2/diagnostics/control` (`inputLedger`,
//! 054 §2.13 / §5.2 S25: ledger write p99 < 5 ms).
//!
//! Process-local, like `control_timing`, but with a fine log-linear
//! histogram: `control_timing`'s buckets (1 ms, 5 ms, 10 ms, …) cannot tell a
//! 4 ms p99 from a 1.1 ms one. Each histogram is a fixed static array of
//! atomics: recording is a handful of relaxed atomic adds, no lock, no heap
//! allocation, no IO. Only the process ledger records, and only while its
//! outbox follows an enabled switch (the `outbox` flag every write computes
//! anyway), so with the switch off nothing is recorded and the diagnostics
//! field is absent.
//!
//! Buckets: values below 16 ns are exact; above, 16 linear sub-buckets per
//! power of two (relative error ≤ 1/16). A quantile reports its bucket's
//! upper bound, capped at the observed maximum (never under-reports).
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

const SUB_BITS: u32 = 4;
const SUB: usize = 1 << SUB_BITS;
/// Values at or above 2^(MAX_EXP+1) ns (~36 min) share the last bucket.
const MAX_EXP: u32 = 40;
const BUCKETS: usize = (MAX_EXP - SUB_BITS + 2) as usize * SUB;
const CLAMP_NS: u64 = (1 << (MAX_EXP + 1)) - 1;

fn bucket_of(ns: u64) -> usize {
    let ns = ns.min(CLAMP_NS);
    if ns < SUB as u64 {
        return ns as usize;
    }
    let exp = 63 - ns.leading_zeros();
    let sub = ((ns >> (exp - SUB_BITS)) as usize) & (SUB - 1);
    (exp - SUB_BITS + 1) as usize * SUB + sub
}

/// Largest value that lands in bucket `index` (inclusive).
fn bucket_upper(index: usize) -> u64 {
    if index < SUB {
        return index as u64;
    }
    let exp = (index / SUB) as u32 + SUB_BITS - 1;
    let width = 1u64 << (exp - SUB_BITS);
    (SUB + index % SUB) as u64 * width + width - 1
}

pub struct Histogram {
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub count: u64,
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub sum_ns: u64,
}

impl Histogram {
    pub const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; BUCKETS],
        }
    }

    /// Hot path: four relaxed atomic RMWs on static memory.
    pub fn record_ns(&self, ns: u64) {
        self.count.fetch_add(1, Relaxed);
        self.sum_ns.fetch_add(ns, Relaxed);
        self.max_ns.fetch_max(ns, Relaxed);
        self.buckets[bucket_of(ns)].fetch_add(1, Relaxed);
    }

    /// Approximate under concurrent writes (each counter is read once); the
    /// quantiles come from one pass over the buckets, so they are
    /// self-consistent.
    pub fn summary(&self) -> Summary {
        let counts: [u64; BUCKETS] = std::array::from_fn(|index| self.buckets[index].load(Relaxed));
        let total: u64 = counts.iter().sum();
        let max_ns = self.max_ns.load(Relaxed);
        let quantile = |numerator: u64, denominator: u64| -> u64 {
            if total == 0 {
                return 0;
            }
            // Nearest rank: the smallest value with at least q·n samples at or below it.
            let rank = (u128::from(total) * u128::from(numerator)).div_ceil(u128::from(denominator)).max(1);
            let mut seen = 0u128;
            for (index, count) in counts.iter().enumerate() {
                seen += u128::from(*count);
                if seen >= rank {
                    return bucket_upper(index).min(max_ns);
                }
            }
            max_ns
        };
        Summary {
            count: total,
            p50_ns: quantile(50, 100),
            p99_ns: quantile(99, 100),
            max_ns,
            sum_ns: self.sum_ns.load(Relaxed),
        }
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

/// What a sample measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// A whole `transition` / `transition_batch` / `delivery_changed` call
    /// from taking the ledger lock to after commit: lock wait, the immediate
    /// transaction, the outbox row(s), the commit (WAL fsync) and unlock.
    Transition,
    /// Part of [`Op::Transition`]: waiting for the in-process ledger lock
    /// (and opening the connection on first use).
    LockWait,
    /// Part of [`Op::Transition`]: `COMMIT` alone (`synchronous=FULL`: the
    /// WAL fsync).
    Commit,
    /// The writer's publish trigger after a commit: draining the outbox into
    /// the event journal (or returning at once when another publisher drains).
    Publish,
}

static TRANSITION: Histogram = Histogram::new();
static LOCK_WAIT: Histogram = Histogram::new();
static COMMIT: Histogram = Histogram::new();
static PUBLISH: Histogram = Histogram::new();

fn histogram(op: Op) -> &'static Histogram {
    match op {
        Op::Transition => &TRANSITION,
        Op::LockWait => &LOCK_WAIT,
        Op::Commit => &COMMIT,
        Op::Publish => &PUBLISH,
    }
}

/// Records the time from [`span`] to its drop. Unarmed spans cost nothing.
#[must_use = "a span records when dropped"]
pub struct Span(Option<(Instant, &'static Histogram)>);

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((started, histogram)) = self.0.take() {
            histogram.record_ns(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
    }
}

/// Start timing `op` when `on` (the write's outbox flag: process ledger,
/// switch on).
pub fn span(op: Op, on: bool) -> Span {
    Span(on.then(|| (Instant::now(), histogram(op))))
}

fn ms(ns: u64) -> f64 {
    // Microsecond precision is enough for a millisecond budget.
    (ns / 1_000) as f64 / 1_000.0
}

fn summary_json(summary: Summary) -> Value {
    let mean = summary.sum_ns.checked_div(summary.count).unwrap_or(0);
    json!({"count": summary.count, "p50Ms": ms(summary.p50_ns), "p99Ms": ms(summary.p99_ns),
        "maxMs": ms(summary.max_ns), "meanMs": ms(mean)})
}

/// `inputLedger` of `/v2/diagnostics/control` (and the bus's
/// `/api/diagnostics/control`): counters since process start.
pub fn snapshot() -> Value {
    json!({"schema": 1, "scope": "process", "approximate": true, "quantileRelativeError": 0.0625,
        "transition": summary_json(TRANSITION.summary()),
        "lockWait": summary_json(LOCK_WAIT.summary()),
        "commit": summary_json(COMMIT.summary()),
        "publish": summary_json(PUBLISH.summary())})
}

/// The field to add, or `None` while the switch is off (the response stays
/// exactly what it was before the field existed).
pub fn diagnostics() -> Option<Value> {
    super::input_ledger::enabled().then(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_contiguous_and_bounded_by_one_sixteenth() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(CLAMP_NS), BUCKETS - 1);
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_upper(BUCKETS - 1), CLAMP_NS);
        for index in 1..BUCKETS {
            let lower = bucket_upper(index - 1) + 1;
            let upper = bucket_upper(index);
            assert_eq!(bucket_of(lower), index, "lower edge of {index}");
            assert_eq!(bucket_of(upper), index, "upper edge of {index}");
            assert!((upper - lower) * 16 <= lower.max(16), "bucket {index} [{lower}, {upper}] wider than 1/16");
        }
        // 5 ms: the S25 budget sits inside a bucket ≤ 0.33 ms wide.
        let at = bucket_of(5_000_000);
        assert!(bucket_upper(at) - bucket_upper(at - 1) <= 262_144);
    }

    #[test]
    fn quantiles_of_known_samples() {
        let empty = Histogram::new();
        assert_eq!(empty.summary(), Summary { count: 0, p50_ns: 0, p99_ns: 0, max_ns: 0, sum_ns: 0 });

        // Exact region (< 16 ns): 1..=10, nearest rank.
        let small = Histogram::new();
        for ns in 1..=10 {
            small.record_ns(ns);
        }
        let s = small.summary();
        assert_eq!((s.count, s.p50_ns, s.p99_ns, s.max_ns, s.sum_ns), (10, 5, 10, 10, 55));

        // 1000 samples: 980 at 1 ms, 15 at 3 ms, 5 at 40 ms. p50 = 1 ms,
        // p99 = rank 990 = 3 ms (bucket upper bound, within 1/16), max 40 ms.
        let load = Histogram::new();
        for _ in 0..980 {
            load.record_ns(1_000_000);
        }
        for _ in 0..15 {
            load.record_ns(3_000_000);
        }
        for _ in 0..5 {
            load.record_ns(40_000_000);
        }
        let s = load.summary();
        assert_eq!(s.count, 1000);
        assert!((1_000_000..=1_000_000 + 1_000_000 / 16).contains(&s.p50_ns), "p50 {}", s.p50_ns);
        assert_eq!(s.p50_ns, bucket_upper(bucket_of(1_000_000)));
        assert!((3_000_000..=3_000_000 + 3_000_000 / 16).contains(&s.p99_ns), "p99 {}", s.p99_ns);
        assert_eq!(s.max_ns, 40_000_000);
        // Rank 991..1000 are the 40 ms tail: p99.5 would see it, p99 does not.
        for _ in 0..6 {
            load.record_ns(40_000_000);
        }
        assert_eq!(load.summary().p99_ns, 40_000_000, "capped at the max, not the bucket bound");

        // A single sample: every quantile is that sample (capped at max).
        let one = Histogram::new();
        one.record_ns(4_321_000);
        let s = one.summary();
        assert_eq!((s.p50_ns, s.p99_ns, s.max_ns), (4_321_000, 4_321_000, 4_321_000));
    }

    #[test]
    fn json_shape_and_unarmed_spans() {
        let value = summary_json(Summary { count: 4, p50_ns: 1_234_567, p99_ns: 4_999_999, max_ns: 7_000_000, sum_ns: 8_000_000 });
        assert_eq!(value, json!({"count": 4, "p50Ms": 1.234, "p99Ms": 4.999, "maxMs": 7.0, "meanMs": 2.0}));
        assert!(span(Op::Publish, false).0.is_none(), "switch off: no clock read, nothing recorded");
        let snapshot = snapshot();
        for key in ["transition", "lockWait", "commit", "publish"] {
            assert!(snapshot[key]["p99Ms"].is_f64(), "{key}");
        }
    }

    /// A process-ledger write (outbox on) is timed: whole call, lock wait,
    /// commit; the publish trigger separately. Counters are process-global
    /// and other tests write too, so only growth is checked.
    #[test]
    fn outboxed_writes_are_timed() {
        use super::super::input_ledger::*;
        fn sink(_: &str, _: String, _: Value) -> anyhow::Result<()> {
            Ok(())
        }
        let root = std::env::temp_dir().join(format!("cutex-input-timing-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let ledger = InputLedger::open(root.join("input-ledger.sqlite3")).with_outbox(sink);
        let before = [&TRANSITION, &LOCK_WAIT, &COMMIT].map(|histogram| histogram.summary().count);
        let input = NewInput {
            kind: InputKind::AgentMessage,
            sender: InputSender { kind: SenderKind::Agent, id: "a".into(), name: "A".into(), host: None, cutex_session_id: None },
            intent: InputIntent { strategy: None, delivery_mode: Some(DeliveryMode::AfterTurn), timing: Timing::AfterTurn },
            preview: "x".into(),
            ask_id: None,
            remote: None,
        };
        let created = ledger.transition("timing-s1", "m1", Expect::NEW, Next::waiting("afterTurn").create(input)).unwrap();
        assert!(created.changed);
        ledger.delivery_changed("timing-s1", &json!({"state": "idle"})).unwrap();
        ledger.transition_batch("timing-s1", vec![("m1".to_string(), Expect::ANY, Next::sending())]).unwrap();
        let after = [&TRANSITION, &LOCK_WAIT, &COMMIT].map(|histogram| histogram.summary().count);
        for (index, name) in ["transition", "lockWait", "commit"].into_iter().enumerate() {
            assert!(after[index] >= before[index] + 3, "{name}: {} -> {}", before[index], after[index]);
        }
        let summary = TRANSITION.summary();
        assert!(summary.max_ns > 0 && summary.p99_ns <= summary.max_ns);
        drop(ledger);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Recording does not allocate: `record_ns` is atomics on a `'static`
    /// array, `span` stores an `Instant` and a `&'static` in an `Option` on
    /// the caller's stack. Microbenchmark (prints the number; the bound is
    /// loose so debug builds pass).
    #[test]
    fn recording_is_cheap() {
        let local = Box::leak(Box::new(Histogram::new()));
        const N: u64 = 1_000_000;
        let started = Instant::now();
        for ns in 0..N {
            local.record_ns(std::hint::black_box(ns * 37));
        }
        let per_record = started.elapsed().as_nanos() as f64 / N as f64;
        let started = Instant::now();
        for _ in 0..N {
            drop(std::hint::black_box(Span(Some((Instant::now(), &*local)))));
        }
        let per_span = started.elapsed().as_nanos() as f64 / N as f64;
        eprintln!("input ledger timing: record_ns {per_record:.1} ns, span+record {per_span:.1} ns");
        assert_eq!(local.summary().count, 2 * N);
        assert!(per_record < 2_000.0 && per_span < 5_000.0);
    }
}
