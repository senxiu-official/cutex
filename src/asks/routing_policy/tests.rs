use super::*;
use chrono::Duration;
use serde_json::json;

fn now() -> DateTime<Utc> {
    "2026-10-27T00:00:00Z".parse().unwrap()
}
fn r(id: &str) -> RoutingRef {
    RoutingRef::new("host-a", id).unwrap()
}
fn agent(id: &str) -> RoutingActor {
    RoutingActor::Agent {
        reference: r(id),
        name: Some(id.into()),
    }
}
fn context(actor: RoutingActor) -> RoutingContext {
    RoutingContext {
        project_id: "p".into(),
        authority_epoch: 7,
        current_director: Some(r("director")),
        actor,
        operator_may_route: false,
    }
}
fn member() -> RoutingContext {
    context(agent("member"))
}
fn owner() -> RoutingContext {
    context(RoutingActor::Owner)
}
fn director() -> RoutingContext {
    context(agent("director"))
}
fn root() -> RootStatus {
    RootStatus::VerifiedOpen(r("ask-root"))
}
fn request() -> RoutingRequest {
    RoutingRequest {
        project_id: "p".into(),
        delegation_ref: r("ask-child"),
        requester_ref: Some(r("member")),
        target_ref: r("reviewer"),
        parent: Some(r("ask-parent")),
        root: Some(r("ask-root")),
        source_attempt_id: Some("attempt-1".into()),
        source_turn_id: Some("turn-1".into()),
        brief: "Review the implementation".into(),
        attachments: vec![json!({"id":"art-1","sha256":"content-v1"})],
    }
}
fn grant_input() -> CreateGrantRequest {
    CreateGrantRequest {
        action_id: "create-grant".into(),
        requester_ref: r("member"),
        target_refs: vec![r("reviewer")],
        root_delegation_ref: r("ask-root"),
        purpose: GrantPurpose::Review,
        max_uses: 1,
        expires_at: Some(now() + Duration::hours(1)),
    }
}
fn granted() -> RoutingState {
    let mut state = RoutingState::default();
    policy(&mut state, MemberMode::AllowlistedOnly);
    state
        .create_grant("grant-1", &director(), &grant_input(), &root(), now())
        .unwrap();
    state
}
fn pending() -> RoutingState {
    let mut state = RoutingState::default();
    assert!(
        state
            .request_approval("approval-1", &member(), &request(), &root(), now())
            .unwrap()
            .1
    );
    state
}
fn decision(state: &RoutingState, approve: bool) -> DecideRoutingRequest {
    let approval = &state.approvals["approval-1"];
    DecideRoutingRequest {
        action_id: "decide-1".into(),
        decision: if approve {
            RoutingDecision::Approve
        } else {
            RoutingDecision::Deny
        },
        expected_digest: approval.request_digest.clone(),
        expected_revision: approval.revision,
        decision_note: None,
    }
}
fn policy(state: &mut RoutingState, mode: MemberMode) {
    let expected_revision = state.policy("p").revision;
    state
        .update_policy(
            &owner(),
            &UpdatePolicyRequest {
                action_id: format!("policy-{expected_revision}"),
                expected_revision,
                member_mode: mode,
            },
            now(),
        )
        .unwrap();
}

#[test]
fn default_is_pending_and_open_requires_explicit_policy_cas() {
    let mut state = RoutingState::default();
    assert_eq!(state.policy("p").member_mode, MemberMode::ApprovalRequired);
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::None,
            now()
        ),
        Ok(RouteVerdict::ApprovalRequired)
    );
    let input = UpdatePolicyRequest {
        action_id: "update".into(),
        expected_revision: 1,
        member_mode: MemberMode::Open,
    };
    assert_eq!(
        state.update_policy(&owner(), &input, now()),
        Err(RoutingError::PolicyRevisionConflict)
    );
    assert!(state.policies.is_empty());
    policy(&mut state, MemberMode::Open);
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::None,
            now()
        ),
        Ok(RouteVerdict::Pass(RouteBasis::Open))
    );
    // Open returns only a routing basis, not an owner capability/lease permit.
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &RootStatus::Closed(r("ask-root")),
            RouteAuthorization::None,
            now()
        ),
        Err(RoutingError::RootClosed)
    );
}

#[test]
fn role_matrix_uses_exact_current_ref_and_operator_never_manages() {
    let mut state = RoutingState::default();
    for (ctx, expected) in [
        (owner(), RouteBasis::Owner),
        (director(), RouteBasis::Director),
    ] {
        let mut req = request();
        req.requester_ref = ctx.actor.reference().cloned();
        assert_eq!(
            state.check_route(&ctx, &req, &root(), RouteAuthorization::None, now()),
            Ok(RouteVerdict::Pass(expected))
        );
    }
    let mut operator = member();
    operator.operator_may_route = true;
    assert_eq!(
        state.check_route(
            &operator,
            &request(),
            &root(),
            RouteAuthorization::None,
            now()
        ),
        Ok(RouteVerdict::Pass(RouteBasis::Operator))
    );
    assert_eq!(
        operator.management_authority(),
        Err(RoutingError::NotAuthorized)
    );
    assert_eq!(
        state.create_grant("bad", &operator, &grant_input(), &root(), now()),
        Err(RoutingError::NotAuthorized)
    );
    let mut spoof = member();
    spoof.actor = RoutingActor::Agent {
        reference: r("member"),
        name: Some("Director".into()),
    };
    assert_eq!(
        spoof.management_authority(),
        Err(RoutingError::NotAuthorized)
    );
    spoof.actor = RoutingActor::Agent {
        reference: RoutingRef::new("host-b", "director").unwrap(),
        name: None,
    };
    assert_eq!(
        spoof.management_authority(),
        Err(RoutingError::NotAuthorized)
    );
}

#[test]
fn help_current_director_is_exempt_but_absent_director_falls_to_owner() {
    let state = RoutingState::default();
    let mut req = request();
    req.target_ref = r("director");
    assert_eq!(
        state.check_route(&member(), &req, &root(), RouteAuthorization::None, now()),
        Ok(RouteVerdict::Pass(RouteBasis::ToDirector))
    );
    let mut ctx = member();
    ctx.current_director = None;
    assert_eq!(
        state.check_route(&ctx, &req, &root(), RouteAuthorization::None, now()),
        Ok(RouteVerdict::ApprovalRequired)
    );
    assert_eq!(ctx.management_authority(), Err(RoutingError::NotAuthorized));
}

#[test]
fn allowlisted_only_requires_live_concrete_grant() {
    let mut state = RoutingState::default();
    policy(&mut state, MemberMode::AllowlistedOnly);
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::None,
            now()
        ),
        Err(RoutingError::TargetNotGranted)
    );
    state
        .create_grant("grant-1", &owner(), &grant_input(), &root(), now())
        .unwrap();
    assert!(matches!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            now()
        ),
        Ok(RouteVerdict::Pass(RouteBasis::Grant { .. }))
    ));
}

#[test]
fn one_grant_can_list_reviewers_but_spends_only_once_total() {
    let mut state = RoutingState::default();
    policy(&mut state, MemberMode::AllowlistedOnly);
    let mut input = grant_input();
    input.target_refs.push(r("other-reviewer"));
    state
        .create_grant("grant-1", &owner(), &input, &root(), now())
        .unwrap();
    state
        .consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now(),
        )
        .unwrap();
    let mut second = request();
    second.target_ref = r("other-reviewer");
    second.delegation_ref = r("ask-2");
    assert_eq!(
        state.consume_route(
            &member(),
            &second,
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate-2",
            now()
        ),
        Err(RoutingError::GrantExhausted)
    );
    assert_eq!(state.grants["grant-1"].used_count, 1);
}

#[test]
fn read_preflight_never_consumes_exact_retry_and_restart_never_double_consume() {
    let mut state = granted();
    for _ in 0..3 {
        state
            .check_route(
                &member(),
                &request(),
                &root(),
                RouteAuthorization::Grant("grant-1"),
                now(),
            )
            .unwrap();
    }
    assert_eq!(state.grants["grant-1"].used_count, 0);
    let first = state
        .consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now(),
        )
        .unwrap();
    let encoded = serde_json::to_vec(&state).unwrap();
    let mut restored: RoutingState = serde_json::from_slice(&encoded).unwrap();
    let before = restored.clone();
    assert_eq!(
        restored.consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now() + Duration::seconds(30)
        ),
        Ok(first)
    );
    assert_eq!(restored, before);
    assert_eq!(
        restored.consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "different-action",
            now()
        ),
        Err(RoutingError::GrantExhausted)
    );
}

#[test]
fn retry_cannot_switch_grants_or_change_request_under_same_action() {
    let mut state = granted();
    state
        .create_grant("grant-2", &owner(), &grant_input(), &root(), now())
        .unwrap();
    state
        .consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now(),
        )
        .unwrap();
    assert_eq!(
        state.consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-2"),
            "delegate",
            now()
        ),
        Err(RoutingError::ActionPayloadConflict)
    );
    let mut changed = request();
    changed.brief.push_str(" changed");
    assert_eq!(
        state.consume_route(
            &member(),
            &changed,
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now()
        ),
        Err(RoutingError::ActionPayloadConflict)
    );
    assert_eq!(
        state.consume_route(
            &member(),
            &changed,
            &root(),
            RouteAuthorization::Grant("grant-2"),
            "delegate",
            now()
        ),
        Err(RoutingError::ActionPayloadConflict)
    );
    assert_eq!(state.grants["grant-2"].used_count, 0);
}

#[test]
fn grant_scope_checks_host_requester_target_root_and_provenance() {
    let state = granted();
    let mut req = request();
    req.target_ref = RoutingRef::new("host-b", "reviewer").unwrap();
    assert_eq!(
        state.check_route(
            &member(),
            &req,
            &root(),
            RouteAuthorization::Grant("grant-1"),
            now()
        ),
        Err(RoutingError::TargetNotGranted)
    );
    req = request();
    req.root = Some(RoutingRef::new("host-b", "ask-root").unwrap());
    assert_eq!(
        state.check_route(
            &member(),
            &req,
            &RootStatus::VerifiedOpen(req.root.clone().unwrap()),
            RouteAuthorization::Grant("grant-1"),
            now()
        ),
        Err(RoutingError::ScopeMismatch)
    );
    req = request();
    req.requester_ref = Some(r("impostor"));
    assert_eq!(
        state.check_route(
            &member(),
            &req,
            &root(),
            RouteAuthorization::Grant("grant-1"),
            now()
        ),
        Err(RoutingError::ScopeMismatch)
    );
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &RootStatus::Unknown,
            RouteAuthorization::Grant("grant-1"),
            now()
        ),
        Err(RoutingError::RootUnverified)
    );
}

#[test]
fn consumed_grant_fails_closed_on_expiry_revoke_root_close_rotation_policy_change() {
    for case in 0..5 {
        let mut state = granted();
        let mut ctx = member();
        let mut root_state = root();
        let mut clock = now();
        state
            .consume_route(
                &ctx,
                &request(),
                &root_state,
                RouteAuthorization::Grant("grant-1"),
                "delegate",
                clock,
            )
            .unwrap();
        let expected = match case {
            0 => {
                clock += Duration::hours(1);
                RoutingError::GrantExpired
            }
            1 => {
                let rev = state.grants["grant-1"].revision;
                state
                    .revoke_grant(
                        "grant-1",
                        &owner(),
                        &RevokeGrantRequest {
                            action_id: "revoke".into(),
                            expected_revision: rev,
                        },
                        now(),
                    )
                    .unwrap();
                RoutingError::GrantRevoked
            }
            2 => {
                root_state = RootStatus::Closed(r("ask-root"));
                RoutingError::RootClosed
            }
            3 => {
                ctx.authority_epoch += 1;
                ctx.current_director = Some(r("new-director"));
                RoutingError::GrantStale
            }
            _ => {
                policy(&mut state, MemberMode::Open);
                RoutingError::GrantStale
            }
        };
        assert_eq!(
            state.consume_route(
                &ctx,
                &request(),
                &root_state,
                RouteAuthorization::Grant("grant-1"),
                "delegate",
                clock
            ),
            Err(expected)
        );
        assert_eq!(state.grants["grant-1"].used_count, 1);
    }
}

#[test]
fn invalid_grant_creation_never_mutates() {
    for case in 0..7 {
        let mut state = RoutingState::default();
        let mut input = grant_input();
        let mut root_state = root();
        match case {
            0 => input.max_uses = 2,
            1 => input.target_refs.clear(),
            2 => input.target_refs.push(r("reviewer")),
            3 => input.target_refs = vec![r("member")],
            4 => input.expires_at = Some(now()),
            5 => root_state = RootStatus::Unknown,
            _ => root_state = RootStatus::Closed(r("ask-root")),
        }
        assert!(state
            .create_grant("grant", &owner(), &input, &root_state, now())
            .is_err());
        assert_eq!(state, RoutingState::default());
    }
}

#[test]
fn grant_cannot_be_reissued_or_mutated_in_place() {
    let mut state = granted();
    let before = state.clone();
    assert_eq!(
        state.create_grant("grant-1", &owner(), &grant_input(), &root(), now()),
        Err(RoutingError::ResourceExists)
    );
    assert_eq!(state, before);
    assert_eq!(
        state.revoke_grant(
            "grant-1",
            &owner(),
            &RevokeGrantRequest {
                action_id: "revoke".into(),
                expected_revision: 0
            },
            now()
        ),
        Err(RoutingError::GrantRevisionConflict)
    );
    assert_eq!(state, before);
}

#[test]
fn refs_only_wire_rejects_aliases_extra_actor_and_legacy_scope_fields() {
    for value in [
        json!("member"),
        json!({"hostId":"host-a","name":"member"}),
        json!({"hostId":"","id":"m"}),
        json!({"hostId":"host-a","id":"*"}),
        json!({"hostId":"host-a","id":"m","name":"Member"}),
    ] {
        assert!(serde_json::from_value::<RoutingRef>(value).is_err());
    }
    let mut value = serde_json::to_value(grant_input()).unwrap();
    value["requesterSessionId"] = json!("member");
    assert!(serde_json::from_value::<CreateGrantRequest>(value).is_err());
    let mut value = serde_json::to_value(decision(&pending(), true)).unwrap();
    value["actor"] = json!("owner");
    assert!(serde_json::from_value::<DecideRoutingRequest>(value).is_err());
    let mut value = serde_json::to_value(grant_input()).unwrap();
    value.as_object_mut().unwrap().remove("maxUses");
    assert_eq!(
        serde_json::from_value::<CreateGrantRequest>(value)
            .unwrap()
            .max_uses,
        1
    );
}

#[test]
fn pending_deduplicates_and_does_not_authorize_delivery() {
    let mut state = pending();
    let before = state.clone();
    assert!(
        !state
            .request_approval("another-id", &member(), &request(), &root(), now())
            .unwrap()
            .1
    );
    assert_eq!(state, before);
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Approval("approval-1"),
            now()
        ),
        Ok(RouteVerdict::ApprovalRequired)
    );
    let a = serde_json::to_value(&state.approvals["approval-1"]).unwrap();
    for field in [
        "decidedBy",
        "decisionAuthority",
        "decisionNote",
        "decidedAt",
    ] {
        assert!(a[field].is_null());
    }
    assert_eq!(a["requestedBy"].as_object().unwrap().len(), 5);
}

#[test]
fn owner_and_current_director_decisions_record_exact_five_field_actor() {
    for (ctx, authority) in [
        (owner(), DecisionAuthority::Owner),
        (director(), DecisionAuthority::Director),
    ] {
        let mut state = pending();
        let input = decision(&state, true);
        let decided = state
            .decide_approval("approval-1", &ctx, &request(), &root(), &input, now())
            .unwrap();
        assert_eq!(decided.decision_authority, Some(authority));
        let actor = decided.decided_by.unwrap();
        let wire = serde_json::to_value(&actor).unwrap();
        assert_eq!(wire.as_object().unwrap().len(), 5);
        if authority == DecisionAuthority::Owner {
            assert_eq!(
                wire,
                json!({"kind":"owner","hostId":null,"sessionId":null,"name":null,"redacted":false})
            );
        } else {
            assert_eq!(
                wire,
                json!({"kind":"agent","hostId":"host-a","sessionId":"director","name":"director","redacted":false})
            );
            assert_eq!(
                serde_json::to_value(actor.redact()).unwrap(),
                json!({"kind":"agent","hostId":null,"sessionId":null,"name":null,"redacted":true})
            );
        }
        assert_eq!(
            state.check_route(
                &member(),
                &request(),
                &root(),
                RouteAuthorization::Approval("approval-1"),
                now()
            ),
            Err(RoutingError::ReviewerGrantRequired)
        );
        state
            .create_grant("grant-1", &owner(), &grant_input(), &root(), now())
            .unwrap();
        assert!(matches!(
            state.consume_route(
                &member(),
                &request(),
                &root(),
                RouteAuthorization::ApprovalAndGrant {
                    approval_id: "approval-1",
                    grant_id: "grant-1"
                },
                "delegate",
                now()
            ),
            Ok(RouteVerdict::Pass(RouteBasis::ApprovedGrant { .. }))
        ));
    }
}

#[test]
fn competing_owner_director_decision_first_valid_cas_wins() {
    for first in [owner(), director()] {
        let mut state = pending();
        let input = decision(&state, true);
        state
            .decide_approval("approval-1", &first, &request(), &root(), &input, now())
            .unwrap();
        let saved = state.clone();
        let second = if first.actor.is_owner() {
            director()
        } else {
            owner()
        };
        let mut deny = input;
        deny.action_id = "other-action".into();
        deny.decision = RoutingDecision::Deny;
        assert_eq!(
            state.decide_approval("approval-1", &second, &request(), &root(), &deny, now()),
            Err(RoutingError::ApprovalAlreadyDecided)
        );
        assert_eq!(state, saved);
    }
}

#[test]
fn member_operator_and_previous_director_cannot_decide_or_change_policy() {
    for mut ctx in [member(), context(agent("operator")), director()] {
        ctx.current_director = Some(r("replacement"));
        ctx.operator_may_route = true;
        let mut state = pending();
        let input = decision(&state, true);
        let before = state.clone();
        assert_eq!(
            state.decide_approval("approval-1", &ctx, &request(), &root(), &input, now()),
            Err(RoutingError::NotAuthorized)
        );
        assert_eq!(
            state.update_policy(
                &ctx,
                &UpdatePolicyRequest {
                    action_id: "p".into(),
                    expected_revision: 0,
                    member_mode: MemberMode::Open
                },
                now()
            ),
            Err(RoutingError::NotAuthorized)
        );
        assert_eq!(state, before);
        assert_eq!(
            state.approval_actions("approval-1", &ctx, &request(), &root()),
            ApprovalActions {
                approve: false,
                deny: false
            }
        );
    }
}

#[test]
fn no_director_owner_can_decide_but_a_named_backup_cannot() {
    let mut state = pending();
    let input = decision(&state, true);
    let mut backup = context(agent("backup-director"));
    backup.current_director = None;
    assert_eq!(
        state.decide_approval("approval-1", &backup, &request(), &root(), &input, now()),
        Err(RoutingError::NotAuthorized)
    );
    let mut ctx = owner();
    ctx.current_director = None;
    assert!(state
        .decide_approval("approval-1", &ctx, &request(), &root(), &input, now())
        .is_ok());
}

#[test]
fn decision_checks_revision_digest_scope_and_current_request_without_mutation() {
    for case in 0..5 {
        let mut state = pending();
        let before = state.clone();
        let mut input = decision(&state, true);
        let mut req = request();
        let mut ctx = owner();
        let expected = match case {
            0 => {
                input.expected_revision = 0;
                RoutingError::ApprovalRevisionConflict
            }
            1 => {
                input.expected_digest = "wrong".into();
                RoutingError::DigestMismatch
            }
            2 => {
                req.brief.push_str("different");
                RoutingError::DigestMismatch
            }
            3 => {
                ctx.project_id = "other-project".into();
                RoutingError::ScopeMismatch
            }
            _ => {
                req.delegation_ref = r("other-ask");
                RoutingError::ScopeMismatch
            }
        };
        assert_eq!(
            state.decide_approval("approval-1", &ctx, &req, &root(), &input, now()),
            Err(expected)
        );
        assert_eq!(state, before);
    }
}

#[test]
fn digest_binds_all_scope_and_execution_facts_and_canonicalizes_attachment_keys() {
    let original = request();
    let digest = original.digest().unwrap();
    for case in 0..10 {
        let mut changed = original.clone();
        match case {
            0 => changed.brief.push('!'),
            1 => changed.attachments[0]["sha256"] = json!("v2"),
            2 => changed.target_ref = RoutingRef::new("host-b", "reviewer").unwrap(),
            3 => changed.parent = Some(r("different-parent")),
            4 => changed.root = None,
            5 => changed.source_attempt_id = Some("attempt-2".into()),
            6 => changed.source_turn_id = Some("turn-2".into()),
            7 => changed.requester_ref = Some(r("another-member")),
            8 => changed.delegation_ref = r("ask-other"),
            _ => changed.project_id = "p2".into(),
        }
        assert_ne!(changed.digest().unwrap(), digest);
    }
    let mut reordered = original;
    reordered.attachments =
        vec![serde_json::from_str(r#"{"sha256":"content-v1","id":"art-1"}"#).unwrap()];
    assert_eq!(reordered.digest().unwrap(), digest);
}

#[test]
fn note_uses_unicode_scalar_count_and_rejects_all_line_separators() {
    let mut state = pending();
    let mut input = decision(&state, true);
    input.decision_note = Some("😀".repeat(200));
    state
        .decide_approval("approval-1", &owner(), &request(), &root(), &input, now())
        .unwrap();
    for note in [
        "😀".repeat(201),
        "x\ny".into(),
        "x\ry".into(),
        "x\u{85}y".into(),
        "x\u{2028}y".into(),
        "x\u{2029}y".into(),
    ] {
        let mut state = pending();
        let mut input = decision(&state, true);
        input.decision_note = Some(note);
        assert_eq!(
            state.decide_approval("approval-1", &owner(), &request(), &root(), &input, now()),
            Err(RoutingError::InvalidDecisionNote)
        );
        assert_eq!(state.approvals["approval-1"].state, ApprovalState::Pending);
    }
}

#[test]
fn denied_is_durable_and_cannot_be_bypassed_by_recreating_same_review() {
    let mut state = pending();
    let input = decision(&state, false);
    state
        .decide_approval("approval-1", &owner(), &request(), &root(), &input, now())
        .unwrap();
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Approval("approval-1"),
            now()
        ),
        Err(RoutingError::ApprovalDenied)
    );
    let (old, created) = state
        .request_approval("new-approval", &member(), &request(), &root(), now())
        .unwrap();
    assert!(!created);
    assert_eq!(old.state, ApprovalState::Denied);
}

#[test]
fn rotation_and_policy_revision_invalidate_both_pending_and_approved_and_keep_audits() {
    for approved in [false, true] {
        let mut state = pending();
        if approved {
            let input = decision(&state, true);
            state
                .decide_approval(
                    "approval-1",
                    &director(),
                    &request(),
                    &root(),
                    &input,
                    now(),
                )
                .unwrap();
        }
        let actor = state.approvals["approval-1"].decided_by.clone();
        let mut ctx = member();
        ctx.authority_epoch += 1;
        ctx.current_director = Some(r("replacement"));
        assert_eq!(
            state.check_route(
                &ctx,
                &request(),
                &root(),
                RouteAuthorization::Approval("approval-1"),
                now()
            ),
            Err(RoutingError::ApprovalStale)
        );
        assert_eq!(state.invalidate_project("p", 0, ctx.authority_epoch), Ok(1));
        assert_eq!(state.approvals["approval-1"].state, ApprovalState::Stale);
        assert_eq!(state.approvals["approval-1"].decided_by, actor);
        let mut new_director = ctx.clone();
        new_director.actor = agent("replacement");
        let input = decision(&state, true);
        assert_eq!(
            state.decide_approval(
                "approval-1",
                &new_director,
                &request(),
                &root(),
                &input,
                now()
            ),
            Err(RoutingError::ApprovalStale)
        );
        assert!(
            state
                .request_approval("fresh-review", &ctx, &request(), &root(), now())
                .unwrap()
                .1
        );
    }
    let mut state = pending();
    policy(&mut state, MemberMode::Open);
    assert_eq!(state.approvals["approval-1"].state, ApprovalState::Stale);
}

#[test]
fn changed_request_supersedes_approval_and_stop_cancels_without_erasing_decision() {
    let mut state = pending();
    let input = decision(&state, true);
    state
        .decide_approval("approval-1", &owner(), &request(), &root(), &input, now())
        .unwrap();
    let mut changed = request();
    changed.brief.push('!');
    assert!(
        state
            .request_approval("new-review", &member(), &changed, &root(), now())
            .unwrap()
            .1
    );
    assert_eq!(state.approvals["approval-1"].state, ApprovalState::Stale);
    assert!(state.approvals["approval-1"].decided_by.is_some());
    assert_eq!(
        state.cancel_for_delegation(&request().delegation_ref),
        Ok(1)
    );
    assert_eq!(
        state.approvals["new-review"].state,
        ApprovalState::Cancelled
    );
}

#[test]
fn unknown_legacy_lineage_can_get_fresh_manual_review_but_never_grant() {
    let mut state = granted();
    policy(&mut state, MemberMode::ApprovalRequired);
    let mut req = request();
    req.parent = None;
    req.root = None;
    assert!(state
        .request_approval(
            "legacy-review",
            &member(),
            &req,
            &RootStatus::Unknown,
            now()
        )
        .is_ok());
    assert_eq!(
        state.check_route(
            &member(),
            &req,
            &RootStatus::Unknown,
            RouteAuthorization::Grant("grant-1"),
            now()
        ),
        Err(RoutingError::ScopeMismatch)
    );
}

#[test]
fn corrupt_approved_records_do_not_authorize_and_invalid_epoch_never_defaults_to_owner() {
    let mut state = pending();
    state.approvals.get_mut("approval-1").unwrap().state = ApprovalState::Approved;
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Approval("approval-1"),
            now()
        ),
        Err(RoutingError::ApprovalStale)
    );
    let mut ctx = owner();
    ctx.authority_epoch = 0;
    assert_eq!(
        ctx.management_authority(),
        Err(RoutingError::InvalidAuthorityEpoch)
    );
}

#[test]
fn corrupt_grant_used_count_never_regains_a_use() {
    let mut state = granted();
    state.grants.get_mut("grant-1").unwrap().used_count = 1;
    assert_eq!(
        state.consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now()
        ),
        Err(RoutingError::InvalidGrant)
    );
}

#[test]
fn persisted_defaults_and_round_trip_keep_full_scope_and_audit() {
    assert_eq!(
        serde_json::from_value::<RoutingState>(json!({})).unwrap(),
        RoutingState::default()
    );
    let mut state = granted();
    policy(&mut state, MemberMode::ApprovalRequired);
    state
        .request_approval("approval-1", &member(), &request(), &root(), now())
        .unwrap();
    let input = decision(&state, true);
    state
        .decide_approval(
            "approval-1",
            &director(),
            &request(),
            &root(),
            &input,
            now(),
        )
        .unwrap();
    let encoded = serde_json::to_value(&state).unwrap();
    assert_eq!(
        encoded["grants"]["grant-1"]["requesterRef"],
        json!({"hostId":"host-a","id":"member"})
    );
    assert_eq!(
        serde_json::from_value::<RoutingState>(encoded).unwrap(),
        state
    );
}

#[test]
fn revision_overflow_does_not_partially_write_policy_or_invalidation() {
    let mut state = pending();
    state.approvals.get_mut("approval-1").unwrap().revision = 9_007_199_254_740_991;
    let before = state.clone();
    assert_eq!(
        state.update_policy(
            &owner(),
            &UpdatePolicyRequest {
                action_id: "p".into(),
                expected_revision: 0,
                member_mode: MemberMode::Open
            },
            now()
        ),
        Err(RoutingError::RevisionOverflow)
    );
    assert_eq!(state, before);
    assert_eq!(
        state.cancel_for_delegation(&r("ask-child")),
        Err(RoutingError::RevisionOverflow)
    );
    assert_eq!(state, before);
}

#[test]
fn caller_lock_serializes_competing_one_use_consumers() {
    // A domain-level lock simulation, NOT an AskStore persistence test. Parent
    // must run the same race through its actual with_state integration.
    let state = std::sync::Arc::new(std::sync::Mutex::new(granted()));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for n in 0..2 {
        let state = state.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut req = request();
            req.delegation_ref = r(&format!("ask-race-{n}"));
            barrier.wait();
            state.lock().unwrap().consume_route(
                &member(),
                &req,
                &root(),
                RouteAuthorization::Grant("grant-1"),
                &format!("action-{n}"),
                now(),
            )
        }));
    }
    barrier.wait();
    let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| **r == Err(RoutingError::GrantExhausted))
            .count(),
        1
    );
    assert_eq!(state.lock().unwrap().grants["grant-1"].used_count, 1);
}

#[test]
fn concurrent_exact_action_replay_never_double_spends() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(granted()));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let state = state.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            state.lock().unwrap().consume_route(
                &member(),
                &request(),
                &root(),
                RouteAuthorization::Grant("grant-1"),
                "same-action",
                now(),
            )
        }));
    }
    barrier.wait();
    for thread in threads {
        assert!(thread.join().unwrap().is_ok());
    }
    let locked = state.lock().unwrap();
    assert_eq!(locked.grants["grant-1"].used_count, 1);
    assert_eq!(locked.grants["grant-1"].revision, 2);
}

#[test]
fn approval_and_grant_reject_same_id_on_different_hosts_and_projects() {
    let mut state = pending();
    let input = decision(&state, true);
    state
        .decide_approval("approval-1", &owner(), &request(), &root(), &input, now())
        .unwrap();
    let mut req = request();
    req.delegation_ref = RoutingRef::new("host-b", "ask-child").unwrap();
    assert_eq!(
        state.check_route(
            &member(),
            &req,
            &root(),
            RouteAuthorization::Approval("approval-1"),
            now()
        ),
        Err(RoutingError::ScopeMismatch)
    );
    let mut other = owner();
    other.project_id = "p2".into();
    let mut state = granted();
    assert_eq!(
        state.revoke_grant(
            "grant-1",
            &other,
            &RevokeGrantRequest {
                action_id: "x".into(),
                expected_revision: 1
            },
            now()
        ),
        Err(RoutingError::ScopeMismatch)
    );
}

#[test]
fn root_closure_blocks_decision_and_allowed_actions_even_for_owner() {
    let mut state = pending();
    let input = decision(&state, true);
    let closed = RootStatus::Closed(r("ask-root"));
    assert_eq!(
        state.decide_approval("approval-1", &owner(), &request(), &closed, &input, now()),
        Err(RoutingError::RootClosed)
    );
    assert_eq!(
        state.approval_actions("approval-1", &owner(), &request(), &closed),
        ApprovalActions {
            approve: false,
            deny: false
        }
    );
    assert_eq!(state.approvals["approval-1"].state, ApprovalState::Pending);
}

#[test]
fn never_assigned_director_epoch_zero_still_routes_review_to_owner() {
    let mut state = RoutingState::default();
    let mut ctx = member();
    ctx.current_director = None;
    ctx.authority_epoch = 0;
    state
        .request_approval("approval-1", &ctx, &request(), &root(), now())
        .unwrap();
    let input = decision(&state, true);
    let mut human = ctx;
    human.actor = RoutingActor::Owner;
    state
        .decide_approval("approval-1", &human, &request(), &root(), &input, now())
        .unwrap();
    let mut first_director = member();
    first_director.authority_epoch = 1;
    assert_eq!(
        state.check_route(
            &first_director,
            &request(),
            &root(),
            RouteAuthorization::Approval("approval-1"),
            now()
        ),
        Err(RoutingError::ApprovalStale)
    );
}

#[test]
fn root_reopening_cannot_revive_old_reviewer_grant_after_restart() {
    let mut state = granted();
    assert_eq!(state.close_root_grants(&r("ask-root"), now()), Ok(1));
    assert_eq!(state.close_root_grants(&r("ask-root"), now()), Ok(0));
    let wire = serde_json::to_vec(&state).unwrap();
    let mut restored: RoutingState = serde_json::from_slice(&wire).unwrap();
    assert_eq!(
        restored.consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now()
        ),
        Err(RoutingError::RootClosed)
    );
    assert_eq!(restored.grants["grant-1"].used_count, 0);
}

#[test]
fn default_policy_needs_both_approval_and_grant_and_spends_only_at_admission() {
    let mut state = pending();
    state
        .create_grant("grant-1", &owner(), &grant_input(), &root(), now())
        .unwrap();
    assert_eq!(
        state.consume_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Grant("grant-1"),
            "delegate",
            now()
        ),
        Ok(RouteVerdict::ApprovalRequired)
    );
    assert_eq!(state.grants["grant-1"].used_count, 0);
    let input = decision(&state, true);
    state
        .decide_approval(
            "approval-1",
            &director(),
            &request(),
            &root(),
            &input,
            now(),
        )
        .unwrap();
    assert_eq!(
        state.check_route(
            &member(),
            &request(),
            &root(),
            RouteAuthorization::Approval("approval-1"),
            now()
        ),
        Err(RoutingError::ReviewerGrantRequired)
    );
    let evidence = RouteAuthorization::ApprovalAndGrant {
        approval_id: "approval-1",
        grant_id: "grant-1",
    };
    assert!(matches!(
        state.check_route(&member(), &request(), &root(), evidence, now()),
        Ok(RouteVerdict::Pass(RouteBasis::ApprovedGrant { .. }))
    ));
    assert_eq!(state.grants["grant-1"].used_count, 0);
    state
        .consume_route(&member(), &request(), &root(), evidence, "delegate", now())
        .unwrap();
    state
        .consume_route(&member(), &request(), &root(), evidence, "delegate", now())
        .unwrap();
    assert_eq!(state.grants["grant-1"].used_count, 1);
}
