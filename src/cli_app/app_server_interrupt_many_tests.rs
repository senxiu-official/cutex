use super::super::tests::{new_owner, params, queued, steered, Fake, SESSION, THREAD};
use super::*;

#[test]
fn paged_history_proves_each_original_user_message_and_ignores_admission_markers() {
    let ids = vec!["a".into(), "b".into(), "c".into()];
    let mut cursors = Vec::new();
    let checks = check_turn_history(&ids, |cursor| {
        cursors.push(cursor.map(str::to_owned));
        Some(match cursor {
            None => {
                json!({"data":[{"item":{"type":"turnContext","owner_input_batch":{"clientIds":["a","b","c"]}}}],"nextCursor":"page-2"})
            }
            Some("page-2") => {
                json!({"data":[{"item":{"type":"userMessage","clientId":"b"}}],"nextCursor":"page-3"})
            }
            Some("page-3") => {
                json!({"data":[{"type":"userMessage","clientUserMessageId":"c"}],"nextCursor":null})
            }
            _ => panic!("unexpected history cursor"),
        })
    });
    assert_eq!(
        checks,
        [TurnCheck::Absent, TurnCheck::Found, TurnCheck::Found]
    );
    assert_eq!(
        cursors,
        [None, Some("page-2".into()), Some("page-3".into())]
    );
}

#[test]
fn incomplete_or_broken_history_preserves_positive_proof_without_inventing_absence() {
    for mode in 0..4 {
        let mut calls = 0;
        let checks = check_turn_history(&["a".into(), "b".into()], |_| {
            calls += 1;
            if calls == 1 {
                return Some(
                    json!({"data":[{"type":"userMessage","clientId":"a"}],"nextCursor":"more"}),
                );
            }
            match mode {
                0 => None,
                1 => Some(json!({"data":[],"nextCursor":"more"})),
                2 => Some(json!({"data":[],"nextCursor":12})),
                _ => Some(json!({"nextCursor":null})),
            }
        });
        assert_eq!(checks, [TurnCheck::Found, TurnCheck::Unavailable]);
        assert_eq!(calls, 2);
    }
}

#[test]
fn bounded_history_scan_leaves_unresolved_ids_unknown_and_empty_selection_does_no_io() {
    let mut calls = 0;
    let checks = check_turn_history(&["a".into()], |_| {
        calls += 1;
        Some(json!({"data":[],"nextCursor":format!("page-{calls}")}))
    });
    assert_eq!(checks, [TurnCheck::Unavailable]);
    assert_eq!(calls, 64);
    assert!(
        check_turn_history(&[], |_| panic!("empty selection must not read history")).is_empty()
    );
}

fn request(records: &[InputRecord]) -> InterruptSendMany {
    InterruptSendMany {
        input_ids: records.iter().map(|r| r.input_id.clone()).collect(),
        expected_revs: records
            .iter()
            .map(|r| (r.input_id.clone(), r.rev))
            .collect(),
        expected_turn_id: "t1".into(),
        expected_runtime_generation: 7,
    }
}

fn held(f: &Fake, id: &str) -> InputRecord {
    let params = params(id, UserInputStrategy::Interrupt);
    f.parked
        .borrow_mut()
        .push(cutex::management::v2::input_ledger_settle::owner_submit_payload(&params));
    f.ledger
        .transition(
            SESSION,
            id,
            Expect::NEW,
            Next::received().create(new_owner()),
        )
        .unwrap();
    f.ledger
        .transition(SESSION, id, Expect::ANY, Next::held("hostBusy"))
        .unwrap()
        .record
}

#[test]
fn new_mailbox_turn_defers_the_whole_unsent_group_and_worker_resumes_one_ordered_start() {
    for claude in [false, true] {
        let mut f = Fake::new();
        f.claude = claude;
        let records = [steered(&f, "a"), queued(&f, "b"), held(&f, "c")];
        let request = request(&records);
        *f.after_stop.borrow_mut() = Some(LiveTurn {
            thread_id: THREAD.into(),
            generation: 7,
            active_turn: Some("mailbox-turn".into()),
        });
        let (outcome, execution) = run_many(&f, SESSION, &request, None).unwrap();
        assert!(matches!(outcome.outcome, Outcome::Held { .. }));
        assert_eq!(execution.unwrap().disposition, UserInputDisposition::Held);
        assert!(f.batches.borrow().is_empty());
        for id in ["a", "b", "c"] {
            let record = f.record(id);
            assert_eq!(record.state, InputState::Held);
            assert_eq!(record.reason.as_deref(), Some("hostBusy"));
            assert_eq!(record.details[MARK]["notSubmittedReason"], "turn_changed");
            assert_eq!(record.details[MARK]["identity"]["request"], json!(request));
            assert!(f.repository.delivery(SESSION, id).unwrap().is_none());
        }
        let calls = f.calls();
        run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(f.calls(), calls, "HTTP replay cannot drive held custody");
        let held_records = [f.record("a"), f.record("b"), f.record("c")];
        let mut resumed = super::tests::request(&held_records);
        resumed.expected_turn_id = "mailbox-turn".into();
        let identity = held_records[0].details[MARK]["identity"].clone();
        run_many_mode(&f, SESSION, &resumed, None, Some(&identity)).unwrap();
        assert_eq!(f.batches.borrow().len(), 1);
        assert_eq!(
            f.batches.borrow()[0]
                .iter()
                .map(|v| v["clientUserMessageId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(f.sent_inputs.borrow()[0][0]["text"], "c");
        assert!(f.parked.borrow().is_empty());
        let calls = f.calls();
        run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(f.calls(), calls);
    }
}

#[test]
fn failed_interrupt_keeps_queue_held_and_draft_definite_but_old_steer_unknown() {
    let f = Fake::new();
    let records = [steered(&f, "a"), queued(&f, "b"), held(&f, "c")];
    let request = request(&records);
    let draft = UserInputSubmitCommand {
        management_request_id: "draft-request".into(),
        cutex_session_id: SESSION.into(),
        thread_id: THREAD.into(),
        params: cutex::management::v2::user_input::UserInputSubmitParams {
            interrupt_send: Some(request.clone()),
            ..params("draft", UserInputStrategy::Interrupt)
        },
    };
    f.ledger
        .transition(
            SESSION,
            "draft",
            Expect::NEW,
            Next::received().create(new_owner()),
        )
        .unwrap();
    *f.interrupt.borrow_mut() = Err(InterruptFailure::Timeout("lost interrupt response".into()));
    run_many(&f, SESSION, &request, Some(&draft)).unwrap();
    assert_eq!(f.record("a").state, InputState::Uncertain);
    for id in ["b", "c", "draft"] {
        assert_eq!(f.record(id).state, InputState::Held);
        assert_eq!(
            f.record(id).details[MARK]["notSubmittedReason"],
            "interrupt_failed"
        );
        assert!(f.repository.delivery(SESSION, id).unwrap().is_none());
    }
    assert!(f.batches.borrow().is_empty());
}

#[test]
fn native_not_submitted_keeps_group_for_worker_and_blocked_refusal_does_not_loop() {
    for reason in [
        "not_idle",
        "pending_trigger_turn",
        "plan_mode",
        "history_unavailable",
        "hook_blocked",
    ] {
        let f = Fake::new();
        let records = [queued(&f, "a"), queued(&f, "b")];
        let mut error = execution_error_message(
            "native_request",
            "native_request_failed",
            "definitely not submitted",
            false,
            false,
        );
        error.details = json!({"nativeError":{"data":{"ownerInputBatchNotSubmitted":reason}}});
        *f.send.borrow_mut() = Some(Err(error));
        run_many(&f, SESSION, &request(&records), None).unwrap();
        let automatic = matches!(reason, "not_idle" | "pending_trigger_turn" | "plan_mode");
        for id in ["a", "b"] {
            let record = f.record(id);
            assert_eq!(
                record.state,
                if automatic {
                    InputState::Held
                } else {
                    InputState::Waiting
                }
            );
            assert_eq!(record.details[MARK]["notSubmittedReason"], reason);
            assert_eq!(record.details[MARK]["deferred"], automatic);
            assert_eq!(
                record.details[MARK]["nativeError"]["data"]["ownerInputBatchNotSubmitted"],
                reason
            );
            if !automatic {
                assert!(record.actions.contains(&il::Action::Retry));
            }
            assert!(f.repository.delivery(SESSION, id).unwrap().is_none());
        }
        assert_eq!(f.parked.borrow().len(), if automatic { 2 } else { 0 });
    }
}

#[test]
fn unmarked_native_and_claude_identity_refusals_never_authorize_retry() {
    for (claude, message, data) in [
        (false, "ownerInputs identity already admitted", json!(null)),
        (true, "already appears in history", json!({"code":-32010})),
        (
            false,
            "malformed empty unsent marker",
            json!({"ownerInputBatchNotSubmitted":""}),
        ),
    ] {
        let mut f = Fake::new();
        f.claude = claude;
        let records = [queued(&f, "a"), queued(&f, "b")];
        let mut error = execution_error_message(
            "native_request",
            "native_request_failed",
            message,
            false,
            false,
        );
        error.details = json!({"nativeError":{"message":message,"data":data}});
        *f.send.borrow_mut() = Some(Err(error));
        let request = request(&records);
        assert!(run_many(&f, SESSION, &request, None).unwrap().1.is_none());
        for id in ["a", "b"] {
            let record = f.record(id);
            assert_eq!(record.state, InputState::Uncertain);
            assert_eq!(record.actions, vec![il::Action::Dismiss]);
            assert!(il::owner_batch_retry_blocked(&record));
            assert!(!cutex::management::v2::input_ledger_settle::retryable(
                &record
            ));
            let refused = cutex::management::v2::input_ledger_settle::retry_transition(
                &f.ledger, SESSION, id, record.rev,
            )
            .unwrap_err();
            assert_eq!(refused.code, "input_not_retryable");
            assert_eq!(
                f.record(id).rev,
                record.rev,
                "retry RPC leaves custody unchanged"
            );
            assert_ne!(record.details[MARK]["blocked"], true);
            assert_eq!(record.details[MARK]["nativeError"]["message"], message);
            assert!(matches!(
                f.repository.delivery(SESSION, id).unwrap(),
                Some(InputDelivery::Unknown { .. })
            ));
        }
        assert!(f.parked.borrow().is_empty());
        let calls = f.calls();
        run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(f.calls(), calls, "a refused batch replay never resends");
    }
}

#[test]
fn incomplete_deferred_group_blocks_all_survivors_in_one_pass_without_native_io() {
    for discarded in [false, true] {
        let f = Fake::new();
        let records = [queued(&f, "a"), queued(&f, "b"), queued(&f, "c")];
        *f.after_stop.borrow_mut() = Some(LiveTurn {
            thread_id: THREAD.into(),
            generation: 7,
            active_turn: Some("competing".into()),
        });
        let original = request(&records);
        run_many(&f, SESSION, &original, None).unwrap();
        f.ledger
            .transition(SESSION, "b", Expect::ANY, Next::failed("admission_timeout"))
            .unwrap();
        if discarded {
            f.ledger
                .transition(SESSION, "b", Expect::ANY, Next::discarded("ownerDiscarded"))
                .unwrap();
        }
        f.held_finish(SESSION, "b").unwrap();
        let settled = f.record("b");
        let resumed = request(&[f.record("a"), settled.clone(), f.record("c")]);
        let identity = f.record("a").details[MARK]["identity"].clone();
        let calls = f.calls();
        let revision = f.ledger.get(SESSION, &[]).unwrap().state_revision;
        assert!(run_many_mode(&f, SESSION, &resumed, None, Some(&identity))
            .unwrap()
            .1
            .is_none());
        assert!(f.batches.borrow().is_empty());
        assert_eq!(f.record("b"), settled, "terminal member remains untouched");
        for id in ["a", "c"] {
            let record = f.record(id);
            assert_eq!(record.state, InputState::Waiting);
            assert_eq!(record.reason.as_deref(), Some("nativeHeld"));
            assert_eq!(
                record.details[MARK]["notSubmittedReason"],
                "groupIncomplete"
            );
            assert_eq!(record.details[MARK]["incompleteIds"], json!(["b"]));
            assert!(record.actions.contains(&il::Action::Retry));
            assert_eq!(
                record.state_revision,
                revision + 1,
                "survivors share one atomic ledger revision"
            );
        }
        assert!(f.parked.borrow().is_empty());
        assert_eq!(f.calls()[calls.len()..], ["held finish a", "held finish c"]);
        let calls = f.calls();
        let revisions = [f.record("a").rev, f.record("c").rev];
        run_many_mode(&f, SESSION, &resumed, None, Some(&identity)).unwrap();
        assert_eq!(f.calls(), calls, "no second round of survivor failures");
        assert_eq!([f.record("a").rev, f.record("c").rev], revisions);
    }
}

#[test]
fn three_unread_owner_ids_are_one_ordered_start_for_both_runtime_kinds() {
    for claude in [false, true] {
        let mut f = Fake::new();
        f.claude = claude;
        let records = [steered(&f, "a"), steered(&f, "b"), steered(&f, "c")];
        let request = request(&records);
        let (result, execution) = run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(result.submitted, ["a", "b", "c"]);
        assert_eq!(result.carrier_input_id.as_deref(), Some("c"));
        assert!(execution.is_some());
        assert_eq!(
            f.batches.borrow()[0]
                .iter()
                .map(|v| v["clientUserMessageId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(f.sent_inputs.borrow()[0][0]["text"], "c");
        assert_eq!(
            f.calls()
                .iter()
                .filter(|s| s.starts_with("interrupt "))
                .count(),
            1
        );
        assert_eq!(
            f.calls()
                .iter()
                .filter(|s| s.starts_with("turn/start "))
                .count(),
            1
        );
        for id in ["a", "b", "c"] {
            let r = f.record(id);
            assert_eq!(r.state, InputState::InContext);
            assert_eq!(r.turn_id.as_deref(), Some("t2"));
        }
        let calls = f.calls();
        let (again, _) = run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(again.submitted, ["a", "b", "c"]);
        assert_eq!(f.calls(), calls, "replay does no interrupt or native IO");
    }
}

#[test]
fn partial_read_only_carries_the_absent_ids_and_unknown_history_is_not_absence() {
    for claude in [false, true] {
        let mut f = Fake::new();
        f.claude = claude;
        let records = [steered(&f, "a"), steered(&f, "b"), steered(&f, "c")];
        *f.checks.borrow_mut() = vec![TurnCheck::Found, TurnCheck::Unavailable, TurnCheck::Absent];
        let request = request(&records);
        let (result, _) = run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(result.already_in_context, ["a"]);
        assert_eq!(result.submitted, ["c"]);
        assert_eq!(f.record("a").turn_id.as_deref(), Some("t1"));
        assert_eq!(f.record("b").state, InputState::Uncertain);
        assert!(f.batches.borrow()[0].is_empty());
        let before = f.calls();
        run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(before, f.calls());
    }
}

#[test]
fn selected_queue_and_draft_are_kept_in_order_without_new_logical_ids() {
    let f = Fake::new();
    let records = [steered(&f, "a"), queued(&f, "b")];
    let request = request(&records);
    let draft = UserInputSubmitCommand {
        management_request_id: "draft-request".into(),
        cutex_session_id: SESSION.into(),
        thread_id: THREAD.into(),
        params: cutex::management::v2::user_input::UserInputSubmitParams {
            interrupt_send: Some(request.clone()),
            expected_turn_id: Some("t1".into()),
            ..params("draft", UserInputStrategy::Interrupt)
        },
    };
    f.ledger
        .transition(
            SESSION,
            "draft",
            Expect::NEW,
            Next::received().create(new_owner()),
        )
        .unwrap();
    let (result, _) = run_many(&f, SESSION, &request, Some(&draft)).unwrap();
    assert_eq!(result.submitted, ["a", "b", "draft"]);
    assert_eq!(result.carrier_input_id.as_deref(), Some("draft"));
    assert_eq!(
        f.batches.borrow()[0]
            .iter()
            .map(|v| v["clientUserMessageId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(f.sent_inputs.borrow()[0][0]["text"], "draft");
    assert!(f.repository.list(SESSION).unwrap().1.is_empty());
    let recovery = f.ledger.payload(SESSION, "draft").unwrap().unwrap();
    assert_eq!(recovery["clientUserMessageId"], "draft");
    assert_eq!(recovery["input"], json!(draft.params.input));
    assert_eq!(recovery["origin"], json!(draft.params.origin));
    assert_eq!(recovery["strategy"], "auto");
    assert!(recovery["expectedTurnId"].is_null());
    assert!(recovery.get("interruptSend").is_none());
    assert!(recovery.get("joinDelegation").is_none());
    let calls = f.calls();
    let (again, _) = run_many(&f, SESSION, &request, Some(&draft)).unwrap();
    assert_eq!(again.submitted, ["a", "b", "draft"]);
    assert_eq!(f.calls(), calls, "original operation still deduplicates");
}

#[test]
fn old_pin_refuses_before_interrupt_and_unknown_start_or_interrupt_never_replays() {
    for mode in [0, 1, 2, 3] {
        let mut f = Fake::new();
        let records = [steered(&f, "a"), steered(&f, "b")];
        let request = request(&records);
        if mode == 0 {
            f.batch_supported = false;
        }
        if mode == 1 {
            *f.interrupt.borrow_mut() = Err(InterruptFailure::Timeout("lost".into()));
        }
        if mode == 2 {
            *f.send.borrow_mut() = Some(Err(execution_error_message(
                "native_request",
                "request_outcome_unknown",
                "lost start",
                false,
                true,
            )));
        }
        if mode == 3 {
            *f.send.borrow_mut() = Some(Err(execution_error_message(
                "native_request",
                "native_request_failed",
                "refused",
                false,
                false,
            )));
        }
        let result = run_many(&f, SESSION, &request, None);
        if mode == 0 {
            assert_eq!(result.unwrap_err().code, "interrupt_send_many_unsupported");
            assert!(f.calls().is_empty());
            continue;
        }
        assert!(result.unwrap().1.is_none());
        for id in ["a", "b"] {
            assert_eq!(f.record(id).state, InputState::Uncertain);
        }
        let calls = f.calls();
        run_many(&f, SESSION, &request, None).unwrap();
        assert_eq!(calls, f.calls());
    }
}

#[test]
fn lost_batch_reply_leaves_no_queue_worker_that_can_resend_a_prefix_alone() {
    let f = Fake::new();
    let records = [queued(&f, "a"), queued(&f, "b")];
    let request = request(&records);
    *f.send.borrow_mut() = Some(Err(execution_error_message(
        "native_request",
        "request_outcome_unknown",
        "lost",
        false,
        true,
    )));
    run_many(&f, SESSION, &request, None).unwrap();
    assert!(f.repository.list(SESSION).unwrap().1.is_empty());
    for id in ["a", "b"] {
        assert_eq!(f.record(id).state, InputState::Uncertain);
        assert_eq!(
            f.ledger.payload(SESSION, id).unwrap().unwrap()["input"][0]["text"],
            id
        );
    }
    let calls = f.calls();
    run_many(&f, SESSION, &request, None).unwrap();
    assert_eq!(f.calls(), calls);
}

#[test]
fn a_dropped_owners_old_external_bundle_is_not_claimed_by_the_new_owner_turn() {
    for read in [false, true] {
        let f = Fake::new();
        let before = steered(&f, "a");
        let mut carry = Next::accepted();
        carry.carries = Some(vec!["old-external-note".into()]);
        let a = f
            .ledger
            .transition(SESSION, "a", Expect::rev(before.rev), carry)
            .unwrap()
            .record;
        let b = steered(&f, "b");
        *f.checks.borrow_mut() = vec![
            if read {
                TurnCheck::Found
            } else {
                TurnCheck::Absent
            },
            TurnCheck::Absent,
        ];
        run_many(&f, SESSION, &request(&[a, b]), None).unwrap();
        assert_eq!(
            f.record("a").carries,
            if read {
                vec!["old-external-note".to_owned()]
            } else {
                Vec::new()
            }
        );
        assert!(f.record("b").carries.is_empty());
        assert!(f.batches.borrow()[0]
            .iter()
            .all(|entry| entry.get("ownerCarries").is_none()));
    }
}
