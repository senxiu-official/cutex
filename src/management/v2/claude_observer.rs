//! Process-local observer scheduling and owner-visible diagnostics. No native probes here.
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub generation: u64,
    pub pid: u32,
    pub connection_id: Option<String>,
}

/// Snapshot only eligible local claims from the cached store. The caller
/// batches PID liveness checks after this read, without opening adapters.
pub fn candidate_identities<'a>(
    records: impl Iterator<Item = &'a crate::session::model::CutexSessionRecord>,
    host: &str,
) -> BTreeMap<String, RuntimeIdentity> {
    records
        .filter_map(|record| {
            if record.is_retired()
                || !crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, host)
            {
                return None;
            }
            Some((
                record.cutex_session_id.clone(),
                RuntimeIdentity {
                    generation: record.runtime_generation,
                    pid: record.runtime_pid?,
                    connection_id: record.claude_session.as_ref()?.connection_id.clone(),
                },
            ))
        })
        .collect()
}

struct Entry {
    identity: RuntimeIdentity,
    active: bool,
    suspended: bool,
    observing: bool,
    attempts: u64,
    failures: u64,
    consecutive: u32,
    retry_at: Instant,
    changed_at: Instant,
    error: Option<(String, String)>,
}

#[derive(Default)]
pub struct ObserverState {
    entries: BTreeMap<String, Entry>,
}

impl ObserverState {
    /// Called after a successful candidate scan. Absent-session history is
    /// bounded; current suspended claims retain their occurrence fence.
    pub fn sync_candidates(
        &mut self,
        candidates: &BTreeMap<String, RuntimeIdentity>,
        now: Instant,
    ) -> Vec<String> {
        let mut dropped = Vec::new();
        for (id, entry) in &mut self.entries {
            if entry.active && !candidates.contains_key(id) {
                entry.active = false;
                entry.changed_at = now;
                dropped.push(id.clone());
            }
        }
        for (id, identity) in candidates {
            if !self
                .entries
                .get(id)
                .is_some_and(|e| e.suspended && &e.identity == identity)
            {
                self.activate(id, identity.clone(), now);
            }
        }
        let mut inactive: Vec<_> = self
            .entries
            .iter()
            .filter(|(id, e)| !e.active && !candidates.contains_key(*id))
            .map(|(id, e)| (e.changed_at, id.clone()))
            .collect();
        inactive.sort();
        let excess = inactive.len().saturating_sub(512);
        for (_, id) in inactive.into_iter().take(excess) {
            self.entries.remove(&id);
        }
        dropped
    }

    pub fn activate(&mut self, id: &str, identity: RuntimeIdentity, now: Instant) {
        let entry = self.entries.entry(id.to_string()).or_insert_with(|| Entry {
            identity: identity.clone(),
            active: true,
            suspended: false,
            observing: false,
            attempts: 0,
            failures: 0,
            consecutive: 0,
            retry_at: now,
            changed_at: now,
            error: None,
        });
        if !entry.active || entry.identity != identity {
            entry.identity = identity;
            entry.active = true;
            entry.suspended = false;
            entry.observing = false;
            entry.consecutive = 0;
            entry.retry_at = now;
            entry.changed_at = now;
            entry.error = None;
        }
    }

    pub fn due(&self, id: &str, now: Instant) -> bool {
        self.entries
            .get(id)
            .is_some_and(|e| e.active && now >= e.retry_at)
    }
    pub fn suspend(&mut self, id: &str, identity: &RuntimeIdentity, now: Instant) -> bool {
        let Some(e) = self.entries.get_mut(id) else {
            return false;
        };
        if !e.active || &e.identity != identity {
            return false;
        }
        e.active = false;
        e.suspended = true;
        e.observing = false;
        e.changed_at = now;
        true
    }
    pub fn attempted(&mut self, id: &str) {
        if let Some(e) = self.entries.get_mut(id) {
            e.attempts += 1;
        }
    }
    /// A slow result from an old occurrence must not overwrite a newer start.
    pub fn complete(
        &mut self,
        id: &str,
        identity: &RuntimeIdentity,
        failure: Option<(&str, &str)>,
        now: Instant,
    ) -> bool {
        if !self
            .entries
            .get(id)
            .is_some_and(|e| e.active && &e.identity == identity)
        {
            return false;
        }
        match failure {
            Some((stage, error)) => self.failed(id, stage, error, now),
            None => self.success(id, now),
        }
    }
    pub fn success(&mut self, id: &str, now: Instant) -> bool {
        let Some(e) = self.entries.get_mut(id) else {
            return false;
        };
        let changed = e.error.take().is_some();
        e.observing = true;
        e.consecutive = 0;
        e.retry_at = now + Duration::from_secs(5);
        if changed {
            e.changed_at = now;
        }
        changed
    }
    /// Return true only when the failure state changes, not on every retry.
    pub fn failed(&mut self, id: &str, stage: &str, error: &str, now: Instant) -> bool {
        let Some(e) = self.entries.get_mut(id) else {
            return false;
        };
        let failure = (stage.to_string(), error.to_string());
        let changed = e.error.as_ref() != Some(&failure);
        e.error = Some(failure);
        e.observing = false;
        e.failures += 1;
        e.consecutive = e.consecutive.saturating_add(1);
        let seconds = (5u64 << e.consecutive.saturating_sub(1).min(6)).min(300);
        e.retry_at = now + Duration::from_secs(seconds);
        if changed {
            e.changed_at = now;
        }
        changed
    }
    pub fn snapshot(&self, now: Instant) -> Value {
        json!({"sessions": self.entries.iter().map(|(id,e)| json!({
            "cutexSessionId": id, "runtimeGeneration": e.identity.generation,
            "state": if !e.active { "inactive" } else if e.error.is_some() { "backoff" } else if e.observing { "observing" } else { "pending" },
            "attempts": e.attempts, "failures": e.failures, "consecutiveFailures": e.consecutive,
            "retryAfterMs": if e.active { e.retry_at.saturating_duration_since(now).as_millis() as u64 } else { 0 },
            "lastErrorStage": e.error.as_ref().map(|e| &e.0),
            "lastError": e.error.as_ref().map(|e| &e.1),
            "inactiveReason": if e.suspended { Some("native_occurrence_offline") } else if !e.active { Some("no_local_live_runtime") } else { None },
        })).collect::<Vec<_>>()})
    }
}

pub fn with_state<T>(f: impl FnOnce(&mut ObserverState) -> T) -> T {
    static STATE: OnceLock<Mutex<ObserverState>> = OnceLock::new();
    let mut state = STATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f(&mut state)
}
pub fn diagnostics() -> Value {
    with_state(|state| state.snapshot(Instant::now()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(generation: u64) -> RuntimeIdentity {
        RuntimeIdentity {
            generation,
            pid: 123,
            connection_id: Some(format!("c{generation}")),
        }
    }
    #[test]
    fn observer_candidates_exclude_retired_pidless_remote_and_non_claude_records() {
        use crate::session::model::{CutexSessionArchiveState, CutexSessionRecord};
        let mut record = CutexSessionRecord::new_at(
            "cutex.observer-test".into(),
            None,
            "local".into(),
            "/unused".into(),
            None,
            "2026-10-25T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_pid = Some(123);
        record.claude_session = Some(serde_json::from_value(json!({
            "deployment":{"python":"/unused","adapter_source":"/unused","claude_cli":"/unused","claude_home":"/unused","tui_binary":"/unused"},
            "thread_id":"test-thread","state_dir":"/unused","model":"test"
        })).unwrap());
        let candidates = |r: &CutexSessionRecord| candidate_identities(std::iter::once(r), "local");
        assert_eq!(candidates(&record).len(), 1);
        record.archive_state = CutexSessionArchiveState::Retired;
        assert!(candidates(&record).is_empty());
        record.archive_state = CutexSessionArchiveState::Active;
        record.runtime_pid = None;
        assert!(candidates(&record).is_empty());
        record.runtime_pid = Some(123);
        record.host_id = "other-host".into();
        assert!(candidates(&record).is_empty());
        record.host_id = "local".into();
        record.claude_session = None;
        assert!(candidates(&record).is_empty());
    }
    #[test]
    fn observer_backoff_caps_and_logs_only_state_changes() {
        let mut state = ObserverState::default();
        let mut now = Instant::now();
        state.activate("a", identity(1), now);
        for (index, seconds) in [5, 10, 20, 40, 80, 160, 300, 300].into_iter().enumerate() {
            assert!(state.due("a", now));
            state.attempted("a");
            assert_eq!(
                state.failed("a", "registration", "unavailable", now),
                index == 0
            );
            assert!(!state.due("a", now + Duration::from_secs(seconds - 1)));
            now += Duration::from_secs(seconds);
        }
        assert_eq!(state.snapshot(now)["sessions"][0]["failures"], 8);
        assert!(state.failed("a", "observation", "offline", now));
        assert!(state.success("a", now));
        assert!(!state.success("a", now));
        assert_eq!(state.snapshot(now)["sessions"][0]["consecutiveFailures"], 0);
    }
    #[test]
    fn observer_drop_restart_and_identity_changes_reset_only_that_session() {
        let mut state = ObserverState::default();
        let now = Instant::now();
        let mut candidates = BTreeMap::from([("a".into(), identity(1)), ("b".into(), identity(1))]);
        state.sync_candidates(&candidates, now);
        state.attempted("a");
        state.failed("a", "observe", "offline", now);
        assert!(state.due("b", now));
        candidates.remove("a");
        assert_eq!(state.sync_candidates(&candidates, now), ["a"]);
        assert!(!state.due("a", now + Duration::from_secs(1000)));
        candidates.insert("a".into(), identity(1));
        state.sync_candidates(&candidates, now);
        assert!(state.due("a", now));
        state.failed("a", "observe", "offline", now);
        candidates.insert("a".into(), identity(2));
        state.sync_candidates(&candidates, now);
        assert!(state.due("a", now));
        let row = &state.snapshot(now)["sessions"][0];
        assert_eq!(row["attempts"], 1);
        assert_eq!(row["failures"], 2);
        assert_eq!(row["runtimeGeneration"], 2);
        assert_eq!(row["consecutiveFailures"], 0);
    }
    #[test]
    fn observer_pid_and_connection_changes_wake_without_generation_change() {
        let mut state = ObserverState::default();
        let now = Instant::now();
        let mut occurrence = identity(1);
        state.activate("a", occurrence.clone(), now);
        for change_pid in [true, false] {
            state.failed("a", "observe", "unavailable", now);
            assert!(!state.due("a", now));
            if change_pid {
                occurrence.pid += 1;
            } else {
                occurrence.connection_id = Some("replacement".into());
            }
            state.sync_candidates(&BTreeMap::from([("a".into(), occurrence.clone())]), now);
            assert!(state.due("a", now));
            assert_eq!(state.snapshot(now)["sessions"][0]["state"], "pending");
        }
    }
    #[test]
    fn observer_late_result_cannot_delay_restarted_occurrence() {
        let mut state = ObserverState::default();
        let now = Instant::now();
        state.activate("a", identity(1), now);
        state.attempted("a");
        state.activate("a", identity(2), now);
        assert!(!state.complete("a", &identity(1), Some(("observe", "old failure")), now));
        assert!(state.due("a", now));
        assert_eq!(state.snapshot(now)["sessions"][0]["failures"], 0);
        state.complete("a", &identity(2), Some(("observe", "new failure")), now);
        assert!(!state.complete("a", &identity(1), None, now));
        assert!(!state.due("a", now));
        state.sync_candidates(&BTreeMap::new(), now);
        assert!(!state.complete("a", &identity(2), Some(("observe", "after drop")), now));
        assert_eq!(state.snapshot(now)["sessions"][0]["failures"], 1);
    }
    #[test]
    fn observer_confirmed_offline_stays_dropped_even_with_stale_cached_claim() {
        let mut state = ObserverState::default();
        let now = Instant::now();
        let mut candidates = BTreeMap::from([("a".into(), identity(1))]);
        state.sync_candidates(&candidates, now);
        state.complete("a", &identity(1), Some(("observe", "offline")), now);
        assert!(state.suspend("a", &identity(1), now));
        for seconds in [5, 300, 1000] {
            let later = now + Duration::from_secs(seconds);
            state.sync_candidates(&candidates, later);
            assert!(!state.due("a", later));
        }
        assert_eq!(state.snapshot(now)["sessions"][0]["state"], "inactive");
        // An explicit Start may re-observe immediately even before the next scan.
        state.activate("a", identity(1), now);
        assert!(state.due("a", now));
        state.suspend("a", &identity(1), now);
        candidates.insert("a".into(), identity(2));
        state.sync_candidates(&candidates, now);
        assert!(state.due("a", now));
    }
    #[test]
    fn observer_dropped_history_is_bounded() {
        let mut state = ObserverState::default();
        let now = Instant::now();
        for n in 0..600 {
            state.activate(&n.to_string(), identity(1), now);
        }
        state.sync_candidates(&BTreeMap::new(), now);
        assert_eq!(
            state.snapshot(now)["sessions"].as_array().unwrap().len(),
            512
        );
    }
}
