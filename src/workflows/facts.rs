//! [`Facts`] from the profile, session and Agent Management stores (used by
//! both the management service and the Agent Bus tool path).

use super::engine::{AuthorFacts, Facts, ProfileFact};
use super::model::RunError;
use super::store::WorkflowStore;

pub struct LibraryFacts {
    /// The workflow store root (step-session check).
    pub store_root: Option<std::path::PathBuf>,
}

impl LibraryFacts {
    pub fn new() -> Self {
        Self { store_root: WorkflowStore::default_root().ok() }
    }
}

impl Default for LibraryFacts {
    fn default() -> Self {
        Self::new()
    }
}

impl Facts for LibraryFacts {
    fn profile_facts(&self, names: &[String]) -> Result<Vec<ProfileFact>, RunError> {
        let store = crate::profiles::settings_api::load_store()
            .map_err(|e| RunError { code: "profiles_unavailable".into(), message: format!("{e:#}") })?;
        names
            .iter()
            .map(|name| {
                let account = crate::profiles::lookup::find_account(&store, name)
                    .ok()
                    .flatten()
                    .ok_or_else(|| RunError { code: "unknown_profile".into(), message: format!("profile \"{name}\" does not exist") })?;
                let runtime = match account.cli_kind {
                    crate::profiles::model::CliKind::Claude => "claude",
                    _ => "codex",
                };
                Ok(ProfileFact { name: account.name.clone(), runtime: runtime.into() })
            })
            .collect()
    }

    fn author_facts(&self, session_id: &str) -> Option<AuthorFacts> {
        let record = crate::session::store::with_cutex_session_store(|store| {
            store
                .sessions
                .get(session_id)
                .or_else(|| store.sessions.values().find(|record| record.cutex_session_id == session_id))
                .cloned()
        })
        .ok()
        .flatten()?;
        let world = crate::asks::scheduler::HostWorld::load();
        let info = crate::asks::scheduler::SchedulerWorld::directory(&world)
            .ok()
            .and_then(|directory| directory.resolve(None, &record.cutex_session_id).ok().cloned());
        let is_step_session = self
            .store_root
            .as_ref()
            .filter(|root| root.join("runs.sqlite3").exists())
            .and_then(|root| WorkflowStore::open(root).ok())
            .and_then(|store| store.is_step_session(&record.cutex_session_id).ok())
            .unwrap_or(false);
        Some(AuthorFacts {
            name: info.as_ref().and_then(|i| i.name.clone()),
            profile: record.profile.clone().or_else(|| info.as_ref().and_then(|i| i.profile.clone())),
            cwd: record.cwd.clone(),
            project_id: info.as_ref().and_then(|i| i.project.clone()),
            is_step_session,
        })
    }
}
