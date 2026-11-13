//! Disposable per-session metadata follows the durable lifecycle, including
//! writes by the Agent Bus process. No native IO while a cache/watch lock lives.
use std::collections::BTreeSet;

pub(super) fn shrink<K: Eq + std::hash::Hash, V>(map: &mut std::collections::HashMap<K, V>) {
    if map.capacity() > map.len().saturating_mul(4) {
        map.shrink_to(map.len().saturating_mul(2));
    }
}

pub(super) fn may_retain(session: &str) -> bool {
    retain_state(crate::session::store::shared_session_row(session).map(|row| row.map(|record| record.is_active())))
}

fn retain_state<E>(state: Result<Option<bool>, E>) -> bool {
    match state {
        Ok(Some(active)) => active,
        // Unit-level native fixtures predate durable registration. Production
        // never admits unknown/deleted identities to long-lived caches.
        Ok(None) => cfg!(test),
        // An unavailable catalog is not evidence of a lifecycle transition.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn catalog_error_preserves_cache_until_lifecycle_is_known() {
        assert!(super::retain_state::<()>(Err(())));
        assert!(super::retain_state::<()>(Ok(Some(true))));
        assert!(!super::retain_state::<()>(Ok(Some(false))));
    }
}

pub(super) fn cached_sessions() -> BTreeSet<String> {
    super::subagent_lineage::cached_sessions().into_iter()
        .chain(super::history::cached_sessions())
        .chain(super::history_turns::cached_sessions())
        .chain(super::turn_index::cached_sessions()).collect()
}

pub(super) fn forget(session: &str) {
    super::subagent_lineage::forget_session(session);
    super::history_turns::invalidate(session);
    super::turn_index::invalidate(session);
    // Do not stall the catalog watcher behind a native read. Explicit file
    // mutations already hold their synchronous offline-reader fence.
    super::history::retire_cached_reader(session);
}
