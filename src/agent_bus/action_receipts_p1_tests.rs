use super::*;
use crate::asks::store::{AskStore, tests::{new_ask, REQUESTER}};
use std::{path::PathBuf,sync::{mpsc,atomic::{AtomicUsize,Ordering}},time::{Duration,Instant}};
fn isolated()->SharedCache {Arc::new(Mutex::new(Cache::default()))}
fn q(id:&str)->Query {Query {tool:"agent_delegate".into(),action_id:id.into()}}
fn temp()->PathBuf {let p=std::env::temp_dir().join(format!("receipts-p1-{}",uuid::Uuid::new_v4()));std::fs::create_dir_all(&p).unwrap();p}
fn until(mut condition:impl FnMut()->bool) {let end=Instant::now()+Duration::from_secs(20);while !condition(){assert!(Instant::now()<end,"test coordination timed out");std::thread::sleep(Duration::from_millis(5));}}
fn ended(cache:&SharedCache,caller:&str,q:&Query){until(||cache.lock().unwrap().rows.get(&key(caller,q)).is_some_and(|r|r.result.is_some()));}
fn unknown(cache:&SharedCache,caller:&str,q:&Query){assert_eq!(start_in(cache.clone(),caller,q.clone(),"digest".into(),||Ok(json!({"ok":false,"status":"response_uncertain","code":"outcome_unknown"})))["status"],"running");ended(cache,caller,q);}
fn reserve(store:&AskStore,id:&str){store.with_state(|s|s.begin_action(REQUESTER,id,"agent_delegate","digest",chrono::Utc::now())).unwrap();}
fn commit_without_reply(store:&AskStore,id:&str){store.with_state(|s|s.create_ask(new_ask(id,"private fixture"),chrono::Utc::now())).unwrap().unwrap();}

#[test]
fn independent_caches_reconcile_commit_after_unknown_without_replay(){
 let root=temp();let a=AskStore::at(root.join("asks"));let b=AskStore::at(root.join("asks"));let qa=q("two-caches");
 let cache_a=isolated();let cache_b=isolated();let (entered,ready)=mpsc::channel();let (release,wait)=mpsc::channel();
 let action=qa.action_id.clone();
 start_in(cache_a.clone(),REQUESTER,qa.clone(),"digest".into(),move||{reserve(&a,&action);entered.send(()).unwrap();wait.recv().unwrap();commit_without_reply(&a,&action);anyhow::bail!("business committed; reply persistence failed")});
 ready.recv_timeout(Duration::from_secs(10)).unwrap();unknown(&cache_b,REQUESTER,&qa);
 assert_eq!(query_in(&cache_b,REQUESTER,&qa,||delegation_query(&b,REQUESTER,&qa))["status"],"unknown");
 let before=b.snapshot().unwrap();assert_eq!(before.asks.len(),0);release.send(()).unwrap();ended(&cache_a,REQUESTER,&qa);
 for cache in [&cache_a,&cache_b]{let result=query_in(cache,REQUESTER,&qa,||delegation_query(&b,REQUESTER,&qa));assert_eq!(result["status"],"completed");}
 let before=serde_json::to_value(b.snapshot().unwrap()).unwrap();
 query_in(&cache_b,REQUESTER,&qa,||delegation_query(&b,REQUESTER,&qa));
 assert_eq!(serde_json::to_value(b.snapshot().unwrap()).unwrap(),before);assert_eq!(b.snapshot().unwrap().asks.len(),1);
}

#[test]
#[ignore = "subprocess fixture only"]
fn receipt_process_child(){
 let root=PathBuf::from(std::env::var("CUTEX_RECEIPT_P1_CHILD_ROOT").expect("private fixture root"));
 let store=AskStore::at(root.join("asks"));reserve(&store,"cross-process");
 std::fs::write(root.join("reserved"),b"reserved").unwrap();until(||root.join("commit").exists());
 commit_without_reply(&store,"cross-process"); // Intentionally omit finish_action: reply write lost.
 std::fs::write(root.join("committed"),b"committed").unwrap();
}
#[test]
fn second_process_commit_overrides_cached_unknown(){
 let root=temp();let exe=std::env::current_exe().unwrap();
 let stdout=std::fs::File::create(root.join("child.stdout")).unwrap();let stderr=std::fs::File::create(root.join("child.stderr")).unwrap();
 let mut child=std::process::Command::new(exe).args(["p1_tests::receipt_process_child","--ignored","--nocapture","--test-threads=1"])
  .env("CUTEX_RECEIPT_P1_CHILD_ROOT",&root).stdout(stdout).stderr(stderr).spawn().unwrap();
 until(||root.join("reserved").exists());let cache=isolated();let query=q("cross-process");unknown(&cache,REQUESTER,&query);
 let store=AskStore::at(root.join("asks"));assert_eq!(query_in(&cache,REQUESTER,&query,||delegation_query(&store,REQUESTER,&query))["status"],"unknown");
 std::fs::write(root.join("commit"),b"go").unwrap();until(||root.join("committed").exists());assert!(child.wait().unwrap().success());
 assert_eq!(query_in(&cache,REQUESTER,&query,||delegation_query(&store,REQUESTER,&query))["status"],"completed");
 assert_eq!(store.snapshot().unwrap().asks.len(),1);assert!(store.snapshot().unwrap().actions.values().all(|a|a.result.is_none()));
}

#[test]
fn panic_before_reservation_is_unknown_until_explicit_original_id_retry(){
 let cache=isolated();let query=q("before-reservation");let root=temp();let store=AskStore::at(root.join("asks"));
 start_in(cache.clone(),REQUESTER,query.clone(),"digest".into(),||panic!("before reservation"));ended(&cache,REQUESTER,&query);
 assert_eq!(query_in(&cache,REQUESTER,&query,||delegation_query(&store,REQUESTER,&query))["status"],"unknown");
 assert!(store.snapshot().unwrap().asks.is_empty());let action=query.action_id.clone();
 start_in(cache.clone(),REQUESTER,query.clone(),"digest".into(),move||{reserve(&store,&action);commit_without_reply(&store,&action);Ok(json!({"ok":true}))});
 ended(&cache,REQUESTER,&query);assert_eq!(query_in(&cache,REQUESTER,&query,||panic!("already known"))["status"],"completed");
}
#[test]
fn stale_read_does_not_overwrite_a_concurrent_explicit_retry(){
 let cache=isolated();let query=q("read-race");unknown(&cache,"caller",&query);
 let read_cache=cache.clone();let read_query=query.clone();let (entered,ready)=mpsc::channel();let (release,wait)=mpsc::channel();
 let reader=std::thread::spawn(move||query_in(&read_cache,"caller",&read_query,||{entered.send(()).unwrap();wait.recv().unwrap();Ok(envelope(&read_query,"unknown",true,None))}));
 ready.recv_timeout(Duration::from_secs(10)).unwrap();
 start_in(cache.clone(),"caller",query.clone(),"digest".into(),||Ok(json!({"ok":true,"committed":true})));ended(&cache,"caller",&query);
 release.send(()).unwrap();reader.join().unwrap();
 assert_eq!(query_in(&cache,"caller",&query,||panic!("must retain newer result"))["status"],"completed");
}

#[test]
fn blocked_workers_are_bounded_before_spawn_and_capacity_recovers(){
 let cache=isolated();let runs=Arc::new(AtomicUsize::new(0));let mut releases=vec![];let mut queries=vec![];
 for i in 0..MAX_WORKERS {
  let caller=format!("caller-{}",i/MAX_CALLER_WORKERS);let query=q(&format!("blocked-{i}"));let count=runs.clone();
  let (tx,rx)=mpsc::channel();let (entered,ready)=mpsc::channel();
  assert_eq!(start_in(cache.clone(),&caller,query.clone(),"digest".into(),move||{count.fetch_add(1,Ordering::SeqCst);entered.send(()).unwrap();rx.recv().unwrap();Ok(json!({"ok":true}))})["status"],"running");
  ready.recv_timeout(Duration::from_secs(10)).unwrap();releases.push(tx);queries.push((caller,query));
  if i==1 {assert_eq!(start_in(cache.clone(),"caller-0",q("caller-excess"),"digest".into(),||panic!("over caller budget"))["retryAdmission"]["code"],"action_caller_capacity");}
 }
 // All workers remain genuinely blocked during the excess admissions and query.
 for i in 0..100 {assert_eq!(start_in(cache.clone(),"other",q(&format!("excess-{i}")),"digest".into(),||panic!("over global budget"))["retryAdmission"]["code"],"action_worker_capacity");}
 assert_eq!(runs.load(Ordering::SeqCst),MAX_WORKERS);assert_eq!(cache.lock().unwrap().rows.len(),MAX_WORKERS);
 assert_eq!(query_in(&cache,"reader",&q("unrelated"),||Ok(envelope(&q("unrelated"),"notFound",true,None)))["status"],"notFound");
 for release in releases {release.send(()).unwrap();}for (caller,query) in queries {ended(&cache,&caller,&query);}
 let query=q("after-budget");assert_eq!(start_in(cache.clone(),"other",query.clone(),"digest".into(),||Ok(json!({"ok":true})))["status"],"running");ended(&cache,"other",&query);
}

#[test]
fn ended_unknown_projection_capacity_is_reusable_without_replay(){
 let cache=isolated();let old=q("evicted-unknown");
 {let mut state=cache.lock().unwrap();for i in 0..MAX_CACHED_RESULTS {let query=if i==0 {old.clone()} else {q(&format!("unknown-{i}"))};state.rows.insert(key(REQUESTER,&query),Running {digest:"digest".into(),generation:i as u64,result:Some(envelope(&query,"unknown",true,None))});}state.generation=MAX_CACHED_RESULTS as u64;}
 let root=temp();let store=AskStore::at(root.join("asks"));reserve(&store,&old.action_id);commit_without_reply(&store,&old.action_id);
 let (release,wait)=mpsc::channel();let new=q("new-key");start_in(cache.clone(),REQUESTER,new.clone(),"digest".into(),move||{wait.recv().unwrap();Ok(json!({"ok":true}))});
 assert_eq!(cache.lock().unwrap().rows.len(),MAX_CACHED_RESULTS);assert!(!cache.lock().unwrap().rows.contains_key(&key(REQUESTER,&old)));
 assert_eq!(query_in(&cache,REQUESTER,&old,||delegation_query(&store,REQUESTER,&old))["status"],"completed");assert_eq!(store.snapshot().unwrap().asks.len(),1);
 // Queries/eviction neither release the reserved action nor enqueue anything.
 assert!(store.snapshot().unwrap().actions.values().all(|a|a.result.is_none()));
 // Only an explicit original-ID retry invokes the original reservation layer.
 let retry_store=AskStore::at(root.join("asks"));let id=old.action_id.clone();
 start_in(cache.clone(),REQUESTER,old.clone(),"digest".into(),move||{
  let lookup=retry_store.with_state(|s|s.begin_action(REQUESTER,&id,"agent_delegate","digest",chrono::Utc::now()))?;
  assert!(matches!(lookup,crate::asks::store::ActionLookup::InFlight));
  Ok(json!({"ok":false,"status":"response_uncertain","code":"outcome_unknown"}))
 });
 ended(&cache,REQUESTER,&old);
 assert_eq!(query_in(&cache,REQUESTER,&old,||delegation_query(&store,REQUESTER,&old))["status"],"completed");
 assert_eq!(store.snapshot().unwrap().asks.len(),1);
 release.send(()).unwrap();ended(&cache,REQUESTER,&new);
}
#[test]
fn concurrent_child_inflight_is_unknown_then_parent_read_converges(){
 use crate::asks::store::tests::TARGET;
 let root=temp();let asks=AskStore::at(root.join("asks"));let management=crate::agent_management::AgentManagementStore::open(&root.join("management")).unwrap();
 let create_id="create-with-task";let child_id=crate::asks::create_task::delegation_action_id(create_id);
 let record:crate::agent_management::AgentActionRecord=serde_json::from_value(json!({"action_id":create_id,"request_sha256":"0".repeat(64),"operation":"create","project_id":"alpha","caller_cutex_session":REQUESTER,"phase":"prepared","created_at":"2026-10-26T00:00:00Z","updated_at":"2026-10-26T00:00:00Z"})).unwrap();
 management.with_state(true,|mut s|{s.actions.insert(record.action_id.clone(),record);Ok((s,(),true))}).unwrap();
 let (entered,ready)=mpsc::channel();let (release,wait)=mpsc::channel();let worker_store=AskStore::at(root.join("asks"));let worker_id=child_id.clone();
 let worker=std::thread::spawn(move||{
  let args=json!({"target":TARGET,"text":"private child task"});let digest=crate::approvals::request_digest(&args);
  worker_store.with_state(|s|s.begin_action(REQUESTER,&worker_id,"agent_delegate",&digest,chrono::Utc::now())).unwrap();
  entered.send(()).unwrap();wait.recv().unwrap();commit_without_reply(&worker_store,&worker_id);
 });
 ready.recv_timeout(Duration::from_secs(10)).unwrap();
 let (world,_)=crate::asks::scheduler::tests::world("receipt-child-race");
 let mut response=json!({"outcome":{"status":"complete","receipt":{"result":{"kind":"created","agent":{"cutex_session_id":TARGET}}}}});
 crate::asks::create_task::attach_with(&mut response,&asks,&world,REQUESTER,"private child task",create_id,false,crate::asks::tools::FACADE_DIRECTOR_ADDRESS);
 let child=&response["outcome"]["receipt"]["result"]["delegation"];
 assert_eq!(child["status"],"response_uncertain","{response}");assert_eq!(child["outcomeUnknown"],true);assert_eq!(child["action_id"],child_id);
 assert!(!child["note"].as_str().unwrap().contains("never got"));assert!(!child["note"].as_str().unwrap().contains("agent_delegate(target"));
 let query=Query {tool:"agent_create".into(),action_id:create_id.into()};assert_eq!(completed(&query,response.clone())["status"],"unknown");
 persist_creation_reply_in(&management,REQUESTER,create_id,Some(&"0".repeat(64)),response.clone()).unwrap();
 let cache=isolated();start_in(cache.clone(),REQUESTER,query.clone(),"parent-digest".into(),move||Ok(response));ended(&cache,REQUESTER,&query);
 let read=||management_query_with(&management.snapshot().unwrap(),REQUESTER,&query,|child|delegation_query(&asks,REQUESTER,child));
 assert_eq!(query_in(&cache,REQUESTER,&query,read)["status"],"unknown");
 release.send(()).unwrap();worker.join().unwrap();
 let management_before=serde_json::to_value(management.snapshot().unwrap()).unwrap();let asks_before=serde_json::to_value(asks.snapshot().unwrap()).unwrap();
 let receipt=query_in(&cache,REQUESTER,&query,read);assert_eq!(receipt["status"],"completed","{receipt}");assert_eq!(receipt["outcomeUnknown"],false);
 assert_eq!(receipt["result"]["outcome"]["receipt"]["result"]["delegation"]["ok"],true);
 assert!(!receipt.to_string().contains("never got"));assert_eq!(asks.snapshot().unwrap().asks.len(),1);
 assert_eq!(serde_json::to_value(management.snapshot().unwrap()).unwrap(),management_before);assert_eq!(serde_json::to_value(asks.snapshot().unwrap()).unwrap(),asks_before);
}

#[test]
fn colliding_display_id_does_not_redirect_child_action_recovery_or_resend() {
 use crate::asks::{scheduler::tests::world, store::tests::TARGET, tools::{ToolContext,TellSend}};
 let root=temp();let asks=AskStore::at(root.join("asks"));
 let management=crate::agent_management::AgentManagementStore::open(&root.join("management")).unwrap();
 let (world,_)=world("receipt-display-collision");
 let create_id="create-display-collision";
 let child_id=crate::asks::create_task::delegation_action_id(create_id);
 let mut response=json!({"outcome":{"status":"complete","receipt":{"result":{"kind":"created","agent":{"cutex_session_id":TARGET}}}}});
 // Real tool creation first: capture exactly the selector the model received.
 crate::asks::create_task::attach_with(&mut response,&asks,&world,REQUESTER,"original child",create_id,false,crate::asks::tools::FACADE_DIRECTOR_ADDRESS);
 let displayed=response["outcome"]["receipt"]["result"]["delegation"]["delegation"].as_str().unwrap().to_owned();
 let original=asks.snapshot().unwrap().asks.values().next().unwrap().clone();
 assert_eq!(original.action_id,child_id);
 let prefix=displayed.strip_prefix("delegation-").expect("model-facing ID");
 assert!(prefix.len()<32,"fixture requires the default abbreviated selector");
 // A later accessible Ask shares the old abbreviation, but has a different
 // action identity. Do not alter the original committed Ask or re-send it.
 asks.with_state(|state| {
  let mut collision=original.clone();
  let mut hex=original.ask_id.strip_prefix("ask-").unwrap().to_owned();
  assert!(hex.starts_with(prefix));
  let replacement=if hex.ends_with('0') {"1"} else {"0"};
  hex.replace_range(31..32,replacement);
  collision.ask_id=format!("ask-{hex}");collision.action_id="different-child-action".into();
  state.asks.insert(collision.ask_id.clone(),collision);
  // Simulate the original child committing before its action reply persisted.
  state.actions.get_mut(&format!("{REQUESTER}\u{1f}{child_id}")).unwrap().result=None;
 }).unwrap();
 let send=|_:&TellSend<'_>|->anyhow::Result<Value>{panic!("read must not send")};
 let wake=|_:&str,_:&str,_:&str|->Option<u32>{panic!("read must not wake")};
 let clock=chrono::Utc::now;let sleep=||panic!("read must not wait");
 let ctx=ToolContext{store:&asks,world:&world,caller:REQUESTER,now:clock(),send:&send,wake_chain:&wake,clock:&clock,sleep:&sleep,wait:false,facade:crate::asks::tools::FACADE_DELEGATIONS};
 let ambiguous=crate::asks::tools::handle(&ctx,json!({"tool":"delegation_read","arguments":{"delegation":displayed},"action_id":"read-collision"}).to_string().as_bytes());
 assert_eq!(ambiguous["ok"],false);assert_eq!(ambiguous["code"],"id_ambiguous","{ambiguous}");
 let record:crate::agent_management::AgentActionRecord=serde_json::from_value(json!({"action_id":create_id,"request_sha256":"0".repeat(64),"operation":"create","project_id":"alpha","caller_cutex_session":REQUESTER,"phase":"prepared","created_at":"2026-10-26T00:00:00Z","updated_at":"2026-10-26T00:00:00Z"})).unwrap();
 management.with_state(true,|mut s|{s.actions.insert(record.action_id.clone(),record);Ok((s,(),true))}).unwrap();
 let child=&mut response["outcome"]["receipt"]["result"]["delegation"];
 child["ok"]=json!(false);child["outcomeUnknown"]=json!(true);
 child["status"]=json!("response_uncertain");
 persist_creation_reply_in(&management,REQUESTER,create_id,Some(&"0".repeat(64)),response).unwrap();
 let query=Query{tool:"agent_create".into(),action_id:create_id.into()};
 let before=serde_json::to_value(asks.snapshot().unwrap()).unwrap();
 let management_before=serde_json::to_value(management.snapshot().unwrap()).unwrap();
 let reads=AtomicUsize::new(0);
 for _ in 0..3 {
  // Cold process caches force the authoritative child lookup on every read;
  // there is no start/execute callback on this path.
  let result=query_in(&isolated(),REQUESTER,&query,||management_query_with(&management.snapshot().unwrap(),REQUESTER,&query,|child|{
   reads.fetch_add(1,Ordering::SeqCst);
   assert_eq!(child.action_id,child_id);assert_eq!(child.tool,"agent_delegate");
   delegation_query(&asks,REQUESTER,child)
  }));
  assert_eq!(result["status"],"completed","{result}");assert_eq!(result["outcomeUnknown"],false);
  assert_eq!(result["result"]["outcome"]["receipt"]["result"]["delegation"]["delegation"],original.ask_id);
 }
 assert_eq!(reads.load(Ordering::SeqCst),3);
 assert_eq!(asks.snapshot().unwrap().asks.len(),2);
 assert_eq!(serde_json::to_value(asks.snapshot().unwrap()).unwrap(),before,"queries must not reserve, enqueue, or complete actions");
 assert_eq!(serde_json::to_value(management.snapshot().unwrap()).unwrap(),management_before);
}

#[test]
fn legacy_not_delegated_unknown_is_normalized_before_final_classification(){
 let query=Query{tool:"agent_create".into(),action_id:"legacy-child".into()};
 let legacy=json!({"outcome":{"status":"complete","receipt":{"result":{"delegation":{"ok":false,"code":"outcome_unknown","note":"never got it; send new agent_delegate"}}}}});
 let receipt=completed(&query,legacy);assert_eq!(receipt["status"],"unknown");assert_eq!(receipt["outcomeUnknown"],true);
 assert_eq!(receipt["result"]["outcome"]["receipt"]["result"]["delegation"]["action_id"],crate::asks::create_task::delegation_action_id("legacy-child"));
 assert!(!receipt.to_string().contains("never got"));
 let refused=json!({"outcome":{"status":"complete","receipt":{"result":{"delegation":{"ok":false,"code":"outside_boundary"}}}}});
 assert_eq!(completed(&query,refused)["status"],"failed");
}
#[test]
fn created_business_survives_full_receipt_storage_error_and_read_reconciles(){
 use crate::asks::store::tests::TARGET;
 let root=temp();let management=crate::agent_management::AgentManagementStore::open(&root.join("management")).unwrap();let asks=AskStore::at(root.join("asks"));
 let action_id="create-receipt-write-fault";let child_id=crate::asks::create_task::delegation_action_id(action_id);let now="2026-10-26T00:00:00Z";
 let spec=json!({"name":"fixture","cwd":root.to_string_lossy(),"profile":"fixture","runtime_backend":"codex","model":"fixture","reasoning":"medium","permissions":"fixture","approval_policy":"never","sandbox_mode":"read-only","groups":[]});
 let agent=json!({"project_id":"alpha","created_by_director_session":REQUESTER,"cutex_session_id":TARGET,"native_session_id":"fixture-native","spec":spec,"created_at":now});
 let observation=json!({"cutex_session_id":TARGET,"native_session_id":"fixture-native","active":false,"cwd":root.to_string_lossy(),"profile":"fixture","runtime_backend":"codex","model":"fixture","reasoning":"medium","permissions":"fixture","approval_policy":"never","sandbox_mode":"read-only","groups":[],"runtime_generation":1,"app_server_runtime":true});
 let response=json!({"schema":"cutex/agent-management/v1","action_id":action_id,"outcome":{"status":"complete","receipt":{"schema":"cutex/agent-management-receipt/v1","action_id":action_id,"request_sha256":"0".repeat(64),"operation":"create","project_id":"alpha","completed_at":now,"result":{"kind":"created","agent":agent,"observation":observation}}}});
 let record:crate::agent_management::AgentActionRecord=serde_json::from_value(json!({"action_id":action_id,"request_sha256":"0".repeat(64),"operation":"create","project_id":"alpha","caller_cutex_session":REQUESTER,"phase":"ready","response":response,"created_at":now,"updated_at":now})).unwrap();
 // Commit real provider evidence; deliberately corrupt only the optional full
 // reply extension so persist_creation_reply_in really returns InvalidStore.
 management.with_state(true,|mut s|{s.actions.insert(record.action_id.clone(),record);s.extra.insert(RECEIPTS_EXTENSION.into(),json!("injected-full-receipt-storage-failure"));Ok((s,(),true))}).unwrap();
 reserve(&asks,&child_id);commit_without_reply(&asks,&child_id);
 let cache=isolated();let query=Query{tool:"agent_create".into(),action_id:action_id.into()};let worker_store=management.clone();
 let (entered,ready)=mpsc::channel();let (release,wait)=mpsc::channel();
 start_in(cache.clone(),REQUESTER,query.clone(),"digest".into(),move||{entered.send(()).unwrap();wait.recv().unwrap();persist_creation_reply_in(&worker_store,REQUESTER,action_id,Some(&"0".repeat(64)),response)});
 ready.recv_timeout(Duration::from_secs(10)).unwrap();assert_eq!(query_in(&cache,REQUESTER,&query,||panic!("running"))["status"],"running");release.send(()).unwrap();ended(&cache,REQUESTER,&query);
 assert_eq!(cache.lock().unwrap().rows[&key(REQUESTER,&query)].result.as_ref().unwrap()["status"],"unknown");
 let before=serde_json::to_value(management.snapshot().unwrap()).unwrap();
 let receipt=query_in(&cache,REQUESTER,&query,||management_query_with(&management.snapshot().unwrap(),REQUESTER,&query,|child|delegation_query(&asks,REQUESTER,child)));
 assert_eq!(receipt["status"],"completed","{receipt}");assert_eq!(asks.snapshot().unwrap().asks.len(),1);assert_eq!(serde_json::to_value(management.snapshot().unwrap()).unwrap(),before);
}
#[test]
fn missing_cache_old_action_and_full_budget_do_not_claim_original_no_write(){
 let root=temp();let store=AskStore::at(root.join("asks"));
 let committed=q("old-committed");let uncertain=q("old-reserved");reserve(&store,&committed.action_id);commit_without_reply(&store,&committed.action_id);reserve(&store,&uncertain.action_id);
 let cache=isolated();let mut releases=vec![];let mut pending=vec![];
 for i in 0..MAX_WORKERS {
  let caller=if i<MAX_CALLER_WORKERS {REQUESTER.to_owned()} else {format!("budget-{}",i/MAX_CALLER_WORKERS)};let query=q(&format!("held-{i}"));let (release,wait)=mpsc::channel();let (entered,ready)=mpsc::channel();
  start_in(cache.clone(),&caller,query.clone(),"digest".into(),move||{entered.send(()).unwrap();wait.recv().unwrap();Ok(json!({"ok":true}))});
  ready.recv_timeout(Duration::from_secs(10)).unwrap();releases.push(release);pending.push((caller,query));
  if i==MAX_CALLER_WORKERS-1 {
   for old in [&committed,&uncertain] {
    let denied=start_in(cache.clone(),REQUESTER,old.clone(),"digest".into(),||panic!("caller budget"));
    assert_eq!(denied["status"],"unknown");assert_eq!(denied["outcomeUnknown"],true);assert_eq!(denied["retryAdmission"]["code"],"action_caller_capacity");
   }
  }
 }
 let before=serde_json::to_value(store.snapshot().unwrap()).unwrap();
 // Old outcomes are absent from this process, as after restart or eviction.
 for query in [&committed,&uncertain] {
  assert!(!cache.lock().unwrap().rows.contains_key(&key(REQUESTER,query)));
  let denied=start_in(cache.clone(),REQUESTER,query.clone(),"digest".into(),||panic!("budget must reject before spawning"));
  assert_eq!(denied["status"],"unknown");assert_eq!(denied["outcomeUnknown"],true);
  assert_eq!(denied["retryAdmission"]["status"],"no_write");assert_eq!(denied["retryAdmission"]["code"],"action_worker_capacity");
  assert!(denied.get("result").is_none());assert!(valid_response(&denied,&query.tool,&query.action_id));
 }
 assert_eq!(query_in(&cache,REQUESTER,&committed,||delegation_query(&store,REQUESTER,&committed))["status"],"completed");
 assert_eq!(query_in(&cache,REQUESTER,&uncertain,||delegation_query(&store,REQUESTER,&uncertain))["status"],"unknown");
 assert_eq!(serde_json::to_value(store.snapshot().unwrap()).unwrap(),before);
 for release in releases {release.send(()).unwrap();}for (caller,query) in pending {ended(&cache,&caller,&query);}
}
