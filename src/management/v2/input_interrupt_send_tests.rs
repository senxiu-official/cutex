use super::*;
use super::super::input_ledger::{
    Action, Evidence, Expect, InputIntent, InputKind, InputLedger, InputSender, InputState, LedgerError, NewInput,
    Next, SenderKind, Stage, Strategy, Surface, Timing, DATABASE_FILE,
};

struct Fixture {
    root: std::path::PathBuf,
    ledger: InputLedger,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-interrupt-send-{}", uuid::Uuid::new_v4().simple()));
        let ledger = InputLedger::open(root.join(DATABASE_FILE));
        Self { root, ledger }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn owner(strategy: Strategy) -> NewInput {
    NewInput {
        kind: InputKind::Owner,
        sender: InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: Some(strategy), delivery_mode: None, timing: Timing::NextStep },
        preview: "owner".into(),
        ask_id: None,
        remote: None,
    }
}

fn bus() -> NewInput {
    NewInput {
        kind: InputKind::AgentMessage,
        sender: InputSender { kind: SenderKind::Agent, id: "a".into(), name: "a".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: None, delivery_mode: None, timing: Timing::AfterTurn },
        preview: "bus".into(),
        ask_id: None,
        remote: None,
    }
}

fn delivery(state: &str, turn: Option<&str>, generation: u64) -> Value {
    json!({
        "state": state,
        "reason": if state == "held" { json!("hostBusy") } else { Value::Null },
        "turn": turn.map(|turn| json!({"turnId": turn, "kind": "owner", "askId": null, "startedAt": null, "inputIds": []})),
        "lease": null, "runtimeConnected": true, "runtimeGeneration": generation,
        "heldCount": 0, "heldUntil": null, "stateRevision": 0, "observedAt": "2026-11-03T00:00:00.000Z",
    })
}

fn request(id: &str, rev: u64, turn: &str, generation: u64) -> InterruptSendRequest {
    InterruptSendRequest { input_id: id.into(), expected_rev: rev, expected_turn_id: turn.into(), expected_runtime_generation: generation }
}

/// An owner steer `accepted` into turn `t1` (rev 3).
fn steered(f: &Fixture, id: &str) -> InputRecord {
    f.ledger.transition("s1", id, Expect::NEW, Next::received().create(owner(Strategy::Auto))).unwrap();
    f.ledger.transition("s1", id, Expect::ANY, Next::sending()).unwrap();
    f.ledger.transition("s1", id, Expect::ANY, Next::accepted().turn("t1")).unwrap().record
}

#[test]
fn params_are_exact() {
    let ok = parse("s1", &json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "t1", "expectedRuntimeGeneration": 7})).unwrap();
    assert_eq!(ok, request("m1", 4, "t1", 7));
    assert!(parse("s1", &json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "t1", "expectedRuntimeGeneration": 0, "cutexSessionId": "s1"})).is_ok());
    for bad in [
        json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "t1"}),
        json!({"inputId": "m1", "expectedRev": 4, "expectedRuntimeGeneration": 7}),
        json!({"inputId": "m1", "expectedTurnId": "t1", "expectedRuntimeGeneration": 7}),
        json!({"expectedRev": 4, "expectedTurnId": "t1", "expectedRuntimeGeneration": 7}),
        json!({"inputId": "m1", "expectedRev": 0, "expectedTurnId": "t1", "expectedRuntimeGeneration": 7}),
        json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "", "expectedRuntimeGeneration": 7}),
        json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "t1", "expectedRuntimeGeneration": -1}),
        json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "t1", "expectedRuntimeGeneration": 7, "force": true}),
        json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "t1", "expectedRuntimeGeneration": 7, "cutexSessionId": "s2"}),
    ] {
        assert_eq!(parse("s1", &bad).unwrap_err().code, "invalid_request", "{bad}");
    }
}

#[test]
fn legality_follows_the_contract() {
    use InputState::*;
    let turn = delivery("turn", Some("t1"), 7);
    let finishing = delivery("finishing", Some("t1"), 7);
    let held = delivery("held", Some("t1"), 7);
    let idle = delivery("idle", None, 7);
    let owner = InputKind::Owner;
    let legal = |kind, state, reason: Option<&str>, turn_id: Option<&str>, delivery: &Value| {
        il::interrupt_send_legal(kind, state, reason, turn_id, false, Some(delivery))
    };
    // accepted into the running turn only.
    assert!(legal(owner, Accepted, None, Some("t1"), &turn));
    assert!(legal(owner, Accepted, None, Some("t1"), &finishing));
    assert!(!legal(owner, Accepted, None, Some("t0"), &turn));
    assert!(!legal(owner, Accepted, None, None, &turn));
    // held (any reason) while a turn runs: §2.6 reports held{hostBusy}.
    assert!(legal(owner, Held, Some("hostBusy"), None, &held));
    assert!(legal(owner, Held, Some("hostBusy"), None, &turn));
    // waiting behind the running turn only.
    for reason in ["turnRunning", "turnFinishing"] {
        assert!(legal(owner, Waiting, Some(reason), None, &turn), "{reason}");
    }
    for reason in ["explicitQueue", "agentOffline", "paused", "ownerQueueAhead", "hostBusy"] {
        assert!(!legal(owner, Waiting, Some(reason), None, &turn), "{reason}");
    }
    // no running turn, other states, other kinds.
    assert!(!legal(owner, Waiting, Some("turnRunning"), None, &idle));
    assert!(!il::interrupt_send_legal(owner, Waiting, Some("turnRunning"), None, false, None));
    let mut paused = delivery("held", Some("t1"), 7);
    paused["reason"] = json!("paused");
    assert!(!legal(owner, Waiting, Some("turnRunning"), None, &paused));
    for state in [Received, Sending, InContext, Uncertain, Failed, Discarded] {
        assert!(!legal(owner, state, None, Some("t1"), &turn), "{state}");
    }
    assert!(!legal(InputKind::AgentMessage, Accepted, None, Some("t1"), &turn));
    assert!(!legal(InputKind::AgentMessage, Waiting, Some("turnRunning"), None, &turn));
    // actions = the base actions plus interruptSend.
    assert_eq!(il::actions_with_delivery(owner, Waiting, Some("turnRunning"), None, false, Some(&turn)), vec![Action::Dismiss, Action::InterruptSend]);
    assert_eq!(il::actions_with_delivery(owner, Waiting, Some("explicitQueue"), None, false, Some(&turn)),
        vec![Action::Cancel, Action::SendNow, Action::Edit, Action::Dismiss]);
    assert_eq!(il::actions_with_delivery(owner, Accepted, None, Some("t1"), false, Some(&idle)), vec![Action::Dismiss]);
}

#[test]
fn precheck_order_and_details() {
    let f = Fixture::new();
    let record = steered(&f, "m1");
    let turn = delivery("turn", Some("t1"), 7);
    let rev = record.rev;
    assert!(precheck(Some(&record), Some(&turn), &request("m1", rev, "t1", 7)).is_ok());
    let missing = precheck(None, Some(&turn), &request("m1", rev, "t1", 7)).unwrap_err();
    assert_eq!((missing.code.as_str(), missing.details["inputId"].as_str()), ("input_not_found", Some("m1")));
    let stale = precheck(Some(&record), Some(&turn), &request("m1", rev - 1, "t1", 7)).unwrap_err();
    assert_eq!(stale.code, "revision_conflict");
    assert_eq!(stale.details, json!({"currentRev": rev, "currentTurnId": "t1", "currentRuntimeGeneration": 7}));
    let other_turn = precheck(Some(&record), Some(&turn), &request("m1", rev, "t0", 7)).unwrap_err();
    assert_eq!(other_turn.code, "revision_conflict");
    let other_generation = precheck(Some(&record), Some(&turn), &request("m1", rev, "t1", 6)).unwrap_err();
    assert_eq!(other_generation.code, "revision_conflict");
    let idle = precheck(Some(&record), Some(&delivery("idle", None, 7)), &request("m1", rev, "t1", 7)).unwrap_err();
    assert_eq!(idle.code, "input_not_interruptible");
    assert_eq!(idle.details, json!({"state": "accepted", "reason": null}));
    let offline = precheck(Some(&record), None, &request("m1", rev, "t1", 7)).unwrap_err();
    assert_eq!(offline.code, "input_not_interruptible");
    // A steer of another turn is not interruptible (the running one is t2).
    let moved = precheck(Some(&record), Some(&delivery("turn", Some("t2"), 7)), &request("m1", rev, "t2", 7)).unwrap_err();
    assert_eq!(moved.code, "input_not_interruptible");
    // Not an owner input.
    f.ledger.transition("s1", "b1", Expect::NEW, Next::waiting("turnRunning").create(bus())).unwrap();
    let bus = f.ledger.get("s1", &["b1".into()]).unwrap().records.pop().unwrap();
    let refused = precheck(Some(&bus), Some(&turn), &request("b1", bus.rev, "t1", 7)).unwrap_err();
    assert_eq!(refused.code, "input_not_interruptible");
    assert_eq!(refused.details, json!({"state": "waiting", "reason": "turnRunning"}));
}

#[test]
fn ledger_edges_and_reason_for_interrupt_send() {
    let f = Fixture::new();
    let record = steered(&f, "m1");
    // A plain `sending` still needs no reason; another reason is invalid.
    assert!(matches!(
        f.ledger.transition("s1", "m1", Expect::ANY, Next::sending().reason("bogus")),
        Err(LedgerError::ForbiddenEdge { .. } | LedgerError::InvalidReason { .. })
    ));
    // accepted → sending only as interruptSend.
    assert!(matches!(f.ledger.transition("s1", "m1", Expect::ANY, Next::sending()), Err(LedgerError::ForbiddenEdge { .. })));
    let sending = f.ledger
        .transition("s1", "m1", Expect::rev(record.rev), Next::sending().reason(SENDING_REASON).details(json!({"interruptedTurnId": "t1"})))
        .unwrap();
    assert_eq!((sending.record.state, sending.record.reason.as_deref()), (InputState::Sending, Some("interruptSend")));
    assert_eq!(sending.record.details["interruptedTurnId"], "t1");
    assert_eq!(sending.record.surface, Surface::Timeline);
    // sending → held{hostBusy} only when marked as the interruptSend's.
    assert!(matches!(f.ledger.transition("s1", "m1", Expect::ANY, Next::held("hostBusy")), Err(LedgerError::ForbiddenEdge { .. })));
    let held = f.ledger
        .transition("s1", "m1", Expect::ANY, Next::held("hostBusy").details(json!({"interruptSend": {"interruptedTurnId": "t1"}})))
        .unwrap();
    assert_eq!(held.record.state, InputState::Held);
    // The admission resumes it as sending{interruptSend}, then inContext.
    let resumed = super::super::input_ledger_owner::interrupt_send_resumed(&held.record).unwrap();
    let again = f.ledger.transition("s1", "m1", Expect::ANY, resumed).unwrap();
    assert_eq!(again.record.reason.as_deref(), Some("interruptSend"));
    let done = f.ledger.transition("s1", "m1", Expect::ANY, Next::in_context(Evidence::TurnStartInput).turn("t2")).unwrap();
    assert_eq!((done.record.surface, done.record.turn_id.as_deref()), (Surface::Timeline, Some("t2")));
    // Not for other kinds.
    f.ledger.transition("s1", "b1", Expect::NEW, Next::waiting("afterTurn").create(bus())).unwrap();
    f.ledger.transition("s1", "b1", Expect::ANY, Next::sending()).unwrap();
    f.ledger.transition("s1", "b1", Expect::ANY, Next::accepted().turn("t1")).unwrap();
    assert!(matches!(
        f.ledger.transition("s1", "b1", Expect::ANY, Next::sending().reason(SENDING_REASON)),
        Err(LedgerError::ForbiddenEdge { .. })
    ));
}

#[test]
fn the_first_transition_and_the_delivery_share_one_revision() {
    let f = Fixture::new();
    f.ledger.delivery_changed("s1", &delivery("turn", Some("t1"), 7)).unwrap();
    let record = steered(&f, "m1");
    assert_eq!(record.actions, vec![Action::Dismiss, Action::InterruptSend], "computed from the stored delivery");
    // Another owner input queued behind the turn.
    let queued = f.ledger.transition("s1", "q1", Expect::NEW, Next::waiting("turnRunning").create(owner(Strategy::Auto))).unwrap();
    assert_eq!(queued.record.actions, vec![Action::Dismiss, Action::InterruptSend]);
    let before = f.ledger.get("s1", &["m1".into()]).unwrap().state_revision;
    let (transitions, change) = f.ledger
        .transition_batch_with_delivery(
            "s1",
            vec![("m1".into(), Expect::rev(record.rev), Next::sending().reason(SENDING_REASON).details(json!({"interruptedTurnId": "t1"})))],
            &finishing(&delivery("turn", Some("t1"), 7)),
        )
        .unwrap();
    assert!(change.changed);
    assert_eq!(change.delivery["state"], "finishing");
    assert_eq!(change.state_revision, before + 1);
    assert_eq!(transitions[0].state_revision, before + 1);
    assert!(transitions[0].record.actions.is_empty(), "sending offers nothing");
    // The queued input stays interruptible under `finishing`: unchanged.
    assert!(change.records.is_empty());
    // A CAS conflict changes nothing (no revision, no delivery).
    let conflict = f.ledger.transition_batch_with_delivery(
        "s1", vec![("m1".into(), Expect::rev(record.rev), Next::in_context(Evidence::HistoryRead))], &delivery("idle", None, 7));
    assert!(matches!(conflict, Err(LedgerError::Conflict { .. })));
    let after = f.ledger.list("s1", il::ListQuery::default()).unwrap();
    assert_eq!(after.state_revision, before + 1);
    assert_eq!(after.delivery.unwrap()["state"], "finishing");
}

#[test]
fn a_delivery_change_refreshes_the_actions_at_the_same_revision() {
    let f = Fixture::new();
    f.ledger.delivery_changed("s1", &delivery("turn", Some("t1"), 7)).unwrap();
    let record = steered(&f, "m1");
    assert_eq!(record.actions, vec![Action::Dismiss, Action::InterruptSend]);
    // The turn ended: the steer is no longer interruptible.
    let change = f.ledger.delivery_changed("s1", &delivery("idle", None, 7)).unwrap();
    assert_eq!(change.records.len(), 1);
    let refreshed = &change.records[0];
    assert_eq!((refreshed.input_id.as_str(), refreshed.rev, refreshed.state_revision), ("m1", record.rev + 1, change.state_revision));
    assert_eq!(refreshed.actions, vec![Action::Dismiss]);
    assert_eq!(refreshed.state, InputState::Accepted);
    let stored = f.ledger.get("s1", &["m1".into()]).unwrap().records.pop().unwrap();
    assert_eq!(&stored, refreshed);
    // Unchanged actions: no record event.
    let again = f.ledger.delivery_changed("s1", &delivery("turn", Some("t9"), 7)).unwrap();
    assert!(again.changed && again.records.is_empty());
}

#[test]
fn claude_offered_stage_counts_as_read() {
    // The runtime half treats a Claude steer at stage `offered` as already
    // in context (the hook gave it to the model); the ledger allows that.
    let f = Fixture::new();
    steered(&f, "m1");
    let offered = f.ledger.transition("s1", "m1", Expect::ANY, Next::accepted().stage(Stage::Offered)).unwrap();
    let done = f.ledger.transition("s1", "m1", Expect::rev(offered.record.rev), Next::in_context(Evidence::HistoryRead)).unwrap();
    assert_eq!(done.record.state, InputState::InContext);
}

#[test]
fn switch_off_and_non_owner_are_refused_without_side_effects() {
    let pin = il::pin_enabled(Some(false));
    let params = json!({"inputId": "m1", "expectedRev": 1, "expectedTurnId": "t1", "expectedRuntimeGeneration": 1});
    let mut called = false;
    let denied = dispatch(true, "s1", &json!({}), &params, || called = true).unwrap_err();
    assert_eq!(denied.code, "cutex_method_denied");
    pin.set(Some(true));
    let refused = dispatch(false, "s1", &json!({}), &params, || called = true).unwrap_err();
    assert_eq!(refused.code, "owner_required");
    drop(pin);
    assert!(!called);
}

#[test]
fn outcomes_on_the_wire() {
    assert_eq!(Outcome::Interrupted { turn_id: "t1".into(), started: true }.wire(), "interrupted");
    assert_eq!(Outcome::AlreadyInContext.wire(), "alreadyInContext");
    assert_eq!(Outcome::AlreadyInContext.interrupted_turn(), None);
    assert_eq!(Outcome::Held { turn_id: Some("t1".into()) }.interrupted_turn(), Some("t1"));
    assert_eq!(Outcome::AgentOffline { turn_id: None }.wire(), "agentOffline");
}

#[test]
fn request_and_result_match_the_methods_schema() {
    use super::super::contract_validation::{validate_cutex_request, validate_cutex_response};
    let params = json!({"inputId": "m1", "expectedRev": 4, "expectedTurnId": "t1", "expectedRuntimeGeneration": 7});
    validate_cutex_request(&json!({"requestId": "r1", "method": METHOD, "params": params})).unwrap();
    let f = Fixture::new();
    let record = steered(&f, "m1");
    let result = json!({"ledgerEpoch": "le-1", "stateRevision": 3, "outcome": "interrupted", "interruptedTurnId": "t1", "record": record});
    validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r1", "cutexSessionId": "s1", "cutex": {"method": METHOD, "result": result}})).unwrap();
    let bad = json!({"ledgerEpoch": "le-1", "stateRevision": 3, "outcome": "sent", "interruptedTurnId": null, "record": record});
    assert!(validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r1", "cutexSessionId": "s1", "cutex": {"method": METHOD, "result": bad}})).is_err());
}
