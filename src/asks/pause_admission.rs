//! Current project pause membership at existing admission reservations.
//!
//! Lock order: session operation lane (where required) -> roster authority
//! fence -> AskStore -> existing subordinate repositories. No runtime IO in
//! these callbacks. Predicates over AskState remain pure.
use crate::agent_management::{AgentManagementSnapshot, AgentManagementStore};
use super::store::{AskState, AskStore};

#[derive(Debug, PartialEq, Eq)]
pub enum Admission<T> { Ready(T), Deferred }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate { Open, Paused, Deferred }

#[derive(Debug)]
pub struct Deferred;
impl std::fmt::Display for Deferred {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { f.write_str("admission_deferred: lifecycle authority fence busy") }
}
impl std::error::Error for Deferred {}

/// Hot admission never waits behind lifecycle: lifecycle may be joining this
/// very bridge worker. No role-action maintenance/pruning belongs on this path.
pub fn try_with_authority<T>(run:impl FnOnce(&AgentManagementSnapshot)->anyhow::Result<T>)->anyhow::Result<Admission<T>> {
    let roster=AgentManagementStore::open_default()?;
    let Some(_fence)=roster.try_lock_delivery_mutations()? else {return Ok(Admission::Deferred)};
    let snapshot=roster.snapshot()?;
    crate::director_address::with_authority_snapshot(&snapshot,||run(&snapshot)).map(Admission::Ready)
}

pub fn try_with_state<T>(store:&AskStore,run:impl FnOnce(&mut AskState,&AgentManagementSnapshot)->T)->anyhow::Result<Admission<T>> {
    try_with_authority(|snapshot|with_state_in(store,snapshot,run))
}

/// Production boundary. Never infer that a fence is unnecessary from a prior
/// AskState read: a pause or a project join can commit before the reservation.
pub fn with_state<T>(
    store: &AskStore,
    run: impl FnOnce(&mut AskState, &AgentManagementSnapshot) -> T,
) -> anyhow::Result<T> {
    match try_with_state(store,run)? {
        Admission::Ready(result)=>Ok(result), Admission::Deferred=>Err(Deferred.into()),
    }
}

pub fn with_state_or<T>(store:&AskStore,deferred:T,run:impl FnOnce(&mut AskState,&AgentManagementSnapshot)->T)->anyhow::Result<T> {
    match try_with_state(store,run)? {Admission::Ready(result)=>Ok(result),Admission::Deferred=>Ok(deferred)}
}

/// The caller already holds the roster mutation fence. This function never
/// acquires it recursively and does not read a roster/Directory under AskStore.
pub fn with_state_in<T>(
    store: &AskStore,
    snapshot: &AgentManagementSnapshot,
    run: impl FnOnce(&mut AskState, &AgentManagementSnapshot) -> T,
) -> anyhow::Result<T> {
    store.with_state(|state| {
        reconcile_in(state, snapshot);
        run(state, snapshot)
    })
}

/// Pure reconciliation over the same protected snapshot as routing checks.
/// Preserve each pause's scope, exclusions and sticky history independently.
pub fn reconcile_in(state: &mut AskState, snapshot: &AgentManagementSnapshot) {
    let projects: Vec<_> = state.project_control.projects.values()
        .filter(|p| p.pause_id.is_some()).map(|p| p.project_id.clone()).collect();
    for project in projects {
        let membership = crate::management::v2::delegations::membership(snapshot, &project);
        super::project_control::reconcile(state, &project, &membership);
    }
}

/// Explicit IO boundary for callers that only need a fresh gate. Final writes
/// should use with_state so this check and their durable claim cannot diverge.
pub fn gate(store:&AskStore,session:&str)->anyhow::Result<Gate> {
    Ok(match try_with_authority(|roster|store.inspect(|state|is_paused_in(state,roster,session)))? {
        Admission::Ready(true)=>Gate::Paused, Admission::Ready(false)=>Gate::Open, Admission::Deferred=>Gate::Deferred,
    })
}

/// Caller supplies a current snapshot protected by its authority guard. No IO,
/// mutation, or whole-state copy; unlike persisted predicates, sees new joins
/// and current role exclusions before the first durable reconciliation.
pub fn is_paused_in(state:&AskState,roster:&AgentManagementSnapshot,session:&str)->bool {
    state.presence.get(session).is_some_and(|p|p.agent_pause.is_some())
        || super::project_control::is_paused_with_roster(state,session,roster)
}

/// A transient deferred admission is a hold, not a persisted project pause.
pub fn is_held(store:&AskStore,session:&str)->anyhow::Result<bool> {
    Ok(gate(store,session)?!=Gate::Open)
}

/// Reserve using the caller's existing state/identity. A held result still
/// persists membership reconciliation; it never synthesizes a global pause.
pub fn unless_paused<T>(
    store: &AskStore,
    session: &str,
    reserve: impl FnOnce(&mut AskState) -> T,
) -> anyhow::Result<Option<T>> {
    with_state_or(store,None, |state, _| {
        (!super::agent_pause::is_paused(state, session)).then(|| reserve(state))
    })
}

#[cfg(test)]
mod tests;
