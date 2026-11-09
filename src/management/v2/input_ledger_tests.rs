use super::*;
use chrono::TimeZone;

struct Fixture {
    root: PathBuf,
    ledger: InputLedger,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-input-ledger-{}", uuid::Uuid::new_v4().simple()));
        let ledger = InputLedger::open(root.join(DATABASE_FILE));
        ledger.set_clock(t0());
        Self { root, ledger }
    }

    fn reopen(mut self) -> Self {
        // The new ledger opens lazily; the old connection closes on assignment.
        self.ledger = InputLedger::open(self.root.join(DATABASE_FILE));
        self.ledger.set_clock(t0());
        self
    }

    fn at(&self, secs: i64) {
        self.ledger.set_clock(t0() + chrono::Duration::seconds(secs));
    }

    fn go(&self, id: &str, next: Next) -> Result<Transition, LedgerError> {
        self.ledger.transition("s1", id, Expect::ANY, next)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap()
}

fn sender(kind: SenderKind) -> InputSender {
    InputSender { kind, id: "sender".into(), name: "Sender".into(), host: None, cutex_session_id: None }
}

fn owner(strategy: Strategy) -> NewInput {
    NewInput {
        kind: InputKind::Owner,
        sender: sender(SenderKind::Owner),
        intent: InputIntent { strategy: Some(strategy), delivery_mode: None, timing: Timing::AfterTurn },
        preview: "hello   world\nthis is the owner speaking".into(),
        ask_id: None,
        remote: None,
    }
}

fn note(kind: InputKind, timing: Timing) -> NewInput {
    NewInput {
        kind,
        sender: sender(SenderKind::Agent),
        intent: InputIntent {
            strategy: None,
            delivery_mode: Some(if timing == Timing::Passive { DeliveryMode::Passive } else { DeliveryMode::AfterTurn }),
            timing,
        },
        preview: "x".repeat(200),
        ask_id: None,
        remote: None,
    }
}

// --- state machine -----------------------------------------------------------

#[test]
fn uncertain_task_notice_explains_notification_only_retry() {
    let f = Fixture::new();
    let mut input = note(InputKind::SystemNotice, Timing::AfterTurn);
    input.sender = sender(SenderKind::ClaudeTask);
    f.go("task", Next::sending().create(input)).unwrap();
    f.go("task", Next::accepted()).unwrap();
    let row = f.go("task", Next::uncertain("receiptTimeout")).unwrap().record;
    assert_eq!(row.details["retryEffect"], "redeliverNotice");
    assert_eq!(row.details["taskRestartsOnRetry"], false);
    assert!(row.actions.contains(&Action::Retry));
    assert!(row.actions.contains(&Action::Dismiss));
}

#[test]
fn notice_hold_is_explicit_and_schema_checked() {
    let request = json!({"inputId": "notice", "expectedRev": 1, "held": true});
    assert_eq!(parse_hold(request.as_object().unwrap()).unwrap(), ("notice".into(), 1, true));
    super::super::contract_validation::validate_cutex_request(&json!({"requestId": "r", "method": HOLD_METHOD, "params": request})).unwrap();
    for invalid in [json!({"inputId":"x","expectedRev":0,"held":true}), json!({"inputId":"x","expectedRev":1,"held":"true"}), json!({"inputId":"x","expectedRev":1,"held":true,"extra":1})] {
        assert!(parse_hold(invalid.as_object().unwrap()).is_err());
    }
    let f = Fixture::new();
    let plain = f.go("notice", Next::waiting("afterTurn").create(note(InputKind::SystemNotice, Timing::AfterTurn))).unwrap();
    assert!(!plain.record.actions.contains(&Action::Hold));
    let eligible = f.go("notice", Next::waiting("afterTurn").details(json!({"ownerHold":false}))).unwrap();
    assert!(eligible.record.actions.contains(&Action::Hold));
    let held = f.go("notice", Next::waiting("ownerPause").details(json!({"ownerHold":true}))).unwrap();
    assert!(held.record.actions.contains(&Action::ReleaseHold));
    assert!(!held.record.actions.contains(&Action::Hold));
    let sending = f.go("notice", Next::sending()).unwrap();
    assert!(!sending.record.actions.contains(&Action::ReleaseHold));
}

#[test]
fn notice_actions_are_read_derived_and_exact_previous_rows_remain_decodable() {
    let f=Fixture::new();
    let transition=f.go("notice",Next::waiting("ownerPause")
        .create(note(InputKind::SystemNotice,Timing::AfterTurn)).details(json!({"ownerHold":true}))).unwrap();
    assert!(transition.record.actions.contains(&Action::ReleaseHold));
    let db=Connection::open(f.ledger.path()).unwrap();
    let raw:String=db.query_row("SELECT record_json FROM input_ledger WHERE input_id='notice'",[],|r|r.get(0)).unwrap();
    let stored:Value=serde_json::from_str(&raw).unwrap();
    assert_eq!(stored["actions"],json!(["dismiss"]));
    assert_eq!(stored["details"]["ownerHold"],true,"choice stays authoritative in its own metadata");
    drop(db);
    let f=f.reopen();
    let assert_action=|records:Vec<InputRecord>| {
        assert_eq!(records.len(),1);
        assert!(records[0].actions.contains(&Action::ReleaseHold));
    };
    assert_action(f.ledger.get("s1",&["notice".into()]).unwrap().records);
    assert_action(f.ledger.list("s1",ListQuery::default()).unwrap().records);
    assert_action(f.ledger.since("s1",0,100).unwrap().records);
    assert_action(f.ledger.unsettled_page(None,100).unwrap());
    let reopened=f.go("notice",Next::waiting("afterTurn").details(json!({"ownerHold":false}))).unwrap();
    assert!(reopened.record.actions.contains(&Action::Hold));
    assert!(f.ledger.get("s1",&["notice".into()]).unwrap().records[0].actions.contains(&Action::Hold));
}

#[test]
fn owner_carries_survive_delivery_transitions_and_reopen() {
    let f = Fixture::new();
    f.go("owner", Next::received().create(owner(Strategy::Auto))).unwrap();
    let mut sending = Next::sending();
    sending.carries = Some(vec!["notice".into(), "result".into()]);
    f.go("owner", sending).unwrap();
    f.go("owner", Next::in_context(Evidence::TurnStartInput).turn("turn1")).unwrap();
    let f = f.reopen();
    let record = f.ledger.get("s1", &["owner".into()]).unwrap().records.remove(0);
    assert_eq!(record.carries, ["notice", "result"]);
    let mut legacy = serde_json::to_value(&record).unwrap();
    legacy.as_object_mut().unwrap().remove("carries");
    assert!(serde_json::from_value::<InputRecord>(legacy).unwrap().carries.is_empty());
}

#[test]
fn owner_carries_reject_invalid_members_and_non_owner_records() {
    let f = Fixture::new();
    f.go("owner", Next::received().create(owner(Strategy::Auto))).unwrap();
    for ids in [vec!["owner".into()], vec!["notice".into(), "notice".into()], vec!["".into()], vec!["x".into(); MAX_BATCH_ITEMS + 1]] {
        let mut next = Next::sending();
        next.carries = Some(ids);
        assert!(matches!(f.go("owner", next), Err(LedgerError::Invalid(_))));
    }
    f.go("notice", Next::received().create(note(InputKind::SystemNotice, Timing::AfterTurn))).unwrap();
    let mut next = Next::sending();
    next.carries = Some(vec!["other".into()]);
    assert!(matches!(f.go("notice", next), Err(LedgerError::Invalid(_))));
}

#[test]
fn carried_notice_still_requires_context_evidence() {
    let f = Fixture::new();
    f.go("notice", Next::received().create(note(InputKind::SystemNotice, Timing::AfterTurn))).unwrap();
    f.go("notice", Next::sending()).unwrap();
    let accepted = f.go("notice", Next::accepted().reason("carriedByOwner").turn("turn1")).unwrap();
    assert_eq!(accepted.record.surface, Surface::NextUp);
    assert!(matches!(f.go("notice", Next::to(InputState::InContext).reason("carriedByOwner")), Err(LedgerError::Invalid(_))));
    let read = f.go("notice", Next::in_context(Evidence::ContextPersisted).reason("carriedByOwner").turn("turn1")).unwrap();
    assert_eq!(read.record.surface, Surface::Timeline);
    assert_eq!(read.record.reason.as_deref(), Some("carriedByOwner"));
    f.go("owner", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("owner", Next::sending()).unwrap();
    assert!(matches!(f.go("owner", Next::accepted().reason("carriedByOwner")), Err(LedgerError::Invalid(_))));
}

fn probe(state: InputState) -> Next {
    let mut next = Next::to(state);
    next.reason = state.reasons().map(|reasons| reasons[0].to_string());
    if state == InputState::InContext {
        next.evidence = Some(Evidence::A4Receipt);
    }
    next
}

/// 057 §6: only a note whose carrier is proven unread goes back, only with
/// that proof, and never an owner message.
#[test]
fn a_carried_note_goes_back_to_waiting_only_with_the_proof() {
    let proof = Next::waiting("returned").details(json!({"carrierUnread": true}));
    for from in [InputState::Accepted, InputState::Uncertain] {
        assert!(edge_allowed(InputKind::AgentMessage, Some(from), &proof), "{from}");
        assert!(edge_allowed(InputKind::DelegationResult, Some(from), &proof), "{from}");
        assert!(!edge_allowed(InputKind::Owner, Some(from), &proof), "{from}");
        assert!(!edge_allowed(InputKind::AgentMessage, Some(from), &Next::waiting("returned")), "{from}");
        assert!(!edge_allowed(InputKind::AgentMessage, Some(from), &Next::waiting("afterTurn").details(json!({"carrierUnread": true}))), "{from}");
    }
    for from in [InputState::InContext, InputState::Discarded, InputState::Rejected, InputState::Superseded] {
        assert!(!edge_allowed(InputKind::AgentMessage, Some(from), &proof), "{from}");
    }
}

#[test]
fn state_machine_edges_match_the_contract() {
    use InputState::*;
    let allowed_any: &[(InputState, InputState)] = &[
        (Received, Received), (Received, Held), (Received, Waiting), (Received, Sending), (Received, Rejected), (Received, Failed),
        (Held, Held), (Held, Sending), (Held, Waiting), (Held, Failed),
        (Waiting, Waiting), (Waiting, Sending), (Waiting, Discarded), (Waiting, Superseded), (Waiting, Rejected),
        (Sending, Sending), (Sending, Accepted), (Sending, InContext), (Sending, Uncertain), (Sending, Waiting),
        (Accepted, Accepted), (Accepted, InContext), (Accepted, Uncertain),
        (Uncertain, Uncertain), (Uncertain, InContext), (Uncertain, Discarded),
    ];
    for kind in [InputKind::Owner, InputKind::AgentMessage] {
        let owner = kind == InputKind::Owner;
        for from in InputState::ALL {
            for to in InputState::ALL {
                // `probe(Sending)` is `sending{interruptSend}` (the owner's
                // cutex/input/interruptSend takes a steer back, r274).
                let expected = allowed_any.contains(&(*from, *to))
                    || owner && matches!((from, to), (Sending, Failed) | (Accepted, Failed) | (Accepted, Sending) | (Waiting, Failed) | (Waiting, Uncertain));
                assert_eq!(edge_allowed(kind, Some(*from), &probe(*to)), expected, "{kind} {from} -> {to}");
            }
        }
        // Retry only as the owner's `ownerRetry`.
        for from in [Uncertain, Failed] {
            assert!(edge_allowed(kind, Some(from), &Next::waiting("ownerRetry")), "{kind} {from} retry");
            assert!(!edge_allowed(kind, Some(from), &Next::waiting("afterTurn")));
        }
        // r274.2 §9.4: only the owner discards a failed record (owner decision 9).
        assert_eq!(edge_allowed(kind, Some(Failed), &Next::discarded("ownerDiscarded")), owner, "{kind} failed -> discarded");
        assert!(!edge_allowed(kind, Some(Failed), &Next::discarded("ownerRemoved")));
        // Read without a delivery turn.
        assert!(edge_allowed(kind, Some(Waiting), &Next::in_context(Evidence::ClaimedByWait)));
        assert!(edge_allowed(kind, Some(Waiting), &Next::in_context(Evidence::ReadByTool)));
        assert!(!edge_allowed(kind, Some(Waiting), &Next::in_context(Evidence::A4Receipt)));
    }
    // Invariant 8: accepted never goes back to waiting.
    for kind in InputKind::ALL {
        assert!(!edge_allowed(*kind, Some(Accepted), &Next::waiting("returned")));
        assert!(!edge_allowed(*kind, Some(InContext), &Next::waiting("ownerRetry")));
    }
    for to in InputState::ALL {
        let expected = matches!(to, Received | Held | Waiting | Sending | Rejected | Failed);
        assert_eq!(edge_allowed(InputKind::Owner, None, &probe(*to)), expected, "new -> {to}");
    }
}

#[test]
fn owner_idle_send_runs_received_sending_in_context_on_the_timeline() {
    let f = Fixture::new();
    let created = f.ledger.transition("s1", "c1", Expect::NEW, Next::received().create(owner(Strategy::Auto))).unwrap();
    assert!(created.changed && created.created);
    assert_eq!((created.record.rev, created.state_revision), (1, 1));
    assert_eq!(created.record.surface, Surface::Timeline);
    assert_eq!(created.record.preview, "hello world this is the owner speaking");
    assert_eq!(created.record.deadline.as_deref(), Some("2026-11-01T12:00:05.000Z"));
    f.at(1);
    let sending = f.ledger.transition("s1", "c1", Expect::rev(1), Next::sending()).unwrap();
    assert_eq!(sending.record.deadline.as_deref(), Some("2026-11-01T12:00:31.000Z"));
    let done = f.go("c1", Next::in_context(Evidence::TurnStartInput).turn("t1")).unwrap();
    assert_eq!((done.record.rev, done.state_revision), (3, 3));
    assert_eq!(done.record.state, InputState::InContext);
    assert_eq!(done.record.turn_id.as_deref(), Some("t1"));
    assert_eq!(done.record.deadline, None);
    assert_eq!(done.record.surface, Surface::Timeline);
    assert!(done.record.actions.is_empty());
    assert_eq!(done.record.since, "2026-11-01T12:00:01.000Z");
}

#[test]
fn revisions_are_gapless_per_session_and_survive_reopen() {
    let f = Fixture::new();
    f.go("a", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("b", Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    let other = f.ledger.transition("s2", "a", Expect::ANY, Next::received().create(owner(Strategy::Auto))).unwrap();
    assert_eq!(other.state_revision, 1, "per-session counter");
    let before = f.go("a", Next::sending()).unwrap();
    assert_eq!(before.state_revision, 3);
    let epoch = before.ledger_epoch.clone();
    let f = f.reopen();
    let after = f.go("b", Next::sending()).unwrap();
    assert_eq!((after.state_revision, after.ledger_epoch.as_str()), (4, epoch.as_str()));
    assert_eq!(after.record.rev, 2);
    let list = f.ledger.list("s1", ListQuery::default()).unwrap();
    let revisions: Vec<u64> = list.records.iter().map(|record| record.state_revision).collect();
    assert_eq!(revisions, vec![3, 4]);
    assert_eq!(list.state_revision, 4);
}

#[test]
fn expect_rev_is_a_compare_and_set() {
    let f = Fixture::new();
    f.go("a", Next::received().create(owner(Strategy::Auto))).unwrap();
    assert!(matches!(
        f.ledger.transition("s1", "a", Expect::NEW, Next::received().create(owner(Strategy::Auto))),
        Err(LedgerError::Conflict { current_rev: 1 })
    ));
    assert!(matches!(
        f.ledger.transition("s1", "a", Expect::rev(5), Next::sending()),
        Err(LedgerError::Conflict { current_rev: 1 })
    ));
    assert!(matches!(f.go("missing", Next::sending()), Err(LedgerError::NotFound)));
    assert!(matches!(f.ledger.transition("s1", "missing", Expect::rev(1), Next::sending()), Err(LedgerError::Conflict { current_rev: 0 })));
}

#[test]
fn an_unchanged_transition_uses_no_revision() {
    let f = Fixture::new();
    f.go("a", Next::waiting("explicitQueue").create(owner(Strategy::Queue))).unwrap();
    let same = f.go("a", Next::waiting("explicitQueue")).unwrap();
    assert!(!same.changed);
    assert_eq!((same.record.rev, same.state_revision), (1, 1));
    // A reason change in the same state is a change (and an event).
    let moved = f.go("a", Next::waiting("ownerPause")).unwrap();
    assert!(moved.changed);
    assert_eq!((moved.record.rev, moved.state_revision), (2, 2));
    assert_eq!(moved.record.since, same.record.since, "since tracks the state, not the reason");
    // Stalled alone is a change; clearing it again too.
    let stalled = f.go("a", Next::waiting("ownerPause").stalled(Some(Stalled { reason: "noProgress".into(), since: "2026-11-01T12:00:30.000Z".into() }))).unwrap();
    assert!(stalled.changed && stalled.record.stalled.is_some());
    assert!(f.go("a", Next::waiting("ownerPause")).unwrap().record.stalled.is_some(), "kept when not given");
    assert!(f.go("a", Next::waiting("ownerPause").stalled(None)).unwrap().changed);
}

#[test]
fn reasons_evidence_and_stage_are_validated() {
    let f = Fixture::new();
    let create = || Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn));
    assert!(matches!(f.go("a", Next::to(InputState::Waiting).create(note(InputKind::AgentMessage, Timing::AfterTurn))), Err(LedgerError::InvalidReason { .. })));
    assert!(matches!(f.go("a", Next::waiting("bogus").create(note(InputKind::AgentMessage, Timing::AfterTurn))), Err(LedgerError::InvalidReason { .. })));
    assert!(matches!(f.go("a", Next::waiting("receiptTimeout").create(note(InputKind::AgentMessage, Timing::AfterTurn))), Err(LedgerError::InvalidReason { .. })));
    f.go("a", create()).unwrap();
    assert!(matches!(f.go("a", Next::sending().reason("afterTurn")), Err(LedgerError::InvalidReason { .. })));
    f.go("a", Next::sending()).unwrap();
    assert!(matches!(f.go("a", Next::to(InputState::InContext)), Err(LedgerError::Invalid(_))));
    assert!(matches!(f.go("a", Next::accepted().evidence(Evidence::A4Receipt)), Err(LedgerError::Invalid(_))));
    assert!(matches!(f.go("a", Next::uncertain("turnFailed").stage(Stage::Offered)), Err(LedgerError::Invalid(_))));
    f.go("a", Next::accepted().stage(Stage::Offered).turn("t1")).unwrap();
    assert!(matches!(f.go("a", Next::accepted().stage(Stage::Registered)), Err(LedgerError::StageRegression { .. })));
    // Same id moves to the next turn (Claude `continue_quota_notes`) and keeps its stage.
    let moved = f.go("a", Next::accepted().turn("t2")).unwrap();
    assert_eq!((moved.record.stage, moved.record.turn_id.as_deref()), (Some(Stage::Offered), Some("t2")));
    let done = f.go("a", Next::in_context(Evidence::ContextPersisted)).unwrap();
    assert_eq!((done.record.stage, done.record.evidence), (None, Some(Evidence::ContextPersisted)));
    assert!(matches!(f.go("a", Next::superseded("mergedInsert")), Err(LedgerError::ForbiddenEdge { .. })));
    assert!(matches!(f.go("b", Next::superseded("mergedInsert").create(owner(Strategy::Auto))), Err(LedgerError::ForbiddenEdge { .. })));
}

#[test]
fn owner_only_and_non_owner_only_edges() {
    let f = Fixture::new();
    f.go("o", Next::sending().create(owner(Strategy::Auto))).unwrap();
    // Alice 11-02 decision 3: an explicit refusal may re-queue the owner's send.
    f.go("o", Next::waiting("hostBusy")).unwrap();
    f.go("o", Next::sending()).unwrap();
    let failed = f.go("o", Next::failed("runtimeRefused")).unwrap();
    assert_eq!(failed.record.error.as_ref().map(|error| (error.code.as_str(), error.retryable)), Some(("runtimeRefused", true)));
    // r274.2 §9.4: an owner's failed message can be sent again or discarded.
    assert_eq!(failed.record.actions, vec![Action::Retry, Action::Dismiss]);
    f.go("n", Next::sending().create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    assert!(matches!(f.go("n", Next::failed("runtimeRefused")), Err(LedgerError::ForbiddenEdge { .. })));
    let returned = f.go("n", Next::waiting("returned")).unwrap();
    assert_eq!(returned.record.actions, vec![Action::Dismiss]);
    f.go("n", Next::sending()).unwrap();
    // Shown in turn t (its native item): on the timeline (r274.2 §9.1).
    f.go("n", Next::accepted().turn("t").details(json!({READ_SIGNAL: "nativeItem"}))).unwrap();
    assert!(matches!(f.go("n", Next::waiting("returned")), Err(LedgerError::ForbiddenEdge { .. })), "invariant 8");
    let uncertain = f.go("n", Next::uncertain("turnInterrupted")).unwrap();
    assert_eq!(uncertain.record.actions, vec![Action::Retry, Action::Dismiss]);
    assert_eq!(uncertain.record.deadline, None);
    assert_eq!(uncertain.record.surface, Surface::Timeline, "uncertain stays where it was");
    // Late evidence is the only automatic way out of uncertain.
    let late = f.go("n", Next::in_context(Evidence::A4Receipt)).unwrap();
    assert_eq!(late.record.state, InputState::InContext);
}

// --- surfaces ---------------------------------------------------------------

#[test]
fn surfaces_follow_the_creation_rules() {
    let intent = |strategy, timing| InputIntent { strategy, delivery_mode: None, timing };
    assert_eq!(creation_surface(InputKind::Owner, &intent(Some(Strategy::Auto), Timing::AfterTurn)), Surface::Timeline);
    assert_eq!(creation_surface(InputKind::Owner, &intent(Some(Strategy::Interrupt), Timing::AfterTurn)), Surface::Timeline);
    assert_eq!(creation_surface(InputKind::Owner, &intent(Some(Strategy::Queue), Timing::AfterTurn)), Surface::NextUp);
    for kind in [InputKind::AgentMessage, InputKind::PeerMessage, InputKind::Alarm, InputKind::SystemNotice,
        InputKind::DelegationResult, InputKind::DelegationQuestion, InputKind::DelegationStatus] {
        assert_eq!(creation_surface(kind, &intent(None, Timing::AfterTurn)), Surface::NextUp, "{kind}");
        assert_eq!(creation_surface(kind, &intent(None, Timing::NextStep)), Surface::NextUp, "{kind}");
        assert_eq!(creation_surface(kind, &intent(None, Timing::Passive)), Surface::PendingNotes, "{kind}");
    }
    assert_eq!(creation_surface(InputKind::DelegationRequest, &intent(None, Timing::AfterTurn)), Surface::NextUp);
}

#[test]
fn wake_moves_to_the_timeline_only_when_a_turn_carries_it() {
    let f = Fixture::new();
    let wake = f.go("w", Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    assert_eq!(wake.record.surface, Surface::NextUp);
    assert_eq!(wake.record.preview.chars().count(), 80);
    f.go("w", Next::sending()).unwrap();
    assert_eq!(f.go("w", Next::accepted()).unwrap().record.surface, Surface::NextUp);
    // r274.2 §9.4: a turn alone is not read evidence; the read signal is.
    assert_eq!(f.go("w", Next::accepted().turn("t1")).unwrap().record.surface, Surface::NextUp);
    let shown = Next::accepted().turn("t1").details(json!({READ_SIGNAL: "nativeItem"}));
    assert_eq!(f.go("w", shown).unwrap().record.surface, Surface::Timeline);
    assert!(matches!(f.go("w", Next::uncertain("turnFailed").surface(Surface::NextUp)), Err(LedgerError::SurfaceRegression { .. })));
    assert_eq!(f.go("w", Next::in_context(Evidence::A4Receipt)).unwrap().record.surface, Surface::Timeline);
}

#[test]
fn passive_note_leaves_pending_notes_when_read_in() {
    let f = Fixture::new();
    let created = f.go("p", Next::waiting("passive").create(note(InputKind::PeerMessage, Timing::Passive))).unwrap();
    assert_eq!(created.record.surface, Surface::PendingNotes);
    f.go("p", Next::sending()).unwrap();
    assert_eq!(f.go("p", Next::in_context(Evidence::ExternalInputPersisted).turn("t")).unwrap().record.surface, Surface::Timeline);
    // Read by a tool / agent_wait: disappears.
    f.go("q", Next::waiting("passive").create(note(InputKind::AgentMessage, Timing::Passive))).unwrap();
    assert_eq!(f.go("q", Next::in_context(Evidence::ReadByTool)).unwrap().record.surface, Surface::None);
    f.go("r", Next::waiting("afterTurn").create(note(InputKind::DelegationResult, Timing::AfterTurn))).unwrap();
    assert_eq!(f.go("r", Next::in_context(Evidence::ClaimedByWait)).unwrap().record.surface, Surface::None);
    f.go("d", Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    let dismissed = f.go("d", Next::discarded("ownerDiscarded")).unwrap();
    assert_eq!((dismissed.record.surface, dismissed.record.actions.len()), (Surface::None, 0));
}

#[test]
fn owner_queue_moves_once_and_only_the_owner_retry_returns_to_next_up() {
    let f = Fixture::new();
    let queued = f.go("q", Next::waiting("explicitQueue").create(owner(Strategy::Queue))).unwrap();
    assert_eq!(queued.record.surface, Surface::NextUp);
    assert_eq!(queued.record.actions, vec![Action::Cancel, Action::SendNow, Action::Edit]);
    f.go("q", Next::sending()).unwrap();
    // r274.2: an accepted steer (sendNow) is not read yet: still nextUp.
    assert_eq!(f.go("q", Next::accepted().turn("t1")).unwrap().record.surface, Surface::NextUp);
    let lost = f.go("q", Next::uncertain("turnInterrupted")).unwrap();
    assert_eq!(lost.record.surface, Surface::NextUp, "uncertain keeps its surface");
    let retry = f.go("q", Next::waiting("ownerRetry")).unwrap();
    assert_eq!((retry.record.surface, retry.record.attempt, retry.record.turn_id.as_deref()), (Surface::NextUp, 2, None));
    f.go("q", Next::sending()).unwrap();
    assert_eq!(f.go("q", Next::in_context(Evidence::TurnStartInput).turn("t2")).unwrap().record.surface, Surface::Timeline);
    // A record already on the timeline (a direct start cutex re-queued,
    // r274.2 note a) never goes back to nextUp.
    f.go("a", Next::received().create(owner(Strategy::Auto))).unwrap();
    assert_eq!(f.go("a", Next::waiting("turnFinishing")).unwrap().record.surface, Surface::Timeline);
    assert!(matches!(f.go("a", Next::waiting("turnFinishing").surface(Surface::NextUp)), Err(LedgerError::SurfaceRegression { .. })));
    // Paused session: the caller places the `auto` message in nextUp at creation.
    let paused = f.go("p", Next::waiting("paused").surface(Surface::NextUp).create(owner(Strategy::Auto))).unwrap();
    assert_eq!(paused.record.surface, Surface::NextUp);
    assert_eq!(f.go("p", Next::sending()).unwrap().record.surface, Surface::NextUp);
    assert_eq!(f.go("p", Next::in_context(Evidence::TurnStartInput).turn("t2")).unwrap().record.surface, Surface::Timeline);
}

/// r274.2 (owner decision 2026-11-03): an owner record on nextUp moves to
/// the timeline only when read (`inContext`, or Claude `accepted[offered]`),
/// once, and never back; non-owner kinds keep their rule.
#[test]
fn owner_next_up_moves_only_when_read() {
    let owner_kind = InputKind::Owner;
    let accepted = Next::accepted();
    let cases: Vec<(&str, Next, Option<Stage>, Surface)> = vec![
        ("accepted", accepted.clone(), None, Surface::NextUp),
        ("accepted[registered]", accepted.clone().stage(Stage::Registered), Some(Stage::Registered), Surface::NextUp),
        ("accepted[offered]", accepted.clone().stage(Stage::Offered), Some(Stage::Offered), Surface::Timeline),
        // The stored stage counts when the transition gives none.
        ("accepted, stored offered", accepted.clone(), Some(Stage::Offered), Surface::Timeline),
        ("inContext(clientIdEcho)", Next::in_context(Evidence::ClientIdEcho), None, Surface::Timeline),
        ("inContext(contextPersisted)", Next::in_context(Evidence::ContextPersisted), None, Surface::Timeline),
        ("inContext(turnStartInput)", Next::in_context(Evidence::TurnStartInput), None, Surface::Timeline),
        ("inContext(historyRead)", Next::in_context(Evidence::HistoryRead), None, Surface::Timeline),
        ("uncertain", Next::uncertain("turnInterrupted"), None, Surface::NextUp),
        ("failed", Next::failed("notDelivered"), None, Surface::NextUp),
        ("waiting", Next::waiting("turnRunning"), None, Surface::NextUp),
        ("sending", Next::sending(), None, Surface::NextUp),
    ];
    for (label, next, stage, expected) in &cases {
        assert_eq!(moved_surface(owner_kind, Surface::NextUp, next, Some("t1"), *stage), *expected, "owner {label}");
        // A timeline owner record stays on the timeline.
        if !matches!(next.state, InputState::Discarded | InputState::Superseded | InputState::Rejected) {
            assert_eq!(moved_surface(owner_kind, Surface::Timeline, next, Some("t1"), *stage), Surface::Timeline, "timeline {label}");
        }
    }
    // Non-owner kinds (r274.2 §9.4): `accepted` + `turnId` alone does not
    // move them (a queued steer, an adapter `registered`); its read signal
    // (native item, steer echo, offered) or `inContext` with a turn does.
    let signal = |value: &str| accepted.clone().details(json!({READ_SIGNAL: value}));
    for wake in [InputKind::AgentMessage, InputKind::Alarm, InputKind::SystemNotice, InputKind::DelegationResult] {
        for surface in [Surface::NextUp, Surface::PendingNotes] {
            assert_eq!(moved_surface(wake, surface, &accepted, Some("t1"), None), surface, "{wake} {surface}");
            assert_eq!(moved_surface(wake, surface, &accepted.clone().stage(Stage::Registered), Some("t1"), Some(Stage::Registered)), surface);
            assert_eq!(moved_surface(wake, surface, &accepted, None, None), surface);
            for value in ["nativeItem", "steerEcho", "offered", "turnStart"] {
                assert_eq!(moved_surface(wake, surface, &signal(value), Some("t1"), None), Surface::Timeline, "{wake} {value}");
            }
            assert_eq!(moved_surface(wake, surface, &signal("nativeItem"), None, None), surface, "a signal without a turn");
            assert_eq!(moved_surface(wake, surface, &Next::in_context(Evidence::A4Receipt), Some("t1"), None), Surface::Timeline);
        }
        assert_eq!(moved_surface(wake, Surface::NextUp, &Next::in_context(Evidence::TurnStartInput), None, None), Surface::Timeline);
    }
    // Through the ledger: never timeline -> nextUp, and the move is once.
    let f = Fixture::new();
    f.go("o", Next::sending().surface(Surface::NextUp).create(owner(Strategy::Auto))).unwrap();
    assert_eq!(f.go("o", Next::accepted().turn("t1")).unwrap().record.surface, Surface::NextUp);
    assert_eq!(f.go("o", Next::accepted().stage(Stage::Registered)).unwrap().record.surface, Surface::NextUp);
    let moved = f.go("o", Next::accepted().stage(Stage::Offered)).unwrap();
    assert_eq!(moved.record.surface, Surface::Timeline);
    assert!(moved.changed);
    assert!(matches!(f.go("o", Next::accepted().surface(Surface::NextUp)), Err(LedgerError::SurfaceRegression { .. })));
    let persisted = f.go("o", Next::in_context(Evidence::ContextPersisted)).unwrap();
    assert_eq!(persisted.record.surface, Surface::Timeline);
    // An owner retry goes to nextUp, an `auto` one too.
    f.go("r", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("r", Next::sending()).unwrap();
    f.go("r", Next::uncertain("outcomeUnknown")).unwrap();
    assert_eq!(f.go("r", Next::waiting("ownerRetry")).unwrap().record.surface, Surface::NextUp);
}

/// r274.2: `interruptSend` is not offered for an owner record the agent was
/// shown (Claude `accepted[offered]`, or §9.1 an `accepted` record bound to
/// its native item by the steer echo); `accepted[registered]` keeps it.
#[test]
fn interrupt_send_is_not_offered_once_shown() {
    let f = Fixture::new();
    let delivery = json!({"state": "turn", "turn": {"turnId": "t1"}});
    let legal = |record: &InputRecord| {
        interrupt_send_legal(InputKind::Owner, record.state, None, record.turn_id.as_deref(), shown_to_agent(record), Some(&delivery))
    };
    f.go("c", Next::sending().create(owner(Strategy::Auto))).unwrap();
    let plain = f.go("c", Next::accepted().turn("t1")).unwrap().record;
    assert!(legal(&plain));
    let registered = f.go("c", Next::accepted().stage(Stage::Registered)).unwrap().record;
    assert!(legal(&registered));
    let echoed = f.go("c", Next::accepted().surface(Surface::Timeline).timeline_item("steer-c")).unwrap().record;
    assert!(shown_to_agent(&echoed) && !legal(&echoed));
    let offered = f.go("c", Next::accepted().stage(Stage::Offered)).unwrap().record;
    assert!(!legal(&offered));
    assert!(!actions_with_delivery(InputKind::Owner, InputState::Accepted, None, Some("t1"), true, Some(&delivery))
        .contains(&Action::InterruptSend));
}

#[test]
fn merged_steers_are_superseded_by_the_new_record() {
    let f = Fixture::new();
    f.go("x", Next::waiting("explicitQueue").create(owner(Strategy::Queue))).unwrap();
    assert!(matches!(f.go("x", Next::superseded("mergedInsert")), Err(LedgerError::Invalid(_))));
    let merged = f.go("x", Next::superseded("mergedInsert").superseded_by("y")).unwrap();
    assert_eq!((merged.record.surface, merged.record.superseded_by.as_deref()), (Surface::None, Some("y")));
}

#[test]
fn delegation_request_reaches_the_timeline_when_its_turn_starts() {
    let f = Fixture::new();
    let mut request = note(InputKind::DelegationRequest, Timing::AfterTurn);
    request.ask_id = Some("ask-1".into());
    let created = f.go("ask-1", Next::waiting("reviewPending").create(request)).unwrap();
    assert!(created.record.actions.is_empty());
    f.go("ask-1", Next::waiting("targetBusy")).unwrap();
    f.go("ask-1", Next::sending()).unwrap();
    let running = f.go("ask-1", Next::in_context(Evidence::TurnStartInput)).unwrap();
    assert_eq!((running.record.surface, running.record.ask_id.as_deref()), (Surface::Timeline, Some("ask-1")));
}

// --- reads, compaction, deadlines --------------------------------------------

#[test]
fn get_returns_records_in_order_and_reports_missing_ids() {
    let f = Fixture::new();
    let empty = f.ledger.get("s1", &["a".into()]).unwrap();
    assert_eq!((empty.state_revision, empty.records.len(), empty.missing.clone()), (0, 0, vec!["a".to_string()]));
    f.go("a", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("b", Next::received().create(owner(Strategy::Auto))).unwrap();
    let got = f.ledger.get("s1", &["b".into(), "zz".into(), "a".into()]).unwrap();
    assert_eq!(got.records.iter().map(|record| record.input_id.as_str()).collect::<Vec<_>>(), vec!["b", "a"]);
    assert_eq!(got.missing, vec!["zz".to_string()]);
    assert_eq!(got.ledger_epoch, empty.ledger_epoch, "a session's first record keeps the epoch clients saw");
    let too_many: Vec<String> = (0..=MAX_GET_IDS).map(|index| index.to_string()).collect();
    assert!(matches!(f.ledger.get("s1", &too_many), Err(LedgerError::Invalid(_))));
}

#[test]
fn list_returns_open_records_and_recently_settled_ones() {
    let f = Fixture::new();
    f.go("open", Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    f.go("old", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("old", Next::sending()).unwrap();
    f.go("old", Next::in_context(Evidence::TurnStartInput)).unwrap();
    f.at(20 * 60);
    f.go("new", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("new", Next::rejected("invalid")).unwrap();
    let list = f.ledger.list("s1", ListQuery::default()).unwrap();
    assert_eq!(list.records.iter().map(|record| record.input_id.as_str()).collect::<Vec<_>>(), vec!["open", "new"]);
    assert!(list.complete && !list.resync_required);
    let wide = f.ledger.list("s1", ListQuery { since_revision: None, include_settled_secs: 3600 }).unwrap();
    assert_eq!(wide.records.len(), 3);
    let since = f.ledger.list("s1", ListQuery { since_revision: Some(4), include_settled_secs: 0 }).unwrap();
    // One entry per record (its latest state), not per change.
    assert_eq!(since.records.iter().map(|record| (record.input_id.as_str(), record.state_revision)).collect::<Vec<_>>(), vec![("new", 6)]);
    let changes = f.ledger.list("s1", ListQuery { since_revision: Some(3), include_settled_secs: 0 }).unwrap();
    assert_eq!(changes.records.iter().map(|record| record.state_revision).collect::<Vec<_>>(), vec![4, 6]);
    let ahead = f.ledger.since("s1", 99, 10).unwrap();
    assert!(ahead.resync_required, "a revision from the future means another epoch / ledger");
    let truncated = f.ledger.since("s1", 0, 2).unwrap();
    assert!(truncated.resync_required && truncated.records.is_empty());
}

#[test]
fn compaction_drops_old_settled_records_and_raises_the_resync_floor() {
    let f = Fixture::new();
    f.go("done", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("done", Next::failed("routeLost")).unwrap();
    f.go("open", Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    let removed = f.ledger.compact(t0() + chrono::Duration::days(8), Duration::from_secs(SETTLED_RETENTION_SECS), 100).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(f.ledger.get("s1", &["done".into()]).unwrap().missing, vec!["done".to_string()]);
    assert!(f.ledger.since("s1", 1, 100).unwrap().resync_required);
    let current = f.ledger.since("s1", 2, 100).unwrap();
    assert!(!current.resync_required);
    assert_eq!(current.records.len(), 1);
    // Open records are never compacted.
    assert_eq!(f.ledger.compact(t0() + chrono::Duration::days(80), Duration::from_secs(1), 100).unwrap(), 0);
}

#[test]
fn deadline_pass_applies_the_table_and_skips_records_that_moved() {
    let f = Fixture::new();
    f.go("r", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("h", Next::held("hostBusy").create(owner(Strategy::Auto))).unwrap();
    f.go("n", Next::received().create(note(InputKind::Alarm, Timing::AfterTurn))).unwrap();
    f.go("x", Next::sending().create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    f.go("w", Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    assert_eq!(next_deadline(&f.ledger).unwrap(), Some(t0() + chrono::Duration::seconds(5)));
    assert!(f.ledger.due(t0() + chrono::Duration::seconds(4), 10).unwrap().is_empty());
    let due = f.ledger.due(t0() + chrono::Duration::seconds(31), 10).unwrap();
    assert_eq!(due.len(), 3, "received ×2 at 5 s, sending at 30 s; held is 60 s; waiting has none");
    f.at(31);
    let mut applied = Vec::new();
    let moved_meanwhile = std::cell::Cell::new(false);
    let count = run_deadline_pass(&f.ledger, t0() + chrono::Duration::seconds(61), 10, |due| {
        if due.input_id == "x" && !moved_meanwhile.replace(true) {
            // Another writer moves the record between read and apply.
            f.go("x", Next::accepted().turn("t")).unwrap();
        }
        let record = f.ledger.get(&due.cutex_session_id, std::slice::from_ref(&due.input_id)).unwrap().records.remove(0);
        deadline_next(&InputRecord { state: due.state, ..record })
    }, |transition| applied.push((transition.record.input_id.clone(), transition.record.state, transition.record.reason.clone()))).unwrap();
    applied.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(count, 3);
    assert_eq!(applied, vec![
        ("h".to_string(), InputState::Failed, Some("admission_timeout".to_string())),
        ("n".to_string(), InputState::Waiting, Some("autoRetry".to_string())),
        ("r".to_string(), InputState::Failed, Some("routeLost".to_string())),
    ]);
    assert_eq!(f.ledger.get("s1", &["x".into()]).unwrap().records[0].state, InputState::Accepted);
}

// --- lock guard --------------------------------------------------------------

#[test]
fn the_ledger_lock_is_a_leaf() {
    let f = Fixture::new();
    f.go("a", Next::received().create(owner(Strategy::Auto))).unwrap();
    assert!(!thread_holds_ledger_lock());
    let lane = super::super::session_coordination::session_lane("input-ledger-leaf-test").unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = f.ledger.lock().unwrap();
        assert!(thread_holds_ledger_lock());
        let _reservation = lane.reserve();
    }));
    if cfg!(debug_assertions) {
        let message = result.unwrap_err();
        let text = message.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(text.contains("leaf"), "{text}");
    }
    assert!(!thread_holds_ledger_lock(), "the guard clears on unwind");
    // Lane → ledger is the allowed order.
    let _reservation = lane.reserve().unwrap();
    f.go("a", Next::sending()).unwrap();
}

// --- contract -----------------------------------------------------------------

fn export() -> Value {
    serde_json::from_str(EXPORTED_SCHEMA.text).unwrap()
}

fn exported(name: &str) -> Vec<String> {
    serde_json::from_value(export()["$defs"][name]["enum"].clone()).unwrap_or_else(|_| panic!("export lacks {name}"))
}

fn methods() -> Value {
    serde_json::from_str(include_str!("schema/cutex-methods-v2.json")).unwrap()
}

fn methods_enum(pointer: &str) -> Vec<Value> {
    serde_json::from_value(methods()["$defs"].pointer(pointer).unwrap_or_else(|| panic!("{pointer}")).clone()).unwrap()
}

fn strings(values: Vec<Value>) -> Vec<String> {
    values.into_iter().filter_map(|value| value.as_str().map(str::to_string)).collect()
}

/// The non-test part of `input_ledger.rs`.
fn production() -> String {
    let source = include_str!("input_ledger.rs").replace("\r\n", "\n");
    source.split("\n#[cfg(test)]\n#[path").next().unwrap().to_string()
}

fn literals(source: &str, call: &str) -> Vec<String> {
    source.split(call).skip(1).filter_map(|rest| {
        let rest = rest.strip_prefix('"')?;
        Some(rest[..rest.find('"')?].to_string())
    }).collect()
}

/// 054 §2.4: constants, export, methods schema and the literals in the code
/// agree. The desktop maps exactly these values.
#[test]
fn input_state_reasons_are_enumerated_everywhere() {
    let pairs: &[(&str, &[&str])] = &[
        ("inputKind", INPUT_KINDS), ("senderKind", SENDER_KINDS), ("inputState", INPUT_STATES),
        ("heldReason", HELD_REASONS), ("waitingReason", WAITING_REASONS), ("uncertainReason", UNCERTAIN_REASONS),
        ("failedReason", FAILED_REASONS), ("rejectedReason", REJECTED_REASONS), ("discardedReason", DISCARDED_REASONS),
        ("supersededReason", SUPERSEDED_REASONS), ("stalledReason", STALLED_REASONS), ("surface", SURFACES),
        ("stage", STAGES), ("evidence", EVIDENCE), ("action", ACTIONS), ("strategy", STRATEGIES),
        ("deliveryMode", DELIVERY_MODES), ("timing", TIMINGS), ("sessionDeliveryState", SESSION_DELIVERY_STATES),
        ("sessionDeliveryHeldReason", SESSION_DELIVERY_HELD_REASONS), ("sessionTurnKind", SESSION_TURN_KINDS),
        ("leaseHolder", LEASE_HOLDERS), ("leaseReconcileState", LEASE_RECONCILE_STATES),
        ("sendingReason", SENDING_REASONS), ("carriedReason", CARRIED_REASONS),
    ];
    for (name, constant) in pairs {
        assert_eq!(exported(name), *constant, "export {name}");
    }
    let defs = export()["$defs"].as_object().unwrap().len();
    assert_eq!(defs, pairs.len() + 2, "every export enum is checked (plus terminal / settled)");
    // Wire enums = constants.
    let wire = |all: Vec<&'static str>| all;
    assert_eq!(wire(InputKind::ALL.iter().map(|v| v.as_str()).collect()), INPUT_KINDS);
    assert_eq!(wire(SenderKind::ALL.iter().map(|v| v.as_str()).collect()), SENDER_KINDS);
    assert_eq!(wire(InputState::ALL.iter().map(|v| v.as_str()).collect()), INPUT_STATES);
    assert_eq!(wire(Surface::ALL.iter().map(|v| v.as_str()).collect()), SURFACES);
    assert_eq!(wire(Stage::ALL.iter().map(|v| v.as_str()).collect()), STAGES);
    assert_eq!(wire(Evidence::ALL.iter().map(|v| v.as_str()).collect()), EVIDENCE);
    assert_eq!(wire(Action::ALL.iter().map(|v| v.as_str()).collect()), ACTIONS);
    assert_eq!(wire(Strategy::ALL.iter().map(|v| v.as_str()).collect()), STRATEGIES);
    assert_eq!(wire(DeliveryMode::ALL.iter().map(|v| v.as_str()).collect()), DELIVERY_MODES);
    assert_eq!(wire(Timing::ALL.iter().map(|v| v.as_str()).collect()), TIMINGS);
    let terminal: Vec<&str> = InputState::ALL.iter().filter(|s| s.is_terminal()).map(|s| s.as_str()).collect();
    let settled: Vec<&str> = InputState::ALL.iter().filter(|s| s.is_settled()).map(|s| s.as_str()).collect();
    assert_eq!(exported("terminalState"), terminal);
    assert_eq!(exported("settledState"), settled);
    // Methods schema.
    assert_eq!(strings(methods_enum("/inputKind/enum")), INPUT_KINDS);
    assert_eq!(strings(methods_enum("/inputSenderKind/enum")), SENDER_KINDS);
    assert_eq!(strings(methods_enum("/inputState/enum")), INPUT_STATES);
    assert_eq!(strings(methods_enum("/inputSurface/enum")), SURFACES);
    assert_eq!(strings(methods_enum("/inputStage/enum")), STAGES);
    assert_eq!(strings(methods_enum("/inputEvidence/enum")), EVIDENCE);
    assert_eq!(strings(methods_enum("/inputAction/enum")), ACTIONS);
    assert_eq!(strings(methods_enum("/inputIntent/properties/strategy/enum")), STRATEGIES);
    assert_eq!(strings(methods_enum("/inputIntent/properties/deliveryMode/enum")), DELIVERY_MODES);
    assert_eq!(strings(methods_enum("/inputIntent/properties/timing/enum")), TIMINGS);
    assert_eq!(strings(methods_enum("/sessionDelivery/properties/state/enum")), SESSION_DELIVERY_STATES);
    assert_eq!(strings(methods_enum("/sessionDelivery/properties/reason/enum")), SESSION_DELIVERY_HELD_REASONS);
    assert_eq!(strings(methods_enum("/inputLease/properties/holder/enum")), LEASE_HOLDERS);
    assert_eq!(strings(methods_enum("/inputLease/properties/reconcile/properties/state/enum")), LEASE_RECONCILE_STATES);
    let mut union: Vec<&str> = Vec::new();
    // Append only: `sending` reasons came in r274 (interruptSend) and are
    // listed after every earlier one.
    // `discarded{superseded}` came in r274.2 and is listed after those, then
    // `discarded{historyReverted}` (r274.2 §9.6).
    const LATER: &[&str] = &["superseded", "historyReverted", "carriedByOwner"];
    let states = InputState::ALL.iter().filter(|state| **state != InputState::Sending).chain([&InputState::Sending]);
    for state in states {
        for reason in state.reasons().unwrap_or(&[]) {
            if !union.contains(reason) && !LATER.contains(reason) {
                union.push(reason);
            }
        }
    }
    union.extend(LATER);
    assert_eq!(strings(methods_enum("/inputReason/enum")), union);
    // Literal reasons in the production code are listed for their state.
    let source = production();
    let mut seen = 0;
    for (call, state) in [
        ("Next::held(", InputState::Held), ("Next::waiting(", InputState::Waiting),
        ("Next::uncertain(", InputState::Uncertain), ("Next::failed(", InputState::Failed),
        ("Next::rejected(", InputState::Rejected), ("Next::discarded(", InputState::Discarded),
        ("Next::superseded(", InputState::Superseded),
    ] {
        for reason in literals(&source, call) {
            seen += 1;
            assert!(state.reasons().unwrap().contains(&reason.as_str()), "unlisted {state} reason {reason}");
        }
    }
    assert!(seen >= 4, "deadline_next literals not found");
}

fn validator(definition: &str) -> jsonschema::Validator {
    let mut schema = methods();
    schema.as_object_mut().unwrap().insert("$ref".into(), Value::String(format!("#/$defs/{definition}")));
    jsonschema::options().with_draft(jsonschema::Draft::Draft202012).build(&schema).unwrap()
}

#[test]
fn records_events_and_routes_match_the_methods_schema() {
    use super::super::contract_validation::{validate_cutex_event_message, validate_cutex_request, validate_cutex_response};
    let f = Fixture::new();
    let record = validator("inputRecord");
    let mut wake = note(InputKind::PeerMessage, Timing::AfterTurn);
    wake.remote = Some(InputRemote { host: "host-b".into(), peer_id: "p".into(), sender_id: "s".into() });
    let steps = [
        Next::waiting("afterTurn").create(wake),
        Next::sending(),
        Next::waiting("nativeHeld").details(json!({"nativeReason": "PlanMode"})),
        Next::sending(),
        Next::accepted().turn("t1").stage(Stage::Registered),
        Next::uncertain("turnInterrupted"),
        Next::in_context(Evidence::A4Receipt),
    ];
    for step in steps {
        let transition = f.go("w", step).unwrap();
        let value = serde_json::to_value(&transition.record).unwrap();
        if let Err(error) = record.validate(&value) {
            panic!("{error}: {value}");
        }
        validate_cutex_event_message(&state_changed_event(&transition)).unwrap();
        validate_cutex_event_message(&host_state_changed_event(&transition)).unwrap();
        assert!(host_state_changed_event(&transition).pointer("/params/record/preview").is_none());
    }
    let failed = f.go("o", Next::failed("tooLarge").create(owner(Strategy::Auto))).unwrap();
    record.validate(&serde_json::to_value(&failed.record).unwrap()).unwrap();
    validate_cutex_request(&json!({"requestId": "r1", "method": GET_METHOD, "params": {"inputIds": ["w"]}})).unwrap();
    validate_cutex_request(&json!({"requestId": "r1", "method": LIST_METHOD, "params": {}})).unwrap();
    validate_cutex_request(&json!({"requestId": "r1", "method": LIST_METHOD, "params": {"sinceRevision": 3, "includeSettledSecs": 60}})).unwrap();
    assert!(validate_cutex_request(&json!({"requestId": "r1", "method": GET_METHOD, "params": {"inputIds": []}})).is_err());
    assert!(validate_cutex_request(&json!({"requestId": "r1", "method": LIST_METHOD, "params": {"since": 1}})).is_err());
    let get = serde_json::to_value(f.ledger.get("s1", &["w".into(), "nope".into()]).unwrap()).unwrap();
    validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r1", "cutexSessionId": "s1", "cutex": {"method": GET_METHOD, "result": get}})).unwrap();
    for query in [ListQuery::default(), ListQuery { since_revision: Some(1), include_settled_secs: 0 }, ListQuery { since_revision: Some(1000), include_settled_secs: 0 }] {
        let list = serde_json::to_value(f.ledger.list("s1", query).unwrap()).unwrap();
        validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r1", "cutexSessionId": "s1", "cutex": {"method": LIST_METHOD, "result": list}})).unwrap();
    }
}

#[test]
fn routes_parse_and_stay_off_without_the_switch() {
    assert_eq!(parse_path("/v2/sessions/cutex.a/inputs"), Some(HttpRoute { cutex_session_id: "cutex.a".into(), input_id: None }));
    assert_eq!(parse_path("/v2/sessions/cutex.a/inputs?since=4"), Some(HttpRoute { cutex_session_id: "cutex.a".into(), input_id: None }));
    assert_eq!(parse_path("/v2/sessions/cutex.a/inputs/c%2D1"), Some(HttpRoute { cutex_session_id: "cutex.a".into(), input_id: Some("c-1".into()) }));
    assert_eq!(parse_path("/v2/sessions/cutex.a/inputsx"), None);
    assert_eq!(parse_path("/v2/sessions/cutex.a/inputs/"), None);
    assert_eq!(parse_path("/v2/sessions/cutex.a/inputs/a/b"), None);
    assert!(is_method(GET_METHOD) && is_method(LIST_METHOD) && is_method(RETRY_METHOD) && !is_method("cutex/input/redo"));
    let pin = pin_enabled(Some(false));
    let error = dispatch("s1", LIST_METHOD, &json!({}), true).unwrap_err();
    assert_eq!(error.code, "cutex_method_denied");
    drop(pin);
    assert_eq!(parse_list_query(None, None).unwrap(), ListQuery::default());
    assert!(parse_list_query(Some(&json!(-1)), None).is_err());
    assert_eq!(parse_list_query(None, Some(&json!(u64::MAX >> 12))).unwrap().include_settled_secs, SETTLED_RETENTION_SECS);
    assert!(parse_ids(Some(&json!(["a", ""]))).is_err());
}

#[test]
fn the_switch_is_a_typed_config_field_that_survives_a_save() {
    let config: crate::profiles::model::CodezConfig =
        serde_json::from_value(json!({"management": {"inputLedger": {"enabled": true}}})).unwrap();
    assert!(config.management.input_ledger.enabled);
    let saved = serde_json::to_value(&config).unwrap();
    assert_eq!(saved["management"], json!({"inputLedger": {"enabled": true}}));
    let default = serde_json::to_value(crate::profiles::model::CodezConfig::default()).unwrap();
    assert!(default.get("management").is_none(), "absent unless set");
}

// --- amendments (2026-11-02): delivery revision, owner decision 9, dismiss ----

fn delivery(state: &str) -> Value {
    json!({"state": state, "reason": null, "turn": null, "lease": null, "runtimeConnected": state != "offline",
        "runtimeGeneration": 3, "heldCount": 0, "heldUntil": null, "stateRevision": 0,
        "observedAt": "2026-11-01T12:00:00.000Z"})
}

#[test]
fn delivery_changes_share_the_gapless_revision_and_reach_incremental_reads() {
    let f = Fixture::new();
    f.go("a", Next::received().create(owner(Strategy::Auto))).unwrap();
    let busy = f.ledger.delivery_changed("s1", &delivery("turn")).unwrap();
    assert!(busy.changed);
    assert_eq!((busy.state_revision, busy.delivery["stateRevision"].as_u64()), (2, Some(2)));
    let mut same = delivery("turn");
    same["observedAt"] = json!("2026-11-01T12:00:09.000Z");
    let unchanged = f.ledger.delivery_changed("s1", &same).unwrap();
    assert!(!unchanged.changed && unchanged.state_revision == 2);
    assert_eq!(f.go("a", Next::sending()).unwrap().state_revision, 3);
    let idle = f.ledger.delivery_changed("s1", &delivery("idle")).unwrap();
    assert_eq!(idle.state_revision, 4);
    let since = f.ledger.list("s1", ListQuery { since_revision: Some(3), include_settled_secs: 0 }).unwrap();
    assert!(since.records.is_empty());
    assert_eq!((since.state_revision, since.delivery.as_ref().unwrap()["state"].as_str()), (4, Some("idle")));
    let full = f.ledger.list("s1", ListQuery::default()).unwrap();
    assert_eq!(full.delivery.unwrap()["stateRevision"], 4);
    let event = delivery_changed_event("s1", &idle);
    super::super::contract_validation::validate_cutex_event_message(&event).unwrap();
    let first = f.ledger.delivery_changed("s9", &delivery("offline")).unwrap();
    assert_eq!((first.state_revision, first.ledger_epoch.as_str()), (1, idle.ledger_epoch.as_str()));
}

#[test]
fn uncertain_never_expires_and_is_always_listed() {
    let f = Fixture::new();
    f.go("u", Next::sending().create(note(InputKind::DelegationResult, Timing::AfterTurn))).unwrap();
    f.go("u", Next::uncertain("outcomeUnknown")).unwrap();
    f.go("t", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("t", Next::failed("routeLost")).unwrap();
    f.at(30 * 24 * 3600);
    let list = f.ledger.list("s1", ListQuery { since_revision: None, include_settled_secs: 0 }).unwrap();
    assert_eq!(list.records.iter().map(|r| r.input_id.as_str()).collect::<Vec<_>>(), vec!["u"]);
    let removed = f.ledger.compact(t0() + chrono::Duration::days(30), Duration::from_secs(SETTLED_RETENTION_SECS), 100).unwrap();
    assert_eq!(removed, 1, "only the terminal one");
    assert_eq!(f.ledger.get("s1", &["u".into()]).unwrap().records.len(), 1);
}

#[test]
fn owner_dismiss_settles_one_or_many_with_cas() {
    let f = Fixture::new();
    f.go("u", Next::sending().create(note(InputKind::DelegationResult, Timing::AfterTurn))).unwrap();
    f.go("u", Next::uncertain("outcomeUnknown")).unwrap();
    f.go("w", Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    f.go("o", Next::waiting("explicitQueue").create(owner(Strategy::Queue))).unwrap();
    let (results, events) = capture_events(|| {
        vec![
            dismiss_one(&f.ledger, "s1", "u", 1).map(|_| ()).unwrap_err(),
            dismiss_one(&f.ledger, "s1", "o", 1).map(|_| ()).unwrap_err(),
            dismiss_one(&f.ledger, "s1", "zz", 1).map(|_| ()).unwrap_err(),
        ]
    });
    assert!(events.is_empty());
    let codes: Vec<&str> = results.iter().map(|failure| failure["code"].as_str().unwrap()).collect();
    assert_eq!(codes, vec!["revision_conflict", "input_not_dismissible", "input_not_found"]);
    assert_eq!(results[0]["currentRev"], 2);
    let (done, events) = capture_events(|| (dismiss_one(&f.ledger, "s1", "u", 2).unwrap(), dismiss_one(&f.ledger, "s1", "w", 1).unwrap()));
    assert_eq!((done.0.record.state, done.0.record.reason.as_deref(), done.0.record.surface), (InputState::Discarded, Some("ownerDiscarded"), Surface::None));
    assert_eq!(done.1.record.state, InputState::Discarded);
    assert_eq!(events.iter().map(|(session, method, _)| (session.as_str(), method.as_str())).collect::<Vec<_>>(),
        vec![("s1", STATE_CHANGED_EVENT), ("s1", STATE_CHANGED_EVENT)]);
    // Bulk params.
    let bulk = json!({"items": [{"inputId": "a", "expectedRev": 1}, {"inputId": "b", "expectedRev": 2}]});
    assert_eq!(parse_dismiss_items(bulk.as_object().unwrap()).unwrap(), vec![("a".into(), 1), ("b".into(), 2)]);
    assert_eq!(parse_dismiss_items(json!({"inputId": "a", "expectedRev": 3}).as_object().unwrap()).unwrap(), vec![("a".into(), 3)]);
    for bad in [json!({"items": []}), json!({"items": [{"inputId": "a"}]}), json!({"inputId": "a", "expectedRev": 0}),
        json!({"items": [{"inputId": "a", "expectedRev": 1}], "x": 1})] {
        assert!(parse_dismiss_items(bad.as_object().unwrap()).is_err(), "{bad}");
    }
    use super::super::contract_validation::{validate_cutex_request, validate_cutex_response};
    validate_cutex_request(&json!({"requestId": "r", "method": DISMISS_METHOD, "params": bulk})).unwrap();
    validate_cutex_request(&json!({"requestId": "r", "method": DISMISS_METHOD, "params": {"inputId": "a", "expectedRev": 1}})).unwrap();
    validate_cutex_request(&json!({"requestId": "r", "method": LEASE_RELEASE_METHOD, "params": {"expectedLease": {
        "holder": "event", "turnId": null, "since": "2026-11-01T12:00:00Z", "runtimeGeneration": 1, "inputIds": ["d"],
        "reconcile": {"state": "stale"}}}})).unwrap();
    let result = json!({"ledgerEpoch": "le-x", "stateRevision": 5, "records": [done.0.record],
        "failed": [results[0].clone()]});
    validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r", "cutexSessionId": "s1", "cutex": {"method": DISMISS_METHOD, "result": result}})).unwrap();
}

#[test]
fn owner_methods_need_the_owner_and_lease_release_checks_the_lease() {
    let pin = pin_enabled(Some(true));
    let denied = dispatch("s1", DISMISS_METHOD, &json!({"inputId": "a", "expectedRev": 1}), false).unwrap_err();
    assert_eq!(denied.code, "owner_required");
    let denied = dispatch("s1", LEASE_RELEASE_METHOD, &json!({"expectedLease": {}}), false).unwrap_err();
    assert_eq!(denied.code, "owner_required");
    let no_lease = dispatch("s1", LEASE_RELEASE_METHOD, &json!({"expectedLease": {}}), true).unwrap_err();
    assert_eq!(no_lease.code, "lease_changed", "served by C4: no lease on s1");
    drop(pin);
    let mut params = json!({"record": {"preview": "p", "actions": ["retry"], "state": "uncertain"}});
    host_projection(STATE_CHANGED_EVENT, &mut params);
    assert_eq!(params, json!({"record": {"state": "uncertain"}}));
}

#[test]
fn record_is_a_no_op_while_the_switch_is_off() {
    let pin = pin_enabled(Some(false));
    assert!(record("s1", "x", Expect::ANY, Next::received().create(owner(Strategy::Auto))).is_none());
    drop(pin);
}

#[test]
fn a_returned_input_counts_its_next_attempt() {
    let f = Fixture::new();
    f.go("n", Next::sending().create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    let returned = f.go("n", Next::waiting("returned")).unwrap();
    assert_eq!(returned.record.attempt, 2);
    f.go("n", Next::sending()).unwrap();
    assert_eq!(f.go("n", Next::waiting("nativeRetry")).unwrap().record.attempt, 2, "only Absent counts");
}

#[test]
fn a_refused_owner_send_waits_again_without_leaving_the_timeline() {
    let f = Fixture::new();
    f.go("o", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("o", Next::sending()).unwrap();
    let back = f.go("o", Next::waiting("hostBusy")).unwrap();
    assert_eq!((back.record.state, back.record.surface), (InputState::Waiting, Surface::Timeline));
    let jump = f.go("o", Next::waiting("hostBusy").surface(Surface::NextUp));
    assert!(jump.is_err(), "timeline never moves back to nextUp: {jump:?}");
}

#[test]
fn a_batch_is_one_transaction_and_one_state_revision() {
    let f = Fixture::new();
    for id in ["q0", "q1", "q2"] {
        f.go(id, Next::waiting("explicitQueue").create(owner(Strategy::Queue))).unwrap();
    }
    let before = f.ledger.get("s1", &["q0".into()]).unwrap().state_revision;
    let items = vec![
        ("q1".to_string(), Expect::rev(1), Next::superseded("mergedInsert").superseded_by("q0")),
        ("q2".to_string(), Expect::rev(1), Next::superseded("mergedInsert").superseded_by("q0")),
        ("q0".to_string(), Expect::ANY, Next::waiting("explicitQueue")),
    ];
    let out = f.ledger.transition_batch("s1", items).unwrap();
    assert_eq!(out.iter().map(|t| t.changed).collect::<Vec<_>>(), [true, true, false]);
    assert!(out[..2].iter().all(|t| t.state_revision == before + 1 && t.record.state_revision == before + 1));
    assert_eq!(f.ledger.get("s1", &["q0".into()]).unwrap().state_revision, before + 1);
    let since = f.ledger.since("s1", before, 100).unwrap();
    assert_eq!(since.records.len(), 2);

    // All or nothing: one stale expectation rolls the whole batch back.
    f.go("q3", Next::waiting("explicitQueue").create(owner(Strategy::Queue))).unwrap();
    f.go("q4", Next::waiting("explicitQueue").create(owner(Strategy::Queue))).unwrap();
    let head = f.ledger.get("s1", &["q3".into()]).unwrap().state_revision;
    let err = f.ledger.transition_batch("s1", vec![
        ("q3".to_string(), Expect::rev(1), Next::superseded("mergedInsert").superseded_by("q4")),
        ("q4".to_string(), Expect::rev(9), Next::sending()),
    ]).unwrap_err();
    assert!(matches!(err, LedgerError::Conflict { current_rev: 1 }), "{err:?}");
    let after = f.ledger.get("s1", &["q3".into()]).unwrap();
    assert_eq!((after.state_revision, after.records[0].state), (head, InputState::Waiting));
    assert!(f.ledger.transition_batch("s1", vec![
        ("q3".to_string(), Expect::ANY, Next::sending()),
        ("q3".to_string(), Expect::ANY, Next::sending()),
    ]).is_err(), "duplicate ids are refused");
}

// ---------------------------------------------------------------------------
// Outbox: publish order across processes without waiting (054 §2.8 + 051)

static JOURNAL: Mutex<Vec<(String, String, Value)>> = Mutex::new(Vec::new());

fn journal_append(session: &str, method: String, params: Value) -> anyhow::Result<()> {
    JOURNAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((session.to_string(), method, params));
    Ok(())
}

/// (method, stateRevision, inputId or "") of `session` in journal order.
fn journal(session: &str) -> Vec<(String, u64, String)> {
    JOURNAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().filter(|(s, _, _)| s == session)
        .map(|(_, method, params)| (
            method.clone(),
            params["stateRevision"].as_u64().unwrap(),
            params["record"]["inputId"].as_str().unwrap_or_default().to_string(),
        ))
        .collect()
}

fn revisions(session: &str) -> Vec<u64> {
    journal(session).into_iter().map(|(_, revision, _)| revision).collect()
}

/// Two ledgers on one file: two processes' process ledgers.
struct TwoProcesses {
    root: PathBuf,
    a: &'static InputLedger,
    b: &'static InputLedger,
}

impl TwoProcesses {
    fn new(sink: fn(&str, String, Value) -> anyhow::Result<()>) -> Self {
        let root = std::env::temp_dir().join(format!("cutex-input-outbox-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let open = || -> &'static InputLedger { Box::leak(Box::new(InputLedger::open(root.join(DATABASE_FILE)).with_outbox(sink))) };
        let (a, b) = (open(), open());
        Self { root, a, b }
    }

    fn lock_file(&self) -> std::fs::File {
        std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(self.a.publish_lock_path()).unwrap()
    }
}

fn session_name() -> String {
    format!("ob-{}", uuid::Uuid::new_v4().simple())
}

fn new_note(ledger: &InputLedger, session: &str, id: &str) -> Transition {
    ledger.transition(session, id, Expect::NEW, Next::waiting("afterTurn").create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap()
}

#[test]
fn outbox_interleaved_commits_of_two_processes_reach_the_journal_in_revision_order() {
    let p = TwoProcesses::new(journal_append);
    let session = session_name();
    // A commits 1, B commits 2 and publishes first: B drains A's row too, in order.
    let one = new_note(p.a, &session, "a1");
    let two = new_note(p.b, &session, "b1");
    assert!(one.outboxed && two.outboxed);
    assert_eq!((one.state_revision, two.state_revision), (1, 2));
    assert_eq!(p.b.drain_outbox().unwrap(), 2);
    assert_eq!(p.a.drain_outbox().unwrap(), 0, "nothing left");
    assert_eq!(revisions(&session), vec![1, 2]);
    // A delivery change shares the sequence; a batch is one revision, one
    // row per changed record in batch order.
    let delivery = p.a.delivery_changed(&session, &json!({"state": "idle"})).unwrap();
    assert_eq!((delivery.state_revision, delivery.outboxed), (3, true));
    let batch = p.b.transition_batch(&session, vec![
        ("a1".to_string(), Expect::ANY, Next::sending()),
        ("b1".to_string(), Expect::ANY, Next::sending()),
    ]).unwrap();
    assert!(batch.iter().all(|t| t.state_revision == 4 && t.outboxed));
    p.a.drain_outbox().unwrap();
    assert_eq!(journal(&session)[2..], [
        (DELIVERY_CHANGED_EVENT.to_string(), 3, String::new()),
        (STATE_CHANGED_EVENT.to_string(), 4, "a1".to_string()),
        (STATE_CHANGED_EVENT.to_string(), 4, "b1".to_string()),
    ]);

    // Many writers of both "processes", each publishing after its commit.
    let session = session_name();
    let threads: Vec<_> = (0..8).map(|thread| {
        let ledger = if thread % 2 == 0 { p.a } else { p.b };
        let session = session.clone();
        std::thread::spawn(move || {
            for i in 0..15 {
                let id = format!("t{thread}-{i}");
                new_note(ledger, &session, &id);
                ledger.drain_outbox().unwrap();
                if i % 5 == 0 {
                    let delivery = ledger.delivery_changed(&session, &json!({"state": "idle", "n": format!("{thread}-{i}")})).unwrap();
                    assert!(delivery.changed);
                    ledger.drain_outbox().unwrap();
                }
            }
        })
    }).collect();
    for thread in threads {
        thread.join().unwrap();
    }
    // A publisher that lost the try-lock left its row to the holder, which
    // checked again after releasing: nothing is left behind.
    assert!(!p.a.outbox_pending().unwrap());
    let head = p.a.get(&session, &[]).unwrap().state_revision;
    assert_eq!(head, 8 * 15 + 8 * 3);
    assert_eq!(revisions(&session), (1..=head).collect::<Vec<_>>(), "gapless, in order, once each");
    let _ = std::fs::remove_dir_all(&p.root);
}

#[test]
fn outbox_publisher_never_waits_for_the_publish_lock() {
    let p = TwoProcesses::new(journal_append);
    let session = session_name();
    // Another process holds the publish lock (a long drain).
    let held = p.lock_file();
    fs2::FileExt::lock_exclusive(&held).unwrap();
    new_note(p.a, &session, "x");
    let started = Instant::now();
    assert_eq!(p.a.drain_outbox().unwrap(), 0);
    assert!(started.elapsed() < Duration::from_millis(500), "a publisher only tries the lock: {:?}", started.elapsed());
    assert!(p.a.outbox_pending().unwrap(), "the row waits for the holder");
    assert!(revisions(&session).is_empty());
    // Another thread of this process holds the in-process publisher.
    let holder = p.b.publisher.lock().unwrap();
    new_note(p.b, &session, "y");
    let started = Instant::now();
    std::thread::scope(|scope| scope.spawn(|| assert_eq!(p.b.drain_outbox().unwrap(), 0)).join().unwrap());
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(p.b.rerun.load(std::sync::atomic::Ordering::SeqCst), "the holder is told to drain again");
    drop(holder);
    // The holder process dies: closing its handle releases the lock (the
    // lock file stays behind and means nothing).
    drop(held);
    assert!(p.a.publish_lock_path().exists());
    assert_eq!(p.b.drain_outbox().unwrap(), 2);
    assert_eq!(revisions(&session), vec![1, 2]);
    let _ = std::fs::remove_dir_all(&p.root);
}

static CRASH_AFTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

/// Appends like the journal; the append after `CRASH_AFTER` more panics
/// (the holder dies mid-drain, after appending some rows).
fn crashing_append(session: &str, method: String, params: Value) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering::SeqCst;
    if CRASH_AFTER.fetch_update(SeqCst, SeqCst, |left| (left != usize::MAX).then(|| left.saturating_sub(1))) == Ok(0) {
        CRASH_AFTER.store(usize::MAX, SeqCst);
        panic!("holder crashed mid-drain (test)");
    }
    journal_append(session, method, params)
}

#[test]
fn outbox_rows_of_a_crashed_holder_are_published_by_the_sweeper() {
    let p = TwoProcesses::new(crashing_append);
    let session = session_name();
    for i in 0..5 {
        new_note(p.a, &session, &format!("n{i}"));
    }
    CRASH_AFTER.store(2, std::sync::atomic::Ordering::SeqCst);
    let crashed = std::panic::catch_unwind(|| p.a.drain_outbox());
    assert!(crashed.is_err(), "the drain died");
    // Appended 1 and 2, deleted nothing: the crash window.
    assert_eq!(revisions(&session), vec![1, 2]);
    assert!(p.a.outbox_pending().unwrap());
    // The other process's backstop picks the rows up; nobody publishes.
    p.b.ensure_sweeper();
    let until = Instant::now() + SWEEP_EVERY * 6;
    while p.b.outbox_pending().unwrap() {
        assert!(Instant::now() < until, "the sweeper drains a crashed holder's rows");
        std::thread::sleep(Duration::from_millis(50));
    }
    // 1 and 2 twice (appended before the crash, not deleted), the rest
    // once; never out of order. Clients drop the repeats (§2.8).
    assert_eq!(revisions(&session), vec![1, 2, 1, 2, 3, 4, 5]);
    // The poisoned in-process publisher still works.
    new_note(p.a, &session, "n5");
    assert_eq!(p.a.drain_outbox().unwrap(), 1);
    assert_eq!(*revisions(&session).last().unwrap(), 6);
    let _ = std::fs::remove_dir_all(&p.root);
}

static FAILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn failing_append(session: &str, method: String, params: Value) -> anyhow::Result<()> {
    if FAILING.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("journal unavailable (test)");
    }
    journal_append(session, method, params)
}

#[test]
fn outbox_failed_append_keeps_order_and_a_poison_row_is_dropped() {
    let p = TwoProcesses::new(failing_append);
    let session = session_name();
    new_note(p.a, &session, "f1");
    new_note(p.a, &session, "f2");
    FAILING.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(p.a.drain_outbox().is_err());
    assert!(revisions(&session).is_empty(), "nothing goes out past the failed head");
    FAILING.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(p.a.drain_outbox().unwrap(), 2);
    assert_eq!(revisions(&session), vec![1, 2]);
    // A row that never appends is dropped after MAX_APPEND_ATTEMPTS rounds.
    new_note(p.a, &session, "f3");
    FAILING.store(true, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..MAX_APPEND_ATTEMPTS {
        assert!(p.a.drain_outbox().is_err());
    }
    FAILING.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(!p.a.outbox_pending().unwrap());
    assert_eq!(revisions(&session), vec![1, 2]);
    let _ = std::fs::remove_dir_all(&p.root);
}

#[test]
fn outbox_is_not_written_while_the_switch_is_off() {
    let root = std::env::temp_dir().join(format!("cutex-input-outbox-{}", uuid::Uuid::new_v4().simple()));
    let ledger = InputLedger::open(root.join(DATABASE_FILE)).with_switched_outbox(journal_append);
    let session = session_name();
    let pin = pin_enabled(Some(false));
    let created = new_note(&ledger, &session, "o1");
    let delivery = ledger.delivery_changed(&session, &json!({"state": "idle"})).unwrap();
    let batch = ledger.transition_batch(&session, vec![("o1".to_string(), Expect::ANY, Next::sending())]).unwrap();
    drop(pin);
    assert!(created.changed && delivery.changed && batch[0].changed);
    assert!(!created.outboxed && !delivery.outboxed && !batch[0].outboxed);
    assert!(!ledger.outbox_pending().unwrap(), "switch off: no outbox rows");
    assert!(!ledger.publish_lock_path().exists(), "and no publish lock");
    assert_eq!(ledger.get(&session, &[]).unwrap().state_revision, 3, "the ledger itself is unchanged");
    // A plain ledger (no outbox) never writes one either.
    let plain = InputLedger::open(root.join("plain.sqlite3"));
    let pin = pin_enabled(Some(true));
    let t = new_note(&plain, &session, "p1");
    drop(pin);
    assert!(!t.outboxed && !plain.outbox_pending().unwrap());
    let _ = std::fs::remove_dir_all(&root);
}

/// r274.2 §9.4 (owner decision 2026-11-03, no auto-redelivery): an owner
/// record a lost runtime left `sending` / `accepted`
/// (`stalled{runtimeDisconnected}`) offers `retry` and `dismiss` on its
/// nextUp row; a failed owner record offers both too. Other kinds and a
/// record whose stall cleared are unchanged.
#[test]
fn stuck_owner_records_offer_retry_and_dismiss() {
    let f = Fixture::new();
    let stalled = || Some(Stalled { reason: "runtimeDisconnected".into(), since: "2026-11-03T00:00:00.000Z".into() });
    f.go("a", Next::sending().surface(Surface::NextUp).create(owner(Strategy::Auto))).unwrap();
    let accepted = f.go("a", Next::accepted().turn("t1")).unwrap().record;
    assert!(accepted.actions.is_empty() && !owner_stuck(&accepted) && !dismissible(&accepted));
    let stuck = f.go("a", Next::accepted().stalled(stalled()).deadline(None)).unwrap().record;
    assert_eq!((stuck.surface, stuck.actions.clone()), (Surface::NextUp, vec![Action::Retry, Action::Dismiss]));
    assert!(owner_stuck(&stuck) && dismissible(&stuck));
    f.go("s", Next::sending().surface(Surface::NextUp).create(owner(Strategy::Auto))).unwrap();
    let sending = f.go("s", Next::sending().stalled(stalled())).unwrap().record;
    assert_eq!(sending.actions, vec![Action::Retry, Action::Dismiss]);
    // The runtime came back (§2.12 reconnect clears the stall): no actions.
    let back = f.go("a", Next::accepted().stalled(None)).unwrap().record;
    assert!(back.actions.is_empty() && !owner_stuck(&back));
    // The delivery refresh keeps them (same rule, `record_actions`).
    assert_eq!(record_actions(&stuck, Some(&json!({"state": "disconnected", "turn": null}))), vec![Action::Retry, Action::Dismiss]);
    // A note stalled the same way: unchanged.
    f.go("n", Next::sending().create(note(InputKind::AgentMessage, Timing::AfterTurn))).unwrap();
    let note_stalled = f.go("n", Next::sending().stalled(stalled())).unwrap().record;
    assert!(note_stalled.actions.is_empty() && !dismissible(&note_stalled));
    // Failed: the owner's is dismissible (retry, dismiss), a note's is not.
    let failed = f.go("s", Next::failed("notDelivered")).unwrap().record;
    assert_eq!(failed.actions, vec![Action::Retry, Action::Dismiss]);
    assert!(dismissible(&failed));
    let discarded = f.go("s", Next::discarded("ownerDiscarded")).unwrap().record;
    assert_eq!((discarded.state, discarded.surface), (InputState::Discarded, Surface::None));
}

#[test]
fn refresh_open_actions_fixes_stored_actions_and_writes_nothing_otherwise() {
    let f = Fixture::new();
    let delivery = json!({"state": "idle", "runtimeConnected": true, "turn": null});
    f.ledger.delivery_changed("s1", &delivery).unwrap();
    f.go("m", Next::received().create(owner(Strategy::Auto))).unwrap();
    f.go("m", Next::sending()).unwrap();
    f.go("m", Next::uncertain("outcomeUnknown")).unwrap();
    f.go("m", Next::waiting("ownerRetry")).unwrap();
    let marked = f.go("m", Next::waiting("ownerRetry").stalled(Some(Stalled { reason: "noProgress".into(), since: "2026-11-01T00:00:00Z".into() }))).unwrap().record;
    assert_eq!(marked.actions, vec![Action::Retry, Action::Dismiss]);
    let revision = f.ledger.list("s1", ListQuery::default()).unwrap().state_revision;
    // Nothing differs: no revision, no write.
    let same = f.ledger.refresh_open_actions("s1").unwrap();
    assert!(!same.changed);
    assert_eq!(f.ledger.list("s1", ListQuery::default()).unwrap().state_revision, revision);
    assert_eq!(f.ledger.get("s1", &["m".into()]).unwrap().records.remove(0).rev, marked.rev);
    // What r277 stored for that row.
    let db = Connection::open(f.ledger.path()).unwrap();
    let raw: String = db.query_row("SELECT record_json FROM input_ledger WHERE input_id='m'", [], |r| r.get(0)).unwrap();
    let mut stored: Value = serde_json::from_str(&raw).unwrap();
    stored["actions"] = json!([]);
    db.execute("UPDATE input_ledger SET record_json=?1 WHERE input_id='m'", [stored.to_string()]).unwrap();
    drop(db);
    assert!(f.ledger.get("s1", &["m".into()]).unwrap().records.remove(0).actions.is_empty());
    f.at(100);
    let fixed = f.ledger.refresh_open_actions("s1").unwrap();
    assert!(fixed.changed);
    assert_eq!(fixed.state_revision, revision + 1);
    assert_eq!(fixed.records.len(), 1);
    let row = f.ledger.get("s1", &["m".into()]).unwrap().records.remove(0);
    assert_eq!(row.actions, vec![Action::Retry, Action::Dismiss]);
    assert_eq!(row.rev, marked.rev + 1);
    // Not a new write to judge: a later restart page still sees this row as left over.
    assert_eq!(row.updated_at, marked.updated_at);
    assert_eq!(row.state_revision, revision + 1);
    // Done: a second pass does nothing. A session with no row or no delivery neither.
    assert!(!f.ledger.refresh_open_actions("s1").unwrap().changed);
    assert!(!f.ledger.refresh_open_actions("nobody").unwrap().changed);
}
