//! r275 bounded wait: an explicit start (or same-digest retry) waits for its
//! worker up to a bound, holding neither the cache mutex nor a business lock.
use super::*;
use std::sync::{atomic::{AtomicUsize, Ordering}, mpsc};
use std::time::Instant;

const LONG: Duration = Duration::from_secs(10);
fn q(id: &str) -> Query { Query {tool:"agent_create".into(),action_id:id.into()} }
fn until(mut condition: impl FnMut() -> bool) {
    let end=Instant::now()+Duration::from_secs(20);
    while !condition() { assert!(Instant::now()<end,"test coordination timed out"); std::thread::sleep(Duration::from_millis(5)); }
}
fn waiters(cache: &SharedCache) -> usize { cache.lock().unwrap().waiters }
fn created() -> Value {
    json!({"schema":"cutex/agent-management/v1","action_id":"fast","outcome":{"status":"complete","receipt":{"result":{
        "kind":"created","agent":{"cutex_session_id":"cutex.00000000-0000-0000-0000-0000000000b2","name":"quokka",
            "profile":"Ceobe","runtime":"claude-r12"},
        "delegation":{"ok":true,"delegation":"delegation-abc123","state":"submitted"}}}}})
}
/// The launch policy's refusal while an owner approval is pending, in the
/// exact shape server.rs serializes (agent_management_no_write).
fn approval_refusal(action_id: &str) -> Value {
    serde_json::to_value(crate::agent_management::AgentManagementResponse {
        schema:crate::agent_management::AgentManagementSchema::V1,
        action_id:crate::agent_management::AgentActionId::new(action_id).unwrap(),
        outcome:crate::agent_management::AgentManagementOutcome::NoWrite {
            code:"approval_required".into(),
            detail:"Nothing was done yet: this needs the owner's approval (profile Ceobe). (approval_id approval-7f3e)".into()},
    }).unwrap()
}

#[test]
fn fast_worker_returns_the_finished_envelope() {
    let cache: SharedCache=Default::default();let query=q("fast");
    let reply=start_in(cache.clone(),LONG,"caller",query.clone(),"digest".into(),||Ok(created()));
    assert_eq!(reply["status"],"completed","{reply}");
    assert!(valid_response(&reply,"agent_create","fast"));
    let result=&reply["result"]["outcome"]["receipt"]["result"];
    assert_eq!(result["agent"]["name"],"quokka");assert_eq!(result["agent"]["profile"],"Ceobe");
    assert_eq!(result["agent"]["runtime"],"claude-r12");
    assert_eq!(result["delegation"]["delegation"],"delegation-abc123");
    // Same shape as the action_status read of the same action.
    assert_eq!(query_in(&cache,"caller",&query,||panic!("cached final result")),reply);
    assert_eq!(waiters(&cache),0);
}

#[test]
fn blocked_worker_answers_running_and_reads_proceed_during_the_wait() {
    let cache: SharedCache=Default::default();let query=q("blocked");
    let (release,wait)=mpsc::channel::<()>();
    let started=Instant::now();
    let reply=start_in(cache.clone(),Duration::from_millis(300),"caller",query.clone(),"digest".into(),move||{wait.recv().unwrap();Ok(created())});
    assert!(started.elapsed()>=Duration::from_millis(300));
    assert_eq!(reply,envelope(&query,"running",false,None));
    // A second explicit start waits while action_status reads go through.
    let (c,qq)=(cache.clone(),query.clone());
    let retry=std::thread::spawn(move||start_in(c,LONG,"caller",qq,"digest".into(),||panic!("replayed")));
    until(||waiters(&cache)==1);
    let read=Instant::now();
    assert_eq!(query_in(&cache,"caller",&query,||panic!("running rows never read disk"))["status"],"running");
    assert_eq!(query_in(&cache,"other",&q("unrelated"),||Ok(envelope(&q("unrelated"),"notFound",true,None)))["status"],"notFound");
    assert!(read.elapsed()<Duration::from_secs(2),"reads must not wait for the waiter");
    assert_eq!(waiters(&cache),1,"the start is still waiting");
    release.send(()).unwrap();
    assert_eq!(retry.join().unwrap()["status"],"completed");
    assert_eq!(waiters(&cache),0);
}

#[test]
fn retry_while_running_waits_then_replays_without_rerunning() {
    let cache: SharedCache=Default::default();let query=q("retry");let runs=Arc::new(AtomicUsize::new(0));
    let (release,wait)=mpsc::channel::<()>();let count=runs.clone();
    assert_eq!(start_in(cache.clone(),Duration::ZERO,"caller",query.clone(),"digest".into(),move||{count.fetch_add(1,Ordering::SeqCst);wait.recv().unwrap();Ok(created())})["status"],"running");
    let (c,qq)=(cache.clone(),query.clone());
    let retry=std::thread::spawn(move||start_in(c,LONG,"caller",qq,"digest".into(),||panic!("replayed")));
    until(||waiters(&cache)==1);
    release.send(()).unwrap();
    let started=Instant::now();
    assert_eq!(retry.join().unwrap()["status"],"completed");
    assert!(started.elapsed()<Duration::from_secs(5),"woken by the worker, not the timeout");
    assert_eq!(start_in(cache.clone(),LONG,"caller",query.clone(),"digest".into(),||panic!("replayed"))["status"],"completed");
    assert_eq!(runs.load(Ordering::SeqCst),1);
}

#[test]
fn digest_conflict_fails_at_once_while_running() {
    let cache: SharedCache=Default::default();let query=q("conflict");
    let (release,wait)=mpsc::channel::<()>();
    start_in(cache.clone(),Duration::ZERO,"caller",query.clone(),"digest".into(),move||{wait.recv().unwrap();Ok(created())});
    let started=Instant::now();
    let reply=start_in(cache.clone(),LONG,"caller",query.clone(),"different".into(),||panic!("changed payload"));
    assert!(started.elapsed()<Duration::from_secs(2));
    assert_eq!(reply["status"],"failed");assert_eq!(reply["result"]["code"],"idempotency_conflict");
    assert_eq!(waiters(&cache),0);
    release.send(()).unwrap();
}

#[test]
fn pending_owner_approval_is_awaiting_and_a_retry_after_approval_runs() {
    let cache: SharedCache=Default::default();let query=q("needs-approval");let runs=Arc::new(AtomicUsize::new(0));
    let count=runs.clone();
    let started=Instant::now();
    let reply=start_in(cache.clone(),LONG,"caller",query.clone(),"digest".into(),move||{count.fetch_add(1,Ordering::SeqCst);Ok(approval_refusal("needs-approval"))});
    assert!(started.elapsed()<Duration::from_secs(5),"no wait for the owner's decision");
    assert_eq!(reply["status"],"awaitingApproval","{reply}");
    assert_eq!(reply["outcomeUnknown"],false);
    assert_eq!(reply["result"]["approval_id"],"approval-7f3e");
    assert!(valid_response(&reply,"agent_create","needs-approval"));
    // Not final: the durable read is consulted; without evidence it still waits.
    assert_eq!(query_in(&cache,"caller",&query,||Ok(envelope(&query,"notFound",true,None)))["status"],"awaitingApproval");
    // The owner approves; the identical retry runs the action again.
    let count=runs.clone();
    let retry=start_in(cache.clone(),LONG,"caller",query.clone(),"digest".into(),move||{count.fetch_add(1,Ordering::SeqCst);Ok(created())});
    assert_eq!(retry["status"],"completed","{retry}");
    assert_eq!(runs.load(Ordering::SeqCst),2);
    // Other no_write refusals stay final failures.
    let denied=completed(&q("denied"),json!({"outcome":{"status":"no_write","code":"approval_denied","detail":"(approval_id x)"}}));
    assert_eq!(denied["status"],"failed");
    assert_eq!(approval_id("text (approval_id a b)"),None);
}

#[test]
fn capacity_limits_hold_while_callers_wait() {
    let cache: SharedCache=Default::default();let runs=Arc::new(AtomicUsize::new(0));
    let mut releases=vec![];let mut starters=vec![];
    for i in 0..MAX_WORKERS {
        let caller=format!("caller-{}",i/MAX_CALLER_WORKERS);let (tx,rx)=mpsc::channel::<()>();
        let (c,count)=(cache.clone(),runs.clone());
        starters.push(std::thread::spawn(move||start_in(c,LONG,&caller,q(&format!("blocked-{i}")),"digest".into(),
            move||{count.fetch_add(1,Ordering::SeqCst);rx.recv().unwrap();Ok(json!({"ok":true}))})));
        releases.push(tx);
        until(||waiters(&cache)==i+1);
        if i==1 {
            assert_eq!(start_in(cache.clone(),LONG,"caller-0",q("caller-excess"),"digest".into(),||panic!("over caller budget"))["retryAdmission"]["code"],"action_caller_capacity");
        }
    }
    let started=Instant::now();
    for i in 0..20 {
        assert_eq!(start_in(cache.clone(),LONG,"other",q(&format!("excess-{i}")),"digest".into(),||panic!("over global budget"))["retryAdmission"]["code"],"action_worker_capacity");
    }
    assert!(started.elapsed()<Duration::from_secs(2),"refusals never wait");
    assert_eq!(cache.lock().unwrap().rows.len(),MAX_WORKERS);
    until(||runs.load(Ordering::SeqCst)==MAX_WORKERS);
    for tx in releases {tx.send(()).unwrap();}
    for starter in starters {assert_eq!(starter.join().unwrap()["status"],"completed");}
    assert_eq!(waiters(&cache),0);
    assert_eq!(start_in(cache.clone(),LONG,"other",q("after"),"digest".into(),||Ok(json!({"ok":true})))["status"],"completed");
}

#[test]
fn waiter_count_is_bounded_and_extra_retries_answer_running() {
    let cache: SharedCache=Default::default();let query=q("crowd");
    let (release,wait)=mpsc::channel::<()>();
    start_in(cache.clone(),Duration::ZERO,"caller",query.clone(),"digest".into(),move||{wait.recv().unwrap();Ok(json!({"ok":true}))});
    let mut retries=vec![];
    for _ in 0..MAX_WAITERS {
        let (c,qq)=(cache.clone(),query.clone());
        retries.push(std::thread::spawn(move||start_in(c,LONG,"caller",qq,"digest".into(),||panic!("replayed"))));
    }
    until(||waiters(&cache)==MAX_WAITERS);
    let started=Instant::now();
    assert_eq!(start_in(cache.clone(),LONG,"caller",query.clone(),"digest".into(),||panic!("replayed"))["status"],"running");
    assert!(started.elapsed()<Duration::from_secs(2));
    release.send(()).unwrap();
    for retry in retries {assert_eq!(retry.join().unwrap()["status"],"completed");}
}

#[test]
fn worker_panic_wakes_the_waiter_with_unknown() {
    let cache: SharedCache=Default::default();let query=q("panics");
    let started=Instant::now();
    let reply=start_in(cache.clone(),LONG,"caller",query.clone(),"digest".into(),||{std::thread::sleep(Duration::from_millis(100));panic!("worker failed")});
    assert!(started.elapsed()<Duration::from_secs(5),"woken by the panic, not the timeout");
    assert_eq!(reply,envelope(&query,"unknown",true,None));
    // A worker that unwinds past catch_unwind still ends its row and wakes waiters.
    let (c,k)=(cache.clone(),key("caller",&q("guard")));
    let generation={let mut s=cache.lock().unwrap();s.generation+=1;let g=s.generation;
        s.rows.insert(k.clone(),Running {digest:"digest".into(),generation:g,result:None});g};
    let guard=std::thread::spawn(move||{std::thread::sleep(Duration::from_millis(100));
        let _end=WorkerEnd {cache:c,key:k,generation,query:q("guard"),value:None};panic!("outside catch_unwind")});
    let started=Instant::now();
    let reply=start_in(cache.clone(),LONG,"caller",q("guard"),"digest".into(),||panic!("replayed"));
    assert!(guard.join().is_err());
    assert!(started.elapsed()<Duration::from_secs(5));
    assert_eq!(reply,envelope(&q("guard"),"unknown",true,None));
}
