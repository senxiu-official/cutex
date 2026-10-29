use super::*;
use crate::asks::{model::*, store::AskStore};

fn failed(id: &str) -> Value {
    json!({"id":id,"status":"failed","error":{"codexErrorInfo":"serverOverloaded"},"items":[]})
}
fn now() -> DateTime<Utc> {
    "2026-10-26T00:00:00Z".parse().unwrap()
}
fn root() -> PathBuf {
    std::env::temp_dir().join(format!("or-{}", uuid::Uuid::new_v4().simple()))
}
fn pending(s: &State, id: &str) -> Plan {
    s.sessions[id].plan.clone().unwrap()
}

#[test]
fn reserved_cancel_rejects_other_lane_even_with_equal_token_without_dispatch() {
    let a = format!("reserved-a-{}", uuid::Uuid::new_v4());
    let b = format!("reserved-b-{}", uuid::Uuid::new_v4());
    let first = super::super::session_coordination::session_lane(&a).unwrap();
    let second = super::super::session_coordination::session_lane(&b).unwrap();
    let held = first.reserve().unwrap();
    let other = second.reserve().unwrap();
    assert_eq!(held.token(), other.token());
    let result: Result<(), _> = dispatch_after_cancel_with_reservation(&b, "cutex/runtime/offline", Some(&held), || {
        panic!("wrong capability must not dispatch or enter the safety fallback")
    });
    assert!(result.is_err());
    assert!(held.is_current_for(&first).unwrap());
    assert!(other.is_current_for(&second).unwrap());
}

#[test]
fn reserved_safety_cancel_retains_outer_lane_through_dispatch_and_unwind() {
    let dir = root();
    let store = Store::at(dir.join("retry.json"));
    let session = format!("reserved-unwind-{}", uuid::Uuid::new_v4());
    store.update(|s| { s.terminal(&session, "thread", 9, &failed("turn"), None, now()); }).unwrap();
    let lane = super::super::session_coordination::session_lane(&session).unwrap();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let held = lane.reserve().unwrap();
        let _: Result<(), super::super::user_input::UserInputExecutionError> =
            dispatch_after_safety_cancel_inner(&store, &session, "ownerStop", Some(&held), || {
                assert!(held.is_current_for(&lane).unwrap());
                assert!(lane.try_lock().is_err());
                assert!(store.snapshot().unwrap().sessions[&session].plan.is_none());
                assert!(!store.is_disabled());
                panic!("native dispatch unwind");
            });
    }));
    assert!(outcome.is_err());
    let snapshot = lane.snapshot().unwrap();
    assert!(snapshot.active.is_none(), "outer capability released on unwind");
    assert!(snapshot.poisoned, "unwind retains existing failclosed lane poison");
    assert!(lane.try_lock().is_err());
    assert!(!store.is_disabled(), "no false overload persistence failure");
}

#[test]
fn busy_safety_cancel_marker_failure_is_not_durable_success() {
    let dir = root();
    let path = dir.join("missing-parent/retry.json");
    let store = Store::at(&path);
    let session = format!("busy-marker-{}", uuid::Uuid::new_v4());
    let lane = super::super::session_coordination::session_lane(&session).unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _held = lane.reserve().unwrap();
            barrier.wait();
            barrier.wait();
        });
        barrier.wait();
        let result: Result<(), _> = dispatch_after_safety_cancel(&store, &session, "ownerStop", || {
            assert!(lane.try_lock().is_err(), "busy capability is never stolen");
            Err(super::super::server::persistence_user_input_error(anyhow::anyhow!("original stop refused")))
        });
        barrier.wait();
        assert!(result.unwrap_err().message.contains("original stop refused"));
    });
    assert!(store.is_disabled());
    assert!(!path.with_extension("disabled").exists());
    let error = store.disable().unwrap_err();
    assert!(error.to_string().contains("cannot create durable disable marker"));
    assert!(Store::at(path).is_disabled(), "process latch survives reopen without claiming durability");
}

#[test]
fn typed_failure_only() {
    assert!(overloaded(&failed("t")));
    assert!(overloaded(
        &json!({"status":"failed","error":{"codexErrorInfo":{"serverOverloaded":{}}}})
    ));
    for v in [
        json!({"status":"completed","error":{"codexErrorInfo":"serverOverloaded"}}),
        json!({"status":"failed","error":{"message":"serverOverloaded 503"}}),
        json!({"status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}),
        json!({"status":"failed","error":{"codexErrorInfo":{"serverOverloaded":{},"other":{}}}}),
    ] {
        assert!(!overloaded(&v), "{v}");
    }
}

fn wait_file(path: &std::path::Path) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < until,
            "barrier timeout: {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
fn child(mode: &str, dir: &std::path::Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "management::v2::model_overload_retry::tests::p1_process_child",
            "--nocapture",
        ])
        .env("CUTEX_RETRY_P1_CHILD", mode)
        .env("CUTEX_RETRY_P1_DIR", dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap()
}
#[test]
fn p1_process_child() {
    let Ok(mode) = std::env::var("CUTEX_RETRY_P1_CHILD") else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os("CUTEX_RETRY_P1_DIR").unwrap());
    let store = Store::at(dir.join("retry.json"));
    match mode.as_str() {
        "hold-os-lock" => {
            use fs2::FileExt;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(dir.join("retry.lock"))
                .unwrap();
            file.lock_exclusive().unwrap();
            std::fs::write(dir.join("os-locked"), b"").unwrap();
            wait_file(&dir.join("release-os"));
        }
        "writer-a" => {
            store
                .update(|s| {
                    std::fs::write(dir.join("a-locked"), b"").unwrap();
                    wait_file(&dir.join("release-a"));
                    s.terminal("uncertain", "thread", 77, &failed("old"), None, now());
                    s.sessions
                        .get_mut("uncertain")
                        .unwrap()
                        .plan
                        .as_mut()
                        .unwrap()
                        .phase = Phase::Uncertain;
                })
                .unwrap();
        }
        "writer-b" => {
            std::fs::write(dir.join("b-start"), b"").unwrap();
            store
                .update(|s| {
                    std::fs::write(dir.join("b-inside"), b"").unwrap();
                    s.sessions
                        .entry("observer".into())
                        .or_default()
                        .blocked_generation = Some(88);
                })
                .unwrap();
        }
        "cancel-crash" | "settle-crash" => {
            use crate::asks::store::tests::TARGET;
            store
                .update(|s| s.cancel(TARGET, "receiptUnresolved", Utc::now()))
                .unwrap();
            if mode == "settle-crash" {
                let p = store.snapshot().unwrap().sessions[TARGET]
                    .settlement
                    .clone()
                    .unwrap();
                settle_cancelled(&AskStore::at(&dir), TARGET, &p.plan, &p.reason).unwrap();
            }
            // Simulate abrupt exit before journal settlement acknowledgement.
            std::process::exit(0);
        }
        _ => panic!("unknown child mode"),
    }
}

#[test]
fn p2_long_os_lock_does_not_block_safety_dispatch_and_latches() {
    let dir = root();
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::at(dir.join("retry.json"));
    store
        .update(|s| {
            s.terminal("s", "thread", 1, &failed("turn"), None, now());
        })
        .unwrap();
    let mut holder = child("hold-os-lock", &dir);
    wait_file(&dir.join("os-locked"));
    let start = std::time::Instant::now();
    let dispatched = std::cell::Cell::new(false);
    let result = dispatch_after_safety_cancel(&store, "s", "ownerStop", || {
        assert!(
            super::super::session_coordination::session_lane("s")
                .unwrap()
                .try_lock()
                .is_ok(),
            "preflight must release its lane before original dispatch"
        );
        dispatched.set(true);
        Ok(())
    });
    let elapsed = start.elapsed();
    let still_holding = holder.try_wait().unwrap().is_none();
    std::fs::write(dir.join("release-os"), b"").unwrap();
    assert!(holder.wait().unwrap().success());
    result.unwrap();
    assert!(dispatched.get());
    assert!(still_holding);
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "safety preflight waited {elapsed:?}"
    );
    assert!(store.is_disabled());
    assert_eq!(
        store.snapshot().unwrap().sessions["s"]
            .plan
            .as_ref()
            .unwrap()
            .phase,
        Phase::Pending
    );
    assert!(!Store::at(dir.join("retry.json")).automatic_enabled());
}

#[test]
fn p2_process_mutex_and_lane_contention_are_nonblocking_without_stealing_lane() {
    for lane_busy in [false, true] {
        let dir = root();
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::at(dir.join("retry.json"));
        let session = format!("p2-lane-{}", uuid::Uuid::new_v4());
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let holder = scope.spawn(|| {
                let lane = super::super::session_coordination::session_lane(&session).unwrap();
                let _lane = lane_busy.then(|| lane.lock().unwrap());
                let _local = (!lane_busy).then(|| LOCK.lock().unwrap());
                barrier.wait();
                barrier.wait();
            });
            barrier.wait();
            let start = std::time::Instant::now();
            let result = dispatch_after_safety_cancel(&store, &session, "ownerStop", || {
                if lane_busy {
                    assert!(
                        super::super::session_coordination::session_lane(&session)
                            .unwrap()
                            .try_lock()
                            .is_err(),
                        "must not steal existing native reservation"
                    );
                }
                Ok(())
            });
            let elapsed = start.elapsed();
            barrier.wait();
            holder.join().unwrap();
            result.unwrap();
            assert!(elapsed < std::time::Duration::from_millis(500));
            assert!(store.is_disabled());
        });
    }
}

#[test]
fn p2_sync_failure_is_reported_and_process_latch_survives_reopen() {
    let dir = root();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("retry.json");
    let store = Store::at(&path);
    let error = store
        .disable_with(|| Err(anyhow::anyhow!("injected marker sync failure")))
        .unwrap_err();
    assert!(error.to_string().contains("sync failure"));
    assert!(!path.with_extension("disabled").exists());
    std::fs::write(&path, b"{}").unwrap();
    assert!(!Store::at(path).automatic_enabled());
}

#[test]
fn p1_os_lock_serializes_two_process_rmw_at_barrier() {
    let dir = root();
    std::fs::create_dir_all(&dir).unwrap();
    let mut a = child("writer-a", &dir);
    wait_file(&dir.join("a-locked"));
    let mut b = child("writer-b", &dir);
    wait_file(&dir.join("b-start"));
    std::thread::sleep(std::time::Duration::from_millis(200));
    let entered_early = dir.join("b-inside").exists();
    std::fs::write(dir.join("release-a"), b"").unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert!(
        !entered_early,
        "second process entered RMW while first held lock"
    );
    let s = Store::at(dir.join("retry.json")).snapshot().unwrap();
    assert_eq!(
        s.sessions["uncertain"].plan.as_ref().unwrap().phase,
        Phase::Uncertain
    );
    assert_eq!(s.sessions["observer"].blocked_generation, Some(88));
}

#[test]
fn p1_cancel_settlement_survives_process_exit_on_both_commit_boundaries() {
    use crate::asks::store::tests::TARGET;
    for mode in ["cancel-crash", "settle-crash"] {
        let (asks, store, id, _) = ask_fixture(mode);
        store
            .update(|s| {
                s.sessions
                    .get_mut(TARGET)
                    .unwrap()
                    .plan
                    .as_mut()
                    .unwrap()
                    .phase = Phase::Uncertain
            })
            .unwrap();
        let dir = store.path.parent().unwrap();
        assert!(child(mode, dir).wait().unwrap().success());
        let restarted = Store::at(store.path.clone());
        assert!(restarted.snapshot().unwrap().sessions[TARGET]
            .settlement
            .is_some());
        recover_settlement(&restarted, &asks, TARGET).unwrap();
        recover_settlement(&restarted, &asks, TARGET).unwrap();
        let s = asks.snapshot().unwrap();
        assert_eq!(s.answers.len(), 1);
        assert_eq!(
            s.asks[&id].attempts[0].interrupt_reason,
            Some(InterruptReason::OutcomeUnknown)
        );
        assert!(restarted.snapshot().unwrap().sessions[TARGET]
            .settlement
            .is_none());
    }
}

#[test]
fn p1_finalize_commit_barrier_cannot_end_rebound_turn_or_generation() {
    use crate::asks::store::tests::TARGET;
    for generation_only in [false, true] {
        let (asks, _, id, plan) = ask_fixture("stale-finalize");
        let lease = asks.snapshot().unwrap().leases[TARGET].clone();
        let gate = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                gate.wait();
                if generation_only {
                    asks.with_state(|s| {
                        s.leases.get_mut(TARGET).unwrap().runtime_generation += 1;
                        s.asks.get_mut(&id).unwrap().attempts[0].runtime_generation += 1;
                    })
                    .unwrap();
                } else {
                    rebind(&asks, TARGET, &plan, "new-turn").unwrap();
                }
                gate.wait();
            });
            crate::asks::observe::finalize_fenced_test(
                &asks,
                TARGET,
                &id,
                1,
                &json!({"id":plan.turn_id,"status":"interrupted"}),
                Some(InterruptReason::OutcomeUnknown),
                Utc::now(),
                &lease,
                &|| {
                    gate.wait();
                    gate.wait();
                },
            )
            .unwrap();
            writer.join().unwrap();
        });
        assert!(asks.snapshot().unwrap().answers.is_empty());
        assert!(asks.snapshot().unwrap().leases.contains_key(TARGET));
    }
}

#[test]
fn p1_scheduler_unreadable_history_racing_rebind_keeps_new_lease() {
    use crate::asks::scheduler::{tests::FakeRuntime, TickReport};
    use crate::asks::store::tests::TARGET;
    let (asks, _, _, p) = ask_fixture("history-race");
    let lease = asks.snapshot().unwrap().leases[TARGET].clone();
    let runtime = FakeRuntime::idle(&[TARGET]);
    runtime.set(TARGET, |s| {
        s.active_turn_id = None;
        s.runtime_generation = lease.runtime_generation + 1;
    });
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let callback_gate = gate.clone();
    *runtime.read_turn_hook.borrow_mut() = Some(Box::new(move || {
        callback_gate.wait();
        callback_gate.wait();
    }));
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            gate.wait();
            rebind(&asks, TARGET, &p, "new-after-history").unwrap();
            gate.wait();
        });
        crate::asks::scheduler::reconcile_bound(
            &runtime,
            &asks,
            &lease,
            &mut TickReport::default(),
            Utc::now() + chrono::Duration::seconds(60),
        )
        .unwrap();
        writer.join().unwrap();
    });
    let s = asks.snapshot().unwrap();
    assert!(s.answers.is_empty());
    assert_eq!(
        s.leases[TARGET].turn_id.as_deref(),
        Some("new-after-history")
    );
}

#[test]
fn p1_journal_fault_does_not_block_stop_but_fences_automation() {
    let dir = root();
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::at(dir.join("retry.json"));
    std::fs::write(&store.path, b"broken json").unwrap();
    let dispatched = std::cell::Cell::new(false);
    let result = dispatch_after_cancel_result(
        Some(&store),
        "ownerStop",
        store.snapshot().map(|_| false),
        || {
            dispatched.set(true);
            Ok(json!({"stopped":true}))
        },
    );
    assert!(result.unwrap()["stopped"].as_bool().unwrap());
    assert!(dispatched.get());
    // Repairing the journal must not silently re-enable the old automatic timer.
    std::fs::write(&store.path, b"{}").unwrap();
    assert!(!store.automatic_enabled());
    let result: Result<(), _> = dispatch_after_cancel_result(
        Some(&store),
        "ownerMessage",
        Err(anyhow::anyhow!("journal unavailable")),
        || panic!("non-safety dispatch"),
    );
    assert_eq!(result.unwrap_err().code, "event_persistence_unavailable");
}

#[test]
fn p1_binding_window_holds_new_overload_terminal_and_recovers() {
    use crate::asks::store::tests::TARGET;
    let (asks, _, id, mut p) = ask_fixture("binding-barrier");
    let session = format!("cutex.binding-{}", uuid::Uuid::new_v4().simple());
    asks.with_state(|s| {
        let mut l = s.leases.remove(TARGET).unwrap();
        l.cutex_session_id = session.clone();
        s.leases.insert(session.clone(), l);
        s.asks.get_mut(&id).unwrap().target_cutex_session_id = session.clone();
    })
    .unwrap();
    p.lease = Some(asks.snapshot().unwrap().leases[&session].clone());
    p.phase = Phase::Uncertain;
    let store = Store::open_default().unwrap();
    store
        .update(|s| s.sessions.entry(session.clone()).or_default().plan = Some(p.clone()))
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let observer = scope.spawn(|| {
            barrier.wait(); // Lease rebound; durable plan is still Binding.
            let turn = failed("new-bound-turn");
            store
                .update(|s| {
                    s.terminal(&session, "t", p.runtime_generation, &turn, None, Utc::now())
                })
                .unwrap();
            crate::asks::observe::observe(
                &asks,
                &session,
                "turn/completed",
                &json!({"threadId":"t","turn":turn}),
                Utc::now(),
            )
            .unwrap();
            assert!(asks.snapshot().unwrap().answers.is_empty());
            barrier.wait();
        });
        begin_binding(&store, &session, &p, "new-bound-turn").unwrap();
        rebind(&asks, &session, &p, "new-bound-turn").unwrap();
        barrier.wait();
        barrier.wait();
        observer.join().unwrap();
        // Restart/recovery resumes from the durable transition, with no new send.
        let restored = Store::open_default().unwrap().snapshot().unwrap().sessions[&session]
            .plan
            .clone()
            .unwrap();
        assert_eq!(restored.phase, Phase::Binding);
        begin_binding(&store, &session, &restored, "new-bound-turn").unwrap();
        rebind(&asks, &session, &restored, "new-bound-turn").unwrap();
        let (early, _) = finish_binding(&store, &session, &restored, "new-bound-turn").unwrap();
        let end = early.unwrap();
        let lease = asks.snapshot().unwrap().leases[&session].clone();
        store
            .update(|s| {
                s.terminal(
                    &session,
                    "t",
                    p.runtime_generation,
                    &end,
                    Some(lease),
                    Utc::now(),
                )
            })
            .unwrap();
        crate::asks::observe::observe(
            &asks,
            &session,
            "turn/completed",
            &json!({"threadId":"t","turn":end}),
            Utc::now(),
        )
        .unwrap();
    });
    assert!(asks.snapshot().unwrap().answers.is_empty());
    let next = store.snapshot().unwrap().sessions[&session]
        .plan
        .clone()
        .unwrap();
    assert_eq!(next.phase, Phase::Pending);
    assert_eq!(next.attempt, 2);
    assert_eq!(next.turn_id, "new-bound-turn");
}

#[test]
fn three_delays_duplicate_terminal_and_exhaustion() {
    let mut s = State::default();
    for attempt in 1..=3 {
        let at = now() + chrono::Duration::seconds(attempt as i64 * 200);
        let id = format!("turn-{attempt}");
        assert!(s.terminal("s", "thread", 9, &failed(&id), None, at));
        let p = pending(&s, "s");
        assert_eq!(p.attempt, attempt);
        assert_eq!(
            p.at,
            stamp(at + chrono::Duration::seconds(DELAYS[(attempt - 1) as usize]))
        );
        let before = s.clone();
        assert!(!s.terminal(
            "s",
            "thread",
            9,
            &failed(&id),
            None,
            at + chrono::Duration::seconds(1)
        ));
        assert_eq!(s, before);
        assert!(s.claim("s", &p.plan_id, at).is_none());
        assert!(s
            .claim(
                "s",
                &p.plan_id,
                at + chrono::Duration::seconds(DELAYS[(attempt - 1) as usize])
            )
            .is_some());
        assert!(s
            .claim("s", &p.plan_id, at + chrono::Duration::seconds(150))
            .is_none());
        let p = s.sessions.get_mut("s").unwrap().plan.as_mut().unwrap();
        p.phase = Phase::Running;
        p.turn_id = format!("turn-{}", attempt + 1);
    }
    assert!(s.terminal(
        "s",
        "thread",
        9,
        &failed("turn-4"),
        None,
        now() + chrono::Duration::seconds(900)
    ));
    assert!(s.sessions["s"].plan.is_none());
    assert_eq!(s.view("s")["last"]["result"], "exhausted");
    assert_eq!(s.view("s")["last"]["attempt"], 3);
}

#[test]
fn success_generation_and_early_receipt_fences() {
    let mut s = State::default();
    s.terminal("s", "t", 3, &failed("a"), None, now());
    let p = pending(&s, "s");
    assert!(!s.terminal("s", "t", 4, &failed("b"), None, now()));
    s.claim("s", &p.plan_id, now() + chrono::Duration::seconds(15));
    let end = json!({"id":"b","status":"completed"});
    assert!(s.terminal("s", "t", 3, &end, None, now()));
    assert_eq!(pending(&s, "s").early_terminal, Some(end.clone()));
    assert!(s.holds("s", Some("a")));
    let p = s.sessions.get_mut("s").unwrap().plan.as_mut().unwrap();
    p.phase = Phase::Running;
    p.turn_id = "b".into();
    assert!(s.terminal("s", "t", 3, &end, None, now()));
    assert!(!s.holds("s", Some("b")));
    assert!(s.sessions["s"].plan.is_none());
}

#[test]
fn durable_revision_compare_cancel_and_already_fired() {
    let dir = root();
    let store = Store::at(dir.join("retry.json"));
    let asks = AskStore::at(dir.join("asks"));
    store
        .update(|s| s.terminal("s", "t", 3, &failed("a"), None, now()))
        .unwrap();
    let state = store.snapshot().unwrap();
    let p = pending(&state, "s");
    let rev = state.sessions["s"].revision;
    assert!(!compare_cancel(&store, &asks, "s", "other", rev).unwrap());
    assert!(!compare_cancel(&store, &asks, "s", &p.plan_id, rev + 1).unwrap());
    assert_eq!(store.snapshot().unwrap(), state);
    assert!(compare_cancel(&store, &asks, "s", &p.plan_id, rev).unwrap());
    assert!(!compare_cancel(&store, &asks, "s", &p.plan_id, rev).unwrap());
    store
        .update(|s| s.terminal("s", "t", 3, &failed("b"), None, now()))
        .unwrap();
    let s = store.snapshot().unwrap();
    let p = pending(&s, "s");
    store
        .update(|s| s.claim("s", &p.plan_id, now() + chrono::Duration::seconds(15)))
        .unwrap();
    assert!(!compare_cancel(
        &store,
        &asks,
        "s",
        &p.plan_id,
        store.snapshot().unwrap().sessions["s"].revision
    )
    .unwrap());
    assert!(
        Store::at(dir.join("retry.json"))
            .snapshot()
            .unwrap()
            .sessions["s"]
            .revision
            > rev
    );
}

fn ask_fixture(label: &str) -> (AskStore, Store, String, Plan) {
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::store::tests::{new_ask, temp_store, TARGET};
    let (world, _) = world(label);
    let (asks, dir) = temp_store(label);
    let rt = FakeRuntime::idle(&[TARGET]);
    let id = asks
        .with_state(|s| {
            s.create_ask(new_ask(label, "work"), Utc::now())
                .unwrap()
                .0
                .ask_id
        })
        .unwrap();
    crate::asks::scheduler::tick(&world, &rt, &asks, Utc::now()).unwrap();
    let lease = asks.snapshot().unwrap().leases[TARGET].clone();
    let store = Store::at(dir.join("retry.json"));
    store
        .update(|s| {
            s.terminal(
                TARGET,
                "t",
                lease.runtime_generation,
                &failed(lease.turn_id.as_deref().unwrap()),
                Some(lease),
                now(),
            )
        })
        .unwrap();
    let p = pending(&store.snapshot().unwrap(), TARGET);
    (asks, store, id, p)
}

#[test]
fn receipt_rebind_is_idempotent_and_old_completion_cannot_end_new_turn() {
    use crate::asks::store::tests::TARGET;
    let (asks, _store, id, p) = ask_fixture("retry-rebind");
    let original = asks.snapshot().unwrap().asks[&id].attempts[0]
        .native_message_id
        .clone();
    rebind(&asks, TARGET, &p, "continued").unwrap();
    rebind(&asks, TARGET, &p, "continued").unwrap(); // crash between stores
    crate::asks::observe::finalize(&asks, TARGET, &id, 1, &p.failed_turn, None, Utc::now())
        .unwrap();
    assert!(asks.snapshot().unwrap().answers.is_empty());
    let completed = json!({"id":"continued","status":"completed","items":[{"type":"agentMessage","text":"done"}]});
    for _ in 0..2 {
        crate::asks::observe::finalize(&asks, TARGET, &id, 1, &completed, None, Utc::now())
            .unwrap();
    }
    let s = asks.snapshot().unwrap();
    assert_eq!(s.answers.len(), 1);
    assert_eq!(s.asks[&id].attempts.len(), 1);
    assert_eq!(s.asks[&id].attempts[0].native_message_id, original);
    assert_eq!(
        s.deliveries
            .values()
            .filter(|d| d.kind == DeliveryKind::Answer)
            .count(),
        1
    );
    assert!(!s.leases.contains_key(TARGET));
    assert!(rebind(&asks, TARGET, &p, "another").is_err());
}

#[test]
fn owner_stop_ends_held_attempt_and_unknown_send_is_never_reported_failed() {
    use crate::asks::store::tests::TARGET;
    for uncertain in [false, true] {
        let (asks, store, id, _) = ask_fixture(if uncertain {
            "retry-unknown"
        } else {
            "retry-stop"
        });
        if uncertain {
            store
                .update(|s| {
                    s.sessions
                        .get_mut(TARGET)
                        .unwrap()
                        .plan
                        .as_mut()
                        .unwrap()
                        .phase = Phase::Uncertain
                })
                .unwrap();
        }
        assert!(cancel_with(&store, &asks, TARGET, "ownerStop").unwrap());
        assert!(!cancel_with(&store, &asks, TARGET, "ownerStop").unwrap());
        let s = asks.snapshot().unwrap();
        assert!(!s.leases.contains_key(TARGET));
        assert_eq!(s.answers.len(), 1);
        assert_eq!(s.asks[&id].attempts[0].state, AttemptState::Interrupted);
        assert_eq!(
            s.asks[&id].attempts[0].interrupt_reason,
            Some(if uncertain {
                InterruptReason::OutcomeUnknown
            } else {
                InterruptReason::OwnerStopped
            })
        );
    }
}

#[test]
fn failure_before_send_and_restart_settle_original_once() {
    use crate::asks::store::tests::TARGET;
    for phase in [Phase::Pending, Phase::Firing] {
        let (asks, store, id, _) = ask_fixture("retry-pre-send");
        store
            .update(|s| {
                s.sessions
                    .get_mut(TARGET)
                    .unwrap()
                    .plan
                    .as_mut()
                    .unwrap()
                    .phase = phase.clone()
            })
            .unwrap();
        cancel_with(&store, &asks, TARGET, "managementRestarted").unwrap();
        let s = asks.snapshot().unwrap();
        assert_eq!(s.answers.len(), 1);
        assert_eq!(s.asks[&id].attempts[0].state, AttemptState::Failed);
        assert!(!s.leases.contains_key(TARGET));
    }
}

#[test]
fn cancellation_never_settles_a_replacement_lease_or_running_turn() {
    use crate::asks::store::tests::TARGET;
    let (asks, store, id, p) = ask_fixture("retry-replacement");
    rebind(&asks, TARGET, &p, "continued").unwrap();
    cancel_with(&store, &asks, TARGET, "managementRestarted").unwrap();
    let s = asks.snapshot().unwrap();
    assert!(s.answers.is_empty());
    assert_eq!(
        s.asks[&id].attempts[0].turn_id.as_deref(),
        Some("continued")
    );
}

#[test]
fn cancel_method_covers_owner_actions_but_not_reads() {
    for method in [
        "cutex/userInput/submit",
        "cutex/turn/stop",
        "cutex/runtime/offline",
        "cutex/runtime/close",
        "cutex/session/retire",
        "cutex/session/delete",
        "cutex/agent/pause",
        "cutex/userInput/queue/pause",
    ] {
        assert!(cancel_method(method).is_some(), "{method}");
    }
    assert!(cancel_method("thread/read").is_none());
}

fn http(method: &str, path: &str, body: Value) -> (u16, Value) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    handle(
        &mut server,
        &SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            headers: Default::default(),
            body: serde_json::to_vec(&body).unwrap(),
        },
    )
    .unwrap();
    drop(server);
    let mut bytes = String::new();
    client.read_to_string(&mut bytes).unwrap();
    let (headers, body) = bytes.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}

#[test]
fn http_contract_get_and_fenced_delete_match_desktop() {
    let id = format!("cutex.retry-http-{}", uuid::Uuid::new_v4().simple());
    let store = Store::open_default().unwrap();
    store
        .update(|s| s.terminal(&id, "t", 3, &failed("a"), None, now()))
        .unwrap();
    let (code, body) = http("GET", &format!("{PATH}?sessionId={id}"), Value::Null);
    assert_eq!(code, 200);
    let view = &body["sessions"][0];
    assert_eq!(view["cutexSessionId"], id);
    assert_eq!(view["pending"]["maxAttempts"], 3);
    let path = format!("{PATH}/{id}/pending");
    for invalid in [
        json!({}),
        json!({"planId":"p"}),
        json!({"planId":"p","revision":-1}),
        json!({"planId":"p","revision":1.5}),
    ] {
        assert_eq!(http("DELETE", &path, invalid).0, 400);
    }
    assert_eq!(
        http(
            "DELETE",
            &path,
            json!({"planId":"other","revision":view["revision"]})
        )
        .0,
        409
    );
    let body = json!({"planId":view["pending"]["planId"],"revision":view["revision"]});
    let (code, result) = http("DELETE", &path, body.clone());
    assert_eq!(code, 200);
    assert!(result["modelOverloadRetry"]["pending"].is_null());
    assert_eq!(http("DELETE", &path, body).0, 409);
}

#[test]
fn concurrent_claim_and_compare_cancel_have_one_winner() {
    let dir = root();
    let store = Store::at(dir.join("retry.json"));
    store
        .update(|s| s.terminal("s", "t", 3, &failed("a"), None, now()))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let plan = pending(&snapshot, "s");
    let revision = snapshot.sessions["s"].revision;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let first = barrier.clone();
    let path = dir.clone();
    let id = plan.plan_id.clone();
    let task = std::thread::spawn(move || {
        first.wait();
        let lane = crate::management::v2::session_coordination::session_lane("s").unwrap();
        let _g = lane.lock().unwrap();
        Store::at(path.join("retry.json"))
            .update(|s| s.claim("s", &id, now() + chrono::Duration::seconds(15)))
            .unwrap()
            .is_some()
    });
    barrier.wait();
    let cancelled = compare_cancel(
        &store,
        &AskStore::at(dir.join("asks")),
        "s",
        &plan.plan_id,
        revision,
    )
    .unwrap();
    let fired = task.join().unwrap();
    assert_ne!(cancelled, fired);
}

#[test]
fn actual_observer_holds_failed_attempt_until_continuation_finishes() {
    use crate::asks::store::tests::TARGET;
    let (asks, _, ask_id, mut plan) = ask_fixture("retry-observer");
    let session = format!("cutex.held-{}", uuid::Uuid::new_v4().simple());
    asks.with_state(|s| {
        let mut lease = s.leases.remove(TARGET).unwrap();
        lease.cutex_session_id = session.clone();
        s.leases.insert(session.clone(), lease);
        s.asks.get_mut(&ask_id).unwrap().target_cutex_session_id = session.clone();
    })
    .unwrap();
    let lease = asks.snapshot().unwrap().leases[&session].clone();
    plan.lease = Some(lease.clone());
    let journal = Store::open_default().unwrap();
    journal
        .update(|s| {
            s.terminal(
                &session,
                "t",
                lease.runtime_generation,
                &plan.failed_turn,
                Some(lease),
                now(),
            )
        })
        .unwrap();
    crate::asks::observe::observe(
        &asks,
        &session,
        "turn/completed",
        &json!({"threadId":"t","turn":plan.failed_turn}),
        Utc::now(),
    )
    .unwrap();
    assert!(asks.snapshot().unwrap().answers.is_empty());
    assert!(asks.snapshot().unwrap().leases.contains_key(&session));
    rebind(&asks, &session, &plan, "new-turn").unwrap();
    journal
        .update(|s| {
            let p = s.sessions.get_mut(&session).unwrap().plan.as_mut().unwrap();
            p.phase = Phase::Running;
            p.turn_id = "new-turn".into();
        })
        .unwrap();
    let finished = json!({"id":"new-turn","status":"completed","items":[{"type":"agentMessage","text":"continued answer"}]});
    journal
        .update(|s| {
            s.terminal(
                &session,
                "t",
                plan.runtime_generation,
                &finished,
                None,
                Utc::now(),
            )
        })
        .unwrap();
    crate::asks::observe::observe(
        &asks,
        &session,
        "turn/completed",
        &json!({"threadId":"t","turn":finished}),
        Utc::now(),
    )
    .unwrap();
    let state = asks.snapshot().unwrap();
    assert_eq!(state.answers.len(), 1);
    assert_eq!(state.asks[&ask_id].attempts.len(), 1);
    let answer = state.answers.values().next().unwrap();
    assert_eq!(answer.status, AnswerStatus::Completed);
    assert!(!state.leases.contains_key(&session));
}
