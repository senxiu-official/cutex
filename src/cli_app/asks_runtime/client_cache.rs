//! Per-session serialization for asks control clients. No transport work under
//! the registry mutex; retiring clients are destroyed under their session lock.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Session<C> = Arc<Mutex<Option<(u64, C)>>>;
pub(super) struct ClientCache<C> {
    sessions: Mutex<HashMap<String, Session<C>>>,
}
impl<C> Default for ClientCache<C> {
    fn default() -> Self { Self { sessions: Mutex::new(HashMap::new()) } }
}
impl<C> ClientCache<C> {
    pub(super) fn with_client<T>(
        &self,
        session: &str,
        generation: u64,
        connect: impl FnOnce() -> anyhow::Result<C>,
        use_client: impl FnOnce(&C) -> T,
        discard: impl FnOnce(&T) -> bool,
    ) -> anyhow::Result<T> {
        let slot = {
            let mut sessions = self.sessions.lock()
                .map_err(|_| anyhow::anyhow!("ask runtime client registry lock poisoned"))?;
            Arc::clone(sessions.entry(session.to_owned()).or_default())
        };
        // Never wait for a session while holding the registry. An Arc pins the
        // slot even for waiters, so cleanup cannot split same-session callers.
        let mut client = slot.lock()
            .map_err(|_| anyhow::anyhow!("ask runtime session client lock poisoned"))?;
        let result = (|| {
            anyhow::ensure!(client.as_ref().is_none_or(|(known, _)| *known <= generation),
                "stale ask runtime generation");
            if client.as_ref().is_none_or(|(known, _)| *known != generation) {
                drop(client.take());
                *client = Some((generation, connect()?));
            }
            let result = use_client(&client.as_ref().expect("connected above").1);
            // Retire the exact client used, before another call/reconnect can
            // acquire this session. No delayed session-name-only deletion.
            if discard(&result) { drop(client.take()); }
            Ok(result)
        })();
        if client.is_none() {
            let mut sessions = self.sessions.lock()
                .map_err(|_| anyhow::anyhow!("ask runtime client registry lock poisoned"))?;
            // Map + this call only. Any caller that already cloned the slot
            // prevents removal; empty failed sessions need no permanent entry.
            if Arc::strong_count(&slot) == 2 {
                sessions.remove(session);
            }
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::{Duration, Instant};

    const DEADLINE: Duration = Duration::from_secs(5);
    struct Client {
        id: u64,
        dropping: Option<(Sender<()>, Receiver<()>)>,
    }
    impl Client {
        fn new(id: u64) -> Self { Self { id, dropping: None } }
    }
    impl Drop for Client {
        fn drop(&mut self) {
            if let Some((entered, release)) = self.dropping.take() {
                entered.send(()).unwrap();
                release.recv_timeout(DEADLINE).unwrap();
            }
        }
    }
    fn call(cache: &ClientCache<Client>, session: &str, generation: u64) -> anyhow::Result<u64> {
        cache.with_client(session, generation, || Ok(Client::new(generation)), |c| c.id, |_| false)
    }
    fn pins(cache: &ClientCache<Client>, session: &str, expected: usize) {
        let start = Instant::now();
        loop {
            let count = cache.sessions.lock().unwrap().get(session).map(Arc::strong_count);
            if count == Some(expected) { return; }
            assert!(start.elapsed() < DEADLINE, "waiting callers did not pin slot: {count:?}");
            std::thread::yield_now();
        }
    }

    #[test]
    fn legacy_registry_lock_reproduces_cross_session_exclusion() {
        // Faithful minimal reproduction of 2b41d404 with_client's lock scope,
        // not a daemon/transport E2E: use_client runs under the whole map lock.
        let clients = Mutex::new(HashMap::from([("a", 1), ("b", 2)]));
        let (entered, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        std::thread::scope(|scope| {
            let clients_ref = &clients;
            scope.spawn(move || {
                let table = clients_ref.lock().unwrap();
                assert_eq!(table.get("a"), Some(&1));
                entered.send(()).unwrap();
                wait.recv_timeout(DEADLINE).unwrap();
            });
            ready.recv_timeout(DEADLINE).unwrap();
            let blocked = matches!(clients.try_lock(), Err(std::sync::TryLockError::WouldBlock));
            release.send(()).unwrap();
            assert!(blocked, "B cannot even look up its already-connected client");
        });
    }

    #[test]
    fn slow_connect_and_rpc_do_not_block_another_session() {
        for during_connect in [true, false] {
            let cache = ClientCache::default();
            let (entered, ready) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            let (completed, done) = mpsc::channel();
            std::thread::scope(|scope| {
                let cache_ref = &cache;
                scope.spawn(move || {
                    let block = || { entered.send(()).unwrap(); wait.recv_timeout(DEADLINE).unwrap(); };
                    cache_ref.with_client("a", 1,
                        || { if during_connect { block(); } Ok(Client::new(1)) },
                        |_| { if !during_connect { block(); } }, |_| false).unwrap();
                });
                ready.recv_timeout(DEADLINE).unwrap();
                scope.spawn(|| { completed.send(call(&cache, "b", 1)).unwrap(); });
                let result = done.recv_timeout(Duration::from_secs(2));
                release.send(()).unwrap();
                assert_eq!(result.unwrap().unwrap(), 1);
            });
        }
    }

    #[test]
    fn same_session_waiters_share_one_slot_and_serialize() {
        let cache = ClientCache::default();
        let (entered, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (completed, done) = mpsc::channel();
        std::thread::scope(|scope| {
            let cache_ref = &cache;
            scope.spawn(move || cache_ref.with_client("a", 1, || Ok(Client::new(1)), |_| {
                entered.send(()).unwrap(); wait.recv_timeout(DEADLINE).unwrap();
            }, |_| false).unwrap());
            ready.recv_timeout(DEADLINE).unwrap();
            scope.spawn(|| { completed.send(cache.with_client("a", 1,
                || panic!("same generation must reuse the connection"), |c| c.id, |_| false)).unwrap(); });
            pins(&cache, "a", 3); // registry, active call, waiting call
            assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
            release.send(()).unwrap();
            assert_eq!(done.recv_timeout(DEADLINE).unwrap().unwrap(), 1);
        });
    }

    #[test]
    fn replacement_and_error_cleanup_drop_without_registry_lock() {
        for replacement in [true, false] {
            let cache = ClientCache::default();
            let (entered, ready) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            cache.with_client("a", 1, || Ok(Client { id: 1, dropping: Some((entered, wait)) }), |_| (), |_| false).unwrap();
            let (completed, done) = mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| cache.with_client("a", if replacement { 2 } else { 1 },
                    || Ok(Client::new(2)), |_| (), |_| !replacement).unwrap());
                ready.recv_timeout(DEADLINE).unwrap();
                scope.spawn(|| { completed.send(call(&cache, "b", 1)).unwrap(); });
                let result = done.recv_timeout(Duration::from_secs(2));
                release.send(()).unwrap();
                assert_eq!(result.unwrap().unwrap(), 1);
            });
            if replacement { assert_eq!(call(&cache, "a", 2).unwrap(), 2); }
            else { assert!(!cache.sessions.lock().unwrap().contains_key("a")); }
        }
    }

    #[test]
    fn failed_old_call_cannot_retire_waiting_new_generation_or_same_generation_retry() {
        for next_generation in [1, 2] {
            let cache = ClientCache::default();
            let (entered, ready) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            std::thread::scope(|scope| {
                let cache_ref = &cache;
                scope.spawn(move || cache_ref.with_client("a", 1, || Ok(Client::new(10)), |_| {
                    entered.send(()).unwrap(); wait.recv_timeout(DEADLINE).unwrap(); Err::<(), _>("RPC failed")
                }, Result::is_err).unwrap().unwrap_err());
                ready.recv_timeout(DEADLINE).unwrap();
                let next = scope.spawn(|| cache.with_client("a", next_generation,
                    || Ok(Client::new(20)), |c| c.id, |_| false).unwrap());
                pins(&cache, "a", 3);
                release.send(()).unwrap();
                assert_eq!(next.join().unwrap(), 20);
            });
            assert_eq!(cache.with_client("a", next_generation, || panic!("new client was removed"), |c| c.id, |_| false).unwrap(), 20);
        }
    }

    #[test]
    fn generation_fence_failed_connect_and_retention_policy() {
        let cache = ClientCache::default();
        assert_eq!(call(&cache, "a", 2).unwrap(), 2);
        assert!(cache.with_client("a", 1, || panic!("stale connect"), |_| panic!("stale RPC"), |_| false).is_err());
        assert_eq!(call(&cache, "a", 2).unwrap(), 2);
        // Expected application errors keep the client; transport errors retire it.
        cache.with_client("a", 2, || panic!("cached"), |_| Err::<(), _>("size policy"), |_| false).unwrap().unwrap_err();
        assert_eq!(call(&cache, "a", 2).unwrap(), 2);
        assert!(cache.with_client("a", 3, || anyhow::bail!("connect failed"), |_| (), |_| false).is_err());
        assert!(!cache.sessions.lock().unwrap().contains_key("a"));
        assert_eq!(call(&cache, "a", 3).unwrap(), 3);
    }
}
