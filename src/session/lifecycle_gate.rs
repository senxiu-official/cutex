//! Shared with Management retirement, restoration, fork and storage cleanup.
//! Callers take this gate before the durable session-store file lock.
use std::sync::OnceLock;
use crate::management::v2::session_coordination::{OperationLane, OperationReservation};

// Retain the global logical barrier for whole-store lifecycle/cleanup ordering,
// but never retain a physical mutex while a caller reads/writes or stops a child.
pub static MANAGEMENT_V2_SESSION_MUTATION_LOCK: OnceLock<OperationLane> = OnceLock::new();

pub fn lock() -> anyhow::Result<OperationReservation<'static>> {
    MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(OperationLane::new)
        .lock()
        .map_err(|_| anyhow::anyhow!("management v2 session mutation lock poisoned"))
}
#[cfg(test)]
pub(crate) fn try_lock_for_cleanup() -> anyhow::Result<OperationReservation<'static>> {
    MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(OperationLane::new)
        .try_lock()
        .map_err(|_| anyhow::anyhow!("session mutation busy; cleanup deferred"))
}
