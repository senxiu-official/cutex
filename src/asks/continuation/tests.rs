use super::*;

fn time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-27T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
fn at(seconds: i64) -> DateTime<Utc> {
    time() + Duration::seconds(seconds)
}
fn context(n: u32) -> AttemptIdentity {
    AttemptIdentity {
        delegation_id: "ask-a".into(),
        attempt_id: format!("ask-a#{n}"),
        owner_session_id: "handler".into(),
        runtime_generation: 7,
        parent_turn_id: format!("turn-{n}"),
    }
}
fn handler() -> Actor {
    Actor::Agent {
        session_id: "handler".into(),
    }
}
fn idle() -> Guards {
    Guards {
        online: true,
        permission_valid: true,
        occurrence_valid: true,
        ..Guards::default()
    }
}
fn running() -> Guards {
    Guards {
        running_turn: true,
        ..idle()
    }
}
fn request(n: u32) -> DeclareRequest {
    DeclareRequest {
        action_id: format!("declare-{n}"),
        expected_attempt_id: context(n).attempt_id,
        expected_revision: None,
        note: format!("stage {n}"),
        wake: None,
    }
}
fn declare(data: &mut ContinuationData, n: u32, now: DateTime<Utc>) -> MutationReceipt {
    data.declare(
        &handler(),
        &context(n),
        &request(n),
        false,
        running(),
        ConditionSupport::default(),
        now,
    )
    .unwrap()
}
fn stage(data: &mut ContinuationData, n: u32, now: DateTime<Utc>) {
    assert_eq!(
        data.end_turn(
            &context(n),
            EndKind::Normal,
            Some(format!("artifact-stage-{n}")),
            idle(),
            now,
            Some("director")
        ),
        EndDisposition::StageNote
    );
}
fn waiting() -> ContinuationData {
    let mut data = ContinuationData::default();
    declare(&mut data, 1, time());
    stage(&mut data, 1, time());
    data
}
fn ready(data: &mut ContinuationData, now: DateTime<Utc>) -> (u64, String) {
    assert_eq!(
        data.evaluate(idle(), now, Some("director")),
        Eligibility::Ready
    );
    let r = data.current.as_ref().unwrap();
    (r.revision, r.wake.as_ref().unwrap().id.clone())
}
fn admit(data: &mut ContinuationData, n: u32, now: DateTime<Utc>) {
    let (revision, wake) = ready(data, now);
    let message = format!("native-{n}");
    assert!(data
        .reserve_admission(
            revision,
            &wake,
            &context(n).attempt_id,
            7,
            &message,
            idle(),
            now,
            Some("director")
        )
        .unwrap());
    assert!(data
        .admission_accepted(&wake, &context(n), &message)
        .unwrap());
    assert!(!data
        .admission_accepted(&wake, &context(n), &message)
        .unwrap());
}
fn child(id: &str) -> ChildRef {
    ChildRef {
        kind: ChildKind::NativeAgent,
        id: id.into(),
    }
}
fn spawn(data: &mut ContinuationData, parent: &AttemptIdentity, id: &str) {
    data.note_spawn(AuthenticatedSpawn::from_verified_bridge(
        parent.clone(),
        child(id),
        format!("spawn-{id}"),
    ))
    .unwrap();
}
fn terminal(
    data: &mut ContinuationData,
    parent: &AttemptIdentity,
    id: &str,
    outcome: ChildOutcome,
) -> Result<bool, Error> {
    data.note_terminal(AuthenticatedTerminal::from_verified_bridge(
        parent.clone(),
        child(id),
        TerminalFact {
            source_event_id: format!("terminal-{id}"),
            outcome,
        },
    ))
}
fn child_request(ids: &[&str], until: Until) -> DeclareRequest {
    DeclareRequest {
        wake: Some(WakeCondition::Children {
            children: ids.iter().map(|id| child(id)).collect(),
            until,
        }),
        ..request(1)
    }
}
fn children_supported() -> ConditionSupport {
    ConditionSupport {
        native_children: true,
        delegation_children: true,
    }
}

#[test]
fn absent_declaration_preserves_the_default_answer_contract() {
    let mut data = ContinuationData::default();
    assert_eq!(
        data.end_turn(
            &context(1),
            EndKind::Normal,
            Some("final".into()),
            idle(),
            time(),
            None
        ),
        EndDisposition::DefaultAnswer
    );
    assert!(data.is_empty());
    assert_eq!(data.evaluate(idle(), time(), None), Eligibility::None);
}

#[test]
fn normal_end_stages_the_whole_artifact_and_never_becomes_a_result() {
    let mut data = ContinuationData::default();
    let receipt = declare(&mut data, 1, time());
    assert_eq!(receipt.continuation.round_number, 1);
    stage(&mut data, 1, at(30));
    assert_eq!(data.current.as_ref().unwrap().state, Phase::Waiting);
    assert_eq!(
        data.stages[0].final_message_artifact.as_deref(),
        Some("artifact-stage-1")
    );
    assert_eq!(data.stages[0].declaration_note, "stage 1");
    assert_eq!(
        data.end_turn(
            &context(1),
            EndKind::Normal,
            Some("duplicate".into()),
            idle(),
            at(31),
            None
        ),
        EndDisposition::AlreadyStaged
    );
    assert_eq!(data.stages.len(), 1);
    assert_eq!(data.evaluate(idle(), at(89), None), Eligibility::Waiting);
    assert_eq!(data.evaluate(idle(), at(90), None), Eligibility::Ready);
}

#[test]
fn normal_answer_default_returns_after_a_resume_without_new_declaration() {
    let mut data = waiting();
    admit(&mut data, 2, at(60));
    assert_eq!(
        data.end_turn(
            &context(2),
            EndKind::Normal,
            Some("real-final".into()),
            idle(),
            at(70),
            None
        ),
        EndDisposition::DefaultAnswer
    );
    assert_eq!(data.current.as_ref().unwrap().state, Phase::Cancelled);
    assert_eq!(data.stages.len(), 1);
    assert_eq!(
        data.evaluate(idle(), at(1000), None),
        Eligibility::Cancelled
    );
}

#[test]
fn declaration_is_bound_to_handler_exact_attempt_lease_generation_and_turn() {
    let mut data = ContinuationData::default();
    let other = Actor::Agent {
        session_id: "another".into(),
    };
    assert_eq!(
        data.declare(
            &other,
            &context(1),
            &request(1),
            false,
            running(),
            children_supported(),
            time()
        ),
        Err(Error::NotCurrentHandler)
    );
    assert_eq!(
        data.declare(
            &Actor::Owner,
            &context(1),
            &request(1),
            false,
            running(),
            children_supported(),
            time()
        ),
        Err(Error::NotCurrentHandler)
    );
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &request(2),
            false,
            running(),
            children_supported(),
            time()
        ),
        Err(Error::NotCurrentHandler)
    );
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &request(1),
            false,
            idle(),
            children_supported(),
            time()
        ),
        Err(Error::Guarded)
    );
    declare(&mut data, 1, time());
    for wrong in [
        AttemptIdentity {
            runtime_generation: 8,
            ..context(1)
        },
        AttemptIdentity {
            parent_turn_id: "old".into(),
            ..context(1)
        },
        context(2),
    ] {
        assert_eq!(
            data.end_turn(&wrong, EndKind::Normal, None, idle(), at(1), None),
            EndDisposition::DefaultAnswer
        );
        assert!(data.stages.is_empty());
    }
    stage(&mut data, 1, at(2));
}

#[test]
fn declarations_and_awaits_conflict_in_both_orders() {
    let mut data = ContinuationData::default();
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &request(1),
            true,
            running(),
            children_supported(),
            time()
        ),
        Err(Error::Conflict)
    );
    assert!(data.is_empty());
    declare(&mut data, 1, time());
    assert_eq!(data.check_await_conflict(&context(1)), Err(Error::Conflict));
    assert_eq!(data.check_await_conflict(&context(2)), Ok(()));
}

#[test]
fn cas_updates_and_replay_cannot_move_the_budget_or_wake_twice() {
    let mut data = ContinuationData::default();
    let first = declare(&mut data, 1, time());
    let replay = declare(&mut data, 1, at(50));
    assert!(replay.replayed);
    assert_eq!(first.continuation, replay.continuation);
    let snapshot = data.clone();
    let mut update = request(1);
    update.action_id = "update".into();
    update.note = "wait longer".into();
    update.wake = Some(WakeCondition::After { delay_seconds: 300 });
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &update,
            false,
            running(),
            children_supported(),
            at(10)
        ),
        Err(Error::RevisionConflict)
    );
    assert_eq!(data, snapshot);
    update.expected_revision = Some(1);
    let second = data
        .declare(
            &handler(),
            &context(1),
            &update,
            false,
            running(),
            children_supported(),
            at(10),
        )
        .unwrap();
    assert_eq!(second.continuation.round_number, 1);
    assert_eq!(second.continuation.started_at, time());
    assert_eq!(second.continuation.revision, 2);
    let mut concurrent = update.clone();
    concurrent.action_id = "concurrent".into();
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &concurrent,
            false,
            running(),
            children_supported(),
            at(11)
        ),
        Err(Error::RevisionConflict)
    );
    update.note = "different payload".into();
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &update,
            false,
            running(),
            children_supported(),
            at(12)
        ),
        Err(Error::ActionPayloadConflict)
    );
    stage(&mut data, 1, at(20));
    assert_eq!(data.evaluate(idle(), at(319), None), Eligibility::Waiting);
    assert_eq!(data.evaluate(idle(), at(320), None), Eligibility::Ready);
    let after = data.clone();
    // A lost response replay returns the original receipt, without reverting
    // today's waiting/ready state or registering another timer.
    assert!(declare(&mut data, 1, at(400)).replayed);
    assert_eq!(data, after);
}

#[test]
fn timers_accept_only_60_through_86400_and_note_limits_are_whole() {
    for delay in [0, 1, 59, 86_401, u32::MAX] {
        let mut data = ContinuationData::default();
        let req = DeclareRequest {
            wake: Some(WakeCondition::After {
                delay_seconds: delay,
            }),
            ..request(1)
        };
        assert_eq!(
            data.declare(
                &handler(),
                &context(1),
                &req,
                false,
                running(),
                children_supported(),
                time()
            ),
            Err(Error::InvalidArguments)
        );
        assert!(data.is_empty());
    }
    for delay in [60, 86_400] {
        let mut data = ContinuationData::default();
        let req = DeclareRequest {
            wake: Some(WakeCondition::After {
                delay_seconds: delay,
            }),
            note: "中".repeat(MAX_NOTE_CHARS),
            ..request(1)
        };
        data.declare(
            &handler(),
            &context(1),
            &req,
            false,
            running(),
            children_supported(),
            time(),
        )
        .unwrap();
        assert_eq!(
            data.current
                .as_ref()
                .unwrap()
                .declaration
                .note
                .chars()
                .count(),
            MAX_NOTE_CHARS
        );
    }
    let req = DeclareRequest {
        note: "中".repeat(MAX_NOTE_CHARS + 1),
        ..request(1)
    };
    assert_eq!(
        ContinuationData::default().declare(
            &handler(),
            &context(1),
            &req,
            false,
            running(),
            children_supported(),
            time()
        ),
        Err(Error::InvalidArguments)
    );
    assert!(serde_json::from_str::<WakeCondition>(
        r#"{"kind":"children","children":[],"until":"each"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<WakeCondition>(
        r#"{"kind":"after","delaySeconds":60,"poll":true}"#
    )
    .is_err());
}

#[test]
fn unsupported_children_are_explicit_and_never_fall_back_to_polling() {
    let mut data = ContinuationData::default();
    spawn(&mut data, &context(1), "one");
    let before = data.clone();
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &child_request(&["one"], Until::All),
            false,
            running(),
            ConditionSupport::default(),
            time()
        ),
        Err(Error::ConditionUnsupported)
    );
    assert_eq!(data, before);
    assert_eq!(
        Error::ConditionUnsupported.code(),
        "continuation_condition_unsupported"
    );
}

#[test]
fn arbitrary_ids_and_foreign_contexts_cannot_bind_children() {
    let req = child_request(&["one"], Until::All);
    for foreign in [
        context(2),
        AttemptIdentity {
            runtime_generation: 8,
            ..context(1)
        },
        AttemptIdentity {
            owner_session_id: "other".into(),
            ..context(1)
        },
    ] {
        let mut data = ContinuationData::default();
        spawn(&mut data, &foreign, "one");
        assert_eq!(
            data.declare(
                &handler(),
                &context(1),
                &req,
                false,
                running(),
                children_supported(),
                time()
            ),
            Err(Error::UnverifiedChild)
        );
    }
    let mut data = ContinuationData::default();
    assert_eq!(
        terminal(&mut data, &context(1), "one", ChildOutcome::Completed),
        Err(Error::UnverifiedChild)
    );
    assert!(data.is_empty());
}

#[test]
fn completion_before_declaration_and_duplicate_events_are_durable_facts() {
    let mut data = ContinuationData::default();
    spawn(&mut data, &context(1), "one");
    assert!(terminal(&mut data, &context(1), "one", ChildOutcome::Completed).unwrap());
    assert!(!terminal(&mut data, &context(1), "one", ChildOutcome::Completed).unwrap());
    assert!(!data
        .note_spawn(AuthenticatedSpawn::from_verified_bridge(
            context(1),
            child("one"),
            "spawn-one".into()
        ))
        .unwrap());
    assert_eq!(
        terminal(
            &mut data,
            &context(1),
            "one",
            ChildOutcome::Cancelled {
                reason: "forged change".into()
            }
        ),
        Err(Error::EvidenceConflict)
    );
    data.declare(
        &handler(),
        &context(1),
        &child_request(&["one"], Until::All),
        false,
        running(),
        children_supported(),
        time(),
    )
    .unwrap();
    stage(&mut data, 1, time());
    let bytes = serde_json::to_vec(&data).unwrap();
    let mut reloaded: ContinuationData = serde_json::from_slice(&bytes).unwrap();
    reloaded.recover_after_restart();
    assert_eq!(
        reloaded.evaluate(idle(), at(59), None),
        Eligibility::Waiting
    );
    admit(&mut reloaded, 2, at(60));
    assert_eq!(reloaded.current.as_ref().unwrap().round_number, 2);
}

#[test]
fn all_and_any_count_failures_and_cancellations_as_terminal() {
    for until in [Until::All, Until::Any] {
        let mut data = ContinuationData::default();
        spawn(&mut data, &context(1), "one");
        spawn(&mut data, &context(1), "two");
        data.declare(
            &handler(),
            &context(1),
            &child_request(&["one", "two"], until),
            false,
            running(),
            children_supported(),
            time(),
        )
        .unwrap();
        stage(&mut data, 1, time());
        assert_eq!(data.evaluate(idle(), at(60), None), Eligibility::Waiting);
        terminal(
            &mut data,
            &context(1),
            "one",
            ChildOutcome::Failed {
                reason: "exit 1".into(),
            },
        )
        .unwrap();
        assert_eq!(
            data.evaluate(idle(), at(60), None),
            if until == Until::All {
                Eligibility::Waiting
            } else {
                Eligibility::Ready
            }
        );
        terminal(
            &mut data,
            &context(1),
            "two",
            ChildOutcome::Cancelled {
                reason: "cancelled".into(),
            },
        )
        .unwrap();
        assert_eq!(data.evaluate(idle(), at(60), None), Eligibility::Ready);
        assert!(
            matches!(&data.child_facts[0].terminal.as_ref().unwrap().outcome, ChildOutcome::Failed { reason } if reason == "exit 1")
        );
    }
}

#[test]
fn children_have_a_real_bound_of_1_through_64_and_no_duplicates() {
    for count in [0, 65] {
        let req = DeclareRequest {
            wake: Some(WakeCondition::Children {
                children: (0..count).map(|i| child(&format!("c{i}"))).collect(),
                until: Until::All,
            }),
            ..request(1)
        };
        assert_eq!(
            ContinuationData::default().declare(
                &handler(),
                &context(1),
                &req,
                false,
                running(),
                children_supported(),
                time()
            ),
            Err(Error::InvalidArguments)
        );
    }
    let mut data = ContinuationData::default();
    let ids: Vec<_> = (0..64).map(|i| format!("c{i}")).collect();
    for id in &ids {
        spawn(&mut data, &context(1), id);
    }
    let mut req = child_request(
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        Until::All,
    );
    data.declare(
        &handler(),
        &context(1),
        &req,
        false,
        running(),
        children_supported(),
        time(),
    )
    .unwrap();
    req.action_id = "duplicates".into();
    req.expected_revision = Some(1);
    req.wake = Some(WakeCondition::Children {
        children: vec![child("c0"), child("c0")],
        until: Until::Any,
    });
    assert_eq!(
        data.declare(
            &handler(),
            &context(1),
            &req,
            false,
            running(),
            children_supported(),
            time()
        ),
        Err(Error::InvalidArguments)
    );
}

#[test]
fn only_one_wake_reservation_and_one_admission_survive_a_reload() {
    let mut data = waiting();
    let (revision, wake) = ready(&mut data, at(60));
    assert_eq!(
        data.reserve_admission(
            revision - 1,
            &wake,
            "ask-a#2",
            7,
            "native-2",
            idle(),
            at(60),
            None
        ),
        Err(Error::RevisionConflict)
    );
    assert!(data
        .reserve_admission(
            revision,
            &wake,
            "ask-a#2",
            7,
            "native-2",
            idle(),
            at(60),
            None
        )
        .unwrap());
    assert!(!data
        .reserve_admission(revision, &wake, "ask-a#3", 7, "other", idle(), at(60), None)
        .unwrap());
    let mut reload: ContinuationData =
        serde_json::from_slice(&serde_json::to_vec(&data).unwrap()).unwrap();
    reload.recover_after_restart();
    assert_eq!(
        reload.evaluate(idle(), at(61), None),
        Eligibility::Reconciling
    );
    assert!(!reload
        .reserve_admission(
            revision,
            &wake,
            "ask-a#2",
            7,
            "native-2",
            idle(),
            at(61),
            None
        )
        .unwrap());
    assert!(reload
        .admission_accepted(&wake, &context(2), "native-2")
        .unwrap());
    assert!(!reload
        .admission_accepted(&wake, &context(2), "native-2")
        .unwrap());
    assert_eq!(reload.current.as_ref().unwrap().round_number, 2);
    assert_eq!(
        reload.admission_accepted(&wake, &context(3), "native-2"),
        Err(Error::EvidenceConflict)
    );
}

#[test]
fn unknown_submission_and_running_turn_never_open_another_round() {
    let mut data = waiting();
    let (revision, wake) = ready(&mut data, at(60));
    assert!(data
        .reserve_admission(
            revision,
            &wake,
            "ask-a#2",
            7,
            "native-2",
            idle(),
            at(60),
            None
        )
        .unwrap());
    data.submission_unknown(&wake).unwrap();
    data.submission_unknown(&wake).unwrap();
    assert_eq!(
        data.evaluate(idle(), at(100), None),
        Eligibility::Reconciling
    );
    assert!(!data
        .reserve_admission(
            revision,
            &wake,
            "ask-a#2",
            7,
            "native-2",
            idle(),
            at(100),
            None
        )
        .unwrap());
    assert_eq!(data.current.as_ref().unwrap().round_number, 1);
    assert!(data
        .admission_accepted(&wake, &context(2), "native-2")
        .unwrap());
    assert_eq!(
        data.evaluate(running(), at(100), None),
        Eligibility::Running
    );
    assert_eq!(data.current.as_ref().unwrap().round_number, 2);
}

#[test]
fn every_live_admission_guard_is_rechecked_after_ready() {
    let held = [
        Guards {
            paused: true,
            ..idle()
        },
        Guards {
            online: false,
            ..idle()
        },
        Guards {
            permission_valid: false,
            ..idle()
        },
        Guards {
            occurrence_valid: false,
            ..idle()
        },
        Guards {
            running_turn: true,
            ..idle()
        },
        Guards {
            submission_unknown: true,
            ..idle()
        },
        Guards {
            restarting: true,
            ..idle()
        },
    ];
    for guard in held {
        let mut data = waiting();
        let (revision, wake) = ready(&mut data, at(60));
        assert_eq!(
            data.reserve_admission(
                revision,
                &wake,
                "ask-a#2",
                7,
                "native-2",
                guard,
                at(60),
                None
            ),
            Err(Error::Guarded)
        );
        assert_eq!(data.current.as_ref().unwrap().round_number, 1);
        assert!(matches!(
            data.current
                .as_ref()
                .unwrap()
                .wake
                .as_ref()
                .unwrap()
                .submission,
            Submission::Unclaimed
        ));
    }
}

#[test]
fn pause_retains_ready_and_never_renews_the_wall_clock_budget() {
    let mut data = waiting();
    let paused = Guards {
        paused: true,
        ..idle()
    };
    assert_eq!(
        data.evaluate(paused, at(60), Some("director")),
        Eligibility::Held
    );
    assert_eq!(data.current.as_ref().unwrap().state, Phase::Ready);
    assert_eq!(
        data.evaluate(paused, at(86_399), Some("director")),
        Eligibility::Held
    );
    assert_eq!(
        data.evaluate(paused, at(86_400), Some("director")),
        Eligibility::Limited
    );
    assert_eq!(
        data.current.as_ref().unwrap().limit_reason,
        Some(LimitReason::Elapsed)
    );
    assert_eq!(
        data.evaluate(idle(), at(86_401), Some("director")),
        Eligibility::Limited
    );
    assert_eq!(data.stages.len(), 1);
}

#[test]
fn stop_and_archive_cancel_wakes_but_retain_stages_and_late_evidence() {
    for guard in [
        Guards {
            stopped: true,
            ..idle()
        },
        Guards {
            archived: true,
            ..idle()
        },
    ] {
        let mut data = waiting();
        let (revision, wake) = ready(&mut data, at(60));
        assert_eq!(
            data.reserve_admission(
                revision,
                &wake,
                "ask-a#2",
                7,
                "native-2",
                guard,
                at(60),
                None
            ),
            Err(Error::Guarded)
        );
        assert_eq!(data.evaluate(idle(), at(100), None), Eligibility::Cancelled);
        assert_eq!(data.stages.len(), 1);
        assert!(data.current.as_ref().unwrap().wake.is_some());
        assert_eq!(
            data.reset_budget(
                Authority {
                    actor: &Actor::Owner,
                    current_director: None
                },
                &ResetRequest {
                    action_id: "reset".into(),
                    expected_revision: data.current.as_ref().unwrap().revision
                },
                idle(),
                at(100)
            ),
            Err(Error::NotLimited)
        );
    }
    let mut data = waiting();
    let (revision, wake) = ready(&mut data, at(60));
    data.reserve_admission(
        revision,
        &wake,
        "ask-a#2",
        7,
        "native-2",
        idle(),
        at(60),
        None,
    )
    .unwrap();
    data.cancel("stop_fence");
    assert!(data
        .admission_accepted(&wake, &context(2), "native-2")
        .unwrap());
    assert_eq!(data.current.as_ref().unwrap().state, Phase::Cancelled);
    assert_eq!(data.evaluate(idle(), at(100), None), Eligibility::Cancelled);
}

#[test]
fn abnormal_end_never_turns_into_a_normal_yield() {
    for end in [EndKind::Interrupted, EndKind::Failed, EndKind::Unknown] {
        let mut data = ContinuationData::default();
        declare(&mut data, 1, time());
        assert_eq!(
            data.end_turn(
                &context(1),
                end,
                Some("retained-on-attempt".into()),
                idle(),
                at(1),
                None
            ),
            EndDisposition::DefaultAnswer
        );
        assert!(data.stages.is_empty());
        assert_eq!(data.evaluate(idle(), at(100), None), Eligibility::Cancelled);
    }
}

#[test]
fn exactly_32_rounds_then_limited_never_completed() {
    let mut data = waiting();
    for n in 2..=32 {
        let now = at(i64::from(n - 1) * 60);
        admit(&mut data, n, now);
        assert_eq!(data.current.as_ref().unwrap().round_number, n);
        declare(&mut data, n, now);
        stage(&mut data, n, now);
    }
    let record = data.current.as_ref().unwrap();
    assert_eq!(record.state, Phase::Limited);
    assert_eq!(record.round_number, 32);
    assert_eq!(record.limit_reason, Some(LimitReason::Rounds));
    assert_eq!(data.stages.len(), 32);
    assert_eq!(
        data.evaluate(idle(), at(1980), Some("director")),
        Eligibility::Limited
    );
    let record = data.current.as_ref().unwrap();
    assert_eq!(
        data.reserve_admission(
            record.revision,
            &record.wake.as_ref().unwrap().id.clone(),
            "ask-a#33",
            7,
            "native-33",
            idle(),
            at(1980),
            None
        ),
        Err(Error::Guarded)
    );
}

#[test]
fn cap_while_running_allows_the_turn_to_finish_and_keep_its_stage() {
    let mut data = waiting();
    admit(&mut data, 2, at(60));
    assert_eq!(
        data.evaluate(running(), at(86_400), Some("director")),
        Eligibility::Limited
    );
    // No interrupt effect is returned, and the current attempt survives.
    assert_eq!(
        data.current.as_ref().unwrap().running_attempt.as_ref(),
        Some(&context(2))
    );
    declare(&mut data, 2, at(86_401));
    stage(&mut data, 2, at(86_402));
    assert_eq!(data.current.as_ref().unwrap().state, Phase::Limited);
    assert_eq!(data.stages.len(), 2);
    assert_eq!(data.current.as_ref().unwrap().round_number, 2);
}

#[test]
fn one_durable_limit_notice_targets_owner_and_the_current_director() {
    let mut data = waiting();
    data.evaluate(idle(), at(86_400), Some("new-director"));
    let notice = data.enqueue_limit_notice().unwrap();
    assert!(notice.owner);
    assert_eq!(notice.director_session_id.as_deref(), Some("new-director"));
    let id = notice.id;
    let mut restored: ContinuationData =
        serde_json::from_slice(&serde_json::to_vec(&data).unwrap()).unwrap();
    restored.evaluate(idle(), at(90_000), Some("another-director"));
    assert!(restored.enqueue_limit_notice().is_none());
    assert_eq!(
        restored
            .current
            .as_ref()
            .unwrap()
            .limit_notice
            .as_ref()
            .unwrap()
            .id,
        id
    );
}

fn limited() -> ContinuationData {
    let mut data = waiting();
    data.evaluate(idle(), at(86_400), Some("director"));
    data.enqueue_limit_notice().unwrap();
    data
}

#[test]
fn only_owner_or_current_director_can_cas_reset_budget() {
    for actor in [
        handler(),
        Actor::Agent {
            session_id: "operator".into(),
        },
        Actor::Agent {
            session_id: "old-director".into(),
        },
    ] {
        let mut data = limited();
        let before = data.clone();
        let req = ResetRequest {
            action_id: "reset".into(),
            expected_revision: data.current.as_ref().unwrap().revision,
        };
        assert_eq!(
            data.reset_budget(
                Authority {
                    actor: &actor,
                    current_director: Some("director")
                },
                &req,
                idle(),
                at(86_401)
            ),
            Err(Error::Forbidden)
        );
        assert_eq!(data, before);
    }
    for actor in [
        Actor::Owner,
        Actor::Agent {
            session_id: "director".into(),
        },
    ] {
        let mut data = limited();
        let revision = data.current.as_ref().unwrap().revision;
        let mut req = ResetRequest {
            action_id: "reset".into(),
            expected_revision: revision - 1,
        };
        assert_eq!(
            data.reset_budget(
                Authority {
                    actor: &actor,
                    current_director: Some("director")
                },
                &req,
                idle(),
                at(86_401)
            ),
            Err(Error::RevisionConflict)
        );
        req.expected_revision = revision;
        let receipt = data
            .reset_budget(
                Authority {
                    actor: &actor,
                    current_director: Some("director"),
                },
                &req,
                idle(),
                at(86_401),
            )
            .unwrap();
        assert_eq!(receipt.continuation.budget_epoch, 2);
        assert_eq!(receipt.continuation.round_number, 1);
        assert_eq!(data.budget_resets.len(), 1);
        let before = data.clone();
        assert!(
            data.reset_budget(
                Authority {
                    actor: &actor,
                    current_director: Some("director")
                },
                &req,
                idle(),
                at(86_450)
            )
            .unwrap()
            .replayed
        );
        assert_eq!(data, before);
        req.action_id = "concurrent-reset".into();
        assert_eq!(
            data.reset_budget(
                Authority {
                    actor: &actor,
                    current_director: Some("director")
                },
                &req,
                idle(),
                at(86_450)
            ),
            Err(Error::RevisionConflict)
        );
        admit(&mut data, 2, at(86_461));
        assert_eq!(
            data.current.as_ref().unwrap().round_number,
            1,
            "first admission of the reset epoch is round 1"
        );
        assert_eq!(data.stages.len(), 1);
    }
}

#[test]
fn reset_never_bypasses_pause_offline_stop_archive_or_unknown_submission() {
    let guards = [
        Guards {
            paused: true,
            ..idle()
        },
        Guards {
            online: false,
            ..idle()
        },
        Guards {
            stopped: true,
            ..idle()
        },
        Guards {
            archived: true,
            ..idle()
        },
        Guards {
            submission_unknown: true,
            ..idle()
        },
        Guards {
            restarting: true,
            ..idle()
        },
        Guards {
            running_turn: true,
            ..idle()
        },
        Guards {
            permission_valid: false,
            ..idle()
        },
        Guards {
            occurrence_valid: false,
            ..idle()
        },
    ];
    for guard in guards {
        let mut data = limited();
        let before = data.clone();
        let req = ResetRequest {
            action_id: "reset".into(),
            expected_revision: data.current.as_ref().unwrap().revision,
        };
        assert_eq!(
            data.reset_budget(
                Authority {
                    actor: &Actor::Owner,
                    current_director: None
                },
                &req,
                guard,
                at(86_401)
            ),
            Err(Error::Guarded)
        );
        assert_eq!(data, before);
    }
}

#[test]
fn restart_of_a_declared_turn_never_replays_it_but_authentic_normal_end_still_stages() {
    let mut data = ContinuationData::default();
    declare(&mut data, 1, time());
    data.recover_after_restart();
    let after = data.clone();
    data.recover_after_restart();
    assert_eq!(data, after);
    assert_eq!(
        data.evaluate(idle(), at(10), None),
        Eligibility::Reconciling
    );
    stage(&mut data, 1, at(20));
    assert_eq!(data.evaluate(idle(), at(79), None), Eligibility::Waiting);
    admit(&mut data, 2, at(80));
}

#[test]
fn stale_wake_from_a_previous_round_cannot_start_any_turn() {
    let mut data = waiting();
    let old_wake = data
        .current
        .as_ref()
        .unwrap()
        .wake
        .as_ref()
        .unwrap()
        .id
        .clone();
    admit(&mut data, 2, at(60));
    declare(&mut data, 2, at(61));
    stage(&mut data, 2, at(62));
    let (revision, _) = ready(&mut data, at(122));
    assert_eq!(
        data.reserve_admission(
            revision,
            &old_wake,
            "ask-a#3",
            7,
            "native-3",
            idle(),
            at(122),
            None
        ),
        Err(Error::StaleWake)
    );
    assert_eq!(data.current.as_ref().unwrap().round_number, 2);
}

#[test]
fn a_foreign_generation_acceptance_cannot_consume_the_reserved_wake() {
    let mut data = waiting();
    let (revision, wake) = ready(&mut data, at(60));
    assert!(data
        .reserve_admission(
            revision,
            &wake,
            "ask-a#2",
            7,
            "native-2",
            idle(),
            at(60),
            None
        )
        .unwrap());
    let before = data.clone();
    assert_eq!(
        data.admission_accepted(
            &wake,
            &AttemptIdentity {
                runtime_generation: 8,
                ..context(2)
            },
            "native-2"
        ),
        Err(Error::StaleWake)
    );
    assert_eq!(data, before);
    assert!(data
        .admission_accepted(&wake, &context(2), "native-2")
        .unwrap());
}

#[test]
fn a_continuation_can_never_resubmit_the_original_attempt() {
    let mut data = waiting();
    let (revision, wake) = ready(&mut data, at(60));
    assert_eq!(
        data.reserve_admission(
            revision,
            &wake,
            "ask-a#1",
            7,
            "original",
            idle(),
            at(60),
            None
        ),
        Err(Error::NotCurrentHandler)
    );
    assert_eq!(data.current.as_ref().unwrap().round_number, 1);
}

#[test]
fn maximum_timer_hits_wall_clock_cap_instead_of_starting_an_extra_turn() {
    let mut data = ContinuationData::default();
    let req = DeclareRequest {
        wake: Some(WakeCondition::After {
            delay_seconds: 86_400,
        }),
        ..request(1)
    };
    data.declare(
        &handler(),
        &context(1),
        &req,
        false,
        running(),
        ConditionSupport::default(),
        time(),
    )
    .unwrap();
    stage(&mut data, 1, time());
    assert_eq!(
        data.evaluate(idle(), at(86_399), None),
        Eligibility::Waiting
    );
    assert_eq!(
        data.evaluate(idle(), at(86_400), None),
        Eligibility::Limited
    );
    assert_eq!(data.current.as_ref().unwrap().round_number, 1);
}

#[test]
fn serialized_competing_claimants_produce_one_runtime_submission_permission() {
    use std::sync::{Arc, Barrier, Mutex};
    // Models callers sharing the AskState lock; it is not an AskStore test.
    let mut data = waiting();
    let (revision, wake) = ready(&mut data, at(60));
    let data = Arc::new(Mutex::new(data));
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (2..=3)
        .map(|n| {
            let data = Arc::clone(&data);
            let barrier = Arc::clone(&barrier);
            let wake = wake.clone();
            std::thread::spawn(move || {
                barrier.wait();
                data.lock()
                    .unwrap()
                    .reserve_admission(
                        revision,
                        &wake,
                        &format!("ask-a#{n}"),
                        7,
                        &format!("native-{n}"),
                        idle(),
                        at(60),
                        None,
                    )
                    .unwrap()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert_eq!(
        data.lock().unwrap().current.as_ref().unwrap().round_number,
        1
    );
}

#[test]
fn reset_requires_the_old_notice_to_be_durably_enqueued_and_is_audited() {
    let mut data = waiting();
    data.evaluate(idle(), at(86_400), Some("director"));
    let req = ResetRequest {
        action_id: "reset".into(),
        expected_revision: data.current.as_ref().unwrap().revision,
    };
    assert_eq!(
        data.reset_budget(
            Authority {
                actor: &Actor::Owner,
                current_director: Some("director")
            },
            &req,
            idle(),
            at(86_401)
        ),
        Err(Error::Guarded)
    );
    let old_notice = data.enqueue_limit_notice().unwrap();
    data.reset_budget(
        Authority {
            actor: &Actor::Owner,
            current_director: Some("director"),
        },
        &req,
        idle(),
        at(86_401),
    )
    .unwrap();
    assert_eq!(
        data.budget_resets[0].previous_limit_notice.as_ref(),
        Some(&old_notice)
    );
    assert_eq!(data.budget_resets[0].actor, Actor::Owner);
    assert_eq!(data.budget_resets[0].previous_epoch, 1);
    assert_eq!(data.budget_resets[0].budget_epoch, 2);
    assert_eq!(data.stages.len(), 1);
}
