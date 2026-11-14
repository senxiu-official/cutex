//! Hand-authored producer histories, rather than a product of wire enums.
//! Source IO is replaced at drain_on's production seam; no runtime advances.
use super::super::super::input_disposal as disposal;
use super::*;

struct History {
    name: &'static str,
    input: NewInput,
    steps: Vec<Next>,
    expected: Vec<(InputState, Option<&'static str>)>,
}

fn source_input(kind: InputKind, timing: Timing) -> NewInput {
    let mut input = note(kind, timing);
    input.sender = sender(match kind {
        InputKind::PeerMessage => SenderKind::Peer,
        InputKind::Alarm => SenderKind::Alarm,
        InputKind::SystemNotice => SenderKind::Cutex,
        _ => SenderKind::Agent,
    });
    if matches!(
        kind,
        InputKind::DelegationRequest
            | InputKind::DelegationResult
            | InputKind::DelegationQuestion
            | InputKind::DelegationStatus
    ) {
        input.ask_id = Some("continuing-ask".into());
    }
    input
}

fn histories() -> Vec<History> {
    use InputState::{Accepted, Failed, Received, Sending, Uncertain, Waiting};
    // Sources: owner plan/offline_waiting, bus target/plan, asks request_job,
    // delivery_job and native carrier callbacks. Expected tuples are literal
    // contract assertions, independent of actions_for and edge_allowed.
    vec![
        History {
            name: "owner-explicit-queue",
            input: owner(Strategy::Queue),
            steps: vec![Next::received(), Next::waiting("explicitQueue")],
            expected: vec![(Received, None), (Waiting, Some("explicitQueue"))],
        },
        History {
            name: "owner-paused",
            input: owner(Strategy::Queue),
            steps: vec![Next::received(), Next::waiting("paused")],
            expected: vec![(Received, None), (Waiting, Some("paused"))],
        },
        History {
            name: "owner-offline",
            input: owner(Strategy::Queue),
            steps: vec![Next::received(), Next::waiting("agentOffline")],
            expected: vec![(Received, None), (Waiting, Some("agentOffline"))],
        },
        History {
            name: "owner-failed",
            input: owner(Strategy::Queue),
            steps: vec![
                Next::received(),
                Next::sending(),
                Next::failed("notDelivered"),
            ],
            expected: vec![
                (Received, None),
                (Sending, None),
                (Failed, Some("notDelivered")),
            ],
        },
        History {
            name: "owner-registered-steer",
            input: owner(Strategy::Queue),
            steps: vec![
                Next::received(),
                Next::sending(),
                Next::accepted()
                    .stage(Stage::Registered)
                    .turn("existing-turn"),
            ],
            expected: vec![(Received, None), (Sending, None), (Accepted, None)],
        },
        History {
            name: "agent-native-blocked",
            input: source_input(InputKind::AgentMessage, Timing::AfterTurn),
            steps: vec![
                Next::waiting("afterTurn"),
                Next::sending(),
                Next::waiting("nativeHeld").details(json!({"nativeReason":"planMode"})),
            ],
            expected: vec![
                (Waiting, Some("afterTurn")),
                (Sending, None),
                (Waiting, Some("nativeHeld")),
            ],
        },
        History {
            name: "peer-awaits-next-read-point",
            input: source_input(InputKind::PeerMessage, Timing::AfterTurn),
            steps: vec![Next::waiting("afterTurn")],
            expected: vec![(Waiting, Some("afterTurn"))],
        },
        History {
            name: "alarm-unread",
            input: source_input(InputKind::Alarm, Timing::AfterTurn),
            steps: vec![
                Next::waiting("afterTurn"),
                Next::sending(),
                Next::accepted(),
            ],
            expected: vec![
                (Waiting, Some("afterTurn")),
                (Sending, None),
                (Accepted, None),
            ],
        },
        History {
            name: "system-passive-unread",
            input: source_input(InputKind::SystemNotice, Timing::Passive),
            steps: vec![Next::waiting("passive"), Next::sending(), Next::accepted()],
            expected: vec![
                (Waiting, Some("passive")),
                (Sending, None),
                (Accepted, None),
            ],
        },
        History {
            name: "request-review",
            input: source_input(InputKind::DelegationRequest, Timing::AfterTurn),
            steps: vec![Next::waiting("reviewPending")],
            expected: vec![(Waiting, Some("reviewPending"))],
        },
        History {
            name: "request-paused",
            input: source_input(InputKind::DelegationRequest, Timing::AfterTurn),
            steps: vec![Next::waiting("paused")],
            expected: vec![(Waiting, Some("paused"))],
        },
        History {
            name: "request-offline",
            input: source_input(InputKind::DelegationRequest, Timing::AfterTurn),
            steps: vec![Next::waiting("agentOffline")],
            expected: vec![(Waiting, Some("agentOffline"))],
        },
        History {
            name: "request-unconfirmed",
            input: source_input(InputKind::DelegationRequest, Timing::AfterTurn),
            steps: vec![
                Next::waiting("targetBusy"),
                Next::sending(),
                Next::uncertain("outcomeUnknown"),
            ],
            expected: vec![
                (Waiting, Some("targetBusy")),
                (Sending, None),
                (Uncertain, Some("outcomeUnknown")),
            ],
        },
        History {
            name: "result-queued-event",
            input: source_input(InputKind::DelegationResult, Timing::AfterTurn),
            steps: vec![
                Next::waiting("resultCoalescing").details(json!({"ownerHold":false})),
                Next::waiting("afterTurn").details(json!({"eventId":"frozen-results"})),
                Next::sending(),
                Next::accepted(),
            ],
            expected: vec![
                (Waiting, Some("resultCoalescing")),
                (Waiting, Some("afterTurn")),
                (Sending, None),
                (Accepted, None),
            ],
        },
        History {
            name: "question-owner-held",
            input: source_input(InputKind::DelegationQuestion, Timing::AfterTurn),
            steps: vec![Next::waiting("ownerPause").details(json!({"ownerHold":true}))],
            expected: vec![(Waiting, Some("ownerPause"))],
        },
        History {
            name: "status-offline",
            input: source_input(InputKind::DelegationStatus, Timing::AfterTurn),
            steps: vec![Next::waiting("agentOffline")],
            expected: vec![(Waiting, Some("agentOffline"))],
        },
    ]
}

fn row(f: &Fixture, id: &str) -> InputRecord {
    f.ledger.get("s1", &[id.into()]).unwrap().records.remove(0)
}

#[test]
fn every_visible_prefix_of_real_producer_histories_has_a_durable_exit() {
    for history in histories() {
        assert_eq!(history.steps.len(), history.expected.len());
        for prefix in 1..=history.steps.len() {
            let f = Fixture::new();
            for (index, next) in history.steps.iter().take(prefix).enumerate() {
                let next = if index == 0 {
                    let mut input = history.input.clone();
                    if input.kind == InputKind::DelegationRequest {
                        input.ask_id = Some(history.name.into());
                    }
                    next.clone().create(input)
                } else {
                    next.clone()
                };
                let actual = f.go(history.name, next).unwrap().record;
                let expected = history.expected[index];
                assert_eq!(
                    (actual.state, actual.reason.as_deref()),
                    expected,
                    "{} step {index}",
                    history.name
                );
                assert!(
                    matches!(actual.surface, Surface::NextUp | Surface::PendingNotes),
                    "{} step {index}",
                    history.name
                );
            }
            let path = f.root.join(DATABASE_FILE);
            let receipt = disposal::request_at(&path, "s1", history.name, t0()).unwrap();
            assert_eq!(receipt.effect,"Stop waiting and prevent further delivery or resend; content already in flight may still be read.");
            assert_eq!(receipt.applied_at, None);
            let mut source_present = true;
            let (done, _) = capture_events(|| {
                disposal::drain_on(&f.ledger, &path, |record| {
                    assert_eq!(record.input_id, history.name);
                    source_present = false;
                    Ok(())
                })
            });
            assert_eq!(done.unwrap(), 1, "{} prefix {prefix}", history.name);
            assert!(!source_present);
            let closed = row(&f, history.name);
            assert_eq!(
                (closed.state, closed.surface),
                (InputState::Discarded, Surface::None)
            );
            assert_eq!(closed.reason.as_deref(), Some("ownerDiscarded"));
            assert!(closed.deadline.is_none());
            assert!(disposal::check_at(&path, "s1", [history.name]).is_err());
            // Both a stale callback and a source backfill are fenced.
            for late in [
                Next::sending(),
                Next::accepted(),
                Next::in_context(Evidence::HistoryRead),
                Next::waiting("afterTurn").create(history.input.clone()),
            ] {
                let late = f
                    .ledger
                    .transition("s1", history.name, Expect::rev(0), late)
                    .unwrap();
                assert!(!late.changed);
                assert_eq!(late.record.state, InputState::Discarded);
            }
        }
    }
}

#[test]
fn transport_budget_starts_after_admission_and_metadata_cannot_renew_it() {
    let f = Fixture::new();
    f.go("retry", Next::sending().create(owner(Strategy::Queue)))
        .unwrap();
    f.at(10);
    f.go("retry", Next::failed("notDelivered")).unwrap();
    f.at(40);
    f.go("retry", Next::waiting("ownerRetry")).unwrap();
    f.at(45);
    let sent = f.go("retry", Next::sending()).unwrap().record;
    let bound = DateTime::parse_from_rfc3339(sent.details["observationDeadline"].as_str().unwrap())
        .unwrap();
    assert_eq!(bound, t0() + chrono::Duration::seconds(105));
    f.at(55);
    f.go("retry", Next::waiting("hostBusy")).unwrap();
    f.at(65);
    let automatic = f
        .go(
            "retry",
            Next::sending().details(json!({"reconnected":true})),
        )
        .unwrap()
        .record;
    assert_eq!(
        automatic.details["observationDeadline"],
        json!((t0() + chrono::Duration::seconds(125))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    );
    // Backward clock movement and clearing the soft deadline cannot extend it.
    f.at(30);
    f.go("retry", Next::sending().deadline(None)).unwrap();
    let lane = super::super::super::session_coordination::session_lane("s1").unwrap();
    let _busy = lane.try_lock().unwrap();
    let (early, _) = capture_events(|| {
        expire_observations(&f.ledger, t0() + chrono::Duration::seconds(124), 100)
    });
    assert_eq!(early.unwrap(), 0);
    let (expired, _) = capture_events(|| {
        expire_observations(&f.ledger, t0() + chrono::Duration::seconds(126), 100)
    });
    assert_eq!(expired.unwrap(), 1);
    assert_eq!(row(&f, "retry").state, InputState::Uncertain);
    let path = f.root.join(DATABASE_FILE);
    disposal::request_at(&path, "s1", "retry", t0() + chrono::Duration::seconds(126)).unwrap();
    let mut removed = false;
    let (done, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |_| {
            removed = true;
            Ok(())
        })
    });
    assert_eq!(done.unwrap(), 1);
    assert!(removed);
    assert_eq!(row(&f, "retry").surface, Surface::None);
}

#[test]
fn failed_bus_retry_refused_before_io_can_still_be_disposed() {
    let f = Fixture::new();
    let path = f.root.join(DATABASE_FILE);
    f.go(
        "notice",
        Next::waiting("afterTurn").create(source_input(InputKind::AgentMessage, Timing::AfterTurn)),
    )
    .unwrap();
    f.go("notice", Next::sending()).unwrap();
    f.go("notice", Next::uncertain("outcomeUnknown")).unwrap();
    f.go("notice", Next::waiting("ownerRetry")).unwrap();
    let refused = f
        .go(
            "notice",
            Next::failed("notDelivered").details(json!({"retryRefused":true})),
        )
        .unwrap()
        .record;
    assert_eq!(refused.state, InputState::Failed);
    disposal::request_at(&path, "s1", "notice", t0()).unwrap();
    let mut retry_pending = true;
    let (done, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |_| {
            retry_pending = false;
            Ok(())
        })
    });
    assert_eq!(done.unwrap(), 1);
    assert!(!retry_pending);
    assert_eq!(row(&f, "notice").surface, Surface::None);
    assert_eq!(
        f.go("notice", Next::waiting("ownerRetry"))
            .unwrap()
            .record
            .state,
        InputState::Discarded
    );
}

#[test]
fn frozen_batch_disposed_child_and_unrelated_sibling_keep_separate_cas_rules() {
    let f = Fixture::new();
    let path = f.root.join(DATABASE_FILE);
    let child = f
        .go(
            "child",
            Next::waiting("afterTurn")
                .create(source_input(InputKind::DelegationResult, Timing::AfterTurn)),
        )
        .unwrap()
        .record;
    let sibling = f
        .go(
            "sibling",
            Next::waiting("afterTurn").create(source_input(
                InputKind::DelegationQuestion,
                Timing::AfterTurn,
            )),
        )
        .unwrap()
        .record;
    disposal::request_at(&path, "s1", "child", t0()).unwrap();
    assert!(
        disposal::check_at(&path, "s1", ["child", "sibling"]).is_err(),
        "frozen envelope must be rebuilt before IO"
    );
    assert!(disposal::check_at(&path, "s1", ["sibling"]).is_ok());
    assert!(
        f.ledger
            .transition_batch(
                "s1",
                vec![
                    ("child".into(), Expect::rev(0), Next::sending()),
                    ("sibling".into(), Expect::rev(0), Next::sending()),
                ]
            )
            .is_err(),
        "the sibling still enforces ordinary CAS"
    );
    assert_eq!(row(&f, "sibling").rev, sibling.rev);
    let result = f
        .ledger
        .transition_batch(
            "s1",
            vec![
                (
                    "child".into(),
                    Expect::rev(child.rev + 100),
                    Next::sending(),
                ),
                ("sibling".into(), Expect::rev(sibling.rev), Next::sending()),
            ],
        )
        .unwrap();
    assert_eq!(result[0].record.state, InputState::Discarded);
    assert_eq!(result[1].record.state, InputState::Sending);
    let mut source_children = vec!["child", "sibling"];
    let (done, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |record| {
            source_children.retain(|id| *id != record.input_id.as_str());
            Ok(())
        })
    });
    assert_eq!(done.unwrap(), 1);
    assert_eq!(source_children, vec!["sibling"]);
    assert_eq!(row(&f, "sibling").state, InputState::Sending);
    assert_eq!(
        f.ledger
            .transition("s1", "child", Expect::rev(child.rev), Next::accepted())
            .unwrap()
            .record
            .surface,
        Surface::None
    );
}

#[test]
fn lost_source_response_reopen_and_later_request_occurrence_are_independent() {
    let f = Fixture::new();
    let path = f.root.join(DATABASE_FILE);
    let old = "continuing-ask";
    let new = crate::asks::model::request_occurrence_id(old, 1);
    f.go(
        old,
        Next::waiting("reviewPending").create(source_input(
            InputKind::DelegationRequest,
            Timing::AfterTurn,
        )),
    )
    .unwrap();
    let receipt = disposal::request_at(&path, "s1", old, t0()).unwrap();
    let mut source_present = true;
    let (lost, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |_| {
            source_present = false;
            Err("cleanup committed; response lost".into())
        })
    });
    assert_eq!(lost.unwrap(), 0);
    assert!(!source_present);
    let f = f.reopen();
    // Replaying the owner's request also makes deferred cleanup due immediately.
    assert_eq!(
        disposal::request_at(&path, "s1", old, t0() - chrono::Duration::days(1)).unwrap(),
        receipt
    );
    let (recovered, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |_| {
            assert!(!source_present, "retry must tolerate already absent source");
            Ok(())
        })
    });
    assert_eq!(recovered.unwrap(), 1);
    let applied = disposal::request_at(&path, "s1", old, t0()).unwrap();
    assert_eq!(applied.disposition_id, receipt.disposition_id);
    assert_eq!(applied.requested_at, receipt.requested_at);
    assert!(applied.applied_at.is_some());
    f.go(
        &new,
        Next::waiting("targetBusy").create(source_input(
            InputKind::DelegationRequest,
            Timing::AfterTurn,
        )),
    )
    .unwrap();
    assert!(disposal::check_at(&path, "s1", [new.as_str()]).is_ok());
    assert_eq!(
        f.go(old, Next::sending()).unwrap().record.state,
        InputState::Discarded
    );
    assert_eq!(row(&f, &new).state, InputState::Waiting);
    let (empty, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |_| {
            panic!("completed decision cleaned twice")
        })
    });
    assert_eq!(empty.unwrap(), 0);
}

#[test]
fn decision_store_failure_does_not_fabricate_a_successful_disposal() {
    let f = Fixture::new();
    let path = f.root.join(DATABASE_FILE);
    f.go(
        "keep",
        Next::waiting("explicitQueue").create(owner(Strategy::Queue)),
    )
    .unwrap();
    // Initialize only the private disposition schema, then fail its commit.
    drop(disposal::Guard::open(&path, true).unwrap());
    let db = Connection::open(f.root.join("input-dispositions.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_decision BEFORE INSERT ON owner_dispositions BEGIN SELECT RAISE(ABORT,'injected disk failure'); END;").unwrap();
    assert!(disposal::request_at(&path, "s1", "keep", t0()).is_err());
    assert!(disposal::check_at(&path, "s1", ["keep"]).is_ok());
    assert_eq!(row(&f, "keep").state, InputState::Waiting);
    let (none, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |_| {
            panic!("failed intent cannot withdraw a source")
        })
    });
    assert_eq!(none.unwrap(), 0);
    db.execute_batch("DROP TRIGGER fail_decision").unwrap();
    drop(db);
    disposal::request_at(&path, "s1", "keep", t0()).unwrap();
    let mut source_present = true;
    let (done, _) = capture_events(|| {
        disposal::drain_on(&f.ledger, &path, |_| {
            source_present = false;
            Ok(())
        })
    });
    assert_eq!(done.unwrap(), 1);
    assert!(!source_present);
    assert_eq!(row(&f, "keep").state, InputState::Discarded);
}

// Complements source histories with all kinds accepted by the stable contract.
// This is enum-compatibility coverage, not invented source reachability.
#[test]
fn admitted_unread_acceptance_has_dismiss_without_transport_expiry_for_each_kind() {
    use InputKind::*;
    for kind in [
        Owner,
        AgentMessage,
        PeerMessage,
        Alarm,
        SystemNotice,
        DelegationRequest,
        DelegationResult,
        DelegationQuestion,
        DelegationStatus,
    ] {
        for timing in [Timing::AfterTurn, Timing::Passive] {
            if matches!(kind, Owner | DelegationRequest) && timing == Timing::Passive {
                continue;
            }
            let f = Fixture::new();
            let input = if kind == Owner {
                owner(Strategy::Queue)
            } else {
                source_input(kind, timing)
            };
            f.go("unread", Next::sending().create(input)).unwrap();
            let accepted = f.go("unread", Next::accepted()).unwrap().record;
            assert!(matches!(
                accepted.surface,
                Surface::NextUp | Surface::PendingNotes
            ));
            assert!(accepted.actions.contains(&Action::Dismiss));
            for seconds in [10, 30, 59] {
                f.at(seconds);
                f.go(
                    "unread",
                    Next::accepted()
                        .deadline(None)
                        .details(json!({"lateMetadata":seconds})),
                )
                .unwrap();
            }
            let (expired, _) = capture_events(|| {
                expire_observations(&f.ledger, t0() + chrono::Duration::seconds(61), 100)
            });
            assert_eq!(expired.unwrap(), 0);
            let unknown = row(&f, "unread");
            assert_eq!(unknown.state, InputState::Accepted);
            assert!(!unknown.actions.contains(&Action::Retry));
            assert!(unknown.actions.contains(&Action::Dismiss));
            disposal::request_at(&f.root.join(DATABASE_FILE), "s1", "unread", t0()).unwrap();
            assert_eq!(
                f.go("unread", Next::accepted()).unwrap().record.surface,
                Surface::None
            );
        }
    }
}
