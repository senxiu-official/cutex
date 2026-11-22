//! Bounded best-effort passive admission before a cutex-started turn. Notes
//! never gate the trigger. Unknown native outcomes retain their exact identity.
use anyhow::{ensure, Context};
use serde::{Serialize, Deserialize};
use serde_json::json;
use crate::launch::stock::ExternalInputBinding;
use super::external_input::{Delivery, DeliveryState, Envelope, ExternalInputClient, ProcessingState, Status};
use crate::management::v2::agent_bus_state::{agent_bus_message_repository, AgentBusMessageRepository};

pub const MAX_NOTES: usize = crate::management::v2::input_ledger::MAX_BATCH_ITEMS - 1;
pub const MAX_TRANSPORT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct Budget { pub notes: usize, pub bytes: usize }
impl Default for Budget { fn default() -> Self { Self { notes:MAX_NOTES, bytes:MAX_TRANSPORT_BYTES } } }
impl Budget {
    pub fn after_carried(count:usize, bytes:usize) -> Self {
        Self { notes:MAX_NOTES.saturating_sub(count), bytes:MAX_TRANSPORT_BYTES.saturating_sub(bytes) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct SkippedNote { pub input_id:String, pub waiting_for:String, pub error:Option<String> }
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct Report {
    pub admitted:Vec<String>, pub skipped:Vec<SkippedNote>, pub errors:Vec<String>,
    pub budget_notes:usize, pub budget_bytes:usize,
}
impl Report {
    pub fn is_empty(&self) -> bool { self.admitted.is_empty() && self.skipped.is_empty() && self.errors.is_empty() }
    pub fn merge(&mut self, other:Self) {
        self.budget_notes=self.budget_notes.saturating_add(other.budget_notes);
        self.budget_bytes=self.budget_bytes.saturating_add(other.budget_bytes);
        for id in other.admitted { self.skipped.retain(|n|n.input_id!=id); if !self.admitted.contains(&id) {self.admitted.push(id);} }
        for note in other.skipped { if !self.admitted.contains(&note.input_id) {self.skipped.retain(|n|n.input_id!=note.input_id);self.skipped.push(note);} }
        self.errors.extend(other.errors);
    }
    pub fn skip(&mut self, repository:&AgentBusMessageRepository, owner:&str, id:&str, waiting:&str, error:Option<String>) {
        if let Err(e)=repository.record_note_attempt_kind(owner,id,"preTurnAdmission",error.as_deref(),waiting) {self.errors.push(format!("{id}: {e:#}"));}
        self.skipped.push(SkippedNote {input_id:id.into(),waiting_for:waiting.into(),error});
    }
    pub fn unavailable(repository:&AgentBusMessageRepository, owner:&str, ids:&[String], error:String) -> Self {
        let mut report=Self::default();
        for id in ids {report.skip(repository,owner,id,"runtimeCapabilityOrBinding",Some(error.clone()));}
        report
    }
    /// The trigger's existing record exposes the same skipped identities as
    /// its response. No new state, reason, action or native receipt is invented.
    pub fn attach_to_input(&self, session:&str, id:&str) {
        if self.is_empty() || !crate::management::v2::input_ledger::enabled() {return;}
        use crate::management::v2::input_ledger as il;
        let result=(||->anyhow::Result<()> {
            for record in il::default_ledger()?.get(session,&[id.to_owned()])?.records {
                self.attach_record(record,|session,id,expect,next| {il::record(session,id,expect,next);})?;
            }
            Ok(())
        })();
        if let Err(e)=result {eprintln!("passive-note trigger diagnostics {session}/{id}: {e:#}");}
    }

    fn attach_record(&self, record:crate::management::v2::input_ledger::InputRecord,
        write:impl FnOnce(&str,&str,crate::management::v2::input_ledger::Expect,crate::management::v2::input_ledger::Next)) -> serde_json::Result<()> {
        use crate::management::v2::input_ledger::{Expect,Next};
        let mut details=record.details.clone();details["passiveNotes"]=serde_json::to_value(self)?;
        let mut next=Next::to(record.state).details(details);next.reason=record.reason;next.stage=record.stage;
        // Diagnostic enrichment may lose a race. Never retry its stale state
        // or metadata over the native progress which won that revision.
        write(&record.cutex_session_id,&record.input_id,Expect::rev(record.rev),next);
        Ok(())
    }
}

#[derive(Default)]
pub struct ProjectionOrderFence {
    senders: std::collections::HashMap<String, String>,
    asks: std::collections::HashMap<String, String>,
}

impl ProjectionOrderFence {
    fn keys(message: &crate::agent_bus::model::AgentBusMessage) -> (String, Option<String>) {
        let sender = if let Some(session) = &message.from_cutex_session_id { format!("session:{session}") }
        else if crate::agent_bus::peer_tell::is_peer_tell(message) {
            message.control_payload.clone().and_then(|v| serde_json::from_value::<crate::agent_bus::peer_tell::PeerTellFacts>(v).ok())
                .map(|p| format!("peer:{}:{}", p.peer_id, p.sender_id)).unwrap_or_else(|| format!("source:{}", message.from))
        } else { format!("source:{}", message.from) };
        let ask = message.control_payload.as_ref().and_then(|p| p.get("askId"))
            .and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        (sender, ask)
    }

    pub fn failed(&mut self, message: &crate::agent_bus::model::AgentBusMessage) {
        let (sender, ask) = Self::keys(message);
        self.senders.entry(sender).or_insert_with(|| message.id.clone());
        if let Some(ask) = ask { self.asks.entry(ask).or_insert_with(|| message.id.clone()); }
    }

    pub fn blocker(&mut self, message: &crate::agent_bus::model::AgentBusMessage) -> Option<String> {
        let (sender, ask) = Self::keys(message);
        let blocker = self.senders.get(&sender).or_else(|| ask.as_ref().and_then(|ask| self.asks.get(ask))).cloned();
        // Preserve both lanes when sender and ask ordering intersect.
        if let Some(id) = &blocker {
            self.senders.entry(sender).or_insert_with(|| id.clone());
            if let Some(ask) = ask { self.asks.entry(ask).or_insert_with(|| id.clone()); }
        }
        blocker
    }

    pub fn blocked(&mut self, message: &crate::agent_bus::model::AgentBusMessage) -> bool {
        self.blocker(message).is_some()
    }
}

type Handler = fn(&str, &str, Option<u64>) -> Report;
static HANDLER: std::sync::OnceLock<Handler> = std::sync::OnceLock::new();

pub fn install(handler: Handler) { let _ = HANDLER.set(handler); }

/// Legacy bridges cannot manufacture native acceptance. The installed CLI
/// handler resolves the exact live connection, or reports why notes remain.
pub fn before_start(session: &str, thread: &str, generation: Option<u64>) -> Report {
    if let Some(handler)=HANDLER.get() {return handler(session,thread,generation);}
    match agent_bus_message_repository().and_then(|r|Ok((r,r.unsubmitted_note_ids(session)?))) {
        Ok((r,ids)) if !ids.is_empty()=>Report::unavailable(r,session,&ids,"passive-note admission transport unavailable".into()),
        Ok(_)=>Report::default(), Err(e)=>Report {errors:vec![format!("{e:#}")],..Default::default()},
    }
}

pub fn with_client(client: &ExternalInputClient) -> Report { with_client_budget(client,Budget::default()) }
pub fn with_client_budget(client: &ExternalInputClient, budget:Budget) -> Report {
    let repository=match agent_bus_message_repository() {Ok(r)=>r,Err(e)=>return Report {errors:vec![format!("{e:#}")],..Default::default()}};
    if !client.supports_structured_view() {
        return match repository.unsubmitted_note_ids(&client.binding().owner_id) {
            Ok(ids)=>Report::unavailable(repository,&client.binding().owner_id,&ids,"runtime cannot accept structured passive notes".into()),
            Err(e)=>Report {errors:vec![format!("{e:#}")],..Default::default()},
        };
    }
    synchronize_budget(repository, client.binding(), budget, |envelope| {
        client.submit(envelope)?.statuses.into_iter().next().context("passive note acceptance missing")
    })
}

pub fn synchronize(
    repository: &AgentBusMessageRepository, binding: &ExternalInputBinding,
    submit: impl FnMut(&Envelope) -> anyhow::Result<Status>,
) -> Report { synchronize_budget(repository,binding,Budget::default(),submit) }

pub fn synchronize_budget(
    repository:&AgentBusMessageRepository, binding:&ExternalInputBinding, budget:Budget,
    submit:impl FnMut(&Envelope)->anyhow::Result<Status>,
) -> Report {
    synchronize_with_freeze(repository,binding,budget,submit,|repository,owner,id,envelope|
        repository.freeze_external_input(owner,id,|_|Ok(envelope)))
}

fn synchronize_with_freeze(
    repository:&AgentBusMessageRepository, binding:&ExternalInputBinding, budget:Budget,
    mut submit:impl FnMut(&Envelope)->anyhow::Result<Status>,
    mut freeze:impl FnMut(&AgentBusMessageRepository,&str,&str,Envelope)->anyhow::Result<Envelope>,
) -> Report {
    // This is the cutoff. Newly queued notes wait for the next input boundary.
    let ids = match repository.unsubmitted_note_ids(&binding.owner_id) {Ok(ids)=>ids,Err(e)=>return Report {errors:vec![format!("{e:#}")],..Default::default()}};
    let mut report=Report::default();let mut count=0usize;let mut bytes=0usize;let mut fence=ProjectionOrderFence::default();
    for id in ids {
        if count>=budget.notes.min(MAX_NOTES) || bytes>=budget.bytes.min(MAX_TRANSPORT_BYTES) {
            report.skip(repository,&binding.owner_id,&id,"nextBoundaryBudget",None);continue;
        }
        count+=1;
        let message=match repository.canonical_message(&id) {Ok(m)=>m,Err(e)=>{report.skip(repository,&binding.owner_id,&id,"projection",Some(format!("{e:#}")));continue;}};
        if let Some(blocker)=fence.blocker(&message) {
            if let Err(e)=repository.record_projection_blocked(&binding.owner_id,&id,&blocker) {report.errors.push(format!("{e:#}"));}
            report.skip(repository,&binding.owner_id,&id,"earlierInput",Some(format!("waiting for {blocker}")));continue;
        }
        let prepared=(||->anyhow::Result<Option<Envelope>> {
            if crate::management::v2::input_disposal::requested(&binding.owner_id,&id)? {return Ok(None);}
            let meta=repository.message_meta(&id)?;
            let envelope=super::bus_bridge::owner_carry_envelope(&message,binding,meta.as_ref(),None)?;
            ensure!(envelope.owner_id==binding.owner_id && envelope.thread_id==binding.thread_id
                && envelope.runtime_generation==binding.runtime_generation && envelope.message.delivery==Delivery::Passive,"passive note binding or delivery changed");
            Ok(Some(envelope))
        })();
        let envelope = match prepared {
            Ok(None)=>continue,
            Ok(Some(envelope))=>envelope,
            Err(error)=>{
                fence.failed(&message);
                let detail=format!("{error:#}");
                if let Err(e)=repository.record_projection_failure(&binding.owner_id,&id,Some(&detail)) {report.errors.push(format!("{e:#}"));}
                report.skip(repository,&binding.owner_id,&id,"projection",Some(detail));continue;
            }
        };
        let size=match serde_json::to_vec(&envelope) {Ok(v)=>v.len(),Err(e)=>{report.skip(repository,&binding.owner_id,&id,"projection",Some(e.to_string()));continue;}};
        if size>budget.bytes.min(MAX_TRANSPORT_BYTES).saturating_sub(bytes) {
            bytes=budget.bytes.min(MAX_TRANSPORT_BYTES);report.skip(repository,&binding.owner_id,&id,"nextBoundaryBudget",None);continue;
        }
        bytes+=size;
        // This original projection is now valid. Do not let a former FIFO
        // blocker/error survive successful admission or mask reconciliation.
        if let Err(e)=repository.record_projection_failure(&binding.owner_id,&id,None) {report.errors.push(format!("{id}: {e:#}"));}
        let frozen=freeze(repository,&binding.owner_id,&id,envelope);
        let envelope=match frozen {
            Ok(envelope) => envelope,
            Err(error) => {
                report.skip(repository,&binding.owner_id,&id,"projection",Some(format!("{error:#}")));continue;
            }
        };
        // Another path can freeze the source after our cutoff/projection. The
        // returned original envelope, rather than the preview, owns custody.
        if envelope.owner_id!=binding.owner_id || envelope.thread_id!=binding.thread_id
            || envelope.runtime_generation!=binding.runtime_generation || envelope.message.delivery!=Delivery::Passive {
            fence.failed(&message);
            report.skip(repository,&binding.owner_id,&id,"runtimeCapabilityOrBinding",Some("frozen passive note binding or delivery changed".into()));continue;
        }
        if let Err(e)=crate::management::v2::input_disposal::check_envelope(&binding.owner_id,&envelope) {report.skip(repository,&binding.owner_id,&id,"projection",Some(format!("{e:#}")));continue;}
        repository.input_ledger_sending(&binding.owner_id, &id);
        let status = match submit(&envelope) {
            Ok(status) => status,
            Err(error) => {
                fence.failed(&message);report.skip(repository,&binding.owner_id,&id,"nativeReconciliation",Some(format!("{error:#}")));continue;
            }
        };
        if let Err(e)=repository.observe_external_input(&id,&envelope,&status) {report.skip(repository,&binding.owner_id,&id,"nativeReconciliation",Some(format!("{e:#}")));continue;}
        // A runtime hold is explicitly outside the new-turn contract. Keep
        // that exact identity held; never release it to make the barrier pass.
        if status.processing.state==ProcessingState::Held {report.skip(repository,&binding.owner_id,&id,"nativeRelease",None);}
        else if matches!(status.delivery_state,DeliveryState::Pending|DeliveryState::ContextPersisted) {report.admitted.push(id);}
        else {report.skip(repository,&binding.owner_id,&id,"nativeReconciliation",Some("native acceptance unresolved".into()));}
    }
    report.budget_notes=count;report.budget_bytes=bytes;report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::management::v2::agent_bus_state::AgentBusQueuedMessage;

    struct Fixture { root:std::path::PathBuf, repository:AgentBusMessageRepository, binding:ExternalInputBinding }
    impl Fixture {
        fn new() -> Self {
            let root=std::env::temp_dir().join(format!("pre-turn-notes-{}",uuid::Uuid::new_v4()));
            Self { repository:AgentBusMessageRepository::open(root.join("bus")).unwrap(),root,
                binding:ExternalInputBinding {version:1,owner_id:format!("cutex.{}",uuid::Uuid::new_v4()),thread_id:"main-thread".into(),runtime_generation:7,canonical_byte_limit:Default::default()} }
        }
        fn note(&self, id:&str) {
            self.note_from(id,"cutex.private-sender",false,&format!("NOTE {id} @literal-path"));
        }
        fn note_from(&self,id:&str,sender:&str,bad:bool,content:&str) {
            let message=serde_json::from_value(json!({"id":id,"kind":"message","to":"private-runtime",
                "from":if bad {"AgentManagementSystem"} else {sender},
                "controlType":if bad {Some("cutex.agent_management.notice.v1")} else {None},
                "fromCutexSessionId":sender,"toCutexSessionId":self.binding.owner_id,"content":content,
                "deliveryMode":"passive","triggerTurn":false,"createdAtEpochSecs":1,"senderKind":"agent"})).unwrap();
            self.repository.record_queued_isolated(AgentBusQueuedMessage {owner_cutex_session_id:sender.into(),message_id:id.into(),
                from_cutex_session_id:Some(sender.into()),to_cutex_session_id:self.binding.owner_id.clone(),from_runtime_agent_id:None,
                to_runtime_agent_id:Some("private-runtime".into()),delivery_mode:"passive".into(),content:content.into(),
                queued_at:chrono::Utc::now(),canonical_envelope:message,semantic_sha256:"a".repeat(64)}).unwrap();
        }
        fn accepted(envelope:&Envelope) -> Status {
            serde_json::from_value(json!({"messageId":envelope.message.id,"semanticSha256":envelope.semantic_sha256,
                "deliveryState":"pending","processing":{"state":"none","reason":null,"attemptId":null},"receipt":null})).unwrap()
        }
    }
    impl Drop for Fixture {fn drop(&mut self) {let _=std::fs::remove_dir_all(&self.root);}}

    #[test]
    fn passive_barrier_admits_durable_notes_without_a_live_bus_queue_before_every_trigger() {
        for kind in ["owner","delegation-result","system"] {
            let f=Fixture::new();f.note("queued-only-in-durable-store");
            let mut inbox=Vec::new();
            synchronize(&f.repository,&f.binding,|envelope| {
                assert_eq!(envelope.message.delivery,Delivery::Passive);
                assert_eq!(envelope.thread_id,f.binding.thread_id);
                assert_eq!(envelope.message.source.id,"cutex.private-sender");
                assert!(envelope.message.text.contains("@literal-path"));
                inbox.push(envelope.message.id.clone());Ok(Fixture::accepted(envelope))
            });
            // A passive submit has started no model turn. The following
            // owner/result/system trigger sees the note in its first input.
            assert_eq!(inbox,["queued-only-in-durable-store"],"{kind}");
            assert!(synchronize(&f.repository,&f.binding,|_|panic!("accepted identity must not be submitted twice")).is_empty());
        }
    }

    #[test]
    fn barrier_cutoff_excludes_later_arrivals_and_does_not_replay_unknown_bindings_or_holds() {
        let f=Fixture::new();
        f.note("held");f.note("unknown");f.note("accepted-other-generation");f.note("fresh");
        f.repository.hold_stale_pending("cutex.private-sender","held",&"a".repeat(64),0,chrono::Utc::now()).unwrap();
        f.repository.freeze_external_input(&f.binding.owner_id,"unknown",|m|super::super::bus_bridge::owner_carry_envelope(m,&f.binding,None,None)).unwrap();
        let other=ExternalInputBinding {runtime_generation:6,..f.binding.clone()};
        let e=f.repository.freeze_external_input(&f.binding.owner_id,"accepted-other-generation",|m|super::super::bus_bridge::owner_carry_envelope(m,&other,None,None)).unwrap();
        f.repository.observe_external_input("accepted-other-generation",&e,&Fixture::accepted(&e)).unwrap();
        let mut ids=Vec::new();
        synchronize(&f.repository,&f.binding,|envelope| {ids.push(envelope.message.id.clone());f.note("arrived-after-cutoff");Ok(Fixture::accepted(envelope))});
        assert_eq!(ids,["fresh"]);
        assert_eq!(f.repository.unsubmitted_note_ids(&f.binding.owner_id).unwrap(),["arrived-after-cutoff"]);
    }

    #[test]
    fn unknown_native_acceptance_does_not_gate_trigger_or_replay_the_identity() {
        let f=Fixture::new();f.note("uncertain");
        let report=synchronize(&f.repository,&f.binding,|_|anyhow::bail!("fixture reply lost"));
        assert_eq!(report.skipped[0].input_id,"uncertain");assert_eq!(report.skipped[0].waiting_for,"nativeReconciliation");
        let frozen=f.repository.snapshot_by_message_id("uncertain").unwrap().unwrap().external_input.unwrap();
        assert!(synchronize(&f.repository,&f.binding,|_|panic!("unknown acceptance is excluded, never blindly resent")).is_empty());
        assert_eq!(f.repository.snapshot_by_message_id("uncertain").unwrap().unwrap().external_input,Some(frozen));
        let (_,facts,_)=f.repository.input_ledger_view("uncertain").unwrap().unwrap();
        let diagnostic=facts.delivery_diagnostics.unwrap();
        assert_eq!(diagnostic["stage"],"bus");assert_eq!(diagnostic["waitingFor"],"nativeReconciliation");
        assert!(diagnostic["lastError"].as_str().unwrap().contains("reply lost"));
    }

    #[test]
    fn bad_projection_retains_fifo_diagnostics_and_admits_an_independent_note() {
        let f=Fixture::new();
        assert!(Report::unavailable(&f.repository,&f.binding.owner_id,&[],"old pin".into()).is_empty(),"no notes means no capability warning");
        f.note_from("bad","cutex.bad",true,"broken");
        f.note_from("follower","cutex.bad",false,"follow");f.note_from("good","cutex.other",false,"healthy");
        let report=synchronize(&f.repository,&f.binding,|e|Ok(Fixture::accepted(e)));
        assert_eq!(report.admitted,["good"]);
        assert_eq!(report.skipped.iter().map(|n|n.input_id.as_str()).collect::<Vec<_>>(),["bad","follower"]);
        let (_,facts,_)=f.repository.input_ledger_view("bad").unwrap().unwrap();
        assert_eq!(facts.delivery_diagnostics.unwrap()["waitingFor"],"projection");
        let (_,facts,_)=f.repository.input_ledger_view("follower").unwrap().unwrap();
        assert_eq!(facts.delivery_diagnostics.unwrap()["blockedByInputId"],"bad");
        assert!(f.repository.snapshot_by_message_id("bad").unwrap().unwrap().external_input.is_none());
        f.repository.hold_stale_pending("cutex.bad","bad",&"a".repeat(64),0,chrono::Utc::now()).unwrap();
        let recovered=synchronize(&f.repository,&f.binding,|e|Ok(Fixture::accepted(e)));
        assert_eq!(recovered.admitted,["follower"]);
        let (_,facts,_)=f.repository.input_ledger_view("follower").unwrap().unwrap();
        let diagnostic=facts.delivery_diagnostics.unwrap();
        assert!(diagnostic["blockedByInputId"].is_null());
        assert_eq!(diagnostic["waitingFor"],"nextInputBoundary");
    }

    #[test]
    fn count_and_total_wire_budget_leave_unfrozen_notes_for_the_next_boundary() {
        let f=Fixture::new();for n in 0..103 {f.note(&format!("count-{n:03}"));}
        let report=synchronize(&f.repository,&f.binding,|e|Ok(Fixture::accepted(e)));
        assert_eq!(report.admitted.len(),MAX_NOTES);assert_eq!(report.skipped.len(),4);
        for n in &report.skipped {
            assert_eq!(n.waiting_for,"nextBoundaryBudget");
            assert!(f.repository.snapshot_by_message_id(&n.input_id).unwrap().unwrap().external_input.is_none());
        }
        assert_eq!(synchronize(&f.repository,&f.binding,|e|Ok(Fixture::accepted(e))).admitted.len(),4);
        let f=Fixture::new();let binding=ExternalInputBinding {canonical_byte_limit:serde_json::from_value(json!("off")).unwrap(),..f.binding.clone()};
        for n in 0..24 {f.note_from(&format!("bytes-{n:03}"),"cutex.bytes",false,&"a".repeat(50_000));}
        let mut bytes=0usize;let report=synchronize(&f.repository,&binding,|e|{bytes+=serde_json::to_vec(e).unwrap().len();Ok(Fixture::accepted(e))});
        assert!(bytes<=MAX_TRANSPORT_BYTES);assert!(!report.admitted.is_empty() && !report.skipped.is_empty());
        let reduced=Budget::after_carried(MAX_NOTES,MAX_TRANSPORT_BYTES);
        assert!(synchronize_budget(&f.repository,&binding,reduced,|_|panic!("owner already used the boundary budget")).admitted.is_empty());
    }

    #[test]
    fn stale_trigger_diagnostics_lose_to_concurrent_native_progress() {
        use crate::management::v2::input_ledger::{InputLedger,Expect,Next,Evidence,LedgerError,
            NewInput,InputKind,InputSender,SenderKind,InputIntent,Strategy,Timing};
        use std::sync::mpsc;
        use std::time::Duration;
        for completed in [false,true] {
            let f=Fixture::new();let owner=&f.binding.owner_id;let id="trigger";
            let ledger=InputLedger::open(f.root.join("ledger.sqlite3"));
            let input=NewInput {kind:InputKind::Owner,
                sender:InputSender {kind:SenderKind::Owner,id:"owner".into(),name:"owner".into(),host:None,cutex_session_id:None},
                intent:InputIntent {strategy:Some(Strategy::Auto),delivery_mode:None,timing:Timing::NextStep},
                preview:"trigger".into(),ask_id:None,remote:None};
            ledger.transition(owner,id,Expect::NEW,Next::received().create(input)).unwrap();
            ledger.transition(owner,id,Expect::ANY,Next::sending()).unwrap();
            ledger.transition(owner,id,Expect::ANY,Next::accepted().details(json!({"old":true}))).unwrap();
            let report=Report {skipped:vec![SkippedNote {input_id:"retained-note".into(),waiting_for:"projection".into(),error:None}],..Default::default()};
            let (read,ready)=mpsc::channel();let (resume,go)=mpsc::channel();
            std::thread::scope(|scope| {
                let ledger_ref=&ledger;let report_ref=&report;
                let worker=scope.spawn(move || {
                    let before=ledger_ref.get(owner,&[id.into()]).unwrap().records.pop().unwrap();
                    report_ref.attach_record(before,|session,id,expect,next| {
                        read.send(()).unwrap();go.recv_timeout(Duration::from_secs(5)).unwrap();
                        assert!(matches!(ledger_ref.transition(session,id,expect,next),Err(LedgerError::Conflict {..})),"stale diagnostics must be discarded by revision CAS");
                    }).unwrap();
                });
                ready.recv_timeout(Duration::from_secs(5)).unwrap();
                let progress=if completed {Next::in_context(Evidence::ExternalInputPersisted)} else {Next::accepted()};
                let advanced=ledger.transition(owner,id,Expect::ANY,progress.turn("native-turn").details(json!({"nativeProgress":true}))).unwrap().record;
                resume.send(()).unwrap();worker.join().unwrap();
                let actual=ledger.get(owner,&[id.into()]).unwrap().records.pop().unwrap();
                assert_eq!(actual,advanced,"diagnostics cannot overwrite newer state, turn or details");
            });
        }
    }

    #[test]
    fn concurrent_freeze_with_another_thread_or_generation_is_never_submitted() {
        use std::sync::mpsc;
        use std::time::Duration;
        for changed_thread in [false,true] {
            let f=Fixture::new();f.note("racing-note");
            let winner=ExternalInputBinding {
                thread_id:if changed_thread {"other-thread".into()} else {f.binding.thread_id.clone()},
                runtime_generation:if changed_thread {f.binding.runtime_generation} else {f.binding.runtime_generation+1},
                ..f.binding.clone()
            };
            let (freeze,start)=mpsc::channel();let (finished,done)=mpsc::channel();
            std::thread::scope(|scope| {
                let fixture=&f;let binding=&winner;
                let worker=scope.spawn(move || {
                    start.recv_timeout(Duration::from_secs(5)).unwrap();
                    let envelope=fixture.repository.freeze_external_input(&binding.owner_id,"racing-note",|m|
                        super::super::bus_bridge::owner_carry_envelope(m,binding,None,None)).unwrap();
                    finished.send(envelope).unwrap();
                });
                let mut original=None;
                let report=synchronize_with_freeze(&f.repository,&f.binding,Budget::default(),
                    |_|panic!("a frozen foreign binding must not reach native IO"),
                    |repository,owner,id,preview| {
                        freeze.send(()).unwrap();original=Some(done.recv_timeout(Duration::from_secs(5)).unwrap());
                        repository.freeze_external_input(owner,id,|_|Ok(preview))
                    });
                worker.join().unwrap();
                assert_eq!(report.skipped[0].waiting_for,"runtimeCapabilityOrBinding");
                let snapshot=f.repository.snapshot_by_message_id("racing-note").unwrap().unwrap();
                assert_eq!(snapshot.external_input,original,"keep the winning frozen identity unchanged");
                assert!(snapshot.external_input_last_observed.is_none());
            });
        }
    }
}
