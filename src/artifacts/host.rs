//! What publishing needs from the running host: the store, settings, a
//! session's paths, a project's workspace root, the event stream and the
//! owner notice on a full store. The Agent Bus process (MCP tools) queues
//! events as asks-store notices, which the management tick publishes; the
//! management process (routes, auto capture) publishes directly.

use std::path::PathBuf;

use serde_json::Value;

use super::store::ArtifactStore;
use crate::config::artifacts::ArtifactSettings;

/// Kind of the owner notice when the store is full (system notices fall
/// back to `system.other` in the notification level tables).
pub const QUOTA_NOTICE_KIND: &str = "artifactsQuotaFull";
/// Meta key: the owner was told the store is full (cleared by the next
/// publish that fits).
pub const QUOTA_NOTIFIED_KEY: &str = "quotaNotifiedAt";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPaths {
    pub cwd: Option<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
}

pub trait ArtifactHost {
    fn store(&self) -> anyhow::Result<ArtifactStore>;
    fn settings(&self) -> ArtifactSettings;
    fn session_paths(&self, session: &str) -> SessionPaths;
    fn project_root(&self, project: &str) -> Option<PathBuf>;
    /// The project a session belongs to (member or Director seat).
    fn session_project(&self, session: &str) -> Option<String>;
    /// Publish `cutex/artifact/*` on the host event stream.
    fn emit(&self, method: &str, params: Value);
    /// The store is full (called once per full episode).
    fn quota_full(&self, session: Option<&str>, name: &str, used: u64, quota: u64);
}

/// A session's cwd and writable roots from the session store.
pub fn session_paths(session: &str) -> SessionPaths {
    crate::session::store::with_cutex_session_store(|store| {
        store
            .sessions
            .get(session)
            .or_else(|| store.sessions.values().find(|record| record.cutex_session_id == session))
            .map(|record| SessionPaths {
                cwd: Some(record.cwd.trim()).filter(|cwd| !cwd.is_empty()).map(PathBuf::from),
                writable_roots: record
                    .sandbox_writable_roots
                    .iter()
                    .map(|root| root.trim())
                    .filter(|root| !root.is_empty())
                    .map(PathBuf::from)
                    .collect(),
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// A project's workspace root from the Agent Management store.
pub fn project_root(project: &str) -> Option<PathBuf> {
    let snapshot = crate::memory::load_agent_management_snapshot().ok()?;
    let id = crate::agent_management::ProjectId::new(project).ok()?;
    snapshot
        .project_states
        .get(&id)
        .and_then(|state| state.workspace_root.as_deref())
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
}

/// The project a session belongs to (member or Director seat).
pub fn session_project(session: &str) -> Option<String> {
    let snapshot = crate::memory::load_agent_management_snapshot().ok()?;
    crate::memory::project_for_session(&snapshot, session).map(|project| project.as_str().to_string())
}

/// Record the owner notice; `publish` = also push it now (management
/// process only; the Agent Bus process leaves it to the management tick).
fn record_quota_notice(session: Option<&str>, name: &str, used: u64, quota: u64, publish: bool) {
    use crate::owner_notify::store::{Level, NewNotification, Origin, OwnerNotifyStore};
    if cfg!(test) {
        return;
    }
    let (settings, evaluator) = crate::notification_levels::Evaluator::current_with_owner();
    let Ok(store) = OwnerNotifyStore::open_default() else {
        return;
    };
    let gib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let text = format!(
        "The artifact store is full ({:.2} of {:.2} GiB): \"{}\" was not published. Delete or archive old artifacts, or raise artifacts.quotaBytes in Settings.",
        gib(used),
        gib(quota),
        crate::asks::truncate_chars(name, 120),
    );
    let new = NewNotification {
        agent_id: session.unwrap_or(crate::management::v2::services::HOST_EVENT_SESSION_ID).to_string(),
        agent_name: None,
        text: crate::asks::truncate_chars(&text, crate::owner_notify::store::MAX_TEXT_CHARS),
        level: Level::Info,
        needs_reply: false,
        delegation: None,
        job: None,
        turn_id: None,
    };
    let created = store.with_state(|state| {
        state.create_decided(new, Origin::System, Some(QUOTA_NOTICE_KIND), &settings, &evaluator, None, chrono::Utc::now())
    });
    if let Err(error) = created {
        eprintln!("artifacts: could not notify the owner: {error:#}");
        return;
    }
    if publish {
        crate::management::v2::owner_notify::publish_pending_notices(&store);
    }
}

/// The Agent Bus process (MCP tools).
pub struct BusHost<'a> {
    pub asks: &'a crate::asks::store::AskStore,
}

impl ArtifactHost for BusHost<'_> {
    fn store(&self) -> anyhow::Result<ArtifactStore> {
        ArtifactStore::open_default()
    }
    fn settings(&self) -> ArtifactSettings {
        crate::config::artifacts::current()
    }
    fn session_paths(&self, session: &str) -> SessionPaths {
        session_paths(session)
    }
    fn project_root(&self, project: &str) -> Option<PathBuf> {
        project_root(project)
    }
    fn session_project(&self, session: &str) -> Option<String> {
        session_project(session)
    }
    fn emit(&self, method: &str, params: Value) {
        let queued = self.asks.with_state(|state| {
            state.notices.push(crate::asks::store::Notice {
                method: method.to_string(),
                sessions: vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()],
                params,
            })
        });
        if let Err(error) = queued {
            eprintln!("artifacts: could not queue {method}: {error:#}");
        }
    }
    fn quota_full(&self, session: Option<&str>, name: &str, used: u64, quota: u64) {
        record_quota_notice(session, name, used, quota, false);
    }
}

/// The management process (Management v2 routes, auto capture).
pub struct ManagementHost;

impl ArtifactHost for ManagementHost {
    fn store(&self) -> anyhow::Result<ArtifactStore> {
        ArtifactStore::open_default()
    }
    fn settings(&self) -> ArtifactSettings {
        crate::config::artifacts::current()
    }
    fn session_paths(&self, session: &str) -> SessionPaths {
        session_paths(session)
    }
    fn project_root(&self, project: &str) -> Option<PathBuf> {
        project_root(project)
    }
    fn session_project(&self, session: &str) -> Option<String> {
        session_project(session)
    }
    fn emit(&self, method: &str, params: Value) {
        if cfg!(test) {
            return;
        }
        crate::management::v2::documents::publish_host_event(method, params);
    }
    fn quota_full(&self, session: Option<&str>, name: &str, used: u64, quota: u64) {
        record_quota_notice(session, name, used, quota, true);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A host on a temporary store with fixed paths; events and quota
    /// notices are collected.
    pub struct TestHost {
        pub dir: PathBuf,
        pub settings: RefCell<ArtifactSettings>,
        pub paths: RefCell<HashMap<String, SessionPaths>>,
        pub roots: RefCell<HashMap<String, PathBuf>>,
        pub projects: RefCell<HashMap<String, String>>,
        pub events: RefCell<Vec<(String, Value)>>,
        pub quota_notices: RefCell<Vec<(Option<String>, String)>>,
    }

    impl TestHost {
        pub fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cutex-artifact-host-{label}-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let dir = super::super::paths::canonical(&dir).unwrap();
            Self {
                dir,
                settings: RefCell::new(ArtifactSettings::default()),
                paths: RefCell::default(),
                roots: RefCell::default(),
                projects: RefCell::default(),
                events: RefCell::default(),
                quota_notices: RefCell::default(),
            }
        }

        pub fn workspace(&self, name: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        pub fn set_session(&self, session: &str, cwd: &std::path::Path) {
            self.paths.borrow_mut().insert(session.into(), SessionPaths { cwd: Some(cwd.to_path_buf()), writable_roots: Vec::new() });
        }

        pub fn set_project_root(&self, project: &str, root: &std::path::Path) {
            self.roots.borrow_mut().insert(project.into(), root.to_path_buf());
        }

        pub fn methods(&self) -> Vec<String> {
            self.events.borrow().iter().map(|(method, _)| method.clone()).collect()
        }
    }

    impl Drop for TestHost {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl ArtifactHost for TestHost {
        fn store(&self) -> anyhow::Result<ArtifactStore> {
            Ok(ArtifactStore::at(self.dir.join("store")))
        }
        fn settings(&self) -> ArtifactSettings {
            self.settings.borrow().clone()
        }
        fn session_paths(&self, session: &str) -> SessionPaths {
            self.paths.borrow().get(session).cloned().unwrap_or_default()
        }
        fn project_root(&self, project: &str) -> Option<PathBuf> {
            self.roots.borrow().get(project).cloned()
        }
        fn session_project(&self, session: &str) -> Option<String> {
            self.projects.borrow().get(session).cloned()
        }
        fn emit(&self, method: &str, params: Value) {
            crate::management::v2::contract_validation::validate_cutex_event_message(&serde_json::json!({
                "method": method, "params": params
            }))
            .unwrap_or_else(|error| panic!("{method} does not match the schema: {error:?}"));
            self.events.borrow_mut().push((method.to_string(), params));
        }
        fn quota_full(&self, session: Option<&str>, name: &str, _used: u64, _quota: u64) {
            self.quota_notices.borrow_mut().push((session.map(str::to_string), name.to_string()));
        }
    }
}
