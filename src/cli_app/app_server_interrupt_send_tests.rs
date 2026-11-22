use super::*;
use std::cell::{Cell, RefCell};

use cutex::management::v2::input_ledger::{
    InputIntent, InputKind, InputSender, NewInput, SenderKind, Strategy, Surface, Timing, DATABASE_FILE,
};
use cutex::management::v2::user_input::{UserInputOrigin, UserInputOriginKind};

pub(super) const SESSION: &str = "cutex.11111111-2222-4333-8444-555555555555";
pub(super) const THREAD: &str = "thread-isend";

pub(super) struct Fake {
    pub(super) root: std::path::PathBuf,
    pub(super) ledger: InputLedger,
    pub(super) repository: UserInputRepository,
    pub(super) events: cutex::management::v2::repository::EventRepository,
    pub(super) sent_inputs: RefCell<Vec<Vec<Value>>>,
    pub(super) native_address: Option<std::net::SocketAddr>,
    pub(super) delivery: RefCell<Option<Value>>,
    pub(super) live: RefCell<Option<LiveTurn>>,
    pub(super) claude: bool,
    pub(super) batch_supported: bool,
    pub(super) batches: RefCell<Vec<Vec<Value>>>,
    /// Answers of `turn_has`, in call order (then `Absent`).
    pub(super) checks: RefCell<Vec<TurnCheck>>,
    pub(super) interrupt: RefCell<Result<(), InterruptFailure>>,
    pub(super) stop: RefCell<Result<(), InterruptFailure>>,
    pub(super) after_stop: RefCell<Option<LiveTurn>>,
    pub(super) reservation: Cell<Option<Reservation>>,
    pub(super) send: RefCell<Option<Result<UserInputSubmitExecution, UserInputExecutionError>>>,
    pub(super) calls: RefCell<Vec<String>>,
    pub(super) parked: RefCell<Vec<Value>>,
}

impl Fake {
    pub(super) fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-isend-{}", Uuid::new_v4().simple()));
        let ledger = InputLedger::open(root.join(DATABASE_FILE));
        let repository = UserInputRepository::open(root.join("user-input")).unwrap();
        let events = cutex::management::v2::repository::EventRepository::open(root.join("events"),"test").unwrap();
        Self {
            root,
            ledger,
            repository,
            events,
            sent_inputs: RefCell::new(Vec::new()),
            native_address: None,
            delivery: RefCell::new(Some(delivery("turn", "t1"))),
            live: RefCell::new(Some(LiveTurn { thread_id: THREAD.into(), generation: 7, active_turn: Some("t1".into()) })),
            claude: false,
            batch_supported: true,
            batches: RefCell::new(Vec::new()),
            checks: RefCell::new(Vec::new()),
            interrupt: RefCell::new(Ok(())),
            stop: RefCell::new(Ok(())),
            after_stop: RefCell::new(None),
            reservation: Cell::new(None),
            send: RefCell::new(None),
            calls: RefCell::new(Vec::new()),
            parked: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    pub(super) fn record(&self, id: &str) -> InputRecord {
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
    fn supports_owner_batch(&self, _: &str, _: u64) -> bool { self.batch_supported }
    fn send_batch(&self, command: &UserInputSubmitCommand, prefix: &[Value], _:u64) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
        self.batches.borrow_mut().push(prefix.to_vec());
        self.send_start(command)
    }

    fn verify_resend(&self, session: &str, id: &str, payload: &Value) -> Result<(), UserInputExecutionError> {
        cutex::management::v2::input_ledger_settle::verify_owner_resend_payload(&self.events,session,id,payload)
    }
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
        if let Some(address)=self.native_address {
            assert_eq!(loopback_native(address,"thread/items/list",json!({"turnId":turn}))["items"],json!([]));
            return TurnCheck::Absent;
        }
        let mut checks = self.checks.borrow_mut();
        if checks.is_empty() { TurnCheck::Absent } else { checks.remove(0) }
    }
    fn interrupt(&self, _: &str, _: &str, turn: &str) -> Result<(), InterruptFailure> {
        self.calls.borrow_mut().push(format!("interrupt {turn}"));
        if let Some(address)=self.native_address {loopback_native(address,"turn/interrupt",json!({"turnId":turn}));}
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
        } else if result.is_ok() {
            if let Some(next)=self.after_stop.borrow_mut().take() {*self.live.borrow_mut()=Some(next);}
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
        self.sent_inputs.borrow_mut().push(command.params.input.clone());
        if let Some(address)=self.native_address {loopback_native(address,"turn/start",json!({"input":command.params.input}));}
        self.send.borrow_mut().take().unwrap_or_else(|| Ok(UserInputSubmitExecution {
            join_refusal: None,
            queued_reason: None,
            disposition: UserInputDisposition::Started,
            app_server_accepted: true,
            native_request_id: Some(json!("n1")),
            native_method: Some("turn/start".into()),
            turn_id: Some("t2".into()),
            queue: None,
            queue_paused: false, passive_notes: None,
        }))
    }
    fn held_find(&self, session: &str, id: &str) -> anyhow::Result<Option<HeldOwnerInput>> {
        Ok(self.parked.borrow().iter().find(|p|p["clientUserMessageId"]==id).map(|params|
            HeldOwnerInput::new(session,id,"held-request",THREAD,params.clone(),self.claude,Utc::now())))
    }
    fn held_finish(&self, _: &str, id: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("held finish {id}"));
        self.parked.borrow_mut().retain(|p|p["clientUserMessageId"]!=id);
        Ok(())
    }
    fn held_park(&self, _: &str, _: &str, _: &str, params: Value) -> anyhow::Result<()> {
        let mut parked=self.parked.borrow_mut();
        if let Some(old)=parked.iter_mut().find(|p|p["clientUserMessageId"]==params["clientUserMessageId"]) {*old=params;}
        else {parked.push(params);}
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

pub(super) fn params(id: &str, strategy: UserInputStrategy) -> UserInputSubmitParams {
    UserInputSubmitParams {
                interrupt_send: None,
        join_delegation: None,
        client_user_message_id: id.into(),
        origin: UserInputOrigin { kind: UserInputOriginKind::Other, client_id: "desktop".into() },
        strategy,
        input: vec![json!({"type": "text", "text": id, "text_elements": []})],
        expected_turn_id: None,
    }
}

pub(super) fn new_owner() -> NewInput {
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
pub(super) fn steered(f: &Fake, id: &str) -> InputRecord {
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
pub(super) fn queued(f: &Fake, id: &str) -> InputRecord {
    let p = params(id, UserInputStrategy::Auto);
    f.repository.enqueue(SESSION, THREAD, "req-q", &p).unwrap();
    f.ledger.delivery_changed(SESSION, &delivery("turn", "t1")).unwrap();
    f.ledger.transition(SESSION, id, Expect::NEW, Next::received().create(new_owner())).unwrap();
    f.ledger.transition(SESSION, id, Expect::ANY, Next::waiting("turnRunning")).unwrap().record
}

#[test]
fn read_now_steer_can_interrupt_send_without_forwarding_private_selection() {
    let f = Fake::new();
    let record = steered(&f, "read-now-owner");
    let mut payload = f.ledger.payload(SESSION, &record.input_id).unwrap().unwrap();
    payload[cutex::management::v2::agent_bus_wake::READ_NOTES_FIELD] = json!(["note-a"]);
    f.ledger.put_payload(SESSION, &record.input_id, &payload).unwrap();
    let parsed = submit_params(&payload).expect("private selection is not a native submit parameter");
    assert_eq!(parsed.client_user_message_id, record.input_id);
    let result = run(&f, SESSION, &request(&record)).unwrap();
    assert!(f.calls().iter().any(|call| call == "turn/start read-now-owner"), "{result:?}");
    assert_eq!(f.sent_inputs.borrow().as_slice(), &[parsed.input]);
    assert_eq!(f.ledger.payload(SESSION, &record.input_id).unwrap().unwrap()[cutex::management::v2::agent_bus_wake::READ_NOTES_FIELD], json!(["note-a"]));
}

fn request(record: &InputRecord) -> InterruptSendRequest {
    InterruptSendRequest {
        input_id: record.input_id.clone(),
        expected_rev: record.rev,
        expected_turn_id: "t1".into(),
        expected_runtime_generation: 7,
    }
}

fn submitted(f: &Fake, id: &str, input: &[Value]) {
    f.events.append(PendingEvent {
        cutex_session_id: SESSION.into(), host_id:"test".into(), source:EventSource::Cutex,
        schema:None, correlation:EventCorrelation {client_user_message_id:Some(id.into()),..Default::default()},
        native:None, cutex:Some(CutexMessage {method:"cutex/userInput/submitted".into(),
            params:json!({"managementRequestId":"merged-test","clientUserMessageId":id,
                "origin":{"kind":"other","clientId":"desktop"},"input":input,
                "disposition":"steered","nativeRequestId":"fake-steer","nativeMethod":"turn/steer",
                "turnId":"t1","appServerAccepted":true,"submittedAt":Utc::now().to_rfc3339()})}),
    }).unwrap();
}

fn loopback_native(address:std::net::SocketAddr,method:&str,params:Value)->Value {
    use std::io::{BufRead,Write};
    let mut stream=std::net::TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    writeln!(stream,"{}",json!({"method":method,"params":params})).unwrap();
    let mut line=String::new();std::io::BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn merged_claude_steer_interrupt_resends_both_texts_and_three_images() {
    let _pin=il::pin_enabled(Some(true));
    let mut f=Fake::new();f.claude=true;
    let id=format!("merged-A-{}",Uuid::new_v4());
    let mut a=params(&id,UserInputStrategy::Queue);
    a.input=vec![json!({"type":"text","text":"A"})];
    let mut b=params("merged-B",UserInputStrategy::Queue);
    b.input=vec![json!({"type":"text","text":"B"}),
        json!({"type":"localImage","path":"D:/fixtures/one.png"}),
        json!({"type":"localImage","path":"D:/fixtures/two.png"}),
        json!({"type":"localImage","path":"D:/fixtures/three.png"})];
    let expected:Vec<Value>=a.input.iter().chain(b.input.iter()).cloned().collect();
    // A loopback-only fake Claude runtime registers a steer without reading
    // it, drops it on interrupt, then accepts the complete replacement turn.
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address=listener.local_addr().unwrap();f.native_address=Some(address);
    let runtime_expected=expected.clone();
    let runtime=std::thread::spawn(move|| {
        use std::io::{BufRead,Write};
        let mut unread=Vec::new();let mut starts=0;let mut interrupted=false;
        loop {
            let (mut stream,_)=listener.accept().unwrap();
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut line=String::new();std::io::BufReader::new(&mut stream).read_line(&mut line).unwrap();
            let request:Value=serde_json::from_str(&line).unwrap();
            let method=request["method"].as_str().unwrap();
            let response=match method {
                "turn/steer"=>{assert_eq!(request["params"]["input"],json!(runtime_expected));unread=runtime_expected.clone();json!({"registered":true})},
                "thread/items/list"=>json!({"items":[]}),
                "turn/interrupt"=>{assert_eq!(unread,runtime_expected);unread.clear();interrupted=true;json!({"stopped":true})},
                "turn/start"=>{assert!(interrupted);assert!(unread.is_empty());assert_eq!(request["params"]["input"],json!(runtime_expected));starts+=1;json!({"turnId":"t2"})},
                "finish"=>json!({"done":true}),
                other=>panic!("unexpected native method {other}"),
            };
            writeln!(stream,"{response}").unwrap();
            if method=="finish" {assert_eq!(starts,1);break;}
        }
    });
    let native_ledger=il::default_ledger().unwrap();
    native_ledger.put_payload(SESSION,&id,&cutex::management::v2::input_ledger_settle::owner_submit_payload(&a)).unwrap();
    f.repository.enqueue(SESSION,THREAD,"a",&a).unwrap();
    f.repository.enqueue(SESSION,THREAD,"b",&b).unwrap();
    let items=f.repository.list(SESSION).unwrap().1;
    let refs:Vec<_>=items.iter().map(|item|(item.queue_id.clone(),item.revision)).collect();
    let (merged,_)=f.repository.merge(SESSION,&refs,None).unwrap().unwrap();
    cutex::management::v2::input_ledger_settle::remember_merged_owner_input(&f.ledger,SESSION,&merged).unwrap();
    assert_eq!(f.ledger.payload(SESSION,&id).unwrap().unwrap()["input"],json!(expected));
    let mut command=UserInputSubmitCommand {management_request_id:"insert".into(),cutex_session_id:SESSION.into(),
        thread_id:THREAD.into(),params:a};
    command.params.strategy=UserInputStrategy::Auto;command.params.input=merged.input.clone();
    let native=|_:&UserInputSubmitCommand,method:&str,body:Value,_:UserInputDisposition| {
        assert_eq!(method,"turn/steer");assert_eq!(body["input"],json!(expected));
        assert_eq!(loopback_native(address,method,body)["registered"],true);
        Ok(UserInputSubmitExecution {join_refusal:None,queued_reason:None,disposition:UserInputDisposition::Steered,
            app_server_accepted:true,native_request_id:Some(json!("steer")),native_method:Some(method.into()),
            turn_id:Some("t1".into()),queue:None,queue_paused: false, passive_notes: None,})
    };
    send_recorded(&f.repository,&native,&command,"turn/steer",
        json!({"threadId":THREAD,"input":command.params.input,"clientUserMessageId":id}),UserInputDisposition::Steered).unwrap();
    // The sender must have replaced the stale A-only copy, independently
    // of the merge helper's write to our separate fixture ledger.
    let payload=native_ledger.payload(SESSION,&id).unwrap().unwrap();
    assert_eq!(payload["input"],json!(expected));
    f.ledger.put_payload(SESSION,&id,&payload).unwrap();
    f.repository.remove(SESSION,&merged.queue_id,merged.revision).unwrap().unwrap();
    submitted(&f,&id,&expected);
    f.ledger.delivery_changed(SESSION,&delivery("turn","t1")).unwrap();
    f.ledger.transition(SESSION,&id,Expect::NEW,Next::received().create(new_owner())).unwrap();
    f.ledger.transition(SESSION,&id,Expect::ANY,Next::sending()).unwrap();
    let accepted=f.ledger.transition(SESSION,&id,Expect::ANY,Next::accepted().stage(Stage::Registered).turn("t1")).unwrap().record;
    assert_eq!(run(&f,SESSION,&request(&accepted)).unwrap(),Outcome::Interrupted {turn_id:"t1".into(),started:true});
    assert_eq!(*f.sent_inputs.borrow(),vec![expected]);
    loopback_native(address,"finish",json!({}));runtime.join().unwrap();
}

#[test]
fn stale_steer_payload_is_refused_before_interrupt_or_resend() {
    let f=Fake::new();let record=steered(&f,"m1");
    let complete=vec![json!({"type":"text","text":"m1"}),json!({"type":"text","text":"B"}),
        json!({"type":"localImage","path":"D:/fixtures/one.png"})];
    submitted(&f,"m1",&complete);
    let error=run(&f,SESSION,&request(&record)).unwrap_err();
    assert_eq!(error.code,"retry_payload_mismatch");
    assert_eq!(error.details["nothingSent"],true);
    assert_eq!(f.calls(),vec!["check t1"]);assert!(f.sent_inputs.borrow().is_empty());
    assert_eq!(f.record("m1"),record);
    assert!(f.record("m1").actions.contains(&cutex::management::v2::input_ledger::Action::Dismiss));
    assert!(error.message.contains("Nothing was sent") && error.message.contains("original input is unchanged"));
}

#[test]
fn already_read_steer_settles_even_when_saved_payload_is_stale_or_missing() {
    for missing in [false,true] {
        let f=Fake::new();let record=steered(&f,"m1");
        submitted(&f,"m1",&[json!({"type":"text","text":"the complete edited input"})]);
        if missing {
            rusqlite::Connection::open(f.ledger.path()).unwrap().execute(
                "DELETE FROM input_ledger_payloads WHERE session=?1 AND input_id='m1'",[SESSION]).unwrap();
        }
        f.checks.borrow_mut().push(TurnCheck::Found);
        assert_eq!(run(&f,SESSION,&request(&record)).unwrap(),Outcome::AlreadyInContext);
        assert_eq!(f.calls(),vec!["check t1"]);
        assert_eq!(f.record("m1").state,InputState::InContext);
        assert!(f.sent_inputs.borrow().is_empty());
    }
}

#[test]
fn random_mixed_queue_merges_preserve_every_part_once_when_sent() {
    // Reproducible generated inputs, with unique part identities. Compare
    // the native turn/start input against the independently collected parts.
    let mut random=0x91d7_42ab_u64;
    for count in 2..=20 {
        let mut f=Fake::new();f.claude=true;
        let mut expected=Vec::new();
        for index in 0..count {
            let mut p=params(&format!("item-{index}"),UserInputStrategy::Queue);
            random=random.wrapping_mul(6364136223846793005).wrapping_add(1);
            p.input=vec![json!({"type":"text","text":format!("part-{count}-{index}-text")})];
            for image in 0..((random>>32)%4) {
                p.input.push(json!({"type":"localImage","path":format!("D:/fixtures/{count}-{index}-{image}.png")}));
            }
            expected.extend(p.input.clone());
            f.repository.enqueue(SESSION,THREAD,&format!("req-{index}"),&p).unwrap();
        }
        let items=f.repository.list(SESSION).unwrap().1;
        let refs:Vec<_>=items.iter().map(|item|(item.queue_id.clone(),item.revision)).collect();
        let (merged,removed)=f.repository.merge(SESSION,&refs,None).unwrap().unwrap();
        assert_eq!(removed.len(),count-1);
        assert_eq!(merged.input,expected);
        cutex::management::v2::input_ledger_settle::remember_merged_owner_input(&f.ledger,SESSION,&merged).unwrap();
        f.ledger.delivery_changed(SESSION,&delivery("turn","t1")).unwrap();
        f.ledger.transition(SESSION,"item-0",Expect::NEW,Next::received().create(new_owner())).unwrap();
        let record=f.ledger.transition(SESSION,"item-0",Expect::ANY,Next::waiting("turnRunning")).unwrap().record;
        run(&f,SESSION,&request(&record)).unwrap();
        assert_eq!(*f.sent_inputs.borrow(),vec![expected.clone()],"{count} merged items");
        for part in &expected {assert_eq!(f.sent_inputs.borrow()[0].iter().filter(|sent|*sent==part).count(),1);}
        assert!(f.repository.list(SESSION).unwrap().1.is_empty());
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
