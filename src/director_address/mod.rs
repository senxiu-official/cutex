//! Additive Director role addressing and immutable delegation ownership provenance.
use crate::asks::{
    model::{AskRecord, DeliveryRecord, DeliveryState},
    store::{AskError, AskState},
};
pub mod management;
mod store;
pub use store::*;
mod authority;
mod read_view;
pub(crate) use read_view::ReadView;
pub(crate) use authority::invalidate as invalidate_authority_cache;
pub(crate) use authority::with_snapshot as with_authority_snapshot;
mod handover;
pub use handover::*;
mod resolution;
mod pruning;
pub use resolution::*;
pub mod api;
pub use api::{read_start, replacement_hint, review_handover, review_start, reviews_for};
#[cfg(test)]
mod tests;

pub fn controller(ask: &AskRecord) -> Result<Option<String>, AskError> {
    let state = Store::open_default()?.snapshot()?;
    controller_in(ask, &state, |project| Ok(authority_for_project(project)?.1))
}

/// Shared live-control checks. A read projection supplies a request-local
/// authority resolver; mutations continue to resolve the current authority.
fn controller_in(
    ask: &AskRecord,
    state: &OwnershipState,
    director: impl FnOnce(&str) -> Result<String, AskError>,
) -> Result<Option<String>, AskError> {
    let Some(ownership) = state.asks.get(&ask.ask_id) else {
        return Ok(None);
    };
    verify_digest(ask, ownership)?;
    if ownership.owner_hold {
        return Err(AskError::new(
            "handover_owner_review",
            "Project work is held for an explicit owner decision",
        ));
    }
    if ownership.pending_reconcile {
        return Err(AskError::new(
            "handover_pending_reconcile",
            "Director handover is awaiting reconciliation",
        ));
    }
    let current = director(&ownership.project_id)?;
    if ownership.controller != current {
        return Err(AskError::new(
            "handover_pending_review",
            "Current Director has not accepted this delegation",
        ));
    }
    Ok(Some(ownership.controller.clone()))
}
pub fn is_controller(ask: &AskRecord, caller: &str) -> bool {
    match controller(ask) {
        Ok(Some(id)) => id == caller,
        Ok(None) => ask.requester.session() == Some(caller),
        Err(_) => false,
    }
}
pub fn can_read(ask: &AskRecord, caller: &str) -> bool {
    if ask.requester.session() == Some(caller) {
        return true;
    }
    let Ok(state) = Store::open_default().and_then(|store| store.snapshot()) else {
        return false;
    };
    state.asks.get(&ask.ask_id).is_some_and(|ownership| {
        verify_digest(ask, ownership).is_ok() && ownership.history.iter().any(|id| id == caller)
    })
}
pub fn stamp(ask: &AskRecord, caller: &str, scope: &str) -> Result<(), AskError> {
    if scope == "session" {
        return Ok(());
    }
    if scope != "projectDirector" {
        return Err(AskError::new(
            "invalid_ownership_scope",
            "ownershipScope must be session or projectDirector",
        ));
    }
    if ask.requester.session() != Some(caller) {
        return Err(AskError::new(
            "not_requester",
            "Only the authenticated requester can stamp a delegation",
        ));
    }
    let snapshot = crate::memory::load_agent_management_snapshot().map_err(store::unavailable)?;
    let projects = crate::asks::directory::Directory::director_projects(&snapshot, caller);
    if projects.len() != 1 {
        return Err(AskError::new(
            "director_project_ambiguous",
            "Project ownership requires one unambiguous current Director project",
        ));
    }
    let authority = snapshot
        .projects
        .values()
        .find(|p| {
            p.project_id.as_str() == projects[0] && p.authorized_director_session.as_str() == caller
        })
        .ok_or_else(|| {
            AskError::new(
                "not_director",
                "Project ownership requires the current Director of the sender's project",
            )
        })?;
    Store::open_default()?.stamp_at(
        ask,
        caller,
        authority.project_id.as_str(),
        authority.authority_epoch,
    )
}
pub fn route_questions(ask: &AskRecord, project: Option<&str>) -> Result<(), AskError> {
    let project = project.ok_or_else(|| {
        AskError::new(
            "director_project_missing",
            "Director questions require an explicit project context",
        )
    })?;
    authority_for_project(project)?;
    Store::open_default()?.update(|state| {
        if let Some(old) = state.question_projects.get(&ask.ask_id) {
            if old != project {
                return Err(AskError::new(
                    "action_conflict",
                    "Question Director project is already pinned",
                ));
            }
        }
        state
            .question_projects
            .insert(ask.ask_id.clone(), project.to_owned());
        Ok(())
    })
}
pub fn delivery_recipient(state: &AskState, delivery: &DeliveryRecord) -> Option<String> {
    if delivery.state != DeliveryState::Pending {
        return Some(
            delivery
                .routed_to
                .clone()
                .unwrap_or_else(|| delivery.requester_cutex_session_id.clone()),
        );
    }
    let ask = state.asks.get(&delivery.ask_id)?;
    controller(ask)
        .ok()
        .map(|id| id.unwrap_or_else(|| delivery.requester_cutex_session_id.clone()))
}
pub fn question_recipient(ask: &AskRecord) -> Result<Option<String>, AskError> {
    if let Some(project) = Store::open_default()?
        .snapshot()?
        .question_projects
        .get(&ask.ask_id)
    {
        return Ok(Some(authority_for_project(project)?.1));
    }
    Ok(controller(ask)?.or_else(|| ask.requester.session().map(str::to_owned)))
}

fn verify_digest(ask: &AskRecord, ownership: &Ownership) -> Result<(), AskError> {
    if ask.request_sha256 != ownership.request_sha256
        || ask.requester.session() != Some(ownership.origin_director.as_str())
    {
        return Err(AskError::new(
            "director_provenance_invalid",
            "Delegation ownership provenance does not match the immutable request",
        ));
    }
    Ok(())
}
pub fn authority_for_project(project: &str) -> Result<(String, String, u64), AskError> {
    authority::get(project)
}
pub fn project_director_state(
    snapshot: &crate::agent_management::AgentManagementSnapshot,
    project: &str,
) -> Result<(Option<String>, u64), AskError> {
    let id = crate::agent_management::ProjectId::new(project)
        .map_err(|_| AskError::new("project_not_found", "Unknown project"))?;
    if !crate::memory::project_is_known(snapshot, &id) {
        return Err(AskError::new("project_not_found", "Unknown project"));
    }
    Ok(match snapshot.projects.get(&id) {
        Some(a) => (
            Some(a.authorized_director_session.as_str().into()),
            a.authority_epoch,
        ),
        None => (
            None,
            snapshot
                .project_states
                .get(&id)
                .and_then(|p| p.last_director_epoch)
                .unwrap_or(0),
        ),
    })
}

impl Store {
    /// Display-only metadata for terminal work. It never grants live control
    /// or reopens a task under an earlier Director's authority.
    pub fn closed_controller(&self, ask: &AskRecord) -> Result<Option<String>, AskError> {
        if ask.is_open() {
            return Ok(None);
        }
        let state = self.snapshot()?;
        let Some(ownership) = state.asks.get(&ask.ask_id) else {
            return Ok(None);
        };
        verify_digest(ask, ownership)?;
        Ok(Some(ownership.controller.clone()))
    }
    pub fn stamp_at(
        &self,
        ask: &AskRecord,
        caller: &str,
        project: &str,
        epoch: u64,
    ) -> Result<(), AskError> {
        if ask.requester.session() != Some(caller) {
            return Err(AskError::new(
                "not_requester",
                "Ownership origin must be the immutable requester",
            ));
        }
        self.update(|state| {
            if let Some(old) = state.asks.get(&ask.ask_id) {
                verify_digest(ask, old)?;
                if old.project_id != project || old.origin_director != caller {
                    return Err(AskError::new(
                        "action_conflict",
                        "Ownership origin is already pinned",
                    ));
                }
                return Ok(());
            }
            state.asks.insert(
                ask.ask_id.clone(),
                Ownership {
                    ask_id: ask.ask_id.clone(),
                    project_id: project.into(),
                    origin_director: caller.into(),
                    origin_epoch: epoch,
                    request_sha256: ask.request_sha256.clone(),
                    controller: caller.into(),
                    controller_revision: 0,
                    history: vec![caller.into()],
                    changes: vec![],
                    pending_reconcile: false,
                    owner_hold: false,
                    summary_pending: false,
                    summary_from: None,
                },
            );
            Ok(())
        })
    }
}
