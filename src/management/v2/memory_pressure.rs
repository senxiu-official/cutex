//! Shrink rebuildable Management state during low host memory.
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

const UNKNOWN: u8 = 0;
const NORMAL: u8 = 1;
const LOW: u8 = 2;
const INTERVAL: Duration = Duration::from_secs(5);
static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

#[cfg(test)]
thread_local! {
    static TEST_STATE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

fn current_state() -> u8 {
    #[cfg(test)]
    if let Some(state) = TEST_STATE.get() {
        return state;
    }
    STATE.load(Ordering::Relaxed)
}

pub(super) fn active() -> bool {
    current_state() == LOW
}

pub(super) fn status() -> &'static str {
    match current_state() {
        LOW => "low",
        NORMAL => "normal",
        _ => "unavailable",
    }
}

fn update_state(state: &AtomicU8, sample: impl FnOnce(bool) -> Option<bool>) -> bool {
    let previous = state.load(Ordering::Relaxed);
    let next = sample(previous == LOW).map_or(previous, |low| if low { LOW } else { NORMAL });
    state.store(next, Ordering::Relaxed);
    next == LOW
}

pub fn start_monitor() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("cutex-memory-pressure".into())
        .spawn(|| loop {
            if update_state(&STATE, crate::platform::memory_pressure::sample) {
                let freed_history = super::history_turns::trim_under_pressure();
                let freed_events = super::repository::trim_subscriptions_under_pressure();
                if freed_history || freed_events {
                    crate::platform::heap::release_free_memory();
                }
            }
            std::thread::sleep(INTERVAL);
        })
        .map(|_| ())
}

#[cfg(test)]
pub(super) fn with_test_pressure<T>(low: bool, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<u8>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_STATE.set(self.0);
        }
    }
    let _restore = Restore(TEST_STATE.replace(Some(if low { LOW } else { NORMAL })));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_switches_low_and_normal_and_keeps_last_valid_sample() {
        let state = AtomicU8::new(UNKNOWN);
        assert!(!update_state(&state, |_| None));
        assert_eq!(state.load(Ordering::Relaxed), UNKNOWN);
        assert!(update_state(&state, |low| {
            assert!(!low);
            Some(true)
        }));
        assert!(update_state(&state, |low| {
            assert!(low);
            None
        }));
        assert!(!update_state(&state, |low| {
            assert!(low);
            Some(false)
        }));
        assert_eq!(state.load(Ordering::Relaxed), NORMAL);
        assert!(update_state(&state, |low| {
            assert!(!low);
            Some(true)
        }));
    }
}
