use super::*;
use crate::asks::store::tests::{new_ask, REQUESTER};
use chrono::Utc;
struct Fixture {
    root: std::path::PathBuf,
    store: Store,
    asks: AskState,
    ask: AskRecord,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("director-own-{}", uuid::Uuid::new_v4()));
        let store = Store::at(&root);
        let mut asks = AskState::default();
        let (ask, _) = asks
            .create_ask(new_ask("origin", "private brief"), Utc::now())
            .unwrap();
        Self {
            root,
            store,
            asks,
            ask,
        }
    }
    fn stamp(&self) {
        self.store
            .stamp_at(&self.ask, REQUESTER, "alpha", 1)
            .unwrap();
    }
    fn plan(&self, trusted: bool, permitted: bool) -> Vec<String> {
        self.store
            .plan_handover(
                &self.asks,
                "alpha",
                REQUESTER,
                "new",
                trusted,
                "change1",
                |_| permitted,
            )
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn returning_to_the_actual_controller_supersedes_the_unaccepted_intermediate_review() {
    let f = Fixture::new();
    f.stamp();
    f.plan(false, true);
    f.store
        .plan_handover(
            &f.asks,
            "alpha",
            "new",
            REQUESTER,
            false,
            "return-a",
            |_| true,
        )
        .unwrap();
    f.plan(false, true); // replay cannot revive the old B review
    let state = f.store.snapshot().unwrap();
    assert_eq!(state.asks[&f.ask.ask_id].controller, REQUESTER);
    assert!(!state.asks[&f.ask.ask_id].owner_hold);
    assert_eq!(state.reviews.len(), 1);
    assert!(state
        .reviews
        .values()
        .all(|review| review.state == "superseded"));
}

#[test]
fn repeated_recovery_for_the_same_destination_keeps_one_owner_decision() {
    let f = Fixture::new();
    f.stamp();
    f.store
        .plan_recovery(&f.asks, "alpha", Some(REQUESTER), Some("new"), "deferred")
        .unwrap();
    f.store
        .plan_recovery(&f.asks, "alpha", None, Some("new"), "recovery")
        .unwrap();
    let state = f.store.snapshot().unwrap();
    assert_eq!(state.reviews.len(), 1);
    let original = state.reviews.values().next().unwrap();
    assert!(original.owner_only);
    assert_eq!(original.change_id, "deferred");
    assert_eq!(original.state, "pending");
    // Moving again supersedes that destination, and still leaves only one
    // actionable owner item even if recovery is rediscovered more than once.
    f.store
        .plan_recovery(&f.asks, "alpha", None, Some("third"), "move-third")
        .unwrap();
    f.store
        .plan_recovery(&f.asks, "alpha", None, Some("third"), "recovery-third")
        .unwrap();
    let state = f.store.snapshot().unwrap();
    assert_eq!(state.reviews.len(), 2);
    assert_eq!(
        state
            .reviews
            .values()
            .filter(|review| review.state == "pending")
            .count(),
        1
    );
    assert!(state.asks[&f.ask.ask_id].owner_hold);
}

#[test]
fn no_record_does_not_infer_ownership_from_target_project() {
    let f = Fixture::new();
    assert!(f.plan(true, true).is_empty());
    assert!(f.store.snapshot().unwrap().reviews.is_empty());
}
#[test]
fn closed_delegations_do_not_automatically_gain_a_new_controller() {
    let mut f = Fixture::new();
    f.stamp();
    f.asks.asks.get_mut(&f.ask.ask_id).unwrap().closed =
        Some(crate::asks::model::CloseOutcome::Accepted);
    assert!(f.plan(true, true).is_empty());
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask.ask_id].controller,
        REQUESTER
    );
    assert!(f.store.snapshot().unwrap().reviews.is_empty());
}
#[test]
fn stamp_records_immutable_origin_and_retries_do_not_reset_controller() {
    let f = Fixture::new();
    f.stamp();
    f.plan(true, true);
    f.stamp();
    let s = f.store.snapshot().unwrap();
    let o = &s.asks[&f.ask.ask_id];
    assert_eq!(o.origin_director, REQUESTER);
    assert_eq!(o.controller, "new");
    assert_eq!(o.origin_epoch, 1);
    assert_eq!(o.request_sha256, f.ask.request_sha256);
    assert_eq!(f.asks.asks[&f.ask.ask_id], f.ask);
}
#[test]
fn only_trusted_retired_replacements_auto_transfer() {
    for (trusted, permitted, automatic) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
        (false, false, false),
    ] {
        let f = Fixture::new();
        f.stamp();
        assert_eq!(!f.plan(trusted, permitted).is_empty(), automatic);
        let s = f.store.snapshot().unwrap();
        assert_eq!(
            s.reviews.values().next().unwrap().state,
            if automatic { "accepted" } else { "pending" }
        );
    }
}
#[test]
fn wrong_project_and_wrong_previous_controller_do_not_transfer() {
    let f = Fixture::new();
    f.stamp();
    assert!(f
        .store
        .plan_handover(&f.asks, "beta", REQUESTER, "new", true, "c", |_| true)
        .unwrap()
        .is_empty());
    assert!(f
        .store
        .plan_handover(&f.asks, "alpha", "other", "new", true, "c", |_| true)
        .unwrap()
        .is_empty());
    let state = f.store.snapshot().unwrap();
    let review = state.reviews.values().next().unwrap();
    assert_eq!(
        review.state, "pending",
        "an unresolved predecessor still needs an actionable current review"
    );
    assert_eq!(review.from, REQUESTER);
}

#[test]
fn consecutive_changes_retarget_unresolved_work_without_inventing_trusted_handover() {
    let f = Fixture::new();
    f.stamp();
    f.plan(false, true);
    let old = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .keys()
        .next()
        .unwrap()
        .clone();
    assert!(f
        .store
        .plan_handover(&f.asks, "alpha", "new", "third", true, "change2", |_| true)
        .unwrap()
        .is_empty());
    let state = f.store.snapshot().unwrap();
    assert_eq!(state.reviews[&old].state, "superseded");
    let next = state.reviews.values().find(|r| r.to == "third").unwrap();
    assert_eq!(next.from, REQUESTER);
    assert_eq!(next.state, "pending");
    f.store
        .decide_review(
            &next.id, 0, "third", true, &f.ask, "alpha", "third", 3, false, true,
        )
        .unwrap();
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask.ask_id].controller,
        "third"
    );
}
#[test]
fn owner_can_resolve_a_stale_review_at_the_current_epoch() {
    let f = Fixture::new();
    f.stamp();
    f.plan(false, true);
    let id = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .keys()
        .next()
        .unwrap()
        .clone();
    assert_eq!(
        f.store
            .decide_review(&id, 0, "third", true, &f.ask, "alpha", "third", 3, false, true)
            .unwrap_err()
            .code,
        "authority_conflict"
    );
    f.store
        .decide_review(
            &id, 0, "owner", true, &f.ask, "alpha", "third", 3, true, true,
        )
        .unwrap();
    let state = f.store.snapshot().unwrap();
    assert_eq!(state.asks[&f.ask.ask_id].controller, "third");
    assert_eq!(state.reviews[&id].to, "new");
    assert_eq!(state.reviews[&id].resolved_to.as_deref(), Some("third"));
}
#[test]
fn discard_creates_owner_hold_with_an_actionable_owner_resolution() {
    let f = Fixture::new();
    f.stamp();
    f.plan(false, true);
    let id = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .keys()
        .next()
        .unwrap()
        .clone();
    f.store
        .decide_review(&id, 0, "new", false, &f.ask, "alpha", "new", 2, false, true)
        .unwrap();
    let state = f.store.snapshot().unwrap();
    assert!(state.asks[&f.ask.ask_id].owner_hold);
    assert_eq!(state.reviews[&id].actor.as_deref(), Some("new"));
    let held = state
        .reviews
        .values()
        .find(|r| r.owner_only && r.state == "pending")
        .unwrap();
    assert_eq!(
        f.store
            .decide_review(&held.id, 0, "new", true, &f.ask, "alpha", "new", 2, false, true)
            .unwrap_err()
            .code,
        "owner_review_required"
    );
    f.store
        .decide_review(
            &held.id, 0, "owner", true, &f.ask, "alpha", "third", 3, true, true,
        )
        .unwrap();
    let after = f.store.snapshot().unwrap();
    assert!(!after.asks[&f.ask.ask_id].owner_hold);
    assert_eq!(after.asks[&f.ask.ask_id].controller, "third");
    assert_eq!(
        after.reviews[&id].state, "discarded",
        "original discard audit stays intact"
    );
}
#[test]
fn recovery_without_previous_director_is_owner_visible_and_not_self_approvable() {
    let f = Fixture::new();
    f.stamp();
    f.store
        .plan_recovery(&f.asks, "alpha", None, Some("new"), "missing-chain")
        .unwrap();
    let state = f.store.snapshot().unwrap();
    let review = state.reviews.values().next().unwrap();
    assert!(review.owner_only);
    assert!(state.asks[&f.ask.ask_id].owner_hold);
    assert_eq!(
        f.store
            .decide_review(&review.id, 0, "new", true, &f.ask, "alpha", "new", 2, false, true)
            .unwrap_err()
            .code,
        "owner_review_required"
    );
    f.store
        .decide_review(
            &review.id, 0, "owner", true, &f.ask, "alpha", "new", 2, true, true,
        )
        .unwrap();
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask.ask_id].controller,
        "new"
    );
}
#[test]
fn vacant_director_recovery_is_visible_and_owner_can_discard_then_retarget() {
    let f = Fixture::new();
    f.stamp();
    f.store
        .plan_recovery(&f.asks, "alpha", Some(REQUESTER), None, "clear")
        .unwrap();
    let state = f.store.snapshot().unwrap();
    let id = state.reviews.keys().next().unwrap().clone();
    assert!(state.reviews[&id].owner_only);
    assert!(state.reviews[&id].to.is_empty());
    assert_eq!(
        f.store
            .decide_review(&id, 0, "owner", true, &f.ask, "alpha", "", 2, true, true)
            .unwrap_err()
            .code,
        "director_not_found"
    );
    f.store
        .decide_review(&id, 0, "owner", false, &f.ask, "alpha", "", 2, true, true)
        .unwrap();
    let next = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .values()
        .find(|r| r.state == "pending")
        .unwrap()
        .clone();
    f.store
        .decide_review(
            &next.id, 0, "owner", true, &f.ask, "alpha", "next", 3, true, true,
        )
        .unwrap();
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask.ask_id].controller,
        "next"
    );
}
#[test]
fn summary_failure_remains_durable_and_retry_repairs_once() {
    let f = Fixture::new();
    f.stamp();
    f.plan(true, true);
    assert!(f.store.snapshot().unwrap().asks[&f.ask.ask_id].summary_pending);
    assert!(f
        .store
        .repair_summaries_with(|_| Err(crate::asks::store::AskError::new(
            "persistence_unavailable",
            "injected ask-store outage"
        )))
        .is_err());
    assert!(f.store.snapshot().unwrap().asks[&f.ask.ask_id].summary_pending);
    let calls = std::cell::Cell::new(0);
    assert_eq!(
        f.store
            .repair_summaries_with(|_| {
                calls.set(calls.get() + 1);
                Ok(true)
            })
            .unwrap(),
        1
    );
    assert_eq!(
        f.store
            .repair_summaries_with(|_| {
                calls.set(calls.get() + 1);
                Ok(true)
            })
            .unwrap(),
        0
    );
    assert_eq!(calls.get(), 1);
}
#[test]
fn newer_ownership_version_is_not_read_as_legacy_or_rewritten() {
    let f = Fixture::new();
    std::fs::create_dir_all(&f.root).unwrap();
    let path = f.root.join("ownership-v1.json");
    let bytes = br#"{"version":99,"revision":0,"futureAuthority":{"onlyOwner":true}}"#;
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(
        f.store.snapshot().unwrap_err().code,
        "director_state_version_unsupported"
    );
    assert_eq!(
        f.store.update(|_| Ok(())).unwrap_err().code,
        "director_state_version_unsupported"
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
#[test]
fn unknown_top_level_ownership_metadata_survives_a_known_version_rewrite() {
    let f = Fixture::new();
    std::fs::create_dir_all(&f.root).unwrap();
    let path = f.root.join("ownership-v1.json");
    std::fs::write(
        &path,
        br#"{"version":1,"revision":0,"futureMetadata":{"note":"keep"}}"#,
    )
    .unwrap();
    f.store.update(|_| Ok(())).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(value["futureMetadata"]["note"], "keep");
}
#[test]
fn owner_approved_current_scope_can_resolve_a_recovery_hold() {
    let f = Fixture::new();
    f.stamp();
    f.store
        .plan_recovery(&f.asks, "alpha", None, Some("new"), "gap")
        .unwrap();
    f.store
        .approve_scope(
            ApprovedScope {
                id: "owner-current-scope".into(),
                project_id: "alpha".into(),
                director: "new".into(),
                ask_ids: vec![f.ask.ask_id.clone()],
                expected_epoch: 2,
            },
            &f.asks,
        )
        .unwrap();
    let id = format!("scope:owner-current-scope:{}", f.ask.ask_id);
    f.store
        .decide_review(&id, 0, "new", true, &f.ask, "alpha", "new", 2, false, true)
        .unwrap();
    assert!(!f.store.snapshot().unwrap().asks[&f.ask.ask_id].owner_hold);
}
#[test]
fn closed_view_reports_historical_controller_without_reviving_control() {
    let mut f = Fixture::new();
    f.stamp();
    f.ask.closed = Some(crate::asks::model::CloseOutcome::Accepted);
    assert_eq!(
        f.store.closed_controller(&f.ask).unwrap().as_deref(),
        Some(REQUESTER)
    );
    let record = f.store.snapshot().unwrap().asks[&f.ask.ask_id].clone();
    let default = Store::open_default().unwrap();
    default
        .update(|s| {
            s.asks.insert(f.ask.ask_id.clone(), record);
            Ok(())
        })
        .unwrap();
    let view = crate::asks::view::control_value(&f.ask).unwrap();
    assert_eq!(view["controlHistorical"], true);
    assert!(view.get("controlError").is_none());
    assert_eq!(view["controller"], REQUESTER);
    default
        .update(|s| {
            s.asks.remove(&f.ask.ask_id);
            Ok(())
        })
        .unwrap();
}
#[test]
fn duplicate_finalize_is_idempotent_and_controller_history_is_a_chain() {
    let f = Fixture::new();
    f.stamp();
    f.plan(true, true);
    f.plan(true, true);
    f.store
        .plan_handover(&f.asks, "alpha", "new", "third", true, "change2", |_| true)
        .unwrap();
    let s = f.store.snapshot().unwrap();
    let o = &s.asks[&f.ask.ask_id];
    assert_eq!(o.controller_revision, 2);
    assert_eq!(o.history, vec![REQUESTER, "new", "third"]);
    assert_eq!(s.reviews.len(), 2);
}
#[test]
fn changed_request_digest_fails_closed() {
    let mut f = Fixture::new();
    f.stamp();
    f.ask.request_sha256 = "forged".into();
    f.asks.asks.insert(f.ask.ask_id.clone(), f.ask.clone());
    assert_eq!(
        f.store
            .plan_handover(&f.asks, "alpha", REQUESTER, "new", true, "c", |_| true)
            .unwrap_err()
            .code,
        "director_provenance_invalid"
    );
    assert_eq!(
        f.store
            .stamp_at(&f.ask, REQUESTER, "alpha", 1)
            .unwrap_err()
            .code,
        "director_provenance_invalid"
    );
}
#[test]
fn stamp_cannot_claim_another_original_requester() {
    let f = Fixture::new();
    assert_eq!(
        f.store
            .stamp_at(&f.ask, "forged", "alpha", 1)
            .unwrap_err()
            .code,
        "not_requester"
    );
}
#[test]
fn existing_origin_project_is_pinned() {
    let f = Fixture::new();
    f.stamp();
    assert_eq!(
        f.store
            .stamp_at(&f.ask, REQUESTER, "beta", 1)
            .unwrap_err()
            .code,
        "action_conflict"
    );
}
#[test]
fn corrupt_present_store_is_never_treated_as_no_ownership() {
    let f = Fixture::new();
    std::fs::create_dir_all(&f.root).unwrap();
    std::fs::write(f.root.join("ownership-v1.json"), b"invalid").unwrap();
    assert_eq!(
        f.store.snapshot().unwrap_err().code,
        "director_state_unavailable"
    );
    assert!(f.store.update(|_| Ok(())).is_err());
}
#[test]
fn independent_store_instances_see_atomic_updates_and_cache_invalidates() {
    let f = Fixture::new();
    assert!(f.store.snapshot().unwrap().asks.is_empty());
    f.stamp();
    let second = Store::at(&f.root);
    assert_eq!(second.snapshot().unwrap().asks.len(), 1);
    f.plan(true, true);
    assert_eq!(
        second.snapshot().unwrap().asks[&f.ask.ask_id].controller,
        "new"
    );
}
#[test]
fn role_retries_keep_the_first_target_and_reject_changed_raw_payload() {
    let f = Fixture::new();
    let a = RoleResolution {
        project_id: "alpha".into(),
        target: "old".into(),
        epoch: 1,
    };
    let b = RoleResolution {
        target: "new".into(),
        epoch: 2,
        ..a.clone()
    };
    f.store
        .pin_role_target(REQUESTER, "a", "tell", "digest", a.clone())
        .unwrap();
    assert_eq!(
        f.store
            .pin_role_target(REQUESTER, "a", "tell", "digest", b)
            .unwrap(),
        a
    );
    assert_eq!(
        f.store
            .role_target_at(REQUESTER, "a", "tell", "different")
            .unwrap_err()
            .code,
        "action_conflict"
    );
    assert!(f
        .store
        .role_target_at(REQUESTER, "a", "delegate", "different")
        .unwrap()
        .is_none());
}
#[test]
fn unknown_legacy_requires_exact_owner_scope_then_explicit_accept() {
    let f = Fixture::new();
    assert!(f.plan(true, true).is_empty());
    let scope = ApprovedScope {
        id: "scope1".into(),
        project_id: "alpha".into(),
        director: "new".into(),
        ask_ids: vec![f.ask.ask_id.clone()],
        expected_epoch: 2,
    };
    f.store.approve_scope(scope, &f.asks).unwrap();
    assert!(f.store.snapshot().unwrap().asks.is_empty());
    let id = format!("scope:scope1:{}", f.ask.ask_id);
    assert_eq!(
        f.store
            .decide_review(&id, 0, "new", true, &f.ask, "alpha", "new", 3, false, true)
            .unwrap_err()
            .code,
        "scope_required"
    );
    f.store
        .decide_review(&id, 0, "new", true, &f.ask, "alpha", "new", 2, false, true)
        .unwrap();
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask.ask_id].controller,
        "new"
    );
}
#[test]
fn review_rejects_old_director_and_stale_revision_and_different_project() {
    let f = Fixture::new();
    f.stamp();
    f.plan(false, true);
    let id = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .keys()
        .next()
        .unwrap()
        .clone();
    assert_eq!(
        f.store
            .decide_review(&id, 0, REQUESTER, true, &f.ask, "alpha", "new", 2, false, true)
            .unwrap_err()
            .code,
        "not_director"
    );
    assert_eq!(
        f.store
            .decide_review(&id, 5, "new", true, &f.ask, "alpha", "new", 2, false, true)
            .unwrap_err()
            .code,
        "revision_conflict"
    );
    assert_eq!(
        f.store
            .decide_review(&id, 0, "new", true, &f.ask, "beta", "new", 2, false, true)
            .unwrap_err()
            .code,
        "authority_conflict"
    );
}
#[test]
fn capability_or_boundary_differences_need_owner_review() {
    let f = Fixture::new();
    f.stamp();
    f.plan(true, false);
    let id = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .keys()
        .next()
        .unwrap()
        .clone();
    assert_eq!(
        f.store
            .decide_review(&id, 0, "new", true, &f.ask, "alpha", "new", 2, false, false)
            .unwrap_err()
            .code,
        "handover_review_required"
    );
    f.store
        .decide_review(
            &id, 0, "owner", true, &f.ask, "alpha", "new", 2, true, false,
        )
        .unwrap();
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask.ask_id].controller,
        "new"
    );
}
#[test]
fn discard_is_durable_and_not_requeued_by_finalize() {
    let f = Fixture::new();
    f.stamp();
    f.plan(false, true);
    let id = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .keys()
        .next()
        .unwrap()
        .clone();
    f.store
        .decide_review(&id, 0, "new", false, &f.ask, "alpha", "new", 2, false, true)
        .unwrap();
    f.plan(false, true);
    let s = f.store.snapshot().unwrap();
    assert_eq!(s.reviews[&id].state, "discarded");
    assert_eq!(s.asks[&f.ask.ask_id].controller, REQUESTER);
}
#[test]
fn review_retry_replays_same_decision_without_double_revision() {
    let f = Fixture::new();
    f.stamp();
    f.plan(false, true);
    let id = f
        .store
        .snapshot()
        .unwrap()
        .reviews
        .keys()
        .next()
        .unwrap()
        .clone();
    for _ in 0..2 {
        f.store
            .decide_review(&id, 0, "new", true, &f.ask, "alpha", "new", 2, false, true)
            .unwrap();
    }
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask.ask_id].controller_revision,
        1
    );
}
#[test]
fn owner_scope_rejects_unknown_ids_empty_selection_and_payload_reuse() {
    let f = Fixture::new();
    let mut scope = ApprovedScope {
        id: "scope".into(),
        project_id: "alpha".into(),
        director: "new".into(),
        ask_ids: vec![],
        expected_epoch: 2,
    };
    assert_eq!(
        f.store
            .approve_scope(scope.clone(), &f.asks)
            .unwrap_err()
            .code,
        "invalid_scope"
    );
    scope.ask_ids = vec!["unknown".into()];
    assert_eq!(
        f.store
            .approve_scope(scope.clone(), &f.asks)
            .unwrap_err()
            .code,
        "ask_not_found"
    );
    scope.ask_ids = vec![f.ask.ask_id.clone()];
    f.store.approve_scope(scope.clone(), &f.asks).unwrap();
    scope.director = "different".into();
    assert_eq!(
        f.store.approve_scope(scope, &f.asks).unwrap_err().code,
        "action_conflict"
    );
}
#[test]
fn r253_rollback_reads_original_records_unchanged() {
    let f = Fixture::new();
    let before = serde_json::to_vec(&f.asks).unwrap();
    f.stamp();
    f.plan(true, true);
    let rollback: AskState = serde_json::from_slice(&before).unwrap();
    assert_eq!(rollback.asks[&f.ask.ask_id].requester, f.ask.requester);
    assert_eq!(
        rollback.asks[&f.ask.ask_id].request_sha256,
        f.ask.request_sha256
    );
    assert_eq!(serde_json::to_vec(&rollback).unwrap(), before);
}
