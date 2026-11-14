use super::*;
use std::cell::{Cell, RefCell};

use cutex::management::v2::input_ledger::{
    InputIntent, InputKind, InputSender, NewInput, SenderKind, Strategy, Surface, Timing, DATABASE_FILE,
};
use cutex::management::v2::user_input::{UserInputOrigin, UserInputOriginKind};

const SESSION: &str = "cutex.11111111-2222-4333-8444-555555555555";
const THREAD: &str = "thread-isend";

struct Fake {
    root: std::path::PathBuf,
    ledger: InputLedger,
    repository: UserInputRepository,
    delivery: RefCell<Option<Value>>,
    live: RefCell<Option<LiveTurn>>,
    claude: bool,
    /// Answers of `turn_has`, in call order (then `Absent`).
    checks: RefCell<Vec<TurnCheck>>,
    interrupt: RefCell<Result<(), InterruptFailure>>,
    stop: RefCell<Result<(), InterruptFailure>>,
    reservation: Cell<Option<Reservation>>,
    send: RefCell<Option<Result<UserInputSubmitExecution, UserInputExecutionError>>>,
    calls: RefCell<Vec<String>>,
    parked: RefCell<Vec<Value>>,
}

impl Fake {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-isend-{}", Uuid::new_v4().simple()));
        let ledger = InputLedger::open(root.join(DATABASE_FILE));
        let repository = UserInputRepository::open(root.join("user-input")).unwrap();
        Self {
            root,
            ledger,
            repository,
            delivery: RefCell::new(Some(delivery("turn", "t1"))),
            live: RefCell::new(Some(LiveTurn { thread_id: THREAD.into(), generation: 7, active_turn: Some("t1".into()) })),
            claude: false,
            checks: RefCell::new(Vec::new()),
            interrupt: RefCell::new(Ok(())),
            stop: RefCell::new(Ok(())),
            reservation: Cell::new(None),
            send: RefCell::new(None),
            calls: RefCell::new(Vec::new()),
            parked: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn record(&self, id: &str) -> InputRecord {
        self.ledger.get(SESSION, &[id.to_string()]).unwrap().records.pop().unwrap()
    }

    fn path(&self, id: &str) -> (InputState, Option<String>, Surface) {
        let record = self.record(id);
        (record.state, record.reason, record.surface)
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Ops for Fake {
    fn ledger(&self) -> anyhow::Result<&InputLedger> {
        Ok(&self.ledger)
    }
    fn repository(&self) -> &UserInputRepository {
        &self.repository
    }
    fn delivery(&self, _: &str) -> Option<Value> {
        self.delivery.borrow().clone()
    }
    fn live(&self, _: &str) -> Option<LiveTurn> {
        self.live.borrow().clone()
    }
    fn claude(&self, _: &str) -> bool {
        self.claude
    }
    fn turn_has(&self, _: &str, _: &str, turn: &str, _: &str) -> TurnCheck {
        self.calls.borrow_mut().push(format!("check {turn}"));
        let mut checks = self.checks.borrow_mut();
        if checks.is_empty() { TurnCheck::Absent } else { checks.remove(0) }
    }
    fn interrupt(&self, _: &str, _: &str, turn: &str) -> Result<(), InterruptFailure> {
        self.calls.borrow_mut().push(format!("interrupt {turn}"));
        let result = self.interrupt.borrow().clone();
        if result.is_ok() {
            *self.live.borrow_mut() = Some(LiveTurn { thread_id: THREAD.into(), generation: 7, active_turn: None });
        }
        result
    }
    fn wait_stopped(&self, _: &str, turn: &str) -> Result<(), InterruptFailure> {
        self.calls.borrow_mut().push(format!("wait {turn}"));
        let result = self.stop.borrow().clone();
        if matches!(result, Err(InterruptFailure::Offline(_))) {
            *self.live.borrow_mut() = None;
        }
        result
    }
    fn reserve(&self, session: &str, id: &str, expected: Option<&InputDelivery>, sending: InputDelivery) -> anyhow::Result<Reservation> {
        self.calls.borrow_mut().push("reserve".into());
        if let Some(forced) = self.reservation.get() {
            return Ok(forced);
        }
        Ok(if self.repository.set_delivery(session, id, expected, Some(sending))? { Reservation::Reserved } else { Reservation::Changed })
    }
    fn send_start(&self, command: &UserInputSubmitCommand) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
        self.calls.borrow_mut().push(format!("turn/start {}", command.params.client_user_message_id));
        self.send.borrow_mut().take().unwrap_or_else(|| Ok(UserInputSubmitExecution {
            join_refusal: None,
            queued_reason: None,
            disposition: UserInputDisposition::Started,
            app_server_accepted: true,
            native_request_id: Some(json!("n1")),
            native_method: Some("turn/start".into()),
            turn_id: Some("t2".into()),
            queue: None,
            queue_paused: false,
        }))
    }
    fn held_find(&self, _: &str, _: &str) -> anyhow::Result<Option<HeldOwnerInput>> {
        Ok(None)
    }
    fn held_finish(&self, _: &str, id: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("held finish {id}"));
        Ok(())
    }
    fn held_park(&self, _: &str, _: &str, _: &str, params: Value) -> anyhow::Result<()> {
        self.parked.borrow_mut().push(params);
        Ok(())
    }
    fn publish(&self, _: &il::Transition) {}
    fn publish_delivery(&self, _: &str, _: &il::DeliveryChange) {}
    fn append_event(&self, _: &str, _: EventCorrelation, method: &str, _: Value) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("event {method}"));
        Ok(())
    }
}

fn delivery(state: &str, turn: &str) -> Value {
    json!({
        "state": state, "reason": null,
        "turn": {"turnId": turn, "kind": "owner", "askId": null, "startedAt": null, "inputIds": []},
        "lease": null, "runtimeConnected": true, "runtimeGeneration": 7,
        "heldCount": 0, "heldUntil": null, "stateRevision": 0, "observedAt": "2026-11-03T00:00:00.000Z",
    })
}

fn params(id: &str, strategy: UserInputStrategy) -> UserInputSubmitParams {
    UserInputSubmitParams {
        join_delegation: None,
        client_user_message_id: id.into(),
        origin: UserInputOrigin { kind: UserInputOriginKind::Other, client_id: "desktop".into() },
        strategy,
        input: vec![json!({"type": "text", "text": id, "text_elements": []})],
        expected_turn_id: None,
    }
}

fn new_owner() -> NewInput {
    NewInput {
        kind: InputKind::Owner,
        sender: InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: Some(Strategy::Auto), delivery_mode: None, timing: Timing::NextStep },
        preview: "owner".into(),
        ask_id: None,
        remote: None,
    }
}

/// An owner input steered into t1 (identity, C-010 receipt, retry payload
/// and the `accepted` record, as the submit route leaves them).
fn steered(f: &Fake, id: &str) -> InputRecord {
    let p = params(id, UserInputStrategy::Auto);
    f.repository.register_identity(SESSION, &p).unwrap();
    f.repository.set_delivery(SESSION, id, None, Some(InputDelivery::Accepted {
        thread_id: THREAD.into(), turn_id: Some("t1".into()), accepted_at: Utc::now().to_rfc3339(),
    })).unwrap();
    f.ledger.put_payload(SESSION, id, &json!({
        "clientUserMessageId": id, "origin": p.origin, "strategy": "auto", "input": p.input, "allowHeld": true,
    })).unwrap();
    f.ledger.delivery_changed(SESSION, &delivery("turn", "t1")).unwrap();
    f.ledger.transition(SESSION, id, Expect::NEW, Next::received().create(new_owner())).unwrap();
    f.ledger.transition(SESSION, id, Expect::ANY, Next::sending()).unwrap();
    f.ledger.transition(SESSION, id, Expect::ANY, Next::accepted().turn("t1")).unwrap().record
}

/// An owner input queued behind t1 (`waiting{turnRunning}`).
fn queued(f: &Fake, id: &str) -> InputRecord {
    let p = params(id, UserInputStrategy::Auto);
    f.repository.enqueue(SESSION, THREAD, "req-q", &p).unwrap();
    f.ledger.delivery_changed(SESSION, &delivery("turn", "t1")).unwrap();
    f.ledger.transition(SESSION, id, Expect::NEW, Next::received().create(new_owner())).unwrap();
    f.ledger.transition(SESSION, id, Expect::ANY, Next::waiting("turnRunning")).unwrap().record
}

fn request(record: &InputRecord) -> InterruptSendRequest {
    InterruptSendRequest {
        input_id: record.input_id.clone(),
        expected_rev: record.rev,
        expected_turn_id: "t1".into(),
        expected_runtime_generation: 7,
    }
}

#[test]
fn cas_conflicts_have_no_side_effect() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    let revision = f.ledger.get(SESSION, &["m1".into()]).unwrap().state_revision;
    let mut stale = request(&record);
    stale.expected_rev -= 1;
    assert_eq!(run(&f, SESSION, &stale).unwrap_err().code, "revision_conflict");
    let mut other_turn = request(&record);
    other_turn.expected_turn_id = "t0".into();
    assert_eq!(run(&f, SESSION, &other_turn).unwrap_err().code, "revision_conflict");
    let mut other_generation = request(&record);
    other_generation.expected_runtime_generation = 6;
    assert_eq!(run(&f, SESSION, &other_generation).unwrap_err().code, "revision_conflict");
    // The connected runtime already moved on (the delivery is a beat late).
    *f.live.borrow_mut() = Some(LiveTurn { thread_id: THREAD.into(), generation: 7, active_turn: Some("t2".into()) });
    let late = run(&f, SESSION, &request(&record)).unwrap_err();
    assert_eq!(late.code, "revision_conflict");
    assert_eq!(late.details["currentRev"], record.rev);
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    assert_eq!(f.record("m1"), record);
    assert_eq!(f.ledger.get(SESSION, &["m1".into()]).unwrap().state_revision, revision);
}

#[test]
fn not_interruptible_has_no_side_effect() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    *f.delivery.borrow_mut() = Some(json!({"state": "idle", "reason": null, "turn": null, "runtimeGeneration": 7}));
    let error = run(&f, SESSION, &request(&record)).unwrap_err();
    assert_eq!(error.code, "input_not_interruptible");
    assert_eq!(error.details, json!({"state": "accepted", "reason": null}));
    assert!(f.calls().is_empty());
    assert_eq!(f.record("m1"), record);
}

#[test]
fn a_steer_already_in_context_is_not_interrupted_or_resent() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    f.checks.borrow_mut().push(TurnCheck::Found);
    assert_eq!(run(&f, SESSION, &request(&record)).unwrap(), Outcome::AlreadyInContext);
    assert_eq!(f.calls(), ["check t1"]);
    let done = f.record("m1");
    assert_eq!((done.state, done.evidence), (InputState::InContext, Some(Evidence::HistoryRead)));
}

#[test]
fn a_claude_steer_offered_to_the_model_is_in_context() {
    let mut f = Fake::new();
    f.claude = true;
    steered(&f, "m1");
    let offered = f.ledger.transition(SESSION, "m1", Expect::ANY, Next::accepted().stage(Stage::Offered)).unwrap().record;
    // r274.2: the agent has it; interruptSend is neither offered nor legal.
    assert!(!offered.actions.contains(&cutex::management::v2::input_ledger::Action::InterruptSend), "{:?}", offered.actions);
    let error = run(&f, SESSION, &request(&offered)).unwrap_err();
    assert_eq!(error.code, "input_not_interruptible");
    assert!(f.calls().is_empty(), "no native request at all: {:?}", f.calls());
    assert_eq!(f.record("m1"), offered, "no side effect");
}

#[test]
fn an_unread_steer_is_interrupted_and_sent_once_as_the_next_turn() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    let revision = f.ledger.get(SESSION, &["m1".into()]).unwrap().state_revision;
    let outcome = run(&f, SESSION, &request(&record)).unwrap();
    assert_eq!(outcome, Outcome::Interrupted { turn_id: "t1".into(), started: true });
    assert_eq!(f.calls(), ["check t1", "interrupt t1", "wait t1", "check t1", "reserve", "turn/start m1", "event cutex/userInput/submitted"]);
    let done = f.record("m1");
    assert_eq!((done.state, done.evidence, done.turn_id.as_deref()), (InputState::InContext, Some(Evidence::TurnStartInput), Some("t2")));
    assert_eq!(done.surface, Surface::Timeline);
    // received, sending, accepted, then ONE revision for sending{interruptSend}
    // + delivery finishing, then inContext.
    assert_eq!(done.rev, record.rev + 2);
    let list = f.ledger.list(SESSION, il::ListQuery::default()).unwrap();
    assert_eq!(list.state_revision, revision + 2);
    assert_eq!(list.delivery.unwrap()["state"], "finishing");
    // C-010: accepted into the new turn (a retry / flush finds it delivered).
    assert!(matches!(f.repository.delivery(SESSION, "m1").unwrap(),
        Some(InputDelivery::Accepted { turn_id: Some(turn), .. }) if turn == "t2"));
}

#[test]
fn a_steer_the_stopped_turn_read_is_not_resent() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    f.checks.borrow_mut().extend([TurnCheck::Absent, TurnCheck::Found]);
    assert_eq!(run(&f, SESSION, &request(&record)).unwrap(), Outcome::Interrupted { turn_id: "t1".into(), started: false });
    assert!(!f.calls().iter().any(|call| call.starts_with("turn/start")), "{:?}", f.calls());
    let done = f.record("m1");
    assert_eq!((done.state, done.evidence, done.turn_id.as_deref()), (InputState::InContext, Some(Evidence::HistoryRead), Some("t1")));
}

#[test]
fn an_unprovable_steer_is_never_resent_blindly() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    f.checks.borrow_mut().extend([TurnCheck::Absent, TurnCheck::Unavailable]);
    run(&f, SESSION, &request(&record)).unwrap();
    assert!(!f.calls().iter().any(|call| call.starts_with("turn/start")));
    assert_eq!(f.path("m1"), (InputState::Uncertain, Some("turnInterrupted".into()), Surface::Timeline));
    assert!(matches!(f.repository.delivery(SESSION, "m1").unwrap(), Some(InputDelivery::Unknown { .. })));
}

#[test]
fn a_queued_input_goes_first_and_leaves_the_queue() {
    let f = Fake::new();
    let record = queued(&f, "q1");
    assert_eq!(record.actions, vec![cutex::management::v2::input_ledger::Action::Dismiss, cutex::management::v2::input_ledger::Action::InterruptSend]);
    let outcome = run(&f, SESSION, &request(&record)).unwrap();
    assert_eq!(outcome, Outcome::Interrupted { turn_id: "t1".into(), started: true });
    // Never in the runtime: no item check.
    assert_eq!(f.calls(), ["interrupt t1", "wait t1", "reserve", "turn/start q1", "event cutex/userInput/submitted", "event cutex/userInput/queueRemoved"]);
    assert!(f.repository.list(SESSION).unwrap().1.is_empty());
    assert_eq!(f.path("q1"), (InputState::InContext, None, Surface::Timeline));
}

#[test]
fn a_busy_host_fence_holds_it_for_the_admission_worker() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    f.reservation.set(Some(Reservation::Busy));
    assert_eq!(run(&f, SESSION, &request(&record)).unwrap(), Outcome::Held { turn_id: Some("t1".into()) });
    assert!(!f.calls().iter().any(|call| call.starts_with("turn/start")));
    let held = f.record("m1");
    assert_eq!((held.state, held.reason.as_deref()), (InputState::Held, Some("hostBusy")));
    assert_eq!(held.details["interruptSend"]["interruptedTurnId"], "t1");
    let parked = f.parked.borrow();
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0]["strategy"], "interrupt");
    assert_eq!(parked[0]["clientUserMessageId"], "m1");
    // The admission must send it: the steer's receipt is gone (proven absent).
    assert_eq!(f.repository.delivery(SESSION, "m1").unwrap(), None);
}

#[test]
fn an_offline_runtime_queues_it_first_and_starts_the_agent() {
    let f = Fake::new();
    let earlier = queued(&f, "q0");
    let _ = earlier;
    let record = steered(&f, "m1");
    *f.stop.borrow_mut() = Err(InterruptFailure::Offline("gone".into()));
    let outcome = run(&f, SESSION, &request(&record)).unwrap();
    assert_eq!(outcome, Outcome::AgentOffline { turn_id: Some("t1".into()) });
    assert!(!f.calls().iter().any(|call| call.starts_with("turn/start")));
    assert_eq!(f.path("m1"), (InputState::Waiting, Some("agentOffline".into()), Surface::Timeline));
    let (_, items) = f.repository.list(SESSION).unwrap();
    let ids: Vec<&str> = items.iter().map(|item| item.client_user_message_id.as_str()).collect();
    assert_eq!(ids, ["m1", "q0"], "the interrupted owner input goes first");
    assert!(items[0].bypass_pause);
    // Not proven absent: the flush reads the history once before sending.
    assert!(matches!(f.repository.delivery(SESSION, "m1").unwrap(),
        Some(InputDelivery::Accepted { turn_id: Some(turn), .. }) if turn == "interruptSend:t1"));
}

#[test]
fn a_refused_interrupt_fails_the_record() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    *f.interrupt.borrow_mut() = Err(InterruptFailure::Refused("no".into()));
    run(&f, SESSION, &request(&record)).unwrap();
    assert_eq!(f.path("m1"), (InputState::Failed, Some("runtimeRefused".into()), Surface::Timeline));
    // r274.2 §9.4: an owner failed record can be sent again or discarded.
    assert_eq!(f.record("m1").actions, vec![il::Action::Retry, il::Action::Dismiss]);
    // The turn still runs with the steer in it: its receipt stays.
    assert!(matches!(f.repository.delivery(SESSION, "m1").unwrap(), Some(InputDelivery::Accepted { .. })));
}

#[test]
fn an_interrupt_that_never_stops_the_turn_times_out() {
    let f = Fake::new();
    let record = queued(&f, "q1");
    *f.stop.borrow_mut() = Err(InterruptFailure::Timeout("10 s".into()));
    run(&f, SESSION, &request(&record)).unwrap();
    assert_eq!(f.path("q1"), (InputState::Failed, Some("interrupt_timeout".into()), Surface::Timeline));
    assert!(f.repository.list(SESSION).unwrap().1.is_empty(), "failed: the retry resends it");
}

/// The real path through `send_start` is in `tests/input_ledger_e2e.rs`
/// (`stuck_interrupt_send`): here, the proof reaches the ledger before the send.
#[test]
fn a_carried_steer_proven_absent_goes_out_without_its_carries() {
    let f = Fake::new();
    let record = steered(&f, "m1");
    let mut carrying = Next::accepted().turn("t1");
    carrying.carries = Some(vec!["note".into()]);
    let record = f.ledger.transition(SESSION, "m1", Expect::rev(record.rev), carrying).unwrap().record;
    assert_eq!(run(&f, SESSION, &request(&record)).unwrap(), Outcome::Interrupted { turn_id: "t1".into(), started: true });
    let done = f.record("m1");
    assert_eq!(done.state, InputState::InContext);
    assert!(done.carries.is_empty(), "{:?}", done.carries);
    assert_eq!(f.calls().iter().filter(|call| call.starts_with("turn/start")).count(), 1);
}
