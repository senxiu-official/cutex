//! Read-time projections of the committed roster (058 S4): the current
//! durable names/configuration overlay and the reversible-archive view.
//!
//! The projected snapshot is cached per roster directory. When only the
//! roster changed, the overlay is re-applied to the agents whose rows changed
//! (the persistent-map diff skips everything else); the whole overlay is
//! rebuilt only when the session store or the pending-forget set changed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::roster_map::{Delta, RosterMap};
use super::{AgentArchiveStage, AgentManagementError, AgentManagementSnapshot, ManagedAgentRecord};
use crate::role_revision::CutexSessionId;
use crate::session::model::CutexSessionStore;

/// Applies the durable session record to one agent and returns whether that
/// session is retired.
pub(super) type ProjectAgent =
    fn(&mut ManagedAgentRecord, &CutexSessionStore) -> Result<bool, AgentManagementError>;

#[derive(Clone)]
struct Entry {
    root: PathBuf,
    names: Option<PathBuf>,
    raw: Arc<AgentManagementSnapshot>,
    deleted: Arc<BTreeSet<String>>,
    sessions: Option<Arc<CutexSessionStore>>,
    projected: AgentManagementSnapshot,
}

static CACHE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
const ENTRIES: usize = 8;

fn cached(root: &Path, names: Option<&Path>) -> Option<Entry> {
    let cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    cache.iter().find(|entry| entry.root == root && entry.names.as_deref() == names).cloned()
}

fn remember(entry: Entry) {
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    cache.retain(|old| !(old.root == entry.root && old.names == entry.names));
    if cache.len() >= ENTRIES {
        cache.remove(0);
    }
    cache.push(entry);
}

fn same_sessions(a: &Option<Arc<CutexSessionStore>>, b: &Option<Arc<CutexSessionStore>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// Isolated stores without a session store: committed archive receipts
/// still suppress ordinary lifecycle/name reservations.
pub(super) fn with_archive_projection(
    root: &Path,
    raw: Arc<AgentManagementSnapshot>,
) -> AgentManagementSnapshot {
    let previous = cached(root, None);
    if let Some(entry) = &previous {
        if Arc::ptr_eq(&entry.raw, &raw) {
            return entry.projected.clone();
        }
    }
    let mut projected = (*raw).clone();
    projected.reversible_archive_projection = match &previous {
        Some(entry) if entry.raw.agent_archive_actions.ptr_eq(&raw.agent_archive_actions) => {
            entry.projected.reversible_archive_projection.clone()
        }
        _ => {
            let mut receipts = raw
                .agent_archive_actions
                .values()
                .filter(|r| r.stage == AgentArchiveStage::Committed)
                .collect::<Vec<_>>();
            receipts.sort_by_key(|r| r.result.as_ref().map(|v| v.durable_revision()).unwrap_or(0));
            receipts
                .into_iter()
                .filter_map(|r| r.result.as_ref())
                .filter_map(|r| {
                    CutexSessionId::new(r.cutex_session_id.clone()).ok().map(|id| (id, r.is_retired()))
                })
                .collect()
        }
    };
    remember(Entry {
        root: root.to_path_buf(),
        names: None,
        raw,
        deleted: Arc::default(),
        sessions: None,
        projected: projected.clone(),
    });
    projected
}

/// The roster as the current durable session records describe it.
pub(super) fn with_current_names(
    root: &Path,
    names: &Path,
    raw: Arc<AgentManagementSnapshot>,
    deleted: Arc<BTreeSet<String>>,
    sessions: Arc<CutexSessionStore>,
    project: ProjectAgent,
) -> Result<AgentManagementSnapshot, AgentManagementError> {
    let sessions = Some(sessions);
    let store = sessions.as_deref().expect("set above");
    let previous = cached(root, Some(names))
        .filter(|entry| Arc::ptr_eq(&entry.deleted, &deleted) && same_sessions(&entry.sessions, &sessions));
    let mut projected = (*raw).clone();
    // r271: a deleted session whose roster row awaits cleanup is gone from
    // this view (instead of failing every reader host-wide).
    for id in deleted.iter() {
        if let Ok(id) = CutexSessionId::new(id.clone()) {
            projected.current_project_memberships.remove(&id);
        }
    }
    match &previous {
        Some(entry) if Arc::ptr_eq(&entry.raw, &raw) => return Ok(entry.projected.clone()),
        Some(entry) => {
            let mut agents = entry.projected.agents.clone();
            let mut retired = entry.projected.reversible_archive_projection.clone();
            for delta in raw.agents.diff(&entry.raw.agents) {
                match delta {
                    Delta::Put(id, _) if deleted.contains(id.as_str()) => {}
                    Delta::Put(id, agent) => {
                        let mut agent = agent.clone();
                        retired.insert(id.clone(), project(&mut agent, store)?);
                        agents.insert(id.clone(), agent);
                    }
                    Delta::Delete(id) => {
                        agents.remove(id);
                        retired.remove(id);
                    }
                }
            }
            projected.agents = agents;
            projected.reversible_archive_projection = retired;
        }
        None => {
            for id in deleted.iter() {
                if let Ok(id) = CutexSessionId::new(id.clone()) {
                    projected.agents.remove(&id);
                }
            }
            let mut retired = RosterMap::new();
            let ids: Vec<CutexSessionId> = projected.agents.keys().cloned().collect();
            for id in ids {
                // Store a copy only when the overlay changed it, so unchanged
                // rows keep sharing the committed generation's nodes.
                let mut agent = projected.agents[&id].clone();
                retired.insert(id.clone(), project(&mut agent, store)?);
                if projected.agents.get(&id) != Some(&agent) {
                    projected.agents.insert(id, agent);
                }
            }
            projected.reversible_archive_projection = retired;
        }
    }
    remember(Entry {
        root: root.to_path_buf(),
        names: Some(names.to_path_buf()),
        raw,
        deleted,
        sessions,
        projected: projected.clone(),
    });
    Ok(projected)
}
