//! Private regressions for scoped lineage bounds, coverage, and cold-root stop.
use super::*;
use crate::asks::{
    model::{AttemptRecord, AttemptState, CloseOutcome, ExecutionState, Requester},
    store::{tests as fixtures, AskState, RuntimeCommand},
};
use serde_json::Value;

const HOST: &str = "bounds-host";

fn reference(id: &str) -> DelegationRef {
    DelegationRef::new(HOST, id).unwrap()
}

fn retain_root(lineage: &mut LineageState, reference: DelegationRef) {
    let record = LineageRecord {
        root: Some(reference.clone()),
        reference,
        parent: ParentFact::None,
        follow_up: None,
    };
    lineage
        .records
        .insert(reference_key(&record.reference), record);
}

fn retain_child(lineage: &mut LineageState, child: DelegationRef, parent: &DelegationRef) {
    let record = LineageRecord {
        reference: child,
        parent: ParentFact::Verified(ParentOccurrence {
            reference: parent.clone(),
            attempt_id: format!("{}#1", parent.id()),
            owner_session_id: fixtures::TARGET.into(),
            runtime_generation: 7,
            turn_id: "synthetic-parent-turn".into(),
            source_event_id: "synthetic-source".into(),
        }),
        root: Some(parent.clone()),
        follow_up: None,
    };
    lineage
        .records
        .insert(reference_key(&record.reference), record);
}

fn unrelated_history(lineage: &mut LineageState) {
    let unrelated = reference("unrelated-root");
    retain_root(lineage, unrelated.clone());
    // One large unrelated component, not just a collection of unlinked roots.
    for n in 0..MAX_LINEAGE_NODES + 1 {
        retain_child(lineage, reference(&format!("unrelated-{n}")), &unrelated);
    }
}

fn insert_ask(state: &mut AskState, id: &str, project: &str) -> DelegationRef {
    let mut request = fixtures::new_ask(id, "synthetic bounds fixture");
    request.ask_id = Some(id.into());
    request.project_id = Some(project.into());
    let ask = AskState::provisional(&request, Utc::now());
    state.asks.insert(id.into(), ask);
    let reference = reference(id);
    retain_root(&mut state.lineage, reference.clone());
    reference
}

fn access() -> NodeAccess {
    NodeAccess {
        visible: true,
        available: true,
        read_attempts: true,
        stop: true,
        stop_jobs: true,
        ..Default::default()
    }
}

fn scope(project: Option<&str>, refs: impl IntoIterator<Item = DelegationRef>) -> ReadScope {
    ReadScope {
        caller: "owner".into(),
        project: project.map(str::to_owned),
        permission_revision: 1,
        resources: refs
            .into_iter()
            .map(|reference| (reference, access()))
            .collect(),
    }
}

fn page(state: &AskState, scope: &ReadScope, root: Option<&DelegationRef>) -> Value {
    TreeSnapshot::capture(state, &state.lineage, HOST, scope, root)
        .unwrap()
        .page("owner", 1, None, 100)
        .unwrap()
}

#[test]
fn self_stop_of_running_root_survives_large_unrelated_history_and_preserves_receipt() {
    let (store, _dir) = fixtures::temp_store("lineage-self-stop-bounds");
    let now = Utc::now();
    let root = store
        .with_state(|state| {
            let (ask, _) = state
                .create_ask(fixtures::new_ask("bounded-self-stop", "work"), now)
                .unwrap();
            state.update_ask(&ask.ask_id, now, |ask| {
                ask.execution = ExecutionState::Running;
                let mut attempt = AttemptRecord::submitting(
                    1,
                    "synthetic-native".into(),
                    7,
                    crate::asks::now_rfc3339(now),
                );
                attempt.state = AttemptState::Running;
                attempt.turn_id = Some("synthetic-running-turn".into());
                ask.attempts.push(attempt);
            });
            unrelated_history(&mut state.lineage);
            ask.ask_id
        })
        .unwrap();
    let before = store.snapshot().unwrap();
    let result = stop(
        &store,
        None,
        &root,
        Cascade::None,
        false,
        "self-stop-large-history",
        None,
        now,
    )
    .unwrap();
    let after = store.snapshot().unwrap();
    assert_eq!(result["nodeOutcomes"][0]["phase"], "pendingInterrupt");
    assert_eq!(result["receipt"]["effectState"], "partial");
    assert!(after.asks[&root].current_attempt().unwrap().stop_requested);
    assert!(after.commands.iter().any(|queued|matches!(&queued.command,RuntimeCommand::StopAttempt{ask_id,attempt,..} if ask_id==&root&&*attempt==1)));
    let root_ref = DelegationRef::new(
        crate::asks::routing_policy::integration::local_host_id(),
        &root,
    )
    .unwrap();
    assert!(!after.lineage.stop_fences[&reference_key(&root_ref)].descendants);
    assert_eq!(
        after.lineage.records, before.lineage.records,
        "unrelated evidence is retained exactly"
    );
    let replay = stop(
        &store,
        None,
        &root,
        Cascade::None,
        false,
        "self-stop-large-history",
        None,
        now,
    )
    .unwrap();
    assert_eq!(replay, result);
    assert_eq!(
        store.snapshot().unwrap(),
        after,
        "replay queues no second effect"
    );
}

#[test]
fn descendant_component_ignores_unrelated_size_and_cycles() {
    let mut lineage = LineageState::default();
    unrelated_history(&mut lineage);
    let root = reference("selected-root");
    let child = reference("selected-child");
    retain_root(&mut lineage, root.clone());
    retain_child(&mut lineage, child.clone(), &root);
    // Keep the cycle separate from the oversized unrelated component: either
    // limit can legitimately be detected first when both affect the same root.
    let other = reference("unrelated-cycle-root");
    let cycle = reference("unrelated-cycle");
    retain_child(&mut lineage, other.clone(), &cycle);
    retain_child(&mut lineage, cycle, &other);
    assert_eq!(
        super::stop::verified_descendants(&lineage, &root, true).unwrap(),
        BTreeSet::from([root.clone(), child])
    );
    assert_eq!(
        super::stop::verified_descendants(&lineage, &other, false).unwrap(),
        BTreeSet::from([other.clone()])
    );
    assert_eq!(
        super::stop::verified_descendants(&lineage, &other, true)
            .unwrap_err()
            .code,
        "lineage_cycle"
    );
}

#[test]
fn component_limit_is_on_discovery_and_self_stop_still_works() {
    let mut lineage = LineageState::default();
    let root = reference("wide-root");
    retain_root(&mut lineage, root.clone());
    for n in 0..MAX_LINEAGE_NODES - 1 {
        retain_child(&mut lineage, reference(&format!("wide-{n}")), &root);
    }
    assert_eq!(
        super::stop::verified_descendants(&lineage, &root, true)
            .unwrap()
            .len(),
        MAX_LINEAGE_NODES
    );
    retain_child(&mut lineage, reference("wide-overflow"), &root);
    let before = lineage.clone();
    assert_eq!(
        super::stop::verified_descendants(&lineage, &root, true)
            .unwrap_err()
            .code,
        "lineage_limit"
    );
    assert_eq!(
        super::stop::verified_descendants(&lineage, &root, false).unwrap(),
        BTreeSet::from([root])
    );
    assert_eq!(lineage, before);
}

#[test]
fn reachable_depth_limit_does_not_restrict_self_stop() {
    let mut lineage = LineageState::default();
    let root = reference("deep-root");
    retain_root(&mut lineage, root.clone());
    let mut parent = root.clone();
    for n in 0..MAX_LINEAGE_DEPTH {
        let child = reference(&format!("deep-{n}"));
        retain_child(&mut lineage, child.clone(), &parent);
        parent = child;
    }
    assert_eq!(
        super::stop::verified_descendants(&lineage, &root, true)
            .unwrap()
            .len(),
        MAX_LINEAGE_DEPTH + 1
    );
    retain_child(&mut lineage, reference("deep-overflow"), &parent);
    assert_eq!(
        super::stop::verified_descendants(&lineage, &root, true)
            .unwrap_err()
            .code,
        "lineage_depth"
    );
    assert_eq!(
        super::stop::verified_descendants(&lineage, &root, false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn tree_limits_only_the_selected_project_root_and_visible_rows() {
    let mut state = AskState::default();
    let root = insert_ask(&mut state, "one-root", "selected");
    let mut selected = scope(Some("selected"), [root.clone()]);
    for n in 0..MAX_LINEAGE_NODES + 1 {
        let reference = insert_ask(&mut state, &format!("outside-{n}"), "other-project");
        selected.resources.insert(reference, access());
    }
    for root_filter in [None, Some(&root)] {
        let result = page(&state, &selected, root_filter);
        assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(result["coverage"], "complete");
    }
    // Same project but invisible unrelated history is also outside this view.
    for ask in state.asks.values_mut() {
        ask.project_id = Some("selected".into());
    }
    selected.resources.retain(|reference, _| reference == &root);
    assert_eq!(
        page(&state, &selected, None)["nodes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // A large visible query is still bounded, instead of allocating its JSON.
    selected.resources = state
        .asks
        .keys()
        .map(|id| (reference(id), access()))
        .collect();
    assert_eq!(
        TreeSnapshot::capture(&state, &state.lineage, HOST, &selected, None)
            .err()
            .unwrap()
            .code,
        "lineage_limit"
    );
    assert_eq!(
        page(&state, &selected, Some(&root))["nodes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn verified_foreign_child_missing_local_row_is_partial_without_endpoint_or_count() {
    let mut state = AskState::default();
    let root = insert_ask(&mut state, "root-with-foreign", "selected");
    // Same bare ID on another host must remain a different canonical ref.
    let foreign = DelegationRef::new("hidden-foreign-host", root.id()).unwrap();
    retain_child(&mut state.lineage, foreign.clone(), &root);
    let before = state.clone();
    for readable in [false, true] {
        let mut selected = scope(Some("selected"), [root.clone()]);
        if readable {
            selected.resources.insert(
                foreign.clone(),
                NodeAccess {
                    visible: true,
                    available: false,
                    ..Default::default()
                },
            );
        }
        for root_filter in [None, Some(&root)] {
            let result = page(&state, &selected, root_filter);
            assert_eq!(result["coverage"], "partial");
            assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
            assert!(!result.to_string().contains("hidden-foreign-host"));
            assert!(result["unresolvedEdges"].as_array().unwrap().is_empty());
            assert!(result.get("missingCount").is_none());
        }
    }
    assert_eq!(state, before, "projection leaves all original facts intact");
}

#[test]
fn hidden_local_child_is_partial_but_unrelated_missing_foreign_child_is_not() {
    let mut state = AskState::default();
    let root = insert_ask(&mut state, "visible-root", "selected");
    let other = reference("outside-root");
    retain_root(&mut state.lineage, other.clone());
    let foreign = DelegationRef::new("hidden-foreign-host", "unrelated-child").unwrap();
    retain_child(&mut state.lineage, foreign, &other);
    let selected = scope(Some("selected"), [root.clone()]);
    assert_eq!(page(&state, &selected, None)["coverage"], "complete");
    let child = insert_ask(&mut state, "hidden-local-child", "selected");
    retain_child(&mut state.lineage, child, &root);
    for root_filter in [None, Some(&root)] {
        let result = page(&state, &selected, root_filter);
        assert_eq!(result["coverage"], "partial");
        assert!(!result.to_string().contains("hidden-local-child"));
        assert!(!result.to_string().contains("hidden-foreign-host"));
        assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn public_stop_promotes_retained_cold_root_and_commits_fence_with_receipt() {
    let (store, dir) = fixtures::temp_store("lineage-cold-root-stop");
    let old: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
    let cutoff = old + chrono::Duration::days(1);
    let now = Utc::now();
    let root = store
        .with_state(|state| {
            let mut request = fixtures::new_ask("cold-root-stop", "retained historical result");
            request.requester = Requester::Owner;
            let (ask, _) = state.create_ask(request, old).unwrap();
            state.update_ask(&ask.ask_id, old, |ask| {
                ask.execution = ExecutionState::Cancelled;
                ask.closed = Some(CloseOutcome::Stopped);
                ask.closed_at = Some(crate::asks::now_rfc3339(old));
            });
            state.notices.clear();
            ask.ask_id
        })
        .unwrap();
    let original = store.snapshot().unwrap().asks[&root].clone();
    assert_eq!(store.archive_before(Some(cutoff), now).unwrap().asks, 1);
    assert!(!store.snapshot().unwrap().asks.contains_key(&root));
    assert_eq!(
        store.lookup_snapshot(&[&root]).unwrap().asks[&root],
        original
    );
    let result = stop(
        &store,
        None,
        &root,
        Cascade::None,
        false,
        "stop-cold-root",
        None,
        now,
    )
    .unwrap();
    assert_eq!(result["nodeOutcomes"][0]["phase"], "alreadyClosed");
    let reloaded = crate::asks::store::AskStore::at(&dir);
    let state = reloaded.snapshot().unwrap();
    assert_eq!(state.asks[&root], original);
    let root_ref = DelegationRef::new(
        crate::asks::routing_policy::integration::local_host_id(),
        &root,
    )
    .unwrap();
    assert!(state
        .lineage
        .stop_fences
        .contains_key(&reference_key(&root_ref)));
    assert_eq!(
        crate::asks::operations::lookup(&state, "owner", "stop-cold-root")["result"],
        result
    );
    assert_eq!(
        stop(
            &reloaded,
            None,
            &root,
            Cascade::None,
            false,
            "stop-cold-root",
            None,
            now
        )
        .unwrap(),
        result
    );
    assert_eq!(reloaded.snapshot().unwrap(), state);
}

#[test]
fn actual_tree_projects_fixed_stop_outcomes_without_native_details_or_receipt_changes() {
    use crate::asks::model::StopDispatchOutcome;
    let now = Utc::now();
    let fixture = || {
        let mut state = AskState::default();
        let root = insert_ask(&mut state, "stop-projection-root", "selected");
        let ask = state.asks.get_mut(root.id()).unwrap();
        ask.execution = ExecutionState::Running;
        let mut attempt = AttemptRecord::submitting(
            1,
            "NATIVE-CONTROL-SHOULD-NOT-LEAK".into(),
            7,
            crate::asks::now_rfc3339(now),
        );
        attempt.state = AttemptState::Running;
        attempt.turn_id = Some("TURN-SHOULD-NOT-LEAK".into());
        ask.attempts.push(attempt);
        assert!(crate::asks::commands::capture_stop_intent(
            &mut state,
            root.id(),
            1,
            now
        ));
        state.lineage.stop_fences.insert(
            reference_key(&root),
            StopFence {
                root: root.clone(),
                action_id: "original-stop".into(),
                requested_by: "owner".into(),
                descendants: false,
                created_at: now,
            },
        );
        state.actions.insert("owner\u{1f}original-stop".into(), crate::asks::store::ActionRecord {
            tool: "delegation-control:delegation.stop".into(), request_sha256: "a".repeat(64),
            at: crate::asks::now_rfc3339(now),
            result: Some(serde_json::json!({"nodeOutcomes":[],"receipt":{"operationId":"original-operation"}})),
        });
        (state, root)
    };
    for (outcome, reason) in [
        (StopDispatchOutcome::Pending, "requested"),
        (StopDispatchOutcome::Claimed, "claimedUncertain"),
        (StopDispatchOutcome::Submitted, "interruptSubmitted"),
        (StopDispatchOutcome::Unknown, "outcomeUnknown"),
        (StopDispatchOutcome::Unsupported, "unsupported"),
        (StopDispatchOutcome::Stale, "stale"),
    ] {
        let (mut state, root) = fixture();
        let attempt = state
            .asks
            .get_mut(root.id())
            .unwrap()
            .current_attempt_mut()
            .unwrap();
        let intent = attempt.stop_intent.as_mut().unwrap();
        intent.outcome = outcome;
        if matches!(
            outcome,
            StopDispatchOutcome::Claimed
                | StopDispatchOutcome::Submitted
                | StopDispatchOutcome::Unknown
        ) {
            intent.claimed_at = Some(crate::asks::now_rfc3339(now));
        }
        attempt.last_error = Some("RPC-ERROR-SHOULD-NOT-LEAK".into());
        let before = state.clone();
        let result = page(
            &state,
            &scope(Some("selected"), [root.clone()]),
            Some(&root),
        );
        let stop = &result["nodes"][0]["stop"];
        assert_eq!(stop["phase"], "pending");
        assert_eq!(stop["reason"], reason);
        assert_eq!(stop["requestedAt"], serde_json::json!(now));
        assert_eq!(stop["operationId"], "original-operation");
        assert_eq!(stop.as_object().unwrap().len(), 4);
        for sensitive in [
            "RPC-ERROR-SHOULD-NOT-LEAK",
            "NATIVE-CONTROL-SHOULD-NOT-LEAK",
            "TURN-SHOULD-NOT-LEAK",
        ] {
            assert!(!result.to_string().contains(sensitive));
        }
        assert_eq!(state, before);
    }
    // A persisted claim is uncertain even if a retained outcome is still pending.
    let (mut claimed_state, claimed_root) = fixture();
    claimed_state
        .asks
        .get_mut(claimed_root.id())
        .unwrap()
        .current_attempt_mut()
        .unwrap()
        .stop_intent
        .as_mut()
        .unwrap()
        .claimed_at = Some(crate::asks::now_rfc3339(now));
    assert_eq!(
        page(
            &claimed_state,
            &scope(Some("selected"), [claimed_root.clone()]),
            Some(&claimed_root)
        )["nodes"][0]["stop"]["reason"],
        "claimedUncertain"
    );
    // Closure and stop in the same transaction use different timestamp precision.
    let (mut closed_state, closed_root) = fixture();
    let ask = closed_state.asks.get_mut(closed_root.id()).unwrap();
    ask.execution = ExecutionState::Interrupted;
    ask.closed = Some(CloseOutcome::Stopped);
    ask.closed_at = Some(crate::asks::now_rfc3339(now));
    let attempt = ask.current_attempt_mut().unwrap();
    attempt.state = AttemptState::Interrupted;
    attempt.stop_intent.as_mut().unwrap().outcome = StopDispatchOutcome::Submitted;
    assert_eq!(
        page(
            &closed_state,
            &scope(Some("selected"), [closed_root.clone()]),
            Some(&closed_root)
        )["nodes"][0]["stop"]["reason"],
        "interruptSubmitted"
    );
    // An already completed result gets a stop fence, not evidence that an
    // interrupt happened. Keep the existing phase and make that reason explicit.
    for original_outcome in [false, true] {
        let (mut state, root) = fixture();
        let ask = state.asks.get_mut(root.id()).unwrap();
        ask.execution = ExecutionState::Answered;
        ask.closed = Some(CloseOutcome::Completed);
        ask.closed_at = Some(crate::asks::now_rfc3339(if original_outcome {
            now
        } else {
            now - chrono::Duration::seconds(1)
        }));
        let attempt = ask.current_attempt_mut().unwrap();
        attempt.state = AttemptState::Answered;
        attempt.stop_intent = None;
        if original_outcome {
            state
                .actions
                .get_mut("owner\u{1f}original-stop")
                .unwrap()
                .result
                .as_mut()
                .unwrap()["nodeOutcomes"] =
                serde_json::json!([{"id":root.id(),"hostId":HOST,"phase":"alreadyClosed"}]);
        }
        let before = state.clone();
        let result = page(
            &state,
            &scope(Some("selected"), [root.clone()]),
            Some(&root),
        );
        assert_eq!(result["nodes"][0]["stop"]["phase"], "stopped");
        assert_eq!(result["nodes"][0]["stop"]["reason"], "alreadyClosed");
        assert_eq!(state, before);
    }
}
