// Private adapter/store/ledger fixtures: no provider, socket or live home.
use super::*;
use super::tests::Fixture;
use serde_json::json;

struct SteerStartWorld;
impl cutex::asks::scheduler::SchedulerWorld for SteerStartWorld {
    fn directory(&self)->Result<cutex::asks::directory::Directory,cutex::asks::store::AskError> {
        Err(cutex::asks::store::AskError::new("fixture","no new admission"))
    }
    fn gate_inputs(&self,_:Option<&str>,_:&str)->Result<cutex::asks::gate::GateInputs,cutex::asks::store::AskError> {
        Err(cutex::asks::store::AskError::new("fixture","no new admission"))
    }
    fn approvals(&self)->Option<&cutex::approvals::ApprovalStore>{None}
}
struct SteerStartRuntime<'a> { fixture: &'a Fixture }
impl cutex::asks::scheduler::AskRuntime for SteerStartRuntime<'_> {
    fn target_status(&self,_:&str)->cutex::asks::scheduler::TargetStatus {
        cutex::asks::scheduler::TargetStatus { online:true,runtime_generation:31,
            active_turn_id:Some("carrier-turn".into()),..Default::default() }
    }
    fn submit_exclusive(&self,_:&str,_:u64,_:&cutex::asks::scheduler::Submission)->cutex::asks::scheduler::SubmitOutcome {panic!("no resend")}
    fn message_status(&self,session:&str,generation:u64,submission:&cutex::asks::scheduler::Submission)->cutex::asks::scheduler::NativeMessageStatus {
        assert_eq!(session,"cutex.private");assert_eq!(generation,31);
        let expected=super::super::owner_note_envelope(session,"thread-original",generation,submission).unwrap();
        assert_eq!(expected,self.fixture.envelope,"the frozen wire envelope and ask ids survive batching");
        match persisted_turn(&self.fixture.path,&expected,Some("carrier")).unwrap() {
            Some(turn_id)=>cutex::asks::scheduler::NativeMessageStatus::Persisted{turn_id},
            None=>cutex::asks::scheduler::NativeMessageStatus::Pending,
        }
    }
    fn interrupt_keep_gate(&self,_:&str,_:&str)->anyhow::Result<()>{panic!("no interrupt")}
    fn resume_gate(&self,_:&str)->anyhow::Result<()>{panic!("no resume")}
    fn read_turn(&self,_:&str,_:&str)->Option<Value>{panic!("no terminal history fallback")}
    fn respond_question(&self,_:&str,_:u64,_:&str,_:&Value,_:cutex::asks::scheduler::QuestionResponse<'_>)->cutex::asks::scheduler::RespondOutcome {panic!("no question")}
}

#[test]
fn owner_message_and_result_clear_next_up_on_context_before_steer_start_turn_ends() {
    use cutex::asks::{model::*,store::{AskStore,NewAsk}};
    use cutex::management::v2::input_ledger::{InputLedger,InputState,Expect,Next,Surface};
    let root=std::env::temp_dir().join(format!("steer-start-result-{}",uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store=AskStore::at(&root);let now=chrono::Utc::now();
    let (ask_id,delivery_id)=store.with_state(|state| {
        let (ask,_)=state.create_ask(NewAsk {
            ask_id:None,action_id:"steer-result".into(),
            requester:Requester::Agent{cutex_session_id:"cutex.private".into(),name:None},
            target_cutex_session_id:"cutex.handler".into(),target_name:None,project_id:None,
            body:AskBody{text:"work".into(),text_artifact:None,text_bytes:4,text_sha256:cutex::asks::sha256_hex(b"work"),attachments:vec![]},
            title:None,options:Default::default(),follow_up_of:None,parent_ask:None,gate:None,
        },now).unwrap();
        state.update_ask(&ask.ask_id,now,|a| {
            a.execution=ExecutionState::Answered;a.closed=Some(CloseOutcome::Completed);a.answer_id=Some("answer-ready".into());
        });
        state.answers.insert("answer-ready".into(),AnswerRecord {
            answer_id:"answer-ready".into(),ask_id:ask.ask_id.clone(),attempt:1,revision:1,status:AnswerStatus::Completed,
            artifact_id:None,bytes:0,metadata:Default::default(),created_at:cutex::asks::now_rfc3339(now),preview:Some("ready".into()),
        });
        let ask=state.asks[&ask.ask_id].clone();
        let delivery=cutex::asks::delivery::record_answer_delivery(state,&ask,"answer-ready",now).unwrap();
        (ask.ask_id.clone(),delivery)
    }).unwrap();
    struct ReleaseFence;
    impl Drop for ReleaseFence {fn drop(&mut self){cutex::asks::pause_admission::release_worker_fence();}}
    cutex::asks::pause_admission::acquire_worker_fence().unwrap();
    let fence=ReleaseFence;
    let bundle=cutex::asks::owner_carries::reserve_owner_results(&store,"cutex.private","carrier",31,
        cutex::app_server::external_input::size::ReceiverLimit::Off,
        cutex::config::documents::DocumentSettings::default(),now).unwrap().unwrap();
    drop(fence);
    assert_eq!(bundle.submission.asks,vec![ask_id.clone()]);
    assert_eq!(bundle.submission.view.as_ref().unwrap()["data"]["items"][0]["askId"],ask_id);
    let envelope=super::super::owner_note_envelope("cutex.private","thread-original",31,&bundle.submission).unwrap();
    let fixture=Fixture::with_envelope(envelope);fixture.steer_start();
    // The real native envelope path has no legacy metadata.asks. An image
    // and text owner input still bind the exact carried result by envelope.
    fixture.change("UPDATE inputs SET payload=json_set(payload,'$.input',json('[{\"type\":\"text\",\"text\":\"owner message\"},{\"type\":\"image\",\"url\":\"fixture-image\"}]'));
        DELETE FROM items WHERE id='model-proof';");
    store.with_state(|s| {s.leases.insert("cutex.private".into(),AdmissionLease {
        cutex_session_id:"cutex.private".into(),holder:LeaseHolder::Owner,native_message_id:"owner-native".into(),
        runtime_generation:31,turn_id:Some("carrier-turn".into()),since:cutex::asks::now_rfc3339(now),
    });}).unwrap();
    let runtime=SteerStartRuntime{fixture:&fixture};
    let queued=store.snapshot().unwrap();
    let ledger=InputLedger::open(root.join("fixture-ledger.sqlite3"));
    let job=cutex::asks::ledger::delivery_job(&queued,&queued,None,&queued.deliveries[&delivery_id]).unwrap();
    cutex::asks::ledger::apply_job(&ledger,&job);
    for next in [Next::sending(),Next::accepted()] {ledger.transition("cutex.private",&delivery_id,Expect::ANY,next).unwrap();}
    let mut report=cutex::asks::scheduler::TickReport::default();
    cutex::asks::returns::admit_events(&SteerStartWorld,&runtime,&store,&mut report,now).unwrap();
    assert!(!store.snapshot().unwrap().events[&bundle.event_id].acknowledged,"staging is not context");
    fixture.change("INSERT INTO items(turn_id,id,payload) VALUES('carrier-turn','model-request',json('{\"type\":\"mcpToolCall\",\"id\":\"model-request\"}'));");
    cutex::asks::returns::admit_events(&SteerStartWorld,&runtime,&store,&mut report,now).unwrap();
    let after=store.snapshot().unwrap();
    assert!(after.events[&bundle.event_id].acknowledged);
    assert_eq!(after.deliveries[&delivery_id].state,DeliveryState::Acknowledged);
    assert!(!after.next_step_leases.contains_key("cutex.private"));
    assert_eq!(after.leases["cutex.private"],queued.leases["cutex.private"],"the active owner turn keeps its lease");
    let accepted=ledger.get("cutex.private",&[delivery_id.clone()]).unwrap().records.pop().unwrap();
    assert_eq!(accepted.state,InputState::Accepted);
    let job=cutex::asks::ledger::drift_job(&after,&accepted).unwrap();
    cutex::asks::ledger::apply_job(&ledger,&job);
    let read=ledger.get("cutex.private",&[delivery_id.clone()]).unwrap().records.pop().unwrap();
    assert_eq!(read.state,InputState::InContext);assert_eq!(read.surface,Surface::Timeline);
    assert_eq!(read.timeline_item_id.as_deref(),Some(bundle.submission.message_id.as_str()));
    let listed=ledger.list("cutex.private",Default::default()).unwrap();
    assert!(!listed.records.iter().any(|row|row.input_id==delivery_id && row.surface==Surface::NextUp),
        "the public input list used by the desktop no longer places this result on Next Up");
    assert!(report.events.is_empty(),"confirmation never submits another result turn");
    let db=Connection::open_with_flags(&fixture.path,OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(db.query_row("SELECT state FROM turns WHERE id='carrier-turn'",[],|r|r.get::<_,String>(0)).unwrap(),"inProgress");
    assert!(db.query_row("SELECT receipt IS NULL FROM external_inputs",[],|r|r.get::<_,bool>(0)).unwrap(),"no terminal receipt is needed");
    drop(db);drop(ledger);drop(store);std::fs::remove_dir_all(root).unwrap();
}
