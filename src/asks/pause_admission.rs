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

/// Whether an error is (or wraps) a deferred admission: the host authority
/// fence was busy, nothing was decided and nothing was sent. Owner-facing
/// routes report it as the retryable `admission_busy` code (reason
/// `hostBusy`), never as a pause, persistence failure or runtime refusal.
pub fn is_deferred(error:&anyhow::Error)->bool {
    error.chain().any(|cause|cause.is::<Deferred>())
}

/// Wire contract of a deferred owner admission (`owner-admission-busy-v1`).
pub const ADMISSION_BUSY_CODE:&str="admission_busy";
pub const HOST_BUSY_REASON:&str="hostBusy";
pub const ADMISSION_BUSY_RETRY_AFTER_MS:u64=1000;

thread_local! {
    /// The host authority fence held by the owner admission worker on this
    /// thread (r271). While set, admission on this thread runs under it
    /// instead of trying the fence again; it is dropped before native IO.
    static WORKER_FENCE: std::cell::RefCell<Option<crate::platform::lock_accounting::FileGuard>> = const { std::cell::RefCell::new(None) };
}

/// Owner admission worker only: wait (FIFO with every other blocking waiter)
/// for the host authority fence and keep it on this thread. The caller must
/// hold no session lane (debug-checked by `lock_mutations`).
/// Accounted (step-0 lock accounting, `Lock::Authority`, blocking) at the
/// worker's call site; the whole hold is one record.
#[track_caller]
pub fn acquire_worker_fence()->anyhow::Result<()> {
    if worker_fence_held() {return Ok(());}
    let fence=AgentManagementStore::open_default()?.lock_mutations()?;
    WORKER_FENCE.with(|held|*held.borrow_mut()=Some(fence));
    Ok(())
}

/// Drop this thread's worker fence (no-op elsewhere). Called right after the
/// owner input's re-check and durable reservation, before any native call.
pub fn release_worker_fence()->bool {
    WORKER_FENCE.with(|held|held.borrow_mut().take().is_some())
}

pub fn worker_fence_held()->bool {
    WORKER_FENCE.with(|held|held.borrow().is_some())
}

/// Hot admission never waits behind lifecycle: lifecycle may be joining this
/// very bridge worker. No role-action maintenance/pruning belongs on this path.
pub fn try_with_authority<T>(run:impl FnOnce(&AgentManagementSnapshot)->anyhow::Result<T>)->anyhow::Result<Admission<T>> {
    let roster=AgentManagementStore::open_default()?;
    // Under the owner admission worker's fence this succeeds (store.rs).
    let Some(_fence)=roster.try_lock_delivery_mutations()? else {return Ok(Admission::Deferred)};
    let snapshot=roster.directory_snapshot_shared()?;
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

/// [`gate`] that rides out a brief authority-fence hold: retries the
/// non-blocking try for at most ~0.5 s (never blocks on the fence, so a
/// lifecycle action holding it still defers). A pause or error is final at
/// once. Used by every one-shot submit/resume check (ACP r266 B3, extended
/// to native/Claude next-step and resume in r273 stab).
pub fn gate_bounded(store:&AskStore,session:&str)->anyhow::Result<Gate> {
    const TRIES: u32 = 25;
    const STEP: std::time::Duration = std::time::Duration::from_millis(20);
    for attempt in 1..=TRIES {
        let gate = gate(store, session)?;
        if attempt < TRIES && gate == Gate::Deferred {
            std::thread::sleep(STEP);
            continue;
        }
        return Ok(gate);
    }
    unreachable!("the last attempt returns")
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
