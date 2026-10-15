//! Bound the idle head start for ridealongs when owner input is waiting.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_WAIT: Duration = Duration::from_secs(30);

struct Wait {
    deadline: Instant,
    notified: bool,
}

fn waits() -> &'static Mutex<HashMap<String, Wait>> {
    static WAITS: OnceLock<Mutex<HashMap<String, Wait>>> = OnceLock::new();
    WAITS.get_or_init(Default::default)
}

pub(crate) fn waiting(session: &str, ridealongs: bool) -> bool {
    waiting_at(session, ridealongs, Instant::now())
}

fn waiting_at(session: &str, ridealongs: bool, now: Instant) -> bool {
    let mut waits = waits().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !ridealongs {
        waits.remove(session);
        return false;
    }
    now >= waits.entry(session.into()).or_insert(Wait { deadline: now + MAX_WAIT, notified: false }).deadline
}

pub(crate) fn expired(session: &str) -> bool {
    waits().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session).is_some_and(|wait| Instant::now() >= wait.deadline)
}

pub fn clear(session: &str) {
    waits().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(session);
}

/// A timeout must retry idle flush even if no source has changed meanwhile.
pub fn wake_expired() {
    wake_expired_at(Instant::now());
}

fn wake_expired_at(now: Instant) {
    let due = {
        let mut waits = waits().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        waits.iter_mut().filter_map(|(session, wait)| {
            if wait.notified || now < wait.deadline { return None; }
            wait.notified = true;
            Some(session.clone())
        }).collect::<Vec<_>>()
    };
    for session in due { super::admission::changed(&session); }
}

#[cfg(test)]
pub(crate) fn elapse_for_test(session: &str) {
    clear(session);
    waiting_at(session, true, Instant::now() - MAX_WAIT);
    wake_expired();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_does_not_slide_and_wakes_only_its_session() {
        let session = format!("admission-wait-{}", uuid::Uuid::new_v4());
        let other = format!("admission-wait-{}", uuid::Uuid::new_v4());
        let now = Instant::now();
        assert!(!waiting_at(&session, true, now));
        assert!(!waiting_at(&session, true, now + Duration::from_secs(29)));
        assert!(!waiting_at(&other, true, now + Duration::from_secs(29)));
        wake_expired_at(now + MAX_WAIT);
        assert!(super::super::admission::take_change(&session));
        assert!(!super::super::admission::take_change(&other));
        assert!(waiting_at(&session, true, now + MAX_WAIT));
        wake_expired_at(now + MAX_WAIT);
        assert!(!super::super::admission::take_change(&session));
        clear(&session);
        clear(&other);
        assert!(!waiting_at(&session, true, now + MAX_WAIT));
        assert!(!waiting_at(&session, false, now + MAX_WAIT));
        assert!(!expired(&session));
    }
}
