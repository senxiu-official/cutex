//! Optional offline rehearsal against private copies; never opens live stores.
use super::*;
use cutex::asks::scheduler::{AskRuntime, SchedulerWorld, Submission, TargetStatus, NativeMessageStatus, SubmitOutcome, RespondOutcome, TickReport};
use cutex::asks::store::{AskStore, AskError};
use cutex::management::v2::input_ledger::{InputLedger, InputState};

const SESSION: &str = "cutex.bd203899-83dc-42aa-9b8a-7f44d39bf30b";
const OWNER: &str = "a68d8dd8-c848-4422-9ba8-a3912334f5b0";
const EVENT: &str = "aev-3811448c0a7840b7b3ae5f50d7104d8e";
const NATIVE: &str = "cutex-ask-owner-42441186895e4839865fd2859744f493";

struct OfflineWorld;
impl SchedulerWorld for OfflineWorld {
    fn directory(&self) -> Result<cutex::asks::directory::Directory,AskError> { Err(AskError::new("offline_fixture","no admission")) }
    fn gate_inputs(&self,_:Option<&str>,_:&str) -> Result<cutex::asks::gate::GateInputs,AskError> { Err(AskError::new("offline_fixture","no admission")) }
    fn approvals(&self) -> Option<&cutex::approvals::ApprovalStore> { None }
}
struct OfflineRuntime { adapter: std::path::PathBuf, expected: Envelope }
impl AskRuntime for OfflineRuntime {
    fn target_status(&self,session:&str)->TargetStatus {
        TargetStatus { runtime_generation:if session==SESSION {36} else {0},..Default::default() }
    }
    fn submit_exclusive(&self,_:&str,_:u64,_:&Submission)->SubmitOutcome { panic!("offline dry-run must never submit") }
    fn message_status(&self,session:&str,generation:u64,submission:&Submission)->NativeMessageStatus {
        if session!=SESSION || submission.message_id!=NATIVE {return NativeMessageStatus::Unavailable("offline fixture".into());}
        assert_eq!(generation,31,"must query the original occurrence");
        let expected=submission.envelope(&cutex::launch::stock::ExternalInputBinding { version:1,owner_id:session.into(),
            thread_id:self.expected.thread_id.clone(),runtime_generation:generation,canonical_byte_limit:Default::default() },true).unwrap();
        assert_eq!(expected,self.expected,"daemon source must reconstruct the exact envelope");
        NativeMessageStatus::Persisted {turn_id:persisted_turn(&self.adapter,&expected,Some(OWNER)).unwrap().unwrap()}
    }
    fn interrupt_keep_gate(&self,_:&str,_:&str)->anyhow::Result<()> {panic!("offline fixture")}
    fn resume_gate(&self,_:&str)->anyhow::Result<()> {panic!("offline fixture")}
    fn read_turn(&self,_:&str,_:&str)->Option<Value>{None}
    fn respond_question(&self,_:&str,_:u64,_:&str,_:&Value,_:&Value)->RespondOutcome{panic!("offline fixture")}
}

#[test]
fn copied_alice_exact_event_check_and_offline_scheduler_dry_run() {
    let Ok(adapter)=std::env::var("CUTEX_TEST_OWNER_NOTE_COPY") else{return;};
    let copied_root=Path::new(&adapter).parent().unwrap().parent().unwrap();
    let root=std::env::temp_dir().join(format!("alice-carry-dry-run-{}",uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(copied_root.join("management-state.sqlite3"),root.join("management-state.sqlite3")).unwrap();
    std::fs::copy(copied_root.join("input-ledger.sqlite3"),root.join("input-ledger.sqlite3")).unwrap();
    let db=Connection::open_with_flags(&adapter,OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let raw:String=db.query_row("SELECT envelope FROM external_inputs WHERE message_id=?1",[NATIVE],|r|r.get(0)).unwrap();
    let envelope:Envelope=serde_json::from_str(&raw).unwrap();
    let store=AskStore::at(&root);
    let before=store.snapshot().unwrap();
    let event=before.events[EVENT].clone();
    let ids=event.delivery_ids.clone();
    assert_eq!(ids.len(),2);
    let lease=before.leases.get(SESSION).or_else(||before.next_step_leases.get(SESSION)).unwrap();
    assert_eq!(lease.runtime_generation,31);
    assert_eq!(lease.native_message_id,NATIVE);
    let ledger=InputLedger::open(root.join("input-ledger.sqlite3"));
    let records=ledger.get(SESSION,&ids).unwrap().records;
    for record in &records {assert_eq!(record.rev,10);assert_eq!(record.state,InputState::Accepted);}
    // Rehydrate only a missing fixture blob from the independently verified
    // adapter text. Its content address MUST equal the frozen source artifact.
    let artifact=store.put_artifact(envelope.message.text.as_bytes(),"text/plain; charset=utf-8",&[],None,chrono::Utc::now()).unwrap();
    assert_eq!(Some(artifact),event.event_artifact);
    let runtime=OfflineRuntime {adapter:adapter.into(),expected:envelope};
    cutex::asks::returns::admit_events(&OfflineWorld,&runtime,&store,&mut TickReport::default(),chrono::Utc::now()).unwrap();
    let after=store.snapshot().unwrap();
    assert!(after.events[EVENT].acknowledged);
    assert!(!after.leases.contains_key(SESSION));
    assert!(!after.next_step_leases.contains_key(SESSION));
    let mut restored=after.clone();
    restored.revision=before.revision;
    restored.events.insert(EVENT.into(),event.clone());
    restored.leases=before.leases.clone();restored.next_step_leases=before.next_step_leases.clone();
    for id in &ids {restored.deliveries.insert(id.clone(),before.deliveries[id].clone());}
    assert_eq!(restored,before,"no unrelated source row or answer text changes");
    for record in records {
        let job=cutex::asks::ledger::drift_job(&after,&record).unwrap();
        cutex::asks::ledger::apply_job(&ledger,&job);
        let settled=ledger.get(SESSION,&[record.input_id]).unwrap().records.pop().unwrap();
        assert_eq!(settled.state,InputState::InContext);
        assert_eq!(settled.turn_id.as_deref(),Some("217f7b75-7fbb-4928-8a1a-60846ac06924"));
        assert_eq!(settled.timeline_item_id.as_deref(),Some(NATIVE));
        assert_eq!(settled.preview,record.preview);
        assert!(cutex::asks::ledger::apply_job(&ledger,&job).is_empty());
    }
    let stable=store.snapshot().unwrap();
    cutex::asks::returns::admit_events(&OfflineWorld,&runtime,&store,&mut TickReport::default(),chrono::Utc::now()).unwrap();
    assert_eq!(store.snapshot().unwrap(),stable,"repeated reconciliation is a no-op");
    eprintln!("offline check/dry-run: exact Alice event, original generation31, two accepted rev10 → inContext at original native item; unrelated source rows/text preserved; no live writes");
    drop(ledger);drop(store);drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
