// Shared by the session store benchmark (`bench.rs`) and, copied, by the
// same benchmark against the JSON store (r216) for the comparison in the
// design doc. Synthetic data only: never a real home.

use std::time::Duration;
use std::time::Instant;

use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;
use serde_json::json;

pub(super) struct Shape {
    pub sessions: usize,
    pub receipts: usize,
    /// Status-animation frames per receipt (≈ 650 bytes each): the bulk of
    /// a real receipt is `selected_projection` (animation frames, settings).
    pub frames: usize,
}

fn session(index: usize) -> CutexSessionRecord {
    let name = format!("agent-{index:03}");
    let mut record = CutexSessionRecord::new_at(
        format!("cutex.{name}"),
        Some(format!("019e{index:04x}-7c1d-7a42-9a51-{index:012x}")),
        "devbox".to_string(),
        format!("/srv/projects/Project-{index:03}/workspace"),
        None,
        "2026-09-01T00:00:00Z".to_string(),
    )
    .expect("record");
    record.formal_agent_name = Some(name.clone());
    record.thread_name = Some(format!("{name} thread with a realistic title"));
    record.note = Some("synthetic benchmark record; ".repeat(8));
    record.agent_groups = vec!["scp".into(), "analysis".into()];
    record.sandbox_writable_roots = vec![format!("/srv/scratch/{name}")];
    record.current_runtime_agent_id = Some(format!("stock.{name}.{index}"));
    record.last_runtime_agent_id = Some(format!("stock.{name}.{}", index + 1));
    record.runtime_generation = index as u64 % 9;
    record.last_seen_at = Some("2026-10-09T00:00:00Z".into());
    record
}

fn frames(seed: usize, count: usize) -> serde_json::Value {
    let frames = (0..count)
        .map(|frame| {
            (0..10)
                .map(|cell| {
                    json!({"text": format!("⣾ working {seed}-{frame}-{cell}"),
                           "fg": format!("#{:06x}", (seed * 31 + frame * 7 + cell) % 0xffffff),
                           "bold": cell % 2 == 0})
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    json!({"frames": frames, "repeat": true, "width": 24})
}

fn receipt(
    index: usize,
    sessions: usize,
    frame_count: usize,
) -> crate::agent_management::ExplicitLaunchActionReceipt {
    let session = format!("cutex.agent-{:03}", index % sessions);
    serde_json::from_value(json!({"kind":"runtime","receipt":{
        "action_id":format!("action-{index:05}"),"stage":"ready","claim_id":format!("claim-{index:05}"),
        "runtime_agent_id":format!("stock.agent.{index}"),"expected_generation":index as u64,"binding":null,
        "publication":null,"error":null,"updated_at":"2026-10-08T00:00:00Z",
        "review":{"subject":{"cutex_session_id":session,"formal_name":"worker","durable_sha256":"a".repeat(64),
            "authority_sha256":"a".repeat(64),"current_project_id":null,"revision":1,"runtime_generation":0},
            "contract":{"version":4,"native_id":"native-1","native_home":"/private/home",
                "bundle_manifest":"/private/manifest","bundle_sha256":"b".repeat(64)},
            "configuration":{
                "selected_projection":{"version":2,"route":"chatgpt_file",
                    "auth":{"path":"/private/auth.json","parent_device":1,"parent_inode":2,"owner":3,"account":"a".repeat(64),"api_file":null},
                    "settings":{"windows":{"status_animation":frames(index, frame_count)}},
                    "catalog":null,"requires_job":false},
                "profile_name":"alpha","profile_id":"private","inherited":false,"profile_sha256":"c".repeat(64),
                "account_sha256":"d".repeat(64),"model":"fixture","reasoning":null,"model_provider":"private",
                "provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses",
                    "requires_openai_auth":false,"supports_websockets":false},
                "sandbox":"danger-full-access","approval":"never"},
            "restart":false}
    }}))
    .expect("synthetic receipt")
}

pub(super) fn synthetic_store(shape: &Shape) -> CutexSessionStore {
    let mut store = CutexSessionStore::default();
    for index in 0..shape.sessions {
        let record = session(index);
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
    }
    for index in 0..shape.receipts {
        store.explicit_launch_receipts.insert(
            format!("action-{index:05}"),
            receipt(index, shape.sessions, shape.frames),
        );
    }
    store.store_revision.set(2_600_000);
    store
}

/// Bytes this process passed to write calls so far.
pub(super) fn written_bytes() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        use windows_sys::Win32::System::Threading::GetProcessIoCounters;
        use windows_sys::Win32::System::Threading::IO_COUNTERS;
        let mut counters: IO_COUNTERS = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut counters) };
        assert!(ok != 0);
        counters.WriteTransferCount
    }
    #[cfg(not(windows))]
    {
        let io = std::fs::read_to_string("/proc/self/io").unwrap();
        io.lines()
            .find_map(|line| line.strip_prefix("wchar:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

/// CPU time (user + kernel) this process used so far.
#[allow(dead_code)]
pub(super) fn cpu_time() -> Duration {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        assert!(ok != 0);
        let ticks = |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
        Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
    }
    #[cfg(not(windows))]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
        let time = |value: libc::timeval| {
            Duration::from_secs(value.tv_sec as u64) + Duration::from_micros(value.tv_usec as u64)
        };
        time(usage.ru_utime) + time(usage.ru_stime)
    }
}

pub(super) struct Stats(Vec<Duration>);

impl Stats {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn time<T>(&mut self, run: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let value = run();
        self.0.push(started.elapsed());
        value
    }
    pub fn summary(&self) -> String {
        let mut sorted = self.0.clone();
        sorted.sort();
        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
        let median = sorted[sorted.len() / 2];
        let p90 = sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)];
        let mean = sorted.iter().sum::<Duration>() / sorted.len() as u32;
        format!(
            "n={} median={:.3} ms mean={:.3} ms p90={:.3} ms",
            sorted.len(),
            ms(median),
            ms(mean),
            ms(p90)
        )
    }
}

pub(super) fn shape_from_env() -> (String, Shape) {
    match std::env::var("CUTEX_SESSION_BENCH_SHAPE").as_deref() {
        Ok("small") => (
            "small (~6.5 MB compact, large host after r216 retention)".into(),
            Shape {
                sessions: 220,
                receipts: 360,
                frames: 26,
            },
        ),
        _ => (
            "large (~35 MB compact, 220 sessions / 360 receipts)".into(),
            Shape {
                sessions: 220,
                receipts: 360,
                frames: 150,
            },
        ),
    }
}
