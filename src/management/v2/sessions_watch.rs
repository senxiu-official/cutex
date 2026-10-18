//! Host event `cutex/sessions/changed`: the agent list or project membership
//! changed, whichever process wrote it.
//!
//! Each process publishes Management v2 events only to its own live
//! subscribers (`EventRepository::publish`); events another process appends
//! to the shared journal reach `/v2/events/stream` clients only when they
//! page again (reconnect). Agent Management `agent_create` / `member_replace`
//! / `agent_archive` run inside `cutex agent serve`, so their session-store
//! and project writes, their `cutex/agentManagement/*` facts and the first
//! events of the runtime that process launched never reached a connected
//! desktop live: a new agent showed up in the sidebar only after some
//! unrelated live event (often the Director's turn end) made it reload.
//!
//! Management serve therefore watches the durable session store and the
//! Agent Management store (like the alarms watcher) and publishes one host
//! event under [`HOST_EVENT_SESSION_ID`] when a session appeared, disappeared,
//! was archived / restored / renamed / regrouped, or the project store
//! changed. Params: `{added, removed, changed, projectsChanged, hostId}`.
//! Runtime state and timestamps are not watched: their own events cover them.
//!
//! Claude runtime pins (`session-claude-runtime`): a session whose pinned
//! release or pending repin (`pending-deployment.json` in its state dir,
//! written by a deferred repin, removed when it applies) changed is listed in
//! `changed`; a change of the host's selected Claude runtime
//! (`runtime/claude/deployment.json`) adds `claudeRuntimeChanged: true`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::session::model::{CutexSessionRecord, CutexSessionStore};

use super::services::HOST_EVENT_SESSION_ID;

pub const SESSIONS_CHANGED_EVENT: &str = "cutex/sessions/changed";
pub const CAPABILITY: &str = "sessions-changed-event";
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// What of one session the sidebar shows (membership, archive state, names,
/// groups). Anything else has its own event.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    archive_state: String,
    formal_agent_name: Option<String>,
    display_name_override: Option<String>,
    groups: Vec<String>,
    /// The pinned Claude runtime's release (Claude sessions only).
    claude_release: Option<String>,
}

impl Fingerprint {
    fn of(record: &CutexSessionRecord) -> Self {
        Self {
            archive_state: serde_json::to_value(record.archive_state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            formal_agent_name: record.formal_agent_name.clone(),
            display_name_override: record.display_name_override.clone(),
            groups: record.agent_groups.clone(),
            claude_release: record
                .claude_session
                .as_ref()
                .map(|session| super::claude_runtime_api::release_of(&session.deployment)),
        }
    }
}

fn fingerprints(store: &CutexSessionStore) -> BTreeMap<String, Fingerprint> {
    store
        .sessions
        .values()
        .map(|record| (record.cutex_session_id.clone(), Fingerprint::of(record)))
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Changes {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
    projects_changed: bool,
    claude_runtime_changed: bool,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && !self.projects_changed
            && !self.claude_runtime_changed
    }
}

fn diff(before: &BTreeMap<String, Fingerprint>, after: &BTreeMap<String, Fingerprint>) -> Changes {
    let mut changes = Changes::default();
    for (id, now) in after {
        match before.get(id) {
            None => changes.added.push(id.clone()),
            Some(old) if old != now => changes.changed.push(id.clone()),
            Some(_) => {}
        }
    }
    changes.removed = before.keys().filter(|id| !after.contains_key(*id)).cloned().collect();
    changes
}

type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

/// Stamps of the Claude runtime files: the host's selected deployment and
/// each Claude session's pending repin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ClaudeStamps {
    pub(super) selected: Stamp,
    pub(super) pending: BTreeMap<String, Stamp>,
}

pub(super) fn claude_stamps(store: &CutexSessionStore) -> ClaudeStamps {
    ClaudeStamps {
        selected: crate::runtime::claude::deployment_path().ok().and_then(|path| stamp(&path)),
        pending: store
            .sessions
            .values()
            .filter_map(|record| {
                let session = record.claude_session.as_ref()?;
                let path = session.state_dir.join(crate::runtime::claude_pin::PENDING_FILE);
                Some((record.cutex_session_id.clone(), stamp(&path)))
            })
            .collect(),
    }
}

#[derive(Default)]
struct Watch {
    primed: bool,
    /// The snapshot last compared; an unchanged store hands out the same one.
    store: Option<Arc<CutexSessionStore>>,
    sessions: BTreeMap<String, Fingerprint>,
    projects: Stamp,
    claude: ClaudeStamps,
}

/// One watcher step on the current session store snapshot, project store
/// stamp and Claude runtime stamps. The first step only primes.
fn step(watch: &mut Watch, store: Arc<CutexSessionStore>, projects: Stamp, claude: ClaudeStamps) -> Changes {
    let projects_changed = watch.primed && projects != watch.projects;
    watch.projects = projects;
    let claude_runtime_changed = watch.primed && claude.selected != watch.claude.selected;
    // A pending repin written or removed: the session's resource changed
    // (sessions that appeared are reported as added).
    let pending_changed = if watch.primed {
        claude
            .pending
            .iter()
            .filter(|(id, now)| watch.claude.pending.get(*id).is_some_and(|before| before != *now))
            .map(|(id, _)| id.clone())
            .collect()
    } else {
        Vec::new()
    };
    watch.claude = claude;
    let same_store = watch.store.as_ref().is_some_and(|known| Arc::ptr_eq(known, &store));
    let mut changes = Changes::default();
    if !same_store {
        let now = fingerprints(&store);
        if watch.primed {
            changes = diff(&watch.sessions, &now);
        }
        watch.sessions = now;
        watch.store = Some(store);
    }
    changes.projects_changed = projects_changed;
    changes.claude_runtime_changed = claude_runtime_changed;
    for id in pending_changed {
        if !changes.changed.contains(&id) && !changes.added.contains(&id) {
            changes.changed.push(id);
        }
    }
    changes.changed.sort();
    watch.primed = true;
    changes
}

pub(crate) fn event_params(changes: &Changes, host_id: &str) -> Value {
    let mut params = json!({
        "added": changes.added,
        "removed": changes.removed,
        "changed": changes.changed,
        "projectsChanged": changes.projects_changed,
        "hostId": host_id,
    });
    // Only when set, so the usual event keeps its earlier shape.
    if changes.claude_runtime_changed {
        params["claudeRuntimeChanged"] = json!(true);
    }
    params
}

fn projects_store_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?
        .join("agent-management")
        .join("v1")
        .join("agent-management-v1.json"))
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

fn poll() -> anyhow::Result<()> {
    let store = crate::session::store::load_cutex_session_store_shared()?;
    let projects = stamp(&projects_store_path()?);
    let claude = claude_stamps(&store);
    let changes = {
        let mut guard = WATCH.lock().unwrap_or_else(|e| e.into_inner());
        step(guard.get_or_insert_with(Watch::default), store, projects, claude)
    };
    if changes.is_empty() {
        return Ok(());
    }
    let repository = super::repository::management_v2_repository()?;
    super::server::append_cutex_event(
        repository,
        HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        SESSIONS_CHANGED_EVENT,
        event_params(&changes, &crate::platform::host::current_host_name()),
    )
}

/// Management serve: watch the session and project stores for
/// `cutex/sessions/changed`.
pub fn start_watcher() -> anyhow::Result<()> {
    std::thread::Builder::new().name("cutex-sessions-watch".into()).spawn(|| {
        let mut failing = false;
        loop {
            match poll() {
                Ok(()) => failing = false,
                Err(error) => {
                    if !failing {
                        eprintln!("sessions watcher: {error:#}");
                    }
                    failing = true;
                }
            }
            std::thread::sleep(WATCH_INTERVAL);
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, name: &str) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            id.to_string(),
            None,
            crate::platform::host::current_host_name(),
            "/tmp".to_string(),
            None,
            "2026-10-15T00:00:00Z".to_string(),
        )
        .expect("session fixture");
        record.formal_agent_name = Some(name.to_string());
        record
    }

    fn store(records: &[CutexSessionRecord]) -> Arc<CutexSessionStore> {
        let mut store = CutexSessionStore::default();
        for record in records {
            store.sessions.insert(record.cutex_session_id.clone(), record.clone());
        }
        Arc::new(store)
    }

    #[test]
    fn primes_then_reports_added_removed_and_changed_sessions() {
        let mut watch = Watch::default();
        let director = record("cutex.d", "director");
        let first = store(&[director.clone()]);
        assert!(step(&mut watch, first.clone(), None, ClaudeStamps::default()).is_empty(), "first step only primes");
        assert!(step(&mut watch, first, None, ClaudeStamps::default()).is_empty(), "same snapshot");

        // agent_create in another process: the new session appears.
        let worker = record("cutex.w", "worker");
        let created = step(&mut watch, store(&[director.clone(), worker.clone()]), None, ClaudeStamps::default());
        assert_eq!(created.added, vec!["cutex.w".to_string()]);
        assert!(created.removed.is_empty() && created.changed.is_empty() && !created.projects_changed);

        // Runtime-only updates (timestamps, generation) are not reported.
        let mut busy = worker.clone();
        busy.updated_at = "2026-10-15T01:00:00Z".into();
        busy.runtime_generation += 1;
        assert!(step(&mut watch, store(&[director.clone(), busy.clone()]), None, ClaudeStamps::default()).is_empty());

        let mut grouped = busy.clone();
        grouped.agent_groups.push("project:p".into());
        let regrouped = step(&mut watch, store(&[director.clone(), grouped]), None, ClaudeStamps::default());
        assert_eq!(regrouped.changed, vec!["cutex.w".to_string()]);

        let removed = step(&mut watch, store(&[director]), None, ClaudeStamps::default());
        assert_eq!(removed.removed, vec!["cutex.w".to_string()]);
    }

    #[test]
    fn project_store_writes_are_reported() {
        let mut watch = Watch::default();
        let snapshot = store(&[record("cutex.d", "director")]);
        let at = |secs| Some((SystemTime::UNIX_EPOCH + Duration::from_secs(secs), 10));
        assert!(step(&mut watch, snapshot.clone(), at(1), ClaudeStamps::default()).is_empty());
        assert!(step(&mut watch, snapshot.clone(), at(1), ClaudeStamps::default()).is_empty());
        let changes = step(&mut watch, snapshot, at(2), ClaudeStamps::default());
        assert!(changes.projects_changed && changes.added.is_empty());
        let params = event_params(&changes, "host");
        assert_eq!(params["projectsChanged"], json!(true));
        assert_eq!(params["hostId"], json!("host"));
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": SESSIONS_CHANGED_EVENT,
            "params": params,
        }))
        .expect("event matches the method schema");
    }

    #[test]
    fn claude_pins_pending_repins_and_the_selected_runtime_are_reported() {
        let mut watch = Watch::default();
        let at = |secs| Some((SystemTime::UNIX_EPOCH + Duration::from_secs(secs), 10));
        let claude = |selected: Stamp, pending: Stamp| ClaudeStamps {
            selected,
            pending: [("cutex.c".to_string(), pending)].into(),
        };
        let snapshot = store(&[record("cutex.c", "claude")]);
        assert!(step(&mut watch, snapshot.clone(), None, claude(at(1), None)).is_empty());
        assert!(step(&mut watch, snapshot.clone(), None, claude(at(1), None)).is_empty());

        // A deferred repin writes the pending file; applying it removes it.
        let deferred = step(&mut watch, snapshot.clone(), None, claude(at(1), at(5)));
        assert_eq!(deferred.changed, vec!["cutex.c".to_string()]);
        assert!(!deferred.claude_runtime_changed);
        assert_eq!(step(&mut watch, snapshot.clone(), None, claude(at(1), None)).changed, vec!["cutex.c".to_string()]);

        // `cutex human claude install`: the selected runtime changed.
        let installed = step(&mut watch, snapshot.clone(), None, claude(at(2), None));
        assert!(installed.claude_runtime_changed && installed.changed.is_empty());
        let params = event_params(&installed, "host");
        assert_eq!(params["claudeRuntimeChanged"], json!(true));
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": SESSIONS_CHANGED_EVENT,
            "params": params,
        }))
        .expect("event matches the method schema");
        assert!(event_params(&deferred, "host").get("claudeRuntimeChanged").is_none());
    }

    #[test]
    fn schema_refuses_unknown_params() {
        assert!(super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": SESSIONS_CHANGED_EVENT,
            "params": {"added": [], "removed": [], "changed": [], "projectsChanged": false, "hostId": "h", "extra": 1},
        }))
        .is_err());
    }

    /// The bug: Agent Management runs in the Agent Bus process, whose journal
    /// appends never reach Management's live subscribers. The watcher turns
    /// the resulting store write into a live host event.
    #[test]
    fn a_session_created_by_another_process_reaches_live_subscribers() {
        use super::super::repository::{management_v2_repository, EventRepository, ReplayQuery};
        use super::super::model::{CutexMessage, EventCorrelation, EventSource, PendingEvent};

        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        *WATCH.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let host = crate::platform::host::current_host_name();
        let director = record("cutex.00000000-0000-0000-0000-0000000000d1", "director");
        let mut before = CutexSessionStore::default();
        before.sessions.insert(director.cutex_session_id.clone(), director.clone());
        crate::session::store::save_cutex_session_store(&before).expect("seed store");

        let repository = management_v2_repository().expect("repository");
        poll().expect("prime");
        let subscription = repository
            .page_and_subscribe(ReplayQuery { limit: 1000, ..ReplayQuery::default() }, 64)
            .expect("subscribe");

        // `cutex agent serve`: its own repository instance on the same journal.
        let agent_bus = EventRepository::open(repository.root(), host.clone()).expect("second process");
        agent_bus
            .append(PendingEvent {
                cutex_session_id: director.cutex_session_id.clone(),
                host_id: host.clone(),
                source: EventSource::Cutex,
                schema: None,
                correlation: EventCorrelation::default(),
                native: None,
                cutex: Some(CutexMessage {
                    method: "cutex/test/otherProcess".into(),
                    params: json!({}),
                }),
            })
            .expect("append from the other process");
        let worker = record("cutex.00000000-0000-0000-0000-0000000000a1", "worker");
        let mut after = before.clone();
        after.sessions.insert(worker.cutex_session_id.clone(), worker.clone());
        crate::session::store::save_cutex_session_store(&after).expect("adopt");
        assert!(
            subscription.receiver.recv_timeout(Duration::from_millis(50)).is_err(),
            "appends of another process are not published live"
        );

        poll().expect("watch");
        let event = subscription.receiver.recv_timeout(Duration::from_secs(1)).expect("live host event");
        assert_eq!(event.cutex_session_id, HOST_EVENT_SESSION_ID);
        let message = event.cutex.expect("cutex message");
        assert_eq!(message.method, SESSIONS_CHANGED_EVENT);
        assert_eq!(message.params["added"], json!([worker.cutex_session_id]));
        poll().expect("unchanged");
        assert!(subscription.receiver.recv_timeout(Duration::from_millis(50)).is_err(), "nothing changed");
    }
}
