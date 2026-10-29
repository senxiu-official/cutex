// Deterministic operation-count and policy tests; no live runtime or ports.
fn scan_fixture(world: &mut FakeWorld, runtime: &FakeRuntime, store: &AskStore,
    count: usize, per_target: usize, paused: bool, now: DateTime<Utc>) -> Vec<String> {
    let mut targets = Vec::new();
    for n in 0..count {
        let target = format!("scan-{n:04}");
        let mut info = world.directory.get(TARGET).unwrap().clone();
        info.cutex_session_id = target.clone();
        world.directory.agents.insert(target.clone(), info);
        runtime.set(&target, |status| {
            status.online = true;
            status.exclusive_admission = true;
            status.runtime_generation = 1;
            status.active_turn_id = Some("busy".into());
        });
        targets.push(target);
    }
    // Roster and authorization membership are independent: register both.
    world.directory.boundary = crate::agent_bus::boundary::AgentBoundary::new(
        &crate::config::agent_security::AgentSecurityConfig::default(),
        world.directory.agents.iter().filter_map(|(id, info)| info.project.as_ref().map(|project| (id.clone(), project.clone()))).collect(),
        Default::default());
    for target in &targets { assert!(world.directory.may_interact(REQUESTER, target)); }
    if let Some(target) = targets.first() { world.inputs.borrow_mut().target.cutex_session_id = target.clone(); }
    store.with_state(|state| {
        for target in &targets {
            for n in 0..per_target {
                let mut new = new_ask(&format!("{target}-{n}"), "fixture");
                new.target_cutex_session_id = target.clone();
                let id = state.create_ask(new, now).unwrap().0.ask_id;
                if paused { state.asks.get_mut(&id).unwrap().hold = Some(Hold::Paused { by: PausedBy::Owner }); }
            }
            if paused {
                state.pauses.insert(target.clone(), AdmissionPause {
                    cutex_session_id: target.clone(), by: PausedBy::Owner, since: now_rfc3339(now),
                    origin: Some(crate::asks::control::ORIGIN_OWNER_PAUSE.into()), stop_hold: false, runtime_generation: None,
                });
            }
        }
    }).unwrap();
    targets
}

#[test]
fn unchanged_paused_300_targets_skip_all_projections_and_resume_changes() {
    let (mut world, _dir) = world("scan-paused");
    let (store, _store_dir) = temp_store("scan-paused");
    let runtime = FakeRuntime::idle(&[TARGET]);
    let now = Utc::now();
    let targets = scan_fixture(&mut world, &runtime, &store, 300, 10, true, now);
    let before = store.shared().unwrap();
    // Exact old target-loop projection work, separated from fixture setup.
    let baseline = std::time::Instant::now();
    for target in &targets {
        let _ = runtime.target_status(target);
        store.queue_repository().unwrap().admission_with_state(target, &before).unwrap();
    }
    println!("old paused loop: 300 probes + 300 projections, {:?}", baseline.elapsed());
    runtime.target_probes.borrow_mut().clear();
    let at = std::time::Instant::now();
    let samples = diagnostics()["samples"].as_u64().unwrap();
    for _ in 0..3 {
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(report.targets_considered, 300);
        assert_eq!(report.paused_targets_skipped, 300);
        assert_eq!(report.targets_scanned, 0);
        assert_eq!(report.queue_projections, 0);
        assert_eq!(report.targets_deferred, 0);
        assert_eq!(store.shared().unwrap().revision, before.revision);
    }
    assert!(runtime.target_probes.borrow().is_empty());
    // Other scheduler tests can publish process-global diagnostics concurrently.
    // Exact operation counts belong to the per-tick reports asserted above.
    let diagnostic = diagnostics();
    assert!(diagnostic["samples"].as_u64().unwrap() >= samples + 3);
    let snapshot = crate::management::v2::control_timing::snapshot();
    let exposed = &snapshot["scheduler"];
    assert!(exposed["samples"].as_u64().unwrap() >= diagnostic["samples"].as_u64().unwrap());
    for value in [&diagnostic, exposed] {
        assert_eq!(value["schema"], 1);
        assert!(value["totalNs"].as_u64().is_some());
        assert!(value["lastPass"]["pausedSkipped"].as_u64().is_some());
        assert!(value["lastPass"]["queueProjections"].as_u64().is_some());
    }
    println!("new 3 complete paused ticks: 0 probes + 0 projections, {:?}", at.elapsed());
    // Changing an ask behind the pause must reconcile it exactly once.
    let id = before.waiting_for_target(&targets[0])[0].ask_id.clone();
    store.with_state(|state| state.asks.get_mut(&id).unwrap().hold = None).unwrap();
    let changed = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(changed.targets_scanned, 0);
    assert_eq!(store.shared().unwrap().asks[&id].hold, Some(Hold::Paused { by: PausedBy::Owner }));
    let revision = store.shared().unwrap().revision;
    tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(store.shared().unwrap().revision, revision);
    // Unpause has no TTL/cache delay, and still selects the oldest ask.
    store.with_state(|state| { crate::asks::control::owner_resume(state, &targets[0], "resumed", now); }).unwrap();
    runtime.set(&targets[0], |status| status.active_turn_id = None);
    let resumed = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(resumed.targets_scanned, 1);
    assert_eq!(resumed.admitted, vec![id]);
}

#[test]
fn bounded_scan_rotates_fairly_and_ready_results_precede_probes() {
    let (mut world, _dir) = world("scan-fair");
    let (store, _store_dir) = temp_store("scan-fair");
    let runtime = FakeRuntime::idle(&[TARGET]);
    let now = Utc::now();
    let targets = scan_fixture(&mut world, &runtime, &store, 70, 1, false, now);
    queued_answer(&store, "ready-first", now);
    let first = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!((first.targets_scanned, first.targets_deferred, first.events.len()), (32, 38, 1));
    let order = runtime.call_order.borrow();
    assert!(order.iter().position(|call| call == &format!("submit:{TARGET}")).unwrap()
        < order.iter().position(|call| call == &format!("probe:{}", targets[0])).unwrap());
    drop(order);
    for _ in 0..2 {
        // Production reopens AskStore each pass; fairness is keyed by store path.
        let reopened = AskStore::at(store.dir());
        assert_eq!(tick(&world, &runtime, &reopened, now).unwrap().targets_scanned, 32);
    }
    let probes = runtime.target_probes.borrow();
    for target in &targets { assert!(probes.contains(target), "starved {target}"); }
    assert_eq!(probes.iter().filter(|target| target.starts_with("scan-")).count(), 96);
    // Existing result lease is never resubmitted by repeated passes.
    assert_eq!(runtime.submitted.borrow().iter().filter(|(session, _)| session == TARGET).count(), 1);
}

#[test]
fn result_arriving_during_scan_waits_only_one_bounded_batch() {
    let (mut world, _dir) = world("scan-arrival");
    let (store, _store_dir) = temp_store("scan-arrival");
    let runtime = FakeRuntime::idle(&[TARGET]);
    let now = Utc::now();
    scan_fixture(&mut world, &runtime, &store, 70, 1, false, now);
    let concurrent_store = store.clone();
    *runtime.status_hook.borrow_mut() = Some(Box::new(move || { queued_answer(&concurrent_store, "during-scan", now); }));
    let report = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!((report.targets_scanned, report.targets_deferred, report.events.len()), (1, 69, 1));
    assert!(report.yielded_for_results);
    assert_eq!(runtime.target_probes.borrow().iter().filter(|target| target.starts_with("scan-")).count(), 1);
}

#[test]
fn policy_diagnostics_distinguish_coalescing_wake_budget_and_capacity() {
    use crate::asks::children::AgentFacts;
    let (mut world, _dir) = world("scan-policy");
    let (store, _store_dir) = temp_store("scan-policy");
    let runtime = FakeRuntime::idle(&[TARGET]);
    let now = Utc::now();
    runtime.result_context.borrow_mut().insert(TARGET.into(), crate::asks::result_coalescing::Context {
        class: crate::config::result_coalescing::ResultClass::ClaudeSubscription, last_activity: Some(now - chrono::Duration::seconds(1)),
    });
    queued_answer(&store, "quiet", now);
    let held = tick(&world, &runtime, &store, now + chrono::Duration::seconds(1)).unwrap();
    assert_eq!(held.result_waits.get(TARGET).map(String::as_str), Some("resultCoalescing"));
    assert!(held.events.is_empty());
    let due = tick(&world, &runtime, &store, now + chrono::Duration::seconds(6)).unwrap();
    assert_eq!(due.events.len(), 1);
    assert!(!due.result_waits.contains_key(TARGET));
    finish_turn(&runtime, &store, TARGET);
    runtime.set(TARGET, |status| status.online = false);
    world.facts = vec![
        AgentFacts { cutex_session_id: REQUESTER.into(), project: Some("alpha".into()), ..Default::default() },
        AgentFacts { cutex_session_id: TARGET.into(), project: Some("alpha".into()), ..Default::default() },
    ];
    store.with_state(|state| {
        for n in 0..world.limits().agent_started_online_per_domain {
            let session = format!("budget-{n}");
            state.woken.insert(session.clone(), ("project:alpha".into(), now_rfc3339(now)));
            state.presence.entry(session).or_default().online = true;
        }
        for n in 0..crate::asks::MAX_QUEUED_PER_TARGET {
            state.create_ask(new_ask(&format!("capacity-{n}"), "waiting"), now).unwrap();
        }
        assert_eq!(state.create_ask(new_ask("overflow", "too many"), now).unwrap_err().code, "target_overloaded");
    }).unwrap();
    let budget = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(budget.waits.get(TARGET).map(String::as_str), Some("autoWakeBudget"));
    assert_eq!(budget.queue_capacity_targets, 1);
    assert!(runtime.wakes.borrow().is_empty());
    assert_eq!(describe_target(&store.shared().unwrap(), TARGET)["queueCapacity"]["full"], true);
    store.with_state(|state| state.woken.clear()).unwrap();
    tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(runtime.wakes.borrow().as_slice(), [TARGET]);
}
#[test]
fn paused_fast_path_keeps_fail_if_busy_and_archive_precedence() {
    let (mut world, _dir) = world("scan-pause-semantics");
    let (store, _store_dir) = temp_store("scan-pause-semantics");
    let runtime = FakeRuntime::idle(&[TARGET]);
    let now = Utc::now();
    let targets = scan_fixture(&mut world, &runtime, &store, 1, 1, true, now);
    let target = &targets[0];
    let (fail, fresh) = store.with_state(|state| {
        let mut fail = new_ask("fail-paused", "refuse if busy");
        fail.target_cutex_session_id = target.clone();
        fail.options.if_busy = IfBusy::Fail;
        let fail = state.create_ask(fail, now).unwrap().0.ask_id;
        let mut fresh = new_ask("fresh-paused", "new arrival");
        fresh.target_cutex_session_id = target.clone();
        let fresh = state.create_ask(fresh, now).unwrap().0.ask_id;
        (fail, fresh)
    }).unwrap();
    let report = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(report.targets_scanned, 0);
    let state = store.shared().unwrap();
    assert_eq!(state.asks[&fail].execution, ExecutionState::Failed);
    assert_eq!(state.asks[&fresh].hold, Some(Hold::Paused { by: PausedBy::Owner }));
    let held = state.deliveries.values().filter(|d| d.ask_id == fresh).count();
    tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(store.shared().unwrap().deliveries.values().filter(|d| d.ask_id == fresh).count(), held);
    world.directory.agents.get_mut(target).unwrap().archived = true;
    let archived = tick(&world, &runtime, &store, now).unwrap();
    assert!(archived.failed.contains(&fresh));
    assert_eq!(store.shared().unwrap().asks[&fresh].execution, ExecutionState::Failed);
    assert!(runtime.wakes.borrow().is_empty());
}

#[test]
fn concurrent_resume_only_delays_one_pass_and_runtime_restart_clears_gate() {
    let (mut world, _dir) = world("scan-resume-race");
    let (store, _store_dir) = temp_store("scan-resume-race");
    let runtime = FakeRuntime::idle(&[TARGET]);
    let now = Utc::now();
    let targets = scan_fixture(&mut world, &runtime, &store, 1, 1, true, now);
    let target = &targets[0];
    let id = store.shared().unwrap().waiting_for_target(target)[0].ask_id.clone();
    // Force the changed branch, then remove pause after its shared snapshot.
    store.with_state(|state| state.asks.get_mut(&id).unwrap().hold = None).unwrap();
    let stale = store.shared().unwrap();
    store.with_state(|state| { crate::asks::control::owner_resume(state, target, "resumed", now); }).unwrap();
    let revision = store.shared().unwrap().revision;
    assert!(paused_target(&store, &stale, target, &[id.clone()], now).unwrap());
    assert_eq!(store.shared().unwrap().revision, revision, "stale pause cannot be reinstated");
    runtime.set(target, |status| status.active_turn_id = None);
    assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![id.clone()]);
    finish_turn(&runtime, &store, target);
    // Runtime-gate generation recovery stays before the paused shortcut.
    store.with_state(|state| {
        let ask = state.asks.get_mut(&id).unwrap();
        ask.execution = ExecutionState::Queued;
        ask.attempts.clear();
        ask.hold = Some(Hold::HeldByRuntime { native: Some(crate::asks::control::NATIVE_GATE_CLOSED.into()) });
        state.pauses.insert(target.clone(), AdmissionPause {
            cutex_session_id: target.clone(), by: PausedBy::Owner, since: now_rfc3339(now),
            origin: Some(crate::asks::control::ORIGIN_RUNTIME_GATE.into()), stop_hold: true, runtime_generation: Some(1),
        });
    }).unwrap();
    runtime.set(target, |status| status.runtime_generation = 2);
    assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![id]);
    assert!(!store.shared().unwrap().pauses.contains_key(target));
}

#[test]
fn uncertain_event_and_fresh_result_reconcile_once_then_deliver_next_tick() {
    let (mut world, world_dir) = world("scan-uncertain-fresh");
    let (store, store_dir) = temp_store("scan-uncertain-fresh");
    let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
    let now = Utc::now();
    let old_delivery = queued_answer(&store, "old-uncertain", now);
    runtime.outcomes.borrow_mut().push(SubmitOutcome::Uncertain("lost result reply".into()));
    tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(runtime.submitted.borrow().len(), 1);
    let old_lease = store.shared().unwrap().leases[TARGET].clone();
    assert!(matches!(&old_lease.holder, LeaseHolder::Event { .. }));
    assert!(old_lease.turn_id.is_none());
    let old_native_id = old_lease.native_message_id.clone();
    assert_eq!(runtime.submitted.borrow()[0].1.message_id, old_native_id);
    runtime.native.borrow_mut().insert(old_native_id.clone(), NativeMessageStatus::Unavailable("still unknown".into()));

    let targets = scan_fixture(&mut world, &runtime, &store, 2, 1, false, now);
    runtime.target_probes.borrow_mut().clear();
    let injected = std::rc::Rc::new(RefCell::new(None::<String>));
    let injected_hook = injected.clone();
    let concurrent_store = store.clone();
    *runtime.status_hook.borrow_mut() = Some(Box::new(move || {
        // Other recipient: the old uncertain Event must keep its own slot.
        *injected_hook.borrow_mut() = Some(queued_answer_for(
            &concurrent_store, "fresh-after-reconcile", REQUESTER, TARGET, now));
    }));
    let queries = *runtime.status_queries.borrow();
    let first = tick(&world, &runtime, &store, now).unwrap();
    let fresh_delivery = injected.borrow().clone().expect("first bulk probe injected result");
    assert_eq!(runtime.target_probes.borrow().first(), Some(&targets[0]));
    assert_eq!((first.targets_scanned, first.targets_deferred), (1, 1));
    assert!(first.yielded_for_results);
    assert!(first.events.is_empty());
    assert_eq!(*runtime.status_queries.borrow(), queries + 1, "no second result reconcile in this tick");
    assert_eq!(runtime.submitted.borrow().len(), 1, "no old replay or fresh result send yet");
    assert!(runtime.steered.borrow().is_empty());
    let state = store.shared().unwrap();
    assert_eq!(state.leases[TARGET], old_lease);
    assert_eq!(state.deliveries[&old_delivery].state, DeliveryState::QueuedAsEvent);
    assert_eq!(state.deliveries[&fresh_delivery].state, DeliveryState::Pending);
    assert!(!state.deliveries[&fresh_delivery].passive);
    drop(state);

    let second = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(*runtime.status_queries.borrow(), queries + 2, "one old receipt query per tick");
    assert_eq!(second.events.len(), 1);
    let state = store.shared().unwrap();
    assert_eq!(state.leases[TARGET], old_lease, "fresh delivery cannot release uncertain lease");
    let fresh = &state.deliveries[&fresh_delivery];
    assert_eq!(fresh.state, DeliveryState::Acknowledged);
    assert_eq!(fresh.event_id.as_deref(), Some(second.events[0].as_str()));
    let submissions = runtime.submitted.borrow();
    assert_eq!(submissions.len(), 2);
    assert_eq!(submissions.iter().filter(|(_, s)| s.message_id == old_native_id).count(), 1);
    assert_eq!(submissions[1].0, REQUESTER);
    assert_ne!(submissions[1].1.message_id, old_native_id);
    drop(submissions);
    drop(state);
    let _ = std::fs::remove_dir_all(store_dir);
    let _ = std::fs::remove_dir_all(world_dir);
}

#[test]
fn paused_ask_rechecks_revoked_boundary_after_owner_resume_without_submit() {
    let (mut world, world_dir) = world("scan-resume-revoked");
    let (store, store_dir) = temp_store("scan-resume-revoked");
    let runtime = FakeRuntime::idle(&[TARGET]);
    let now = Utc::now();
    let targets = scan_fixture(&mut world, &runtime, &store, 1, 1, true, now);
    let target = &targets[0];
    let id = store.shared().unwrap().waiting_for_target(target)[0].ask_id.clone();
    assert!(world.directory.may_interact(REQUESTER, target));
    let paused = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!((paused.paused_targets_skipped, paused.targets_scanned), (1, 0));
    assert!(runtime.submitted.borrow().is_empty());

    // Revoke reach, not roster existence: this must fail the boundary, not archive.
    world.directory.agents.get_mut(target).unwrap().project = Some("revoked-domain".into());
    world.directory.boundary = crate::agent_bus::boundary::AgentBoundary::new(
        &crate::config::agent_security::AgentSecurityConfig::default(),
        world.directory.agents.iter().filter_map(|(id, info)|
            info.project.as_ref().map(|project| (id.clone(), project.clone()))).collect(),
        Default::default());
    assert!(!world.directory.may_interact(REQUESTER, target));
    assert!(!world.directory.get(target).unwrap().archived);
    store.with_state(|state| {
        crate::asks::control::owner_resume(state, target, "resumed after revocation", now);
    }).unwrap();
    runtime.set(target, |status| status.active_turn_id = None);
    let resumed = tick(&world, &runtime, &store, now).unwrap();
    assert_eq!(resumed.targets_scanned, 1);
    assert_eq!(resumed.failed, vec![id.clone()]);
    assert!(resumed.admitted.is_empty());
    let state = store.shared().unwrap();
    assert!(!state.pauses.contains_key(target));
    assert_eq!(state.asks[&id].execution, ExecutionState::Failed);
    assert!(state.asks[&id].attempts.is_empty());
    assert!(!state.leases.contains_key(target));
    assert!(state.deliveries.values().any(|d|
        d.ask_id == id && d.kind == DeliveryKind::State && d.ref_id == format!("recipient_outside_boundary@{}", state.asks[&id].revision)));
    assert!(runtime.submitted.borrow().is_empty());
    assert!(runtime.steered.borrow().is_empty());
    drop(state);
    let _ = std::fs::remove_dir_all(store_dir);
    let _ = std::fs::remove_dir_all(world_dir);
}
