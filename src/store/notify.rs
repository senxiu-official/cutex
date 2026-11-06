//! In-process change wake-ups. A hint only: subscribers always re-read
//! `changes` from SQLite, so a lost or coalesced wake never loses data and
//! writers in other processes are found by the next read.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::keyed::Scope;
use super::Seq;

#[derive(Default)]
struct Slot {
    /// Newest committed seq per scope, plus the store-wide newest.
    scopes: HashMap<Scope, Seq>,
    any: Seq,
}

struct Channel {
    slot: Mutex<Slot>,
    wake: Condvar,
}

static CHANNELS: OnceLock<Mutex<HashMap<&'static str, Arc<Channel>>>> = OnceLock::new();

fn channel(store: &'static str) -> Arc<Channel> {
    CHANNELS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(store)
        .or_insert_with(|| {
            Arc::new(Channel {
                slot: Mutex::new(Slot::default()),
                wake: Condvar::new(),
            })
        })
        .clone()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Newest seq committed in this process for the subscribed range.
    Changed(Seq),
    Timeout,
}

/// Host-scope subscribers wake on every change of the store; other scopes
/// only on their own scope.
pub struct Subscription {
    channel: Arc<Channel>,
    scope: Scope,
    seen: Seq,
}

impl Subscription {
    fn current(&self, slot: &Slot) -> Seq {
        match self.scope {
            Scope::Host => slot.any,
            _ => slot.scopes.get(&self.scope).copied().unwrap_or(0),
        }
    }

    /// Wait up to `timeout` for a commit newer than the last one returned.
    /// Several commits coalesce into one wake.
    pub fn wait(&mut self, timeout: Duration) -> Wake {
        let deadline = Instant::now() + timeout;
        let mut slot = self
            .channel
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let current = self.current(&slot);
            if current > self.seen {
                self.seen = current;
                return Wake::Changed(current);
            }
            let now = Instant::now();
            if now >= deadline {
                return Wake::Timeout;
            }
            slot = self
                .channel
                .wake
                .wait_timeout(slot, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// Subscribe from now on; commits before this call do not wake.
pub fn subscribe(store: &'static str, scope: Scope) -> Subscription {
    let channel = channel(store);
    let mut subscription = Subscription {
        channel,
        scope,
        seen: 0,
    };
    let slot = subscription
        .channel
        .slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let seen = subscription.current(&slot);
    drop(slot);
    subscription.seen = seen;
    subscription
}

/// Called by `StoreDb::write` after commit.
pub(crate) fn publish(store: &'static str, scopes: impl IntoIterator<Item = (Scope, Seq)>) {
    let channel = channel(store);
    let mut slot = channel
        .slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (scope, seq) in scopes {
        let entry = slot.scopes.entry(scope).or_insert(0);
        *entry = (*entry).max(seq);
        slot.any = slot.any.max(seq);
    }
    drop(slot);
    channel.wake.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_coalesce_and_respect_scope() {
        let mut host = subscribe("notify-fixture", Scope::Host);
        let mut session = subscribe("notify-fixture", Scope::Session("a".into()));
        let mut other = subscribe("notify-fixture", Scope::Session("b".into()));
        publish("notify-fixture", [(Scope::Session("a".into()), 5)]);
        publish("notify-fixture", [(Scope::Session("a".into()), 6)]);
        assert_eq!(host.wait(Duration::ZERO), Wake::Changed(6));
        assert_eq!(session.wait(Duration::ZERO), Wake::Changed(6));
        assert_eq!(session.wait(Duration::ZERO), Wake::Timeout);
        assert_eq!(other.wait(Duration::from_millis(5)), Wake::Timeout);
        let waiter = std::thread::spawn(move || other.wait(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        publish("notify-fixture", [(Scope::Session("b".into()), 7)]);
        assert_eq!(waiter.join().unwrap(), Wake::Changed(7));
    }
}
