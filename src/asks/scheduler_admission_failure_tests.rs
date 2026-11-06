use super::*;
use super::tests::{world, FakeRuntime};
use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

/// Anonymized shape of the EVA r276 incident: the child was created while
/// its root was open, then the root answered while the receiver was held.
fn queued_child(state: &mut AskState, now: DateTime<Utc>, closed_root: bool) -> (String, String) {
    let mut root_request = new_ask("root", "parent");
    root_request.requester = Requester::Agent { cutex_session_id: TARGET.into(), name: None };
    root_request.target_cutex_session_id = REQUESTER.into();
    let root = state.create_ask(root_request, now).unwrap().0.ask_id;
    state.update_ask(&root, now, |ask| {
        ask.execution = ExecutionState::Running;
        let mut attempt = AttemptRecord::submitting(1, "root-message".into(), 1, now_rfc3339(now));
        attempt.state = AttemptState::Running;
        attempt.turn_id = Some("root-turn".into());
        ask.attempts.push(attempt);
    });
    state.leases.insert(REQUESTER.into(), AdmissionLease {
        cutex_session_id: REQUESTER.into(), holder: LeaseHolder::Ask { ask_id: root.clone(), attempt: 1 },
        native_message_id: "root-message".into(), runtime_generation: 1,
        turn_id: Some("root-turn".into()), since: now_rfc3339(now),
    });
    let mut child_request = new_ask("child", "waiting child");
    child_request.parent_ask = Some(root.clone());
    let child = state.create_ask(child_request, now).unwrap().0.ask_id;
    assert!(state.asks[&child].source_lineage.is_some());
    let host = super::super::routing_policy::integration::local_host_id();
    let mut encoded = serde_json::to_value(&*state.asks[&child]).unwrap();
    let fixture: Value = serde_json::from_str(include_str!("testdata/queued-closed-root.json")).unwrap();
    for (key, value) in fixture.as_object().unwrap() { encoded[key] = value.clone(); }
    // This optional field did not exist in the persisted old record.
    assert!(encoded.get("admissionFailure").is_none());
    encoded["parentAsk"] = json!(root);
    encoded["routingRequest"] = json!({
        "projectId":"alpha", "delegationRef":{"hostId":host,"id":child},
        "requesterRef":{"hostId":host,"id":REQUESTER},
        "targetRef":{"hostId":host,"id":TARGET},
        "parent":{"hostId":host,"id":root}, "root":{"hostId":host,"id":root},
        "sourceAttemptId":format!("{root}#1"), "sourceTurnId":"root-turn",
        "brief":"legacy fixture", "attachments":[]
    });
    state.asks.insert(child.clone(), serde_json::from_value::<AskRecord>(encoded).unwrap().into());
    if closed_root {
        state.leases.remove(REQUESTER);
        state.update_ask(&root, now, |ask| {
            ask.execution = ExecutionState::Answered;
            ask.closed = Some(CloseOutcome::Completed);
            ask.closed_at = Some(now_rfc3339(now));
            ask.attempts[0].state = AttemptState::Answered;
            ask.attempts[0].ended_at = Some(now_rfc3339(now));
        });
    }
    (root, child)
}

#[test]
fn admission_failure_old_closed_root_ends_on_first_pass_and_frees_owner_head_once() {
    let (world, world_dir) = world("permanent-admission");
    let (store, dir) = temp_store("permanent-admission");
    let now = Utc::now();
    let (root, child) = store.with_state(|state| queued_child(state, now, true)).unwrap();
    let queue = store.queue_repository().unwrap();
    let params = crate::management::v2::user_input::parse_user_input_submit_params(&json!({
        "clientUserMessageId":"owner-after-child", "origin":{"kind":"other","clientId":"test"},
        "strategy":"queue", "input":[{"type":"text","text":"owner"}]
    })).unwrap();
    queue.enqueue(TARGET, "thread", "owner-request", &params).unwrap();
    let owner = queue.list(TARGET).unwrap().1.remove(0);
    let before = store.snapshot().unwrap();
    let root_before = before.asks[&root].clone();
    assert!(!owner_may_flush(&before, TARGET, &queue.admission_with_state(TARGET, &before).unwrap(), Some(&owner.queue_id)));
    // Reopen persisted legacy rows as an upgrade/restart would. No importer.
    drop(store);
    let store = AskStore::at(&dir);
    let runtime = FakeRuntime::idle(&[TARGET]);
    tick(&world, &runtime, &store, now).unwrap();
    let after = store.snapshot().unwrap();
    let failed = &after.asks[&child];
    assert_eq!(failed.execution, ExecutionState::Failed);
    assert_eq!(failed.admission_failure.as_deref(), Some("delegation_root_closed"));
    assert!(failed.attempts.is_empty());
    assert!(failed.hold.is_none());
    assert_eq!(failed.closed, Some(CloseOutcome::Completed));
    assert_eq!(after.asks[&root], root_before);
    assert!(runtime.submitted.borrow().iter().all(|(sid, _)| sid != TARGET));
    assert!(owner_may_flush(&after, TARGET, &queue.admission_with_state(TARGET, &after).unwrap(), Some(&owner.queue_id)));
    assert_eq!(queue.list(TARGET).unwrap().1[0], owner);
    let deliveries: Vec<_> = after.deliveries.values().filter(|d| d.ask_id == child).collect();
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].ref_id.starts_with("delegation_root_closed@"));
    assert!(!deliveries[0].passive);
    assert_eq!(crate::asks::view::owner_view(&after, failed)["admissionFailure"], "delegation_root_closed");
    assert_eq!(crate::asks::view::summary(&after, failed)["admissionFailure"], "delegation_root_closed");
    assert!(crate::asks::render::state_note("delegation_root_closed", TARGET, &child).unwrap().contains("closed as failed"));
    tick(&world, &runtime, &store, now + chrono::Duration::seconds(1)).unwrap();
    assert_eq!(store.snapshot().unwrap().asks[&child], *failed);
    assert_eq!(store.snapshot().unwrap().deliveries.values().filter(|d| d.ask_id == child).count(), 1);
    drop(queue);
    drop(store);
    // SQLite caches may retain handles until this test process exits on
    // Windows; the marked private test-home runner cleans the enclosing TEMP.
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(world_dir);
}

#[test]
fn admission_failure_runtime_waits_do_not_end_open_root_child() {
    for reason in ["busy", "quota", "offline", "paused"] {
        let (world, world_dir) = world(reason);
        let (store, dir) = temp_store(reason);
        let now = Utc::now();
        let (_, child) = store.with_state(|state| queued_child(state, now, false)).unwrap();
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.set(REQUESTER, |s| s.active_turn_id = Some("root-turn".into()));
        match reason {
            "busy" => runtime.set(TARGET, |s| s.active_turn_id = Some("owner-turn".into())),
            "quota" => runtime.set(TARGET, |s| s.blocked = Some(BlockedReason::Quota)),
            "offline" => runtime.set(TARGET, |s| s.online = false),
            "paused" => { store.with_state(|s| { crate::asks::agent_pause::pause(s, TARGET, "owner", None, now); }).unwrap(); },
            _ => unreachable!(),
        }
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert!(state.asks[&child].is_open(), "{reason}");
        assert_eq!(state.asks[&child].execution, ExecutionState::Queued, "{reason}");
        assert!(state.asks[&child].admission_failure.is_none(), "{reason}");
        assert!(state.asks[&child].attempts.is_empty(), "{reason}");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(world_dir);
    }
}

#[test]
fn admission_failure_upgrade_clears_closed_root_while_offline_or_paused_without_waking() {
    for paused in [false, true] {
        let (world, world_dir) = world("upgrade-offline");
        let (store, dir) = temp_store("upgrade-offline");
        let now = Utc::now();
        let (root, child) = store.with_state(|state| {
            let ids = queued_child(state, now, true);
            state.owner_offline.insert(TARGET.into());
            if paused { crate::asks::agent_pause::pause(state, TARGET, "owner", None, now); }
            ids
        }).unwrap();
        let before = store.snapshot().unwrap();
        drop(store);
        let store = AskStore::at(&dir);
        let runtime = FakeRuntime::default();
        tick(&world, &runtime, &store, now).unwrap();
        let after = store.snapshot().unwrap();
        assert_eq!(after.asks[&child].execution, ExecutionState::Failed);
        assert_eq!(after.asks[&child].admission_failure.as_deref(), Some("delegation_root_closed"));
        assert!(after.asks[&child].attempts.is_empty());
        assert_eq!(after.asks[&root], before.asks[&root]);
        assert_eq!(after.owner_offline, before.owner_offline);
        assert_eq!(after.presence.get(TARGET), before.presence.get(TARGET));
        assert_eq!(after.pauses, before.pauses);
        assert!(runtime.wakes.borrow().is_empty());
        assert!(runtime.managed_wakes.borrow().is_empty());
        assert!(runtime.reconnects.borrow().is_empty());
        assert!(runtime.target_probes.borrow().iter().all(|id| id != TARGET));
        assert!(runtime.submitted.borrow().is_empty());
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(world_dir);
    }
}

#[test]
fn admission_failure_classifies_binding_refusals_but_preserves_review_and_unknown_waits() {
    use crate::asks::routing_policy::RoutingError::*;
    for error in [RootClosed, GrantRevoked, GrantExpired, GrantExhausted, GrantStale, ScopeMismatch, DigestMismatch, TargetNotGranted] {
        let mut state = AskState::default();
        let now = Utc::now();
        let ask = state.create_ask(new_ask("error", "work"), now).unwrap().0.ask_id;
        let error = crate::asks::routing_policy::integration::error(error);
        assert!(crate::asks::admission_failure::end(&mut state, &ask, &error, now));
        assert_eq!(state.asks[&ask].admission_failure.as_deref(), Some(error.code));
        assert!(state.asks[&ask].attempts.is_empty());
    }
    for code in [RootUnverified.code(), ReviewerGrantRequired.code(), ApprovalStale.code(), "handover_pending", "persistence_unavailable", "host_busy", "agent_paused", "quota"] {
        let mut state = AskState::default();
        let now = Utc::now();
        let ask = state.create_ask(new_ask("wait", "work"), now).unwrap().0.ask_id;
        let before = state.clone();
        assert!(!crate::asks::admission_failure::end(&mut state, &ask, &AskError::new(code, "wait"), now));
        assert_eq!(state, before, "{code}");
    }
}
