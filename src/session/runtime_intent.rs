//! Durable runtime online intent and restore candidates after an unclean stop
//! (capability `runtime-restore-candidates`).
//!
//! Linux host, 2026-10-13: after a power loss nothing said which agents had been
//! online, so the owner brought ~20 back by hand. This store keeps the owner's
//! intent: an agent is added when its runtime goes online (owner, CLI, auto
//! wake) and removed only on a requested `cutex/runtime/offline` / `close`
//! (or archive / delete). Process death and host shutdown never clear it.
//!
//! On Management startup, after dead runtimes were reconciled, every intended
//! agent that is no longer online (and not archived) becomes a restore
//! candidate: the previous stop was unclean (host reboot, crash, killed
//! service). `GET /v2/runtimes/restore-candidates` lists them; the owner
//! restores them through the ordinary `cutex/runtime/online` or dismisses the
//! list (`DELETE`), which also drops the dismissed agents' intent. Nothing is
//! started automatically.
//!
//! File: `<cutex runtime dir>/runtime-intent/online.json`, created on the
//! first write. Only Management writes it (one process; a process-local lock
//! orders writers).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::session::model::{CutexSessionRecord, CutexSessionStore};

pub const PATH: &str = "/v2/runtimes/restore-candidates";
pub const CAPABILITY: &str = "runtime-restore-candidates";

static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineIntent {
    pub last_online_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UncleanStop {
    /// When a Management start found intended runtimes offline.
    pub detected_at: String,
    /// `host_rebooted` (boot time changed since the last start) or
    /// `runtimes_exited` (same boot: Management or the runtimes died).
    pub reason: String,
    pub candidates: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub intents: BTreeMap<String, OnlineIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unclean: Option<UncleanStop>,
    /// Host boot time seen by the last Management start (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_time: Option<String>,
}

pub fn store_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("runtime-intent").join("online.json"))
}

fn load_at(path: &Path) -> anyhow::Result<IntentState> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(IntentState::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_at(path: &Path, state: &IntentState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::profiles::materialize::create_private_dir_all(parent)?;
    }
    let mut state = state.clone();
    state.version = 1;
    crate::config::atomic::write_private_pretty_json_atomic(path, &state, "runtime online intent")
}

/// Read-modify-write under the process lock. A missing file stays missing
/// when `update` reports no change.
fn update_at<T>(path: &Path, update: impl FnOnce(&mut IntentState) -> (bool, T)) -> anyhow::Result<T> {
    let _guard = LOCK.lock().map_err(|_| anyhow::anyhow!("runtime intent lock poisoned"))?;
    let mut state = load_at(path)?;
    let (changed, value) = update(&mut state);
    if changed {
        save_at(path, &state)?;
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Intent

pub fn note_online_at(path: &Path, cutex_session_id: &str, now: &str) -> anyhow::Result<()> {
    update_at(path, |state| {
        state.intents.insert(cutex_session_id.to_string(), OnlineIntent { last_online_at: now.to_string() });
        (true, ())
    })
}

pub fn note_offline_at(path: &Path, cutex_session_id: &str) -> anyhow::Result<()> {
    update_at(path, |state| {
        let mut changed = state.intents.remove(cutex_session_id).is_some();
        if let Some(unclean) = &mut state.unclean {
            let before = unclean.candidates.len();
            unclean.candidates.retain(|id| id != cutex_session_id);
            changed |= unclean.candidates.len() != before;
        }
        (changed, ())
    })
}

/// The current intent store (empty when it does not exist yet).
pub fn load() -> anyhow::Result<IntentState> {
    let path = store_path()?;
    let _guard = LOCK.lock().map_err(|_| anyhow::anyhow!("runtime intent lock poisoned"))?;
    load_at(&path)
}

/// Best effort: the runtime went online (never fails the request).
pub fn note_online(cutex_session_id: &str) {
    let result = store_path().and_then(|path| note_online_at(&path, cutex_session_id, &chrono::Utc::now().to_rfc3339()));
    if let Err(error) = result {
        eprintln!("runtime intent: could not record {cutex_session_id} online: {error:#}");
    }
}

/// Best effort: an offline / close / archive was requested.
pub fn note_offline(cutex_session_id: &str) {
    let result = store_path().and_then(|path| note_offline_at(&path, cutex_session_id));
    if let Err(error) = result {
        eprintln!("runtime intent: could not record {cutex_session_id} offline: {error:#}");
    }
}

// ---------------------------------------------------------------------------
// Startup detection

/// Whether the record's runtime counts as online now: a binding whose
/// process is not proven gone (previous boot, exited, reused PID).
pub fn record_online(record: &CutexSessionRecord) -> bool {
    record.app_server_runtime.is_some()
        && !crate::session::boot_fence::with_host_probe(|host| crate::session::boot_fence::binding_is_proven_gone(record, host))
}

fn candidate_ids(state: &IntentState, sessions: &CutexSessionStore, online: &dyn Fn(&CutexSessionRecord) -> bool) -> Vec<String> {
    state
        .intents
        .keys()
        .filter(|id| sessions.sessions.get(*id).is_some_and(|record| !crate::agent_management::agent_execution_blocked(&record) && !online(record)))
        .cloned()
        .collect()
}

/// Management startup (after dead runtimes were reconciled): record an
/// unclean stop when intended runtimes are offline. Candidates of an earlier,
/// undismissed unclean stop are kept. Returns the candidate count.
pub fn detect_unclean_stop_at(
    path: &Path,
    sessions: &CutexSessionStore,
    online: &dyn Fn(&CutexSessionRecord) -> bool,
    boot_time: Option<&str>,
    now: &str,
) -> anyhow::Result<usize> {
    update_at(path, |state| {
        let boot_changed = matches!((state.boot_time.as_deref(), boot_time), (Some(previous), Some(current)) if !same_boot(previous, current));
        let mut changed = boot_time.is_some() && state.boot_time.as_deref() != boot_time;
        if let Some(boot_time) = boot_time {
            state.boot_time = Some(boot_time.to_string());
        }
        // Agents archived or deleted since are no longer intended.
        let before = state.intents.len();
        state.intents.retain(|id, _| sessions.sessions.get(id).is_some_and(|record| !crate::agent_management::agent_execution_blocked(&record)));
        changed |= state.intents.len() != before;
        let fresh = candidate_ids(state, sessions, online);
        if fresh.is_empty() {
            return (changed, state.unclean.as_ref().map_or(0, |u| u.candidates.len()));
        }
        let reason = if boot_changed { "host_rebooted" } else { "runtimes_exited" };
        let unclean = state.unclean.get_or_insert_with(|| UncleanStop {
            detected_at: now.to_string(),
            reason: reason.to_string(),
            candidates: Vec::new(),
        });
        let mut all: BTreeSet<String> = unclean.candidates.iter().cloned().collect();
        all.extend(fresh);
        unclean.candidates = all.into_iter().collect();
        if boot_changed {
            unclean.reason = reason.to_string();
        }
        (true, unclean.candidates.len())
    })
}

/// Boot times are derived from uptime and may jitter by a second or two.
fn same_boot(previous: &str, current: &str) -> bool {
    match (chrono::DateTime::parse_from_rfc3339(previous), chrono::DateTime::parse_from_rfc3339(current)) {
        (Ok(previous), Ok(current)) => (previous - current).num_seconds().abs() <= 30,
        _ => previous == current,
    }
}

/// Management startup with the real host and session store (best effort).
pub fn detect_unclean_stop_at_startup() {
    let result = (|| -> anyhow::Result<usize> {
        let sessions = crate::session::store::load_cutex_session_store()?;
        let boot = crate::platform::process::system_boot_time().ok().map(|time| time.to_rfc3339());
        detect_unclean_stop_at(&store_path()?, &sessions, &record_online, boot.as_deref(), &chrono::Utc::now().to_rfc3339())
    })();
    match result {
        Ok(0) => {}
        Ok(count) => println!("runtime restore: {count} agent(s) were online before an unclean stop"),
        Err(error) => eprintln!("runtime restore detection warning: {error:#}"),
    }
}

// ---------------------------------------------------------------------------
// Resource

/// `GET /v2/runtimes/restore-candidates`. Candidates that are online again,
/// archived, deleted or no longer intended are left out.
pub fn candidates_resource_at(
    path: &Path,
    sessions: &CutexSessionStore,
    online: &dyn Fn(&CutexSessionRecord) -> bool,
    host_id: &str,
) -> anyhow::Result<Value> {
    let state = {
        let _guard = LOCK.lock().map_err(|_| anyhow::anyhow!("runtime intent lock poisoned"))?;
        load_at(path)?
    };
    let Some(unclean) = &state.unclean else {
        return Ok(json!({"hostId": host_id, "uncleanSince": null, "reason": null, "items": []}));
    };
    let items: Vec<Value> = unclean
        .candidates
        .iter()
        .filter_map(|id| {
            let intent = state.intents.get(id)?;
            let record = sessions.sessions.get(id)?;
            (!crate::agent_management::agent_execution_blocked(&record) && !online(record)).then(|| {
                json!({
                    "cutexSessionId": id,
                    "displayName": crate::session::metadata::cutex_session_display_name(record),
                    "lastOnlineAt": intent.last_online_at,
                })
            })
        })
        .collect();
    Ok(json!({
        "hostId": host_id,
        "uncleanSince": if items.is_empty() { Value::Null } else { json!(unclean.detected_at) },
        "reason": if items.is_empty() { Value::Null } else { json!(unclean.reason) },
        "items": items,
    }))
}

/// `DELETE /v2/runtimes/restore-candidates`: forget the unclean stop and the
/// intent of every listed agent still offline (the owner chose not to
/// restore them). Returns how many were dismissed.
pub fn dismiss_at(
    path: &Path,
    sessions: &CutexSessionStore,
    online: &dyn Fn(&CutexSessionRecord) -> bool,
) -> anyhow::Result<usize> {
    update_at(path, |state| {
        let Some(unclean) = state.unclean.take() else {
            return (false, 0);
        };
        let mut dismissed = 0;
        for id in &unclean.candidates {
            let offline = sessions.sessions.get(id).is_none_or(|record| crate::agent_management::agent_execution_blocked(&record) || !online(record));
            if offline && state.intents.remove(id).is_some() {
                dismissed += 1;
            }
        }
        (true, dismissed)
    })
}

pub fn is_path(path: &str) -> bool {
    path == PATH
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(ids: &[&str]) -> CutexSessionStore {
        let mut sessions = CutexSessionStore::default();
        for id in ids {
            let record = CutexSessionRecord::new(
                (*id).into(),
                Some(uuid::Uuid::new_v4().to_string()),
                "private".into(),
                "/private".into(),
                None,
            )
            .unwrap();
            sessions.sessions.insert((*id).into(), record);
        }
        sessions
    }

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("cutex-runtime-intent-{}", uuid::Uuid::new_v4())).join("online.json")
    }

    #[test]
    fn requested_offline_clears_intent_but_process_death_does_not() {
        let path = temp();
        let sessions = store(&["cutex.a", "cutex.b", "cutex.c"]);
        note_online_at(&path, "cutex.a", "2026-10-13T01:00:00Z").unwrap();
        note_online_at(&path, "cutex.b", "2026-10-13T01:05:00Z").unwrap();
        note_online_at(&path, "cutex.c", "2026-10-13T01:06:00Z").unwrap();
        note_offline_at(&path, "cutex.c").unwrap();
        // First start ever records the boot time only... and finds a and b
        // offline (they died with the host).
        let offline = |_: &CutexSessionRecord| false;
        assert_eq!(detect_unclean_stop_at(&path, &sessions, &offline, Some("2026-10-13T00:00:00Z"), "2026-10-13T02:00:00Z").unwrap(), 2);
        let body = candidates_resource_at(&path, &sessions, &offline, "devbox").unwrap();
        assert_eq!(body["uncleanSince"], "2026-10-13T02:00:00Z");
        assert_eq!(body["reason"], "runtimes_exited");
        let ids: Vec<_> = body["items"].as_array().unwrap().iter().map(|i| i["cutexSessionId"].as_str().unwrap()).collect();
        assert_eq!(ids, ["cutex.a", "cutex.b"]);
        assert_eq!(body["items"][1]["lastOnlineAt"], "2026-10-13T01:05:00Z");
        assert!(body["items"][0]["displayName"].is_string());
        // a is restored: it leaves the list; b is still offline.
        let a_online = |record: &CutexSessionRecord| record.cutex_session_id == "cutex.a";
        let body = candidates_resource_at(&path, &sessions, &a_online, "devbox").unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        // Dismiss drops b's intent; a (online) keeps its intent.
        assert_eq!(dismiss_at(&path, &sessions, &a_online).unwrap(), 1);
        let state = load_at(&path).unwrap();
        assert!(state.unclean.is_none());
        assert_eq!(state.intents.keys().collect::<Vec<_>>(), ["cutex.a"]);
        let body = candidates_resource_at(&path, &sessions, &a_online, "devbox").unwrap();
        assert_eq!(body, json!({"hostId":"devbox","uncleanSince":null,"reason":null,"items":[]}));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn clean_start_records_nothing_and_reboot_is_reported() {
        let path = temp();
        let sessions = store(&["cutex.a"]);
        let online = |_: &CutexSessionRecord| true;
        // No intent, nothing to write.
        assert_eq!(detect_unclean_stop_at(&path, &sessions, &online, None, "t0").unwrap(), 0);
        assert!(!path.exists());
        note_online_at(&path, "cutex.a", "t1").unwrap();
        assert_eq!(detect_unclean_stop_at(&path, &sessions, &online, Some("2026-10-12T00:00:00Z"), "t2").unwrap(), 0);
        assert!(load_at(&path).unwrap().unclean.is_none());
        // Reboot: new boot time, the runtime is gone.
        let offline = |_: &CutexSessionRecord| false;
        assert_eq!(detect_unclean_stop_at(&path, &sessions, &offline, Some("2026-10-13T00:00:00Z"), "t3").unwrap(), 1);
        let unclean = load_at(&path).unwrap().unclean.unwrap();
        assert_eq!((unclean.reason.as_str(), unclean.detected_at.as_str()), ("host_rebooted", "t3"));
        // A requested offline removes the candidate too.
        note_offline_at(&path, "cutex.a").unwrap();
        assert_eq!(candidates_resource_at(&path, &sessions, &offline, "h").unwrap()["items"], json!([]));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn archived_and_deleted_agents_are_never_candidates() {
        let path = temp();
        let mut sessions = store(&["cutex.a", "cutex.b"]);
        note_online_at(&path, "cutex.a", "t1").unwrap();
        note_online_at(&path, "cutex.b", "t1").unwrap();
        note_online_at(&path, "cutex.gone", "t1").unwrap();
        sessions.sessions.get_mut("cutex.b").unwrap().archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        let offline = |_: &CutexSessionRecord| false;
        assert_eq!(detect_unclean_stop_at(&path, &sessions, &offline, None, "t2").unwrap(), 1);
        assert_eq!(load_at(&path).unwrap().intents.keys().collect::<Vec<_>>(), ["cutex.a"]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
