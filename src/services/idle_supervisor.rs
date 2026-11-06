//! Idle-stop transitions share a per-service gate with admitted proxy requests.
use super::*;

impl Supervisor {
    fn idle_gate(&self, slug: &str) -> Arc<Mutex<()>> {
        self.idle_gates
            .lock()
            .expect("service idle gates")
            .entry(slug.into())
            .or_default()
            .clone()
    }

    pub(super) fn stop_idle_services(self: &Arc<Self>) {
        self.stop_idle_services_at(chrono::Utc::now());
    }
    fn stop_idle_services_at(self: &Arc<Self>, now: chrono::DateTime<chrono::Utc>) {
        let slugs: Vec<_> = self
            .inner
            .lock()
            .expect("services supervisor")
            .records
            .values()
            .filter(|r| {
                super::super::idle::eligible(r)
                    && r.idle_stop_preset.seconds().is_some()
                    && r.stopped_idle_at.is_none()
                    && !r.stopped
                    && !r.archived
            })
            .map(|r| r.slug.clone())
            .collect();
        for slug in slugs {
            let gate = self.idle_gate(&slug);
            let Ok(_gate) = gate.try_lock() else { continue };
            let mut events = Vec::new();
            let mut inner = self.inner.lock().expect("services supervisor");
            let Some(record) = inner.records.get(&slug).cloned() else {
                continue;
            };
            let traffic = super::super::traffic::snapshot_for(&record);
            let last = traffic.last_traffic_at.as_deref().or_else(|| {
                inner
                    .entries
                    .get(&slug)
                    .and_then(|e| e.snapshot.last_traffic_at.as_deref())
            });
            if self
                .lifecycle
                .idle_stop_at(
                    &record,
                    last,
                    inner
                        .entries
                        .get(&slug)
                        .and_then(|e| e.snapshot.started_at.as_deref()),
                    traffic.active_connections,
                    now,
                )
                .is_none_or(|at| at > now)
            {
                continue;
            }
            let result = self.store.mutate(|registry| {
                let Some(current) = registry.services.get_mut(&slug) else {
                    return Ok(false);
                };
                if *current != record {
                    return Ok(false);
                }
                current.stopped_idle_at = Some(now.to_rfc3339());
                current.idle_wake_failed_at = None;
                Ok(true)
            });
            if result == Ok(true) {
                self.reload_locked(&mut inner, &mut events, false);
                self.reconcile_locked(&mut inner, &mut events);
                self.persist_locked(&mut inner);
            }
            drop(inner);
            self.emit(events);
        }
    }

    /// Only an authenticated proxy request may implicitly wake an idle stop.
    /// Manual stops and failed attempts require an explicit owner start/restart.
    pub fn ready_for_proxy(self: &Arc<Self>, slug: &str) -> ServicesResult<ServiceRecord> {
        let deadline = Instant::now() + OPEN_READY_TIMEOUT;
        let gate = self.idle_gate(slug);
        let _gate = loop {
            match gate.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(ServicesError::unavailable("service wake gate unavailable"))
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => {
                    return Err(ServicesError::new(
                        503,
                        "service_start_timeout",
                        "Service startup timed out. Use Wake in the service menu to retry.",
                    ))
                }
            }
        };
        let record = self.require_supervised(slug)?;
        if record.stopped {
            return Err(ServicesError::new(
                503,
                if super::super::idle::eligible(&record) {
                    "service_manually_stopped"
                } else {
                    "service_stopped"
                },
                "Stopped by the owner; use Start in the service menu.",
            ));
        }
        if record.stopped_idle_at.is_none() {
            return Ok(record);
        }
        let mut events = Vec::new();
        let attempt = (|| -> ServicesResult<u64> {
            let mut inner = self.inner.lock().expect("services supervisor");
            self.reload_locked(&mut inner, &mut events, false);
            let generation = inner.generation + 1;
            inner.generation = generation;
            let entry = inner
                .entries
                .get_mut(slug)
                .ok_or_else(|| ServicesError::not_found(slug))?;
            if record.idle_wake_failed_at.is_some() || entry.idle_wake_failed {
                return Err(ServicesError::new(
                    503,
                    "service_wake_failed",
                    "The last startup failed. Use Wake in the service menu to retry.",
                ));
            }
            self.store.mutate(|registry| {
                if registry.services.get(slug) != Some(&record) {
                    return Err(ServicesError::new(
                        409,
                        "service_changed",
                        "Service changed while starting; open it again.",
                    ));
                }
                entry.idle_waking = true;
                self.spawn_locked(&record, entry, generation, &mut events);
                Ok(())
            })?;
            self.persist_locked(&mut inner);
            Ok(generation)
        })();
        self.emit(events);
        let generation = attempt?;
        let ready = loop {
            let alive = {
                let inner = self.inner.lock().expect("services supervisor");
                inner
                    .entries
                    .get(slug)
                    .and_then(|e| e.running.as_ref())
                    .is_some_and(|r| r.generation == generation)
            };
            if !alive || Instant::now() >= deadline {
                break false;
            }
            if super::super::health::probe_until(record.port, record.path.as_deref(), deadline) {
                break true;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        self.finish_idle_wake(&record, generation, ready)?;
        if !ready {
            return Err(ServicesError::new(
                503,
                "service_wake_failed",
                "Service did not become healthy. Use Wake in the service menu to retry.",
            ));
        }
        self.store.get(slug)
    }

    fn finish_idle_wake(
        self: &Arc<Self>,
        record: &ServiceRecord,
        generation: u64,
        ready: bool,
    ) -> ServicesResult<()> {
        let mut inner = self.inner.lock().expect("services supervisor");
        let result = self.store.mutate(|registry| {
            let current = registry
                .services
                .get_mut(&record.slug)
                .ok_or_else(|| ServicesError::not_found(&record.slug))?;
            if current.stopped
                || current.archived
                || current.created_at != record.created_at
                || current.launch_fingerprint() != record.launch_fingerprint()
                || current.stopped_idle_at != record.stopped_idle_at
            {
                return Err(ServicesError::new(
                    409,
                    "service_changed",
                    "Service changed while starting; open it again.",
                ));
            }
            if ready {
                current.stopped_idle_at = None;
                current.idle_wake_failed_at = None;
            } else {
                current.idle_wake_failed_at = Some(now_rfc3339());
            }
            Ok(())
        });
        let same_record = inner.records.get(&record.slug).is_some_and(|current| {
            current.created_at == record.created_at
                && current.launch_fingerprint() == record.launch_fingerprint()
        });
        if same_record {
            if let Some(entry) = inner.entries.get_mut(&record.slug).filter(|entry| {
                entry
                    .running
                    .as_ref()
                    .is_none_or(|r| r.generation == generation)
            }) {
                entry.idle_waking = false;
                if ready && result.is_ok() {
                    entry.snapshot.health = ServiceHealth::Reachable;
                    entry.snapshot.last_probe_at = Some(now_rfc3339());
                    entry.snapshot.last_seen_at = entry.snapshot.last_probe_at.clone();
                }
                if !ready {
                    // Failed storage still suppresses repeated attempts in this daemon.
                    // An owner replacement/stop must not inherit this attempt's failure.
                    entry.idle_wake_failed =
                        result.is_ok() || result.as_ref().is_err_and(|error| error.status >= 500);
                    if let Some(running) = entry.running.take() {
                        running.tree.kill();
                    }
                    entry.snapshot.pid = None;
                    entry.snapshot.started_at = None;
                    entry.snapshot.health = ServiceHealth::Stopped;
                    entry.next_restart = None;
                }
            }
        }
        let mut events = Vec::new();
        self.reload_locked(&mut inner, &mut events, false);
        self.reconcile_locked(&mut inner, &mut events);
        self.persist_locked(&mut inner);
        drop(inner);
        self.emit(events);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::store::{Caller, RegisterRequest, ServiceCommand};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct FixtureRoot(PathBuf);
    impl FixtureRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("cutex-idle-fixture-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join(".idle-fixture"), b"owned test fixture").unwrap();
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for FixtureRoot {
        fn drop(&mut self) {
            if self.0.starts_with(std::env::temp_dir()) && self.0.join(".idle-fixture").is_file() {
                if let Err(error) = std::fs::remove_dir_all(&self.0) {
                    eprintln!("owned idle fixture cleanup: {error}");
                }
            }
        }
    }
    fn setup(
        command: ServiceCommand,
        lifetime: &str,
    ) -> (
        FixtureRoot,
        ServiceStore,
        ServiceRecord,
        Arc<Supervisor>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let root = FixtureRoot::new();
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let store = ServiceStore::new(root.path(), "fixture").with_port_probe(|_| true);
        let (record, _) = store
            .register(
                RegisterRequest {
                    slug: format!("idle-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
                    runtime: Some("managed".into()),
                    lifetime: Some(lifetime.into()),
                    port: Some(port),
                    command: Some(command),
                    env: Some([("CUTEX_IDLE_TEST_SERVER".into(), "1".into())].into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let failures = Arc::new(AtomicUsize::new(0));
        let a = starts.clone();
        let b = failures.clone();
        let supervisor = Supervisor::new(
            store.clone(),
            Box::new(move |event| {
                if event.reason == "started" {
                    a.fetch_add(1, Ordering::SeqCst);
                }
                if event.reason == "exited" {
                    b.fetch_add(1, Ordering::SeqCst);
                }
            }),
        );
        (root, store, record, supervisor, starts, failures)
    }
    fn server() -> ServiceCommand {
        ServiceCommand::Argv(vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--exact".into(),
            "services::supervisor::idle_supervisor::tests::private_tcp_fixture".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ])
    }
    #[test]
    #[ignore = "owned child fixture only, invoked through a managed-service process tree"]
    fn private_tcp_fixture() {
        use std::io::{Read, Write};
        assert_eq!(std::env::var("CUTEX_IDLE_TEST_SERVER").as_deref(), Ok("1"));
        let port: u16 = std::env::var("PORT").unwrap().parse().unwrap();
        assert!(![24270, 24260, 25370].contains(&port));
        if let Ok(delay) = std::env::var("CUTEX_IDLE_TEST_DELAY_MS") {
            std::thread::sleep(Duration::from_millis(delay.parse().unwrap()));
        }
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
            let mut bytes = [0; 1024];
            if stream.read(&mut bytes).unwrap_or(0) > 0 {
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture",
                );
            }
        }
    }
    fn make_sleeping(store: &ServiceStore, record: &ServiceRecord) {
        store
            .mutate(|registry| {
                let row = registry.services.get_mut(&record.slug).unwrap();
                row.stopped_idle_at = Some(now_rfc3339());
                row.idle_stop_preset = super::super::super::idle::IdleStopPreset::Hour;
                Ok(())
            })
            .unwrap();
    }
    fn shut_down(supervisor: &Supervisor, port: u16) {
        supervisor.shutdown();
        let deadline = Instant::now() + Duration::from_secs(5);
        while probe(port, None) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !probe(port, None),
            "owned fixture process remained on its port"
        );
    }

    #[test]
    fn thirty_two_proxy_waiters_share_one_wake_and_daemon_restart_leaves_sleepers_asleep() {
        let (_root, store, record, supervisor, starts, _) = setup(server(), "persistent");
        make_sleeping(&store, &record);
        supervisor.start().unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        supervisor.probe_all();
        let sleeping = store.get(&record.slug).unwrap();
        let snapshots = supervisor.snapshots();
        assert!(!crate::services::tile::fetchable(
            &sleeping,
            snapshots.get(&record.slug)
        ));
        crate::services::tile::TileCache::default().answer(
            "sleep-test",
            &sleeping,
            false,
            Instant::now(),
            |_| panic!("a tile must not wake a sleeper"),
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(crate::services::traffic::snapshot(&record.slug)
            .last_traffic_at
            .is_none());
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..32)
                .map(|_| {
                    let s = supervisor.clone();
                    let slug = record.slug.clone();
                    scope.spawn(move || s.ready_for_proxy(&slug))
                })
                .collect();
            for handle in handles {
                assert_eq!(handle.join().unwrap().unwrap().port, record.port);
            }
        });
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(store.load_state()[&record.slug].idle_stop_at.is_some());
        assert!(store.get(&record.slug).unwrap().stopped_idle_at.is_none());
        supervisor.probe_all();
        assert_eq!(
            crate::services::tile::fetch(
                &record,
                "/tile",
                1024,
                Duration::from_secs(2),
                "application/json"
            )
            .unwrap(),
            b"fixture"
        );
        assert!(
            crate::services::traffic::snapshot(&record.slug)
                .last_traffic_at
                .is_none(),
            "health, tile and direct-port requests are not proxy use"
        );
        assert_eq!(
            supervisor.view(&record.slug).unwrap()["process"]["restarts"],
            0
        );
        let future = chrono::Utc::now() + chrono::Duration::hours(2);
        let connection = crate::services::traffic::begin(&record.slug);
        supervisor.stop_idle_services_at(future);
        assert!(store.get(&record.slug).unwrap().stopped_idle_at.is_none());
        drop(connection);
        let lease = supervisor
            .lifecycle
            .lease(&record.slug, None, None, future)
            .unwrap();
        supervisor.sync();
        assert!(store.load_state()[&record.slug].idle_stop_at.is_none());
        supervisor.stop_idle_services_at(future);
        assert!(store.get(&record.slug).unwrap().stopped_idle_at.is_none());
        supervisor
            .lifecycle
            .release(&record.slug, &lease.lease_id, future);
        // A real supervisor tick can briefly own the per-service gate. Keep
        // the same logical time and wait for an admitted idle-stop pass.
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.get(&record.slug).unwrap().stopped_idle_at.is_none() {
            supervisor.stop_idle_services_at(future + chrono::Duration::hours(2));
            assert!(Instant::now() < deadline, "idle stop never acquired its service gate");
            std::thread::sleep(Duration::from_millis(10));
        }
        let slept = store.get(&record.slug).unwrap();
        assert!(slept.stopped_idle_at.is_some());
        assert_eq!(slept.port, record.port);
        assert!(
            store
                .register(
                    RegisterRequest {
                        slug: "cannot-take-sleeping-port".into(),
                        port: Some(record.port),
                        ..Default::default()
                    },
                    &Caller::Owner
                )
                .is_err(),
            "sleep keeps the port reserved"
        );
        assert_eq!(
            supervisor.view(&record.slug).unwrap()["health"],
            "stoppedIdle"
        );
        shut_down(&supervisor, record.port);
        let second_starts = Arc::new(AtomicUsize::new(0));
        let count = second_starts.clone();
        let restarted = Supervisor::new(
            store.clone(),
            Box::new(move |event| {
                if event.reason == "started" {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            }),
        );
        restarted.start().unwrap();
        restarted.tick();
        assert_eq!(second_starts.load(Ordering::SeqCst), 0);
        assert_eq!(store.get(&record.slug).unwrap().port, record.port);
        restarted.start_service(&record.slug).unwrap();
        assert_eq!(second_starts.load(Ordering::SeqCst), 1);
        restarted.stop_idle_services_at(future + chrono::Duration::hours(2));
        assert!(store.get(&record.slug).unwrap().stopped_idle_at.is_some());
        store.request_restart(&record.slug, &Caller::Owner).unwrap();
        restarted.sync();
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.get(&record.slug).unwrap().stopped_idle_at.is_some()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(store.get(&record.slug).unwrap().stopped_idle_at.is_none());
        assert_eq!(second_starts.load(Ordering::SeqCst), 2);
        shut_down(&restarted, record.port);
    }

    #[test]
    fn manual_stop_during_idle_start_wins_over_the_pending_request() {
        let (_root, store, record, supervisor, starts, _) = setup(server(), "persistent");
        store
            .update(
                &record.slug,
                crate::services::store::UpdateRequest {
                    env: Some([("CUTEX_IDLE_TEST_DELAY_MS".into(), Some("1500".into()))].into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        make_sleeping(&store, &record);
        supervisor.start().unwrap();
        let s = supervisor.clone();
        let slug = record.slug.clone();
        let wake = std::thread::spawn(move || s.ready_for_proxy(&slug));
        let deadline = Instant::now() + Duration::from_secs(5);
        while starts.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        supervisor.stop_service(&record.slug).unwrap();
        assert_eq!(wake.join().unwrap().unwrap_err().code, "service_changed");
        supervisor.tick();
        assert!(store.get(&record.slug).unwrap().stopped);
        assert!(supervisor.snapshots()[&record.slug].pid.is_none());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        shut_down(&supervisor, record.port);
    }

    #[test]
    fn failed_wake_is_one_attempt_until_explicit_restart_and_manual_stop_never_wakes() {
        let missing =
            ServiceCommand::Argv(vec!["cutex-idle-fixture-command-does-not-exist".into()]);
        let (_root, store, record, supervisor, starts, failures) = setup(missing, "persistent");
        make_sleeping(&store, &record);
        supervisor.start().unwrap();
        for _ in 0..32 {
            assert_eq!(
                supervisor.ready_for_proxy(&record.slug).unwrap_err().code,
                "service_wake_failed"
            );
        }
        supervisor.tick();
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(store
            .get(&record.slug)
            .unwrap()
            .idle_wake_failed_at
            .is_some());
        let fresh = Supervisor::new(store.clone(), Box::new(|_| {}));
        fresh.start().unwrap();
        fresh.tick();
        assert_eq!(
            fresh.ready_for_proxy(&record.slug).unwrap_err().code,
            "service_wake_failed"
        );
        assert!(fresh.snapshots()[&record.slug].pid.is_none());
        fresh.shutdown();
        assert_eq!(
            supervisor.restart_service(&record.slug).unwrap_err().code,
            "service_wake_failed"
        );
        assert_eq!(failures.load(Ordering::SeqCst), 2);
        supervisor.stop_service(&record.slug).unwrap();
        assert_eq!(
            supervisor.ready_for_proxy(&record.slug).unwrap_err().code,
            "service_manually_stopped"
        );
        assert_eq!(
            supervisor.open_service(&record.slug).unwrap_err().code,
            "service_manually_stopped"
        );
        assert_eq!(
            store
                .request_restart(&record.slug, &Caller::Owner)
                .unwrap_err()
                .code,
            "service_stopped"
        );
        assert!(store.get(&record.slug).unwrap().stopped_idle_at.is_none());
        assert_eq!(failures.load(Ordering::SeqCst), 2);
        supervisor.shutdown();
    }
}
