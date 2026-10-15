//! Boot-aware liveness of a recorded runtime occurrence.
//!
//! A plain `process_is_running(pid)` is weak evidence after a host reboot:
//! PIDs restart from low numbers, so a PID recorded in the previous boot can
//! belong to an unrelated live process. Every app-server binding records
//! `started_at` (the process creation time for stock launches, the spawn
//! time for managed launches), so two checks need no new durable field:
//!
//! - **previous boot**: a binding recorded before this boot began (with
//!   [`PREVIOUS_BOOT_SLACK`] for clock corrections) cannot be running now;
//!   every PID of that occurrence (app-server, TUI, alden) is dead or reused.
//! - **PID reuse**: a live binding PID whose creation time lies outside
//!   `[started_at - SPAWN_WINDOW, started_at + START_TOLERANCE]` is another
//!   process.
//!
//! A live binding PID whose creation time matches always counts as live, so
//! a wall-clock step after boot never turns a running runtime offline by the
//! boot check alone. Records without a binding (legacy `runtime_pid` /
//! `alden_pid` only) or with an unparseable `started_at` keep the plain
//! process check.

use chrono::{DateTime, Duration, Utc};

use crate::session::model::{CutexAppServerRuntimeBinding, CutexSessionRecord};

/// Clock-correction margin for the previous-boot proof.
pub const PREVIOUS_BOOT_SLACK_SECS: i64 = 120;
/// A managed launch stamps `started_at` right after spawning; the process
/// may have been created up to this long before that stamp.
pub const SPAWN_WINDOW_SECS: i64 = 120;
/// Rounding margin for a process created "after" its recorded stamp.
pub const START_TOLERANCE_SECS: i64 = 2;

/// What the host says about a binding's recorded app-server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingProcess {
    /// The PID runs and its creation time matches the recorded occurrence.
    Same,
    /// The occurrence was recorded before this boot began.
    PreviousBoot,
    /// The PID is not running.
    Exited,
    /// The PID runs but was created at another time: a reused PID.
    Reused,
    /// No usable evidence (no timestamp, creation time unavailable): callers
    /// fall back to plain process liveness.
    Unverified,
}

/// Host facts, injectable for tests.
pub struct HostProbe<'a> {
    pub boot_time: Option<DateTime<Utc>>,
    pub running: &'a dyn Fn(u32) -> bool,
    pub started: &'a dyn Fn(u32) -> Option<DateTime<Utc>>,
}

fn live_running(pid: u32) -> bool {
    crate::platform::process::process_is_running(pid)
}

fn live_started(pid: u32) -> Option<DateTime<Utc>> {
    crate::platform::process::process_started_at(pid).ok()
}

/// Runs `probe` with the real host facts (boot time read once).
pub fn with_host_probe<T>(probe: impl FnOnce(&HostProbe<'_>) -> T) -> T {
    let host = HostProbe {
        boot_time: crate::platform::process::system_boot_time().ok(),
        running: &live_running,
        started: &live_started,
    };
    probe(&host)
}

fn recorded_start(binding: &CutexAppServerRuntimeBinding) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(binding.started_at.trim())
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn recorded_before_boot(recorded: DateTime<Utc>, boot_time: Option<DateTime<Utc>>) -> bool {
    boot_time.is_some_and(|boot| recorded < boot - Duration::seconds(PREVIOUS_BOOT_SLACK_SECS))
}

/// Classifies the binding's app-server process.
pub fn binding_process(binding: &CutexAppServerRuntimeBinding, host: &HostProbe<'_>) -> BindingProcess {
    let recorded = recorded_start(binding);
    let previous_boot = recorded.is_some_and(|recorded| recorded_before_boot(recorded, host.boot_time));
    if binding.pid == 0 || !(host.running)(binding.pid) {
        return if previous_boot { BindingProcess::PreviousBoot } else { BindingProcess::Exited };
    }
    let Some(recorded) = recorded else {
        return BindingProcess::Unverified;
    };
    match (host.started)(binding.pid) {
        Some(actual)
            if actual >= recorded - Duration::seconds(SPAWN_WINDOW_SECS)
                && actual <= recorded + Duration::seconds(START_TOLERANCE_SECS) =>
        {
            BindingProcess::Same
        }
        Some(_) if previous_boot => BindingProcess::PreviousBoot,
        Some(_) => BindingProcess::Reused,
        None if previous_boot => BindingProcess::PreviousBoot,
        None => BindingProcess::Unverified,
    }
}

/// True when the record's binding is proven to be from an earlier boot (its
/// processes are gone; any live PID with the same number is unrelated).
pub fn binding_is_from_previous_boot(record: &CutexSessionRecord, host: &HostProbe<'_>) -> bool {
    record
        .app_server_runtime
        .as_ref()
        .is_some_and(|binding| binding_process(binding, host) == BindingProcess::PreviousBoot)
}

/// True when the recorded binding is proven not running: previous boot,
/// exited, or its PID reused. `Unverified` and `Same` are not proof.
pub fn binding_is_proven_gone(record: &CutexSessionRecord, host: &HostProbe<'_>) -> bool {
    record.app_server_runtime.as_ref().is_some_and(|binding| {
        matches!(
            binding_process(binding, host),
            BindingProcess::PreviousBoot | BindingProcess::Exited | BindingProcess::Reused
        )
    })
}

/// Boot-aware replacement for `process_is_running(pid)` when `pid` is one of
/// `record`'s occurrence PIDs (binding, `runtime_pid`, `alden_pid`).
pub fn occurrence_pid_is_live_with(record: &CutexSessionRecord, pid: u32, host: &HostProbe<'_>) -> bool {
    if pid == 0 || !(host.running)(pid) {
        return false;
    }
    let Some(binding) = record.app_server_runtime.as_ref() else {
        // Legacy record without a binding: plain liveness.
        return true;
    };
    let proof = binding_process(binding, host);
    if pid == binding.pid {
        return !matches!(
            proof,
            BindingProcess::PreviousBoot | BindingProcess::Exited | BindingProcess::Reused
        );
    }
    // A companion (TUI / alden) of an occurrence recorded before this boot
    // is a reused PID; otherwise keep the plain check.
    proof != BindingProcess::PreviousBoot
}

/// [`occurrence_pid_is_live_with`] against the real host.
pub fn occurrence_pid_is_live(record: &CutexSessionRecord, pid: u32) -> bool {
    with_host_probe(|host| occurrence_pid_is_live_with(record, pid, host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::CutexAppServerTransport;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    fn record(pid: u32, started_at: &str) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex-boot".into(),
            None,
            "devbox".into(),
            "/tmp/project".into(),
            None,
            "2026-10-12T00:00:00Z".into(),
        )
        .unwrap();
        record.app_server_runtime = Some(CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::LoopbackWebSocket,
            endpoint: "ws://127.0.0.1:1".into(),
            pid,
            runtime_dir: "/missing".into(),
            launched_profile: None,
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/missing/j".into(),
            schema_version: "v".into(),
            schema_sha256: "s".into(),
            started_at: started_at.into(),
        });
        record.runtime_pid = Some(pid + 1);
        record
    }

    const BOOT: &str = "2026-10-12T06:39:54Z";

    fn probe<'a>(
        running: &'a dyn Fn(u32) -> bool,
        started: &'a dyn Fn(u32) -> Option<DateTime<Utc>>,
    ) -> HostProbe<'a> {
        HostProbe { boot_time: Some(at(BOOT)), running, started }
    }

    #[test]
    fn binding_from_another_boot_is_gone_even_when_its_pids_were_reused() {
        let record = record(4242, "2026-10-12T05:00:00Z");
        let running = |_: u32| true;
        let started = |_: u32| Some(at("2026-10-12T06:41:00Z"));
        let host = probe(&running, &started);
        assert_eq!(binding_process(record.app_server_runtime.as_ref().unwrap(), &host), BindingProcess::PreviousBoot);
        assert!(binding_is_from_previous_boot(&record, &host));
        assert!(!occurrence_pid_is_live_with(&record, 4242, &host));
        assert!(!occurrence_pid_is_live_with(&record, 4243, &host), "companion PID of an old boot");
        // Creation time unavailable: the boot check alone is proof.
        let unknown = |_: u32| None;
        let host = probe(&running, &unknown);
        assert!(!occurrence_pid_is_live_with(&record, 4242, &host));
        let dead = |_: u32| false;
        let host = probe(&dead, &unknown);
        assert_eq!(binding_process(record.app_server_runtime.as_ref().unwrap(), &host), BindingProcess::PreviousBoot);
    }

    #[test]
    fn live_binding_of_this_boot_stays_live() {
        let record = record(4242, "2026-10-12T06:45:00.500Z");
        let running = |_: u32| true;
        // Stock launch: started_at is the creation time itself.
        let exact = |_: u32| Some(at("2026-10-12T06:45:00.500Z"));
        let host = probe(&running, &exact);
        assert_eq!(binding_process(record.app_server_runtime.as_ref().unwrap(), &host), BindingProcess::Same);
        assert!(occurrence_pid_is_live_with(&record, 4242, &host));
        assert!(occurrence_pid_is_live_with(&record, 4243, &host));
        // Managed launch: stamped shortly after the spawn.
        let spawned = |_: u32| Some(at("2026-10-12T06:44:59Z"));
        let host = probe(&running, &spawned);
        assert!(occurrence_pid_is_live_with(&record, 4242, &host));
        assert!(!binding_is_proven_gone(&record, &host));
    }

    #[test]
    fn a_live_process_with_the_recorded_start_time_wins_over_a_clock_step() {
        // Recorded "before boot" only because the clock stepped after it.
        let record = record(4242, "2026-10-12T06:30:00Z");
        let running = |_: u32| true;
        let same = |_: u32| Some(at("2026-10-12T06:30:00Z"));
        let host = probe(&running, &same);
        assert!(occurrence_pid_is_live_with(&record, 4242, &host));
        assert!(!binding_is_from_previous_boot(&record, &host));
    }

    #[test]
    fn recycled_pid_with_a_different_start_time_is_not_live() {
        let record = record(4242, "2026-10-12T06:45:00Z");
        let running = |_: u32| true;
        let later = |_: u32| Some(at("2026-10-12T07:10:00Z"));
        let host = probe(&running, &later);
        assert_eq!(binding_process(record.app_server_runtime.as_ref().unwrap(), &host), BindingProcess::Reused);
        assert!(!occurrence_pid_is_live_with(&record, 4242, &host));
        assert!(binding_is_proven_gone(&record, &host));
        // Same boot: a live companion keeps the existing plain check.
        assert!(occurrence_pid_is_live_with(&record, 4243, &host));
        let much_earlier = |_: u32| Some(at("2026-10-12T06:40:00Z"));
        let host = probe(&running, &much_earlier);
        assert_eq!(binding_process(record.app_server_runtime.as_ref().unwrap(), &host), BindingProcess::Reused);
    }

    #[test]
    fn records_without_usable_evidence_keep_the_plain_process_check() {
        let mut legacy = record(4242, "2026-10-12T06:45:00Z");
        legacy.app_server_runtime = None;
        let running = |_: u32| true;
        let unknown = |_: u32| None;
        let host = probe(&running, &unknown);
        assert!(occurrence_pid_is_live_with(&legacy, 4243, &host));
        let dead = |_: u32| false;
        let host = probe(&dead, &unknown);
        assert!(!occurrence_pid_is_live_with(&legacy, 4243, &host));

        let unparseable = record(4242, "");
        let host = probe(&running, &unknown);
        assert_eq!(binding_process(unparseable.app_server_runtime.as_ref().unwrap(), &host), BindingProcess::Unverified);
        assert!(occurrence_pid_is_live_with(&unparseable, 4242, &host));
        // Unknown boot time: only the process checks apply.
        let started = |_: u32| Some(at("2026-10-12T06:45:00Z"));
        let no_boot = HostProbe { boot_time: None, running: &running, started: &started };
        let old = record(4242, "2026-10-12T05:00:00Z");
        assert_eq!(binding_process(old.app_server_runtime.as_ref().unwrap(), &no_boot), BindingProcess::Reused);
    }
}
