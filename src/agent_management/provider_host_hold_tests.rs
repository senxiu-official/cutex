// r273 stab (051 step 3, A02/A03): a create/start no longer holds the host
// authority fence or the provider execution mutex for its runtime-online
// wait. Included into provider.rs tests (FakeLifecycle, fixtures).

/// A runtime start that blocks in its readiness wait until the test opens it.
#[derive(Default)]
struct OnlineGate {
    state: Mutex<OnlineGateState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct OnlineGateState {
    entered: usize,
    active: usize,
    max_active: usize,
    open: bool,
    panic_next: bool,
    /// Only the first start waits; later ones pass straight through.
    only_first: bool,
}

impl OnlineGate {
    fn pass(&self) {
        let mut state = self.state.lock().unwrap();
        if state.only_first && state.entered > 0 {
            return;
        }
        state.entered += 1;
        state.active += 1;
        state.max_active = state.max_active.max(state.active);
        self.changed.notify_all();
        while !state.open {
            state = self.changed.wait(state).unwrap();
        }
        state.active -= 1;
        let crash = std::mem::take(&mut state.panic_next);
        drop(state);
        if crash {
            panic!("injected creator crash during the runtime-online wait");
        }
    }

    fn wait_entered(&self, count: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut state = self.state.lock().unwrap();
        while state.entered < count {
            let now = std::time::Instant::now();
            assert!(now < deadline, "runtime start never reached its readiness wait");
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
    }

    fn open(&self) {
        self.state.lock().unwrap().open = true;
        self.changed.notify_all();
    }

    fn entered(&self) -> usize {
        self.state.lock().unwrap().entered
    }

    fn max_active(&self) -> usize {
        self.state.lock().unwrap().max_active
    }
}

fn gate_online(lifecycle: &FakeLifecycle) -> std::sync::Arc<OnlineGate> {
    let gate = std::sync::Arc::new(OnlineGate::default());
    lifecycle.state.lock().unwrap().online_gate = Some(gate.clone());
    gate
}

/// Opens the gate when a test assertion unwinds, so the scoped start
/// thread finishes and the failure is reported instead of hanging.
struct OpenOnDrop(std::sync::Arc<OnlineGate>);
impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

fn online_request(action_id: &str, agent: &CutexSessionId) -> AgentManagementRequest {
    AgentManagementRequest {
        schema: AgentManagementSchema::V1,
        action_id: action(action_id),
        project_id: Some(project()),
        operation: AgentOperation::Online {
            cutex_session_id: agent.clone(),
            allow_unlisted: false,
        },
    }
}

/// The admission check only TRIES the fence (pause_admission::try_with_authority
/// -> try_lock_delivery_mutations). Before r273 a create held it for the whole
/// 28-40 s online wait, so every admission in that window was deferred.
#[test]
fn slow_runtime_start_leaves_the_fence_to_admission_and_other_agents() {
    let root = root("hold-admission");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    // One lifecycle (agent ids are per fake); only the first start is slow.
    let slow_lifecycle = FakeLifecycle::default();
    let gate = gate_online(&slow_lifecycle);
    gate.state.lock().unwrap().only_first = true;
    let other_lifecycle = &slow_lifecycle;
    std::thread::scope(|scope| {
        let _open = OpenOnDrop(gate.clone());
        let slow = scope.spawn(|| {
            provider.execute(
                &invocation("cutex.director"),
                &create_request("create-slow", "slow", AgentStartMode::BootstrapOnly),
                &slow_lifecycle,
            )
        });
        gate.wait_entered(1);
        // Admission's one-shot try succeeds during the readiness wait.
        for _ in 0..20 {
            let fence = provider.store().try_lock_delivery_mutations().unwrap();
            assert!(fence.is_some(), "admission deferred: the online wait holds the host fence");
            drop(fence);
            assert!(provider_execution_lock().try_lock().is_ok(), "the online wait holds the execution mutex");
        }
        // Another agent's create runs to completion meanwhile.
        let started = std::time::Instant::now();
        completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-other", "other", AgentStartMode::BootstrapOnly),
            other_lifecycle,
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(gate.entered(), 1, "the slow start is still waiting");
        gate.open();
        let created = created_agent(&completed(slow.join().unwrap()));
        assert!(slow_lifecycle.observe(&created.cutex_session_id).unwrap().app_server_runtime);
    });
    // Both entries left nothing behind.
    assert!(provider.store().try_lock_delivery_mutations().unwrap().is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// The bus during `online_register`: a timeout (so a real backoff), then a
/// wait at the gate (a slow answer), then success.
struct RetryingBus {
    tries: std::sync::atomic::AtomicUsize,
    gate: std::sync::Arc<OnlineGate>,
}
impl crate::app_server::bus_bridge::RuntimeAgentBus for RetryingBus {
    fn register(&self, _: &crate::agent_bus::model::AgentBusRegisterRequest) -> anyhow::Result<()> {
        if self.tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Err(anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::TimedOut)).context("Failed to read agent bus response"));
        }
        self.gate.pass();
        Ok(())
    }
    fn unregister(&self, _: &str) -> anyhow::Result<bool> { Ok(true) }
    fn poll(&self, _: &str) -> anyhow::Result<Vec<crate::agent_bus::model::AgentBusMessage>> { Ok(vec![]) }
    fn ack(&self, _: &str, ids: &[String]) -> anyhow::Result<usize> { Ok(ids.len()) }
}

/// Delegation-994e28: while one create retries its bus registration, the fence
/// and the execution mutex are free and another agent's create and online run
/// to completion. Both the backoff and the slow try are covered.
#[test]
fn another_agents_create_and_online_proceed_during_a_bus_registration_retry() {
    use std::sync::atomic::Ordering::SeqCst;
    let root = root("hold-bus-retry");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let other = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("create-existing", "existing", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    lifecycle.offline(&other.cutex_session_id).unwrap();
    let gate = std::sync::Arc::new(OnlineGate::default());
    let bus = std::sync::Arc::new(RetryingBus { tries: Default::default(), gate: gate.clone() });
    lifecycle.state.lock().unwrap().online_register = Some(bus.clone());
    std::thread::scope(|scope| {
        let _open = OpenOnDrop(gate.clone());
        let slow = scope.spawn(|| {
            provider.execute(
                &invocation("cutex.director"),
                &create_request("create-retrying", "retrying", AgentStartMode::BootstrapOnly),
                &lifecycle,
            )
        });
        let free = || {
            let fence = provider.store().try_lock_delivery_mutations().unwrap();
            assert!(fence.is_some(), "the bus registration retry holds the host fence");
            drop(fence);
            assert!(provider_execution_lock().try_lock().is_ok(), "the bus registration retry holds the execution mutex");
        };
        // The 1 s backoff after the first timeout.
        while bus.tries.load(SeqCst) == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(bus.tries.load(SeqCst), 1, "still in the backoff");
        free();
        // The second try, waiting for a slow bus.
        gate.wait_entered(1);
        free();
        let started = std::time::Instant::now();
        completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-during-retry", "during-retry", AgentStartMode::BootstrapOnly),
            &lifecycle,
        ));
        completed(provider.execute(
            &invocation("cutex.director"),
            &online_request("online-during-retry", &other.cutex_session_id),
            &lifecycle,
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(gate.entered(), 1, "the retrying create is still waiting");
        gate.open();
        let created = created_agent(&completed(slow.join().unwrap()));
        assert!(lifecycle.observe(&created.cutex_session_id).unwrap().app_server_runtime);
        assert_eq!(bus.tries.load(SeqCst), 2);
    });
    assert!(provider.store().try_lock_delivery_mutations().unwrap().is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// Two Director starts of the same agent (different action ids) while the
/// fence is released: the second waits for the first outside every lock and
/// never runs a second runtime start alongside it.
#[test]
fn concurrent_starts_of_the_same_agent_stay_single_flight() {
    let root = root("hold-single-agent");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let created = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("create", "worker", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    lifecycle.offline(&created.cutex_session_id).unwrap();
    let launches = lifecycle.launch_count();
    let gate = gate_online(&lifecycle);
    std::thread::scope(|scope| {
        let _open = OpenOnDrop(gate.clone());
        let first = scope.spawn(|| {
            provider.execute(&invocation("cutex.director"), &online_request("online-a", &created.cutex_session_id), &lifecycle)
        });
        gate.wait_entered(1);
        let second = scope.spawn(|| {
            provider.execute(&invocation("cutex.director"), &online_request("online-b", &created.cutex_session_id), &lifecycle)
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(gate.entered(), 1, "a second start entered while the first was in flight");
        assert!(provider.store().try_lock_delivery_mutations().unwrap().is_some());
        gate.open();
        completed(first.join().unwrap());
        completed(second.join().unwrap());
    });
    assert_eq!(gate.max_active(), 1);
    assert_eq!(lifecycle.launch_count(), launches + 1, "exactly one runtime was started");
    let _ = std::fs::remove_dir_all(root);
}

/// The same create action submitted twice concurrently (a Director retry):
/// one native bootstrap, one runtime start, and the duplicate replays the
/// first one's receipt.
#[test]
fn concurrent_duplicate_create_replays_the_single_flight() {
    let root = root("hold-same-action");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let gate = gate_online(&lifecycle);
    let request = create_request("create-dup", "worker", AgentStartMode::BootstrapOnly);
    let (first, second) = std::thread::scope(|scope| {
        let _open = OpenOnDrop(gate.clone());
        let first = scope.spawn(|| provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        gate.wait_entered(1);
        let second = scope.spawn(|| provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(gate.entered(), 1);
        gate.open();
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(first, second);
    completed(first);
    assert_eq!(lifecycle.bootstrap_count(), 1);
    assert_eq!(lifecycle.launch_count(), 1);
    let _ = std::fs::remove_dir_all(root);
}

/// The creator dies between the registered intent (durable Configured phase,
/// in-flight keys) and the completion, inside the released online wait. No
/// lock or in-flight key survives it, and replaying the same action finishes
/// the create without a second bootstrap or a second runtime.
#[test]
fn crash_between_intent_and_completion_is_recovered_by_replay() {
    let root = root("hold-crash");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let gate = gate_online(&lifecycle);
    {
        let mut state = gate.state.lock().unwrap();
        state.open = true;
        state.panic_next = true;
    }
    let request = create_request("create-crash", "worker", AgentStartMode::BootstrapOnly);
    let crashed = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    provider.execute(&invocation("cutex.director"), &request, &lifecycle)
                }))
            })
            .join()
            .unwrap()
    });
    assert!(crashed.is_err(), "the injected crash must unwind the entry");
    assert!(provider_execution_lock().try_lock().is_ok(), "execution mutex released and not poisoned");
    assert!(provider.store().try_lock_delivery_mutations().unwrap().is_some(), "fence released");
    assert!(
        super::super::host_hold::InFlight::try_register(vec![format!("action:{}", request.action_id.as_str())])
            .unwrap()
            .is_some(),
        "in-flight key released"
    );
    let state = provider.store().snapshot().unwrap();
    let action = &state.actions[&request.action_id];
    assert_eq!(action.phase, AgentActionPhase::Configured, "intent is durable");
    assert!(action.response.is_none());
    completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
    assert_eq!(lifecycle.bootstrap_count(), 1);
    assert_eq!(lifecycle.launch_count(), 1);
    let _ = std::fs::remove_dir_all(root);
}

/// The fence comes back before the outcome is published, and a nested
/// release while released is a plain call.
#[test]
fn outside_host_fence_restores_the_registered_hold() {
    let root = root("hold-restore");
    let provider = AgentManagementProvider::open(&root).unwrap();
    assert!(!crate::agent_management::host_fence_registered());
    // Unregistered: a plain call.
    assert_eq!(crate::agent_management::outside_host_fence("plain", || 7).unwrap(), 7);
    {
        let execution = provider_execution_lock().lock().unwrap();
        let fence = provider.store().lock_mutations().unwrap();
        let _hold = super::super::host_hold::HostHold::enter(provider.store(), Some(execution), fence);
        assert!(crate::agent_management::host_fence_registered());
        let other = std::thread::scope(|scope| {
            scope.spawn(|| provider.store().try_lock_delivery_mutations().unwrap().is_some()).join().unwrap()
        });
        assert!(!other, "registered fence is held");
        let inner = crate::agent_management::outside_host_fence("outer", || {
            assert!(!crate::agent_management::host_fence_registered());
            let free = std::thread::scope(|scope| {
                scope.spawn(|| provider.store().try_lock_delivery_mutations().unwrap().is_some()).join().unwrap()
            });
            assert!(free, "released during the effect");
            crate::agent_management::outside_host_fence("nested", || 3).unwrap()
        })
        .unwrap();
        assert_eq!(inner, 3);
        assert!(crate::agent_management::host_fence_registered());
        let other = std::thread::scope(|scope| {
            scope.spawn(|| provider.store().try_lock_delivery_mutations().unwrap().is_some()).join().unwrap()
        });
        assert!(!other, "fence reacquired before the entry continues");
    }
    assert!(!crate::agent_management::host_fence_registered());
    assert!(provider.store().try_lock_delivery_mutations().unwrap().is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// r273.1 (B1): the slow live-claim proof runs outside the registered
/// fence, and only counts when the claim is unchanged once the fence is back.
#[test]
fn verify_claim_outside_host_fence_releases_and_compares() {
    let root = root("hold-verify-claim");
    let provider = AgentManagementProvider::open(&root).unwrap();
    // Unregistered: a plain call, the claim is never read.
    let value = crate::agent_management::verify_claim_outside_host_fence(
        "plain",
        || -> anyhow::Result<u64> { panic!("claim read without a hold") },
        || Ok(5),
    )
    .unwrap();
    assert_eq!(value, 5);
    let claim = std::cell::Cell::new(7u64);
    {
        let execution = provider_execution_lock().lock().unwrap();
        let fence = provider.store().lock_mutations().unwrap();
        let _hold = super::super::host_hold::HostHold::enter(provider.store(), Some(execution), fence);
        // Unchanged claim: the proof ran without the fence and counts.
        let free = crate::agent_management::verify_claim_outside_host_fence(
            "unchanged",
            || Ok(claim.get()),
            || {
                assert!(!crate::agent_management::host_fence_registered());
                Ok(std::thread::scope(|scope| {
                    scope.spawn(|| provider.store().try_lock_delivery_mutations().unwrap().is_some()).join().unwrap()
                }))
            },
        )
        .unwrap();
        assert!(free, "the live-claim proof held the host fence");
        assert!(crate::agent_management::host_fence_registered(), "fence back before publishing");
        // A claim that changed in the window fails the entry.
        let error = crate::agent_management::verify_claim_outside_host_fence(
            "changed",
            || Ok(claim.get()),
            || {
                claim.set(8);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("durable claim changed"), "{error:#}");
        // A failed proof is returned as is (fail closed), fence back.
        let error = crate::agent_management::verify_claim_outside_host_fence(
            "failed",
            || Ok(claim.get()),
            || -> anyhow::Result<()> { anyhow::bail!("not owned") },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("not owned"));
        assert!(crate::agent_management::host_fence_registered());
    }
    assert!(provider.store().try_lock_delivery_mutations().unwrap().is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// r273.1 (B1, host-a): after the readiness wait the native adapter proves
/// the live claim before its reconnect (on Windows a 3.2 s PowerShell child,
/// held under the re-taken fence for 91-94% of a create/online). The fake
/// now models that second release point: admission gets the fence during it.
#[test]
fn live_claim_proof_after_readiness_leaves_the_fence_to_admission() {
    let root = root("hold-live-claim");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let created = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("create", "worker", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    lifecycle.offline(&created.cutex_session_id).unwrap();
    let gate = std::sync::Arc::new(OnlineGate::default());
    lifecycle.state.lock().unwrap().live_claim_gate = Some(gate.clone());
    std::thread::scope(|scope| {
        let _open = OpenOnDrop(gate.clone());
        let online = scope.spawn(|| {
            provider.execute(&invocation("cutex.director"), &online_request("online-verify", &created.cutex_session_id), &lifecycle)
        });
        gate.wait_entered(1);
        for _ in 0..10 {
            let fence = provider.store().try_lock_delivery_mutations().unwrap();
            assert!(fence.is_some(), "admission deferred: the live-claim proof holds the host fence");
            drop(fence);
            assert!(provider_execution_lock().try_lock().is_ok(), "the live-claim proof holds the execution mutex");
        }
        gate.open();
        completed(online.join().unwrap());
    });
    assert!(lifecycle.observe(&created.cutex_session_id).unwrap().app_server_runtime);

    // A claim that changes during the proof (a stop / new generation landed
    // in the window) fails the entry instead of publishing the reconnect.
    lifecycle.offline(&created.cutex_session_id).unwrap();
    let gate = std::sync::Arc::new(OnlineGate::default());
    lifecycle.state.lock().unwrap().live_claim_gate = Some(gate.clone());
    std::thread::scope(|scope| {
        let _open = OpenOnDrop(gate.clone());
        let online = scope.spawn(|| {
            provider.execute(&invocation("cutex.director"), &online_request("online-changed", &created.cutex_session_id), &lifecycle)
        });
        gate.wait_entered(1);
        lifecycle
            .state
            .lock()
            .unwrap()
            .agents
            .get_mut(&created.cutex_session_id)
            .unwrap()
            .runtime_generation += 1;
        gate.open();
        let response = online.join().unwrap();
        assert!(
            !matches!(response.outcome, AgentManagementOutcome::Complete { .. }),
            "a reconnect to a changed claim was published: {response:?}"
        );
    });
    lifecycle.state.lock().unwrap().live_claim_gate = None;
    assert!(provider.store().try_lock_delivery_mutations().unwrap().is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// Load profile (r273 stab report, `--ignored`): Director creates whose
/// runtime takes ONLINE_MS to come online, run concurrently with owner
/// admission traffic using the real one-shot admission primitive
/// (`pause_admission::try_with_authority`) on the same private home. Prints
/// one `FENCE_STORM_PROFILE` JSON line. The r271.2 baseline is the same
/// harness without the release (the adapter held the fence for the wait).
/// Creates currently inside their (simulated) runtime-online wait.
static STORM_ONLINE_WAITING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
#[ignore]
fn fence_busy_storm_profile() {
    let env = |name: &str, default: u64| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let (creators, per_creator, online_ms, admitters, admit_hz) = (
        env("STORM_CREATORS", 4), env("STORM_CREATES", 3), env("STORM_ONLINE_MS", 3000),
        env("STORM_ADMITTERS", 16), env("STORM_ADMIT_HZ", 20),
    );
    let root = crate::config::paths::runtime_dir().unwrap().join("agent-management").join("v1");
    let _ = std::fs::remove_dir_all(&root);
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    lifecycle.state.lock().unwrap().online_delay = Some(std::time::Duration::from_millis(online_ms));
    let done = std::sync::atomic::AtomicBool::new(false);
    let (ready, deferred) = (std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0));
    // The same counts for attempts made while some create was in its online wait.
    let (wait_ready, wait_deferred) = (std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0));
    let control_secs = env("STORM_CONTROL_SECS", 0);
    let admit_latency = Mutex::new(Vec::<f64>::new());
    let create_latency = Mutex::new(Vec::<f64>::new());
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..admitters {
            scope.spawn(|| {
                let period = std::time::Duration::from_millis(1000 / admit_hz.max(1));
                // Independent callers: random phase, jittered period.
                let jitter = |span: std::time::Duration| span.mul_f64((uuid::Uuid::new_v4().as_u128() % 1000) as f64 / 1000.0);
                std::thread::sleep(jitter(period));
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    let at = std::time::Instant::now();
                    let waiting = STORM_ONLINE_WAITING.load(std::sync::atomic::Ordering::SeqCst) > 0;
                    let outcome = crate::asks::pause_admission::try_with_authority(|_| Ok(())).unwrap();
                    let waiting = waiting && STORM_ONLINE_WAITING.load(std::sync::atomic::Ordering::SeqCst) > 0;
                    match outcome {
                        crate::asks::pause_admission::Admission::Ready(()) => {
                            ready.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if waiting { wait_ready.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
                        }
                        crate::asks::pause_admission::Admission::Deferred => {
                            deferred.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if waiting { wait_deferred.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
                        }
                    }
                    admit_latency.lock().unwrap().push(at.elapsed().as_secs_f64() * 1000.0);
                    std::thread::sleep((period / 2 + jitter(period)).saturating_sub(at.elapsed()));
                }
            });
        }
        let workers: Vec<_> = (0..creators)
            .map(|creator| {
                let (provider, lifecycle, create_latency) = (&provider, &lifecycle, &create_latency);
                scope.spawn(move || {
                    for index in 0..per_creator {
                        let at = std::time::Instant::now();
                        let request = create_request(&format!("storm-{creator}-{index}"), &format!("storm{creator}x{index}"), AgentStartMode::BootstrapOnly);
                        completed(provider.execute(&invocation("cutex.director"), &request, lifecycle));
                        create_latency.lock().unwrap().push(at.elapsed().as_secs_f64());
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        std::thread::sleep(std::time::Duration::from_secs(control_secs));
        done.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let pct = |values: &Mutex<Vec<f64>>, p: f64| {
        let mut values = values.lock().unwrap().clone();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        values.get(((values.len() as f64 * p) as usize).min(values.len().saturating_sub(1))).copied().unwrap_or(0.0)
    };
    let (ready, deferred) = (ready.into_inner(), deferred.into_inner());
    let (wait_ready, wait_deferred) = (wait_ready.into_inner(), wait_deferred.into_inner());
    println!(
        "FENCE_STORM_PROFILE {}",
        serde_json::json!({
            "creators": creators, "createsEach": per_creator, "onlineMs": online_ms,
            "admitters": admitters, "admitHz": admit_hz,
            "wallSeconds": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "admissions": ready + deferred, "admitted": ready, "deferred": deferred,
            "deferredPct": (deferred as f64 * 1000.0 / (ready + deferred).max(1) as f64).round() / 10.0,
            "admitP50ms": pct(&admit_latency, 0.5), "admitP99ms": pct(&admit_latency, 0.99),
            "createP50s": pct(&create_latency, 0.5), "createMaxs": pct(&create_latency, 1.0),
            "duringOnlineWait": {"admissions": wait_ready + wait_deferred, "deferred": wait_deferred,
                "deferredPct": (wait_deferred as f64 * 1000.0 / (wait_ready + wait_deferred).max(1) as f64).round() / 10.0},
        })
    );
    let _ = std::fs::remove_dir_all(root);
}
