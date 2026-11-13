#[derive(Clone, Copy)]
enum ReadinessScenario {
    Delayed,
    Never,
    Replaced,
    Duplicate,
    WrongEndpoint,
    NativeError,
    Stopped,
}

fn readiness_lifecycle_request(id: &CutexSessionId, restart: bool) -> AgentManagementRequest {
    AgentManagementRequest {
        schema: AgentManagementSchema::V1,
        action_id: action(if restart {
            "delayed-restart"
        } else {
            "delayed-online"
        }),
        project_id: Some(project()),
        operation: if restart {
            AgentOperation::Restart {
                cutex_session_id: id.clone(),
                allow_unlisted: false,
            }
        } else {
            AgentOperation::Online {
                cutex_session_id: id.clone(),
                allow_unlisted: false,
            }
        },
    }
}

#[test]
fn non_claude_readiness_default_keeps_the_existing_authority_hold() {
    let root = root("default-ready-authority");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let gate = std::sync::Arc::new(OnlineGate::default());
    lifecycle.state.lock().unwrap().readiness_gate = Some(gate.clone());
    let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
    std::thread::scope(|scope| {
        let _open = OpenOnDrop(gate.clone());
        let worker =
            scope.spawn(|| provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        gate.wait_entered(1);
        assert!(provider
            .store()
            .try_lock_delivery_mutations()
            .unwrap()
            .is_none());
        assert!(provider_execution_lock().try_lock().is_err());
        gate.open();
        completed(worker.join().unwrap());
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn claude_lifecycle_readiness_online_and_restart_wait_for_exact_adoption_once() {
    for restart in [false, true] {
        let root = root(&format!("claude-lifecycle-ready-{restart}"));
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let agent = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        if !restart {
            lifecycle.offline(&agent.cutex_session_id).unwrap();
        }
        lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Delayed);
        let launches = lifecycle.launch_count();
        let request = readiness_lifecycle_request(&agent.cutex_session_id, restart);
        let result = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(
            matches!(result.outcome, AgentManagementOutcome::Complete { .. }),
            "{result:?}"
        );
        assert_eq!(lifecycle.launch_count(), launches + 1);
        assert_eq!(lifecycle.bootstrap_count(), 1);
        assert_eq!(lifecycle.message_count(), 0);
        assert_eq!(
            provider.execute(&invocation("cutex.director"), &request, &lifecycle),
            result
        );
        assert_eq!(lifecycle.launch_count(), launches + 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn claude_lifecycle_readiness_owner_stop_and_replacement_cannot_finish_old_wait() {
    for restart in [false, true] {
        for replace in [false, true] {
            let root = root(&format!("claude-lifecycle-race-{restart}-{replace}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let lifecycle = FakeLifecycle::default();
            let agent = created_agent(&completed(provider.execute(
                &invocation("cutex.director"),
                &create_request("create", "worker", AgentStartMode::BootstrapOnly),
                &lifecycle,
            )));
            if !restart {
                lifecycle.offline(&agent.cutex_session_id).unwrap();
            }
            let gate = std::sync::Arc::new(OnlineGate::default());
            lifecycle.state.lock().unwrap().readiness_gate = Some(gate.clone());
            lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Delayed);
            let request = readiness_lifecycle_request(&agent.cutex_session_id, restart);
            let launches = lifecycle.launch_count();
            std::thread::scope(|scope| {
                let _open = OpenOnDrop(gate.clone());
                let worker = scope.spawn(|| {
                    provider.execute(&invocation("cutex.director"), &request, &lifecycle)
                });
                gate.wait_entered(1);
                assert!(provider
                    .store()
                    .try_lock_delivery_mutations()
                    .unwrap()
                    .is_some());
                assert!(provider_execution_lock().try_lock().is_ok());
                if replace {
                    lifecycle.advance_runtime_occurrence(&agent.cutex_session_id);
                } else {
                    lifecycle.offline(&agent.cutex_session_id).unwrap();
                }
                gate.open();
                let result = worker.join().unwrap();
                assert!(
                    !matches!(result.outcome, AgentManagementOutcome::Complete { .. }),
                    "{result:?}"
                );
                assert_eq!(lifecycle.launch_count(), launches + 1);
                assert_eq!(lifecycle.message_count(), 0);
            });
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[test]
fn claude_create_readiness_wait_releases_authority_and_fences_replacement_on_reacquire() {
    for mutation in 0..4 {
        let root = root(&format!("claude-ready-authority-{mutation}"));
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let gate = std::sync::Arc::new(OnlineGate::default());
        lifecycle.state.lock().unwrap().readiness_gate = Some(gate.clone());
        lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Delayed);
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);
        std::thread::scope(|scope| {
            let _open = OpenOnDrop(gate.clone());
            let worker = scope
                .spawn(|| provider.execute(&invocation("cutex.director"), &request, &lifecycle));
            gate.wait_entered(1);
            assert!(provider
                .store()
                .try_lock_delivery_mutations()
                .unwrap()
                .is_some());
            assert!(provider_execution_lock().try_lock().is_ok());
            let snapshot = provider.store().snapshot().unwrap();
            let id = snapshot.actions[&request.action_id]
                .known_successor_cutex_session
                .clone()
                .unwrap();
            drop(snapshot);
            // Each races while the readiness barrier is outside authority.
            match mutation {
                0 => lifecycle.advance_runtime_occurrence(&id),
                1 => lifecycle.offline(&id).unwrap(),
                2 => provider
                    .store()
                    .with_state(true, |mut state| {
                        state
                            .actions
                            .get_mut(&request.action_id)
                            .unwrap()
                            .phase_sequence += 1;
                        Ok((state, (), true))
                    })
                    .unwrap(),
                _ => lifecycle.state.lock().unwrap().readiness_pid = Some(101),
            }
            gate.open();
            let result = worker.join().unwrap();
            assert!(
                !matches!(result.outcome, AgentManagementOutcome::Complete { .. }),
                "{result:?}"
            );
            if mutation == 0 {
                assert!(serde_json::to_string(&result)
                    .unwrap()
                    .contains("claude_readiness_occurrence_changed"));
            }
            assert_eq!(lifecycle.message_count(), 0);
        });
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn claude_create_readiness_delayed_registration_completes_and_replays_task_once() {
    let root = root("claude-ready-delayed");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Delayed);
    let request = create_request("create", "worker", AgentStartMode::CustomMessage);
    let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
    assert!(
        matches!(response.outcome, AgentManagementOutcome::Complete { .. }),
        "{response:?}"
    );
    assert_eq!(lifecycle.bootstrap_count(), 1);
    assert_eq!(lifecycle.launch_count(), 1);
    assert_eq!(lifecycle.message_count(), 1);
    assert_eq!(
        provider.execute(&invocation("cutex.director"), &request, &lifecycle),
        response
    );
    assert_eq!(lifecycle.message_count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn claude_create_readiness_timeout_keeps_action_without_task_then_recovers_once() {
    let root = root("claude-ready-timeout");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Never);
    let request = create_request("create", "worker", AgentStartMode::CustomMessage);
    let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
    let wire = serde_json::to_value(&response).unwrap();
    assert!(
        wire.to_string().contains("claude_readiness_timeout"),
        "{wire}"
    );
    assert!(
        wire.to_string().contains("external outcome unknown"),
        "{wire}"
    );
    assert!(!matches!(
        response.outcome,
        AgentManagementOutcome::Complete { .. }
    ));
    assert_eq!(lifecycle.message_count(), 0);
    let snapshot = provider.store().snapshot().unwrap();
    let action = &snapshot.actions[&request.action_id];
    assert_eq!(action.phase, AgentActionPhase::Online);
    assert!(action.known_successor_cutex_session.is_some());
    drop(snapshot);
    // A new provider models creator-process recovery of the same durable action.
    drop(provider);
    let provider = AgentManagementProvider::open(&root).unwrap();
    lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Delayed);
    let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
    assert!(
        matches!(response.outcome, AgentManagementOutcome::Complete { .. }),
        "{response:?}"
    );
    assert_eq!(lifecycle.bootstrap_count(), 1);
    assert_eq!(lifecycle.launch_count(), 1);
    assert_eq!(lifecycle.message_count(), 1);
    assert_eq!(
        provider.execute(&invocation("cutex.director"), &request, &lifecycle),
        response
    );
    assert_eq!(lifecycle.message_count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn claude_create_readiness_conflicts_stop_and_native_failure_never_send_task() {
    for (index, scenario) in [
        ReadinessScenario::Replaced,
        ReadinessScenario::Duplicate,
        ReadinessScenario::WrongEndpoint,
        ReadinessScenario::NativeError,
        ReadinessScenario::Stopped,
    ]
    .into_iter()
    .enumerate()
    {
        let root = root(&format!("claude-ready-conflict-{index}"));
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        lifecycle.state.lock().unwrap().readiness_scenario = Some(scenario);
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);
        let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(
            !matches!(response.outcome, AgentManagementOutcome::Complete { .. }),
            "{response:?}"
        );
        assert_eq!(lifecycle.message_count(), 0);
        assert_eq!(lifecycle.bootstrap_count(), 1);
        assert_eq!(lifecycle.launch_count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn claude_readiness_replay_keeps_prior_pending_and_queued_delivery_truth() {
    for phase in [
        AgentActionPhase::MessagePending,
        AgentActionPhase::MessageQueued,
    ] {
        let root = root(&format!("claude-ready-delivery-{phase:?}"));
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);
        completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        let message = provider.store().snapshot().unwrap().actions[&request.action_id]
            .external_message_id
            .clone();
        // Crash after the send: Pending represents an unrecorded/uncertain
        // acknowledgement; Queued has already recorded it. Neither means unsent.
        provider
            .store()
            .with_state(true, |mut state| {
                let action = state.actions.get_mut(&request.action_id).unwrap();
                action.phase = phase;
                action.phase_sequence += 1;
                action.response = None;
                Ok((state, (), true))
            })
            .unwrap();
        lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Never);
        let result = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let wire = serde_json::to_string(&result).unwrap();
        assert!(wire.contains("claude_readiness_timeout"), "{wire}");
        assert!(wire.contains("earlier delivery"), "{wire}");
        assert!(!wire.contains("no task was sent"));
        assert_eq!(lifecycle.message_count(), 1);
        assert_eq!(
            provider.store().snapshot().unwrap().actions[&request.action_id].external_message_id,
            message
        );
        lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Delayed);
        let result = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(
            matches!(result.outcome, AgentManagementOutcome::Complete { .. }),
            "{result:?}"
        );
        assert_eq!(lifecycle.state.lock().unwrap().messages.len(), 1);
        assert_eq!(
            lifecycle.message_count(),
            if phase == AgentActionPhase::MessagePending {
                2
            } else {
                1
            }
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn claude_readiness_close_after_ready_recovery_does_not_deny_prior_delivery() {
    let root = root("claude-ready-close-after-delivery");
    let provider = AgentManagementProvider::open(&root)
        .unwrap()
        .with_fail_after_predecessor_close_once();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let predecessor = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("create-old", "worker-old", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    let request = AgentManagementRequest {
        schema: AgentManagementSchema::V1,
        action_id: action("replace"),
        project_id: Some(project()),
        operation: AgentOperation::Replace {
            bootstrap_intent: None,
            predecessor_cutex_session_id: predecessor.cutex_session_id,
            policy: AgentReplacePolicy::CloseAfterReady,
            successor: spec("worker-new"),
            start_mode: AgentStartMode::CustomMessage,
            frozen_message: Some("Exact replacement task".into()),
        },
    };
    let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
    assert!(matches!(
        first.outcome,
        AgentManagementOutcome::NoWrite { .. }
    ));
    assert_eq!(lifecycle.message_count(), 1);
    lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Never);
    let result = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
    let wire = serde_json::to_string(&result).unwrap();
    assert!(wire.contains("claude_readiness_timeout"), "{wire}");
    assert!(wire.contains("earlier delivery"), "{wire}");
    assert!(!wire.contains("no task was sent"));
    assert_eq!(lifecycle.message_count(), 1);
    lifecycle.state.lock().unwrap().readiness_scenario = Some(ReadinessScenario::Delayed);
    let result = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
    assert!(
        matches!(result.outcome, AgentManagementOutcome::Complete { .. }),
        "{result:?}"
    );
    assert_eq!(lifecycle.message_count(), 1);
    assert_eq!(lifecycle.bootstrap_count(), 2);
    std::fs::remove_dir_all(root).unwrap();
}
