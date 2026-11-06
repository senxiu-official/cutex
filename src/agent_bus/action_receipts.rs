//! Read-only receipts for slow tools. Business authority/idempotency remains
//! in AgentManagementProvider and AskStore. A query never resumes an action.
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, LockResult, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const TOOL: &str = "action_status";
pub const HEADER: &str = "x-cutex-action-receipt";
pub const SCHEMA: &str = "cutex.tool-action-receipt.v1";
const RECEIPTS_EXTENSION: &str = "tool_action_receipts_v1";
#[path = "action_receipt_dispatch.rs"]
pub mod dispatch;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub action_id: String,
    pub tool: String,
}

impl Query {
    pub fn valid(&self) -> bool {
        matches!(self.tool.as_str(), "agent_create" | "agent_delegate")
            && !self.action_id.is_empty() && self.action_id.len() <= 128
            && self.action_id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._:-/@".contains(&c))
    }
}

pub fn definition() -> Value {
    json!({"name":TOOL,"description":"Read the outcome of your agent_create or agent_delegate by its exact action_id. Read-only: never starts, retries or resumes work. Unknown/notFound is not proof that nothing happened. running means the daemon is still handling this call; completed means the tool operation finished (not that delegated work finished); awaitingApproval (approval_id) means the owner must approve: result.approval.state says whether the owner has decided (approved: repeat the identical call now with the same action_id; denied or expired answer failed), and a delegation under owner review shows its current state once reviewed.",
        "annotations":{"readOnlyHint":true},
        "inputSchema":{"type":"object","additionalProperties":false,"required":["action_id","tool"],
            "properties":{"action_id":{"type":"string","minLength":1,"maxLength":128},"tool":{"type":"string","enum":["agent_create","agent_delegate"]}}}})
}

pub fn valid_response(value: &Value, tool: &str, action_id: &str) -> bool {
    let status=value["status"].as_str().unwrap_or("");
    value["schema"] == SCHEMA && value["tool"] == tool && value["action_id"] == action_id
        && value["ok"] == true
        && value["outcomeUnknown"] == json!(matches!(status,"unknown"|"notFound"))
        && matches!(status,"running"|"completed"|"failed"|"awaitingApproval"|"unknown"|"notFound")
        && value.get("result").is_none_or(Value::is_object)
        && value.get("retryAdmission").is_none_or(|a| a.is_object() && a["status"]=="no_write" && a["retryable"]==true
            && matches!(a["code"].as_str(),Some("action_worker_capacity"|"action_caller_capacity"|"action_receipt_capacity"|"action_worker_unavailable")))
        && (!matches!(status,"completed"|"failed"|"awaitingApproval") || value["result"].is_object())
}

fn envelope(q: &Query, status: &str, unknown: bool, result: Option<Value>) -> Value {
    let mut value = json!({"schema":SCHEMA,"ok":true,"action_id":q.action_id,"tool":q.tool,
        "status":status,"outcomeUnknown":unknown});
    if let Some(result) = result { value["result"] = result; }
    value
}

fn completed(q: &Query, mut result: Value) -> Value {
    crate::asks::create_task::normalize_uncertain_attachment(&mut result, &q.action_id);
    if !result.is_object() { return envelope(q,"unknown",true,None); }
    // The launch policy answers a pending owner approval as a no_write whose
    // code is approval_required; it must stay retryable, not a final failure.
    let approval = result["outcome"]["status"] == "no_write" && result["outcome"]["code"] == "approval_required";
    if approval && result["approval_id"].is_null() {
        if let Some(id) = approval_id(result["outcome"]["detail"].as_str().unwrap_or("")) { result["approval_id"] = json!(id); }
    }
    let status = if approval || result["outcome"]["status"] == "approval_required" || result["status"] == "approval_required"
        || result["state"] == "pendingReview" { "awaitingApproval" }
    else if result["outcome"]["status"] == "response_uncertain" || result["status"] == "response_uncertain"
        || result["outcome"]["status"] == "owner_action_required" || result["outcomeUnknown"] == true
        || crate::asks::create_task::is_uncertain(&result["outcome"]["receipt"]["result"]["delegation"]) { "unknown" }
    else if result["ok"] == false || result["outcome"]["status"] == "no_write"
        || result["outcome"]["receipt"]["result"]["delegation"]["ok"] == false { "failed" }
    else if result["ok"] == true || result["outcome"]["status"] == "complete" { "completed" }
    else { "unknown" };
    envelope(q, status, status == "unknown", Some(result))
}

/// The policy refusal carries the id only inside its detail text, as
/// "... (approval_id <id>)".
fn approval_id(detail: &str) -> Option<&str> {
    let id = detail.rsplit_once("(approval_id ")?.1.strip_suffix(')')?;
    (!id.is_empty() && !id.contains(char::is_whitespace)).then_some(id)
}

fn refused(q: &Query, code: &str) -> Value {
    completed(q,json!({"ok":false,"action_id":q.action_id,"status":"no_write","code":code}))
}

// These are admission limits, not timeouts: blocked business locks consume a
// permit until the worker actually exits. No queue and no thread per rejected key.
const MAX_WORKERS: usize = 8;
const MAX_CALLER_WORKERS: usize = 2;
const MAX_CACHED_RESULTS: usize = 256;
/// How long an explicit start (or same-digest retry) waits for its worker
/// before answering running. Below every façade's client timeout (the oldest
/// adapters default to 30 s). The wait holds no business lock and no cache
/// mutex; at most MAX_WAITERS callers wait at once, the rest answer running.
pub const START_WAIT: Duration = Duration::from_secs(20);
const MAX_WAITERS: usize = 32;
/// Admission says only that THIS invocation did not obtain a worker. A missing
/// process cache (restart/eviction/another process) is not evidence that the
/// original action has no effects. Keep its outcome unknown, separately report
/// the retry's no-write decision; query can reconcile authoritative evidence.
fn not_admitted(q: &Query, code: &str, previous: Option<Value>) -> Value {
    let mut value=previous.unwrap_or_else(||envelope(q,"unknown",true,None));
    value["retryAdmission"]=json!({"status":"no_write","code":code,"retryable":true});
    value
}

#[derive(Clone)]
struct Running {
    digest: String,
    generation: u64,
    result: Option<Value>,
}
type Key = (String, String, String);
#[derive(Default)]
struct Cache { rows: BTreeMap<Key, Running>, generation: u64, waiters: usize }
/// The projection plus a signal raised whenever a worker ends.
#[derive(Default)]
struct Shared { state: Mutex<Cache>, ended: Condvar }
impl Shared {
    fn lock(&self) -> LockResult<MutexGuard<'_, Cache>> { self.state.lock() }
}
type SharedCache = Arc<Shared>;
static RUNNING: OnceLock<SharedCache> = OnceLock::new();
fn calls() -> &'static SharedCache { RUNNING.get_or_init(Default::default) }
fn key(caller: &str, q: &Query) -> Key { (caller.into(), q.tool.clone(), q.action_id.clone()) }

/// Only an explicit authenticated request calls start. The cache is a bounded
/// projection, NOT idempotency authority: every execution (including retries
/// after eviction/restart) must use the original provider/AskStore action ID.
/// Neither eviction nor query schedules work, releases a reservation or invents
/// a new action ID. Unknown replies can be retried explicitly through that same
/// durable idempotency layer, never through an unfenced direct business closure.
pub fn start(caller: &str, q: Query, digest: String,
    run: impl FnOnce() -> anyhow::Result<Value> + Send + 'static) -> Value {
    start_in(Arc::clone(calls()),START_WAIT,caller,q,digest,run)
}

/// Ends a worker's row even if the worker unwinds past catch_unwind, then
/// wakes every waiter. A missing value is recorded as unknown, never failed.
struct WorkerEnd { cache: SharedCache, key: Key, generation: u64, query: Query, value: Option<Value> }
impl Drop for WorkerEnd {
    fn drop(&mut self) {
        let value=self.value.take().unwrap_or_else(||envelope(&self.query,"unknown",true,None));
        let mut state=self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(row)=state.rows.get_mut(&self.key) {
            if row.generation==self.generation && row.result.is_none() {row.result=Some(value);}
        }
        drop(state);
        self.cache.ended.notify_all();
    }
}

/// Waits (cache mutex released) until this generation ends or `wait` passes.
/// Anything else (timeout, eviction, a newer generation) answers running.
fn await_end(cache: &Shared, mut state: MutexGuard<'_, Cache>, k: &Key, generation: u64, wait: Duration, q: &Query) -> Value {
    let pending=|s: &mut Cache| s.rows.get(k).is_some_and(|r| r.generation==generation && r.result.is_none());
    if !wait.is_zero() && pending(&mut state) && state.waiters<MAX_WAITERS {
        state.waiters+=1;
        state=match cache.ended.wait_timeout_while(state,wait,pending) {
            Ok((state,_))=>state,
            Err(poisoned)=>poisoned.into_inner().0,
        };
        state.waiters-=1;
    }
    state.rows.get(k).filter(|r| r.generation==generation).and_then(|r| r.result.clone())
        .unwrap_or_else(|| envelope(q,"running",false,None))
}

fn start_in(cache: SharedCache, wait: Duration, caller: &str, q: Query, digest: String,
    run: impl FnOnce() -> anyhow::Result<Value> + Send + 'static) -> Value {
    if !q.valid() { return refused(&q,"invalid_arguments"); }
    let k = key(caller, &q);
    let mut state = match cache.lock() { Ok(rows) => rows, Err(_) => return envelope(&q,"unknown",true,None) };
    let previous=state.rows.get(&k).cloned();
    if let Some(row) = &previous {
        if row.digest != digest { return refused(&q,"idempotency_conflict"); }
        if row.result.as_ref().is_none_or(|r| !matches!(r["status"].as_str(),Some("unknown"|"awaitingApproval"))
            && r["result"]["outcome"]["receipt"]["result"]["delegation"]["ok"] != false) {
            // Same-digest retry: replay a final result, or wait on the running worker.
            if let Some(result)=&row.result {return result.clone();}
            return await_end(&cache,state,&k,row.generation,wait,&q);
        }
    }
    let active=state.rows.iter().filter(|(_,r)|r.result.is_none()).count();
    let caller_active=state.rows.iter().filter(|(k,r)|k.0==caller && r.result.is_none()).count();
    let denied=if active>=MAX_WORKERS {Some("action_worker_capacity")}
        else if caller_active>=MAX_CALLER_WORKERS {Some("action_caller_capacity")} else {None};
    if let Some(code)=denied {
        // The retry wasn't admitted, but the original action may have effects.
        return not_admitted(&q,code,previous.as_ref().and_then(|r|r.result.clone()));
    }
    if previous.is_none() && state.rows.len()>=MAX_CACHED_RESULTS {
        // Drop only an ended worker's *projection*, never its durable action.
        // Unknown may remain unknown on disk; no replay is triggered here.
        let ended=state.rows.iter().filter(|(_,r)|r.result.is_some())
            .min_by_key(|(_,r)|r.generation).map(|(k,_)|k.clone());
        if let Some(ended)=ended {state.rows.remove(&ended);}
        else {return not_admitted(&q,"action_receipt_capacity",None);}
    }
    state.generation+=1;let generation=state.generation;
    state.rows.insert(k.clone(),Running {digest,generation,result:None});
    // The guard is built inside the thread: a closure dropped by a failed
    // spawn must not re-lock the cache this function still holds.
    let query=q.clone();let worker_key=k.clone();let worker_cache=Arc::clone(&cache);
    let spawned=std::thread::Builder::new().name("tool-action-receipt".into()).spawn(move || {
        let mut end=WorkerEnd {cache:worker_cache,key:worker_key,generation,query,value:None};
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        end.value=Some(match result {Ok(Ok(result))=>completed(&end.query,result),_=>envelope(&end.query,"unknown",true,None)});
    });
    if spawned.is_err() {
        if let Some(previous)=previous {let response=previous.result.clone();state.rows.insert(k,previous);return not_admitted(&q,"action_worker_unavailable",response);}
        state.rows.remove(&k);return not_admitted(&q,"action_worker_unavailable",None);
    }
    await_end(&cache,state,&k,generation,wait,&q)
}

pub fn query(caller: &str, args: Value) -> Value {
    let q: Query=match serde_json::from_value(args) {
        Ok(q) if Query::valid(&q)=>q,
        _=>return json!({"ok":false,"status":"no_write","code":"invalid_arguments"}),
    };
    query_with(calls(),caller,&q,||durable_query(caller,&q),live_approval)
}

#[cfg(test)]
fn query_in(cache: &SharedCache, caller: &str, q: &Query,
    durable: impl FnOnce()->anyhow::Result<Value>) -> Value {
    query_with(cache,caller,q,durable,|_| None)
}

/// approval-live-state: awaitingApproval is not final. A delegation under the
/// owner's review answers its live state (the durable read); an agent_create
/// refused for the owner's approval says what the owner decided (`approval`
/// looks the record up by its id). Neither retries anything.
fn query_with(cache: &SharedCache, caller: &str, q: &Query,
    durable: impl FnOnce()->anyhow::Result<Value>,
    approval: impl Fn(&str)->Option<crate::approvals::ApprovalRecord>) -> Value {
    let k=key(caller,q);
    let cached=cache.lock().ok().and_then(|s|s.rows.get(&k).cloned());
    let mut waiting=None;
    if let Some(row)=&cached {
        if row.result.is_none() {return envelope(q,"running",false,None);}
        // Legacy nested uncertainty is normalized before considering finality.
        let value=row.result.as_ref().unwrap();
        let value=value.get("result").cloned().map(|r|completed(q,r)).unwrap_or_else(||value.clone());
        match value["status"].as_str() {
            Some("awaitingApproval") => waiting=Some(value),
            Some("unknown"|"failed") => {}
            _ => return value,
        }
    }
    // No mutex across disk IO and NO business execution from this path.
    let authoritative=durable().unwrap_or_else(|_|envelope(q,"unknown",true,None));
    if let Some(waiting)=waiting {
        // The live delegation / the created agent wins once it exists; else
        // the call still waits for (or was refused by) the owner.
        let live=!matches!(authoritative["status"].as_str(),Some("notFound"|"unknown"));
        let value=if live {authoritative} else {waiting};
        if live && value["status"]!="awaitingApproval" {
            if let Ok(mut state)=cache.lock() {
                if let Some(row)=state.rows.get_mut(&k) {
                    let old=cached.as_ref().map(|r|r.generation);
                    if Some(row.generation)==old && row.result.is_some() {row.result=Some(value.clone());}
                }
            }
        }
        return with_approval(value,&approval);
    }
    let value=if authoritative["status"]=="notFound" && cached.is_some() {
        let old=cached.as_ref().and_then(|r|r.result.as_ref()).unwrap();
        let old=old.get("result").cloned().map(|r|completed(q,r)).unwrap_or_else(||old.clone());
        if old["status"]=="failed" {old} else {envelope(q,"unknown",true,None)} // Missing evidence never proves no effects.
    } else {authoritative};
    if let Some(old)=cached {
        if let Ok(mut state)=cache.lock() {
            if let Some(row)=state.rows.get_mut(&k) {
                // A concurrent explicit retry owns its newer generation/result.
                if row.generation==old.generation && row.result.is_some() {row.result=Some(value.clone());}
            }
        }
    }
    with_approval(value,&approval)
}

/// An awaitingApproval agent_create receipt with the owner's decision so far
/// (`result.approval`). Approved stays awaitingApproval: only the identical
/// retry creates the agent. Denied and expired are failed. Not cached: a
/// retry still runs and asks the launch policy again.
fn with_approval(value: Value, approval: &dyn Fn(&str)->Option<crate::approvals::ApprovalRecord>) -> Value {
    if value["tool"]!="agent_create" || value["status"]!="awaitingApproval" {return value;}
    let record=value["result"]["approval_id"].as_str().and_then(approval);
    approval_view(value,record.as_ref())
}

fn approval_view(mut value: Value, record: Option<&crate::approvals::ApprovalRecord>) -> Value {
    use crate::approvals::ApprovalState;
    let Some(record)=record else {return value;};
    let (state,status,next)=match record.state {
        ApprovalState::Pending=>("pending","awaitingApproval","The owner has not decided yet; you are told when they do. Do not poll."),
        ApprovalState::Approved=>("approved","awaitingApproval","The owner approved: repeat the identical agent_create call now with the same action_id; it proceeds once (the approval lapses after 24 h)."),
        ApprovalState::Consumed=>("consumed","unknown","The approval was used by a retry of this call; query again for that retry's outcome."),
        ApprovalState::Denied=>("denied","failed","The owner denied this launch; do not retry the same call."),
        ApprovalState::Expired=>("expired","failed","The approval lapsed before it was used; an identical call asks the owner again."),
    };
    value["status"]=json!(status);
    value["outcomeUnknown"]=json!(status=="unknown");
    let mut approval=json!({"approval_id":record.approval_id,"state":state,"next":next});
    if let (ApprovalState::Denied,Some(note))=(record.state,record.note.as_deref()) {
        approval["note"]=json!(note.chars().take(500).collect::<String>());
    }
    value["result"]["approval"]=approval;
    value
}

/// After a restart the refusal is gone: an agent_create whose resolution was
/// recorded but that never reached the provider is found by the approval of
/// its launch request (requester + digest, newest first).
fn restart_approval(snapshot: &crate::agent_management::AgentManagementSnapshot, caller: &str, q: &Query,
    find: impl FnOnce(&str)->Option<crate::approvals::ApprovalRecord>) -> Option<Value> {
    if q.tool!="agent_create" {return None;}
    let id=crate::agent_management::AgentActionId::new(&q.action_id).ok()?;
    if snapshot.actions.contains_key(&id) {return None;}
    let sparse=snapshot.sparse_requests.get(&id).filter(|s|s.caller_cutex_session.as_str()==caller)?;
    let record=find(&crate::agent_bus::server::agent_profile_approval_digest(&sparse.request))?;
    let result=json!({"outcome":{"status":"no_write","code":"approval_required"},"approval_id":record.approval_id});
    Some(approval_view(envelope(q,"awaitingApproval",false,Some(result)),Some(&record)))
}

fn live_approval(id: &str) -> Option<crate::approvals::ApprovalRecord> {
    crate::approvals::ApprovalStore::open_default().ok()?.get(id,chrono::Utc::now()).ok().flatten()
}

const RETRY_CREATE_HINT: &str = "The agent exists, but its runtime did not start and its task (if any) is not delivered yet. \
Retry the same agent_create (same action_id and arguments), or start it with member_runtime online: either finishes the create and delivers the task.";

/// The allowlisted reason of an `owner_action_required` failure detail
/// (`[owner_action_required: ]<code>: …`) and whether an exact retry of the
/// action is the way on.
fn failure_reason(detail: &str) -> Option<(&'static str, bool)> {
    let detail = detail.strip_prefix("owner_action_required: ").unwrap_or(detail);
    [("agent_bus_observation_failed", "agent_bus_observation_failed", false),
     ("agent_bus_unavailable", "agent_bus_unavailable", true),
     ("session_online_failed", "runtime_start_failed", true)]
        .into_iter()
        .find(|(code, _, _)| detail.strip_prefix(code).is_some_and(|rest| rest.starts_with(": ")))
        .map(|(_, reason, retry)| (reason, retry))
}

fn durable_query(caller: &str, q: &Query) -> anyhow::Result<Value> {
    if q.tool == "agent_delegate" {
        let store = crate::asks::store::AskStore::open_default()?;
        return delegation_query(&store, caller, q);
    }
    let store = crate::agent_management::AgentManagementStore::open_default()?;
    let snapshot = store.snapshot()?;
    let value = management_query(&snapshot,caller,q)?;
    if value["status"]!="notFound" {return Ok(value);}
    Ok(restart_approval(&snapshot,caller,q,|digest| {
        let store=crate::approvals::ApprovalStore::open_default().ok()?;
        store.list(true,chrono::Utc::now()).ok()?.into_iter()
            .find(|record| record.requester_cutex_session_id==caller && record.request_sha256==digest)
    }).unwrap_or(value))
}

fn management_query(snapshot: &crate::agent_management::AgentManagementSnapshot, caller: &str, q: &Query) -> anyhow::Result<Value> {
    management_query_with(snapshot,caller,q,|child| {
        let store=crate::asks::store::AskStore::open_default()?;
        delegation_query(&store,caller,child)
    })
}
fn management_query_with(snapshot: &crate::agent_management::AgentManagementSnapshot, caller: &str, q: &Query,
    read_child: impl FnOnce(&Query)->anyhow::Result<Value>) -> anyhow::Result<Value> {
    let action=snapshot.actions.values().find(|a|a.action_id.as_str()==q.action_id
        && a.caller_cutex_session.as_str()==caller && a.operation==crate::agent_management::AgentOperationKind::Create);
    let Some(action)=action else {return Ok(envelope(q,"notFound",true,None));};
    let stored=snapshot.tool_action_receipt(&q.action_id);
    let mut result=if let Some(receipt)=stored {
        anyhow::ensure!(receipt["caller"]==caller && receipt["request_sha256"]==action.request_sha256.as_str(),"tool receipt does not match its original action");
        let result=receipt.get("result").filter(|v|v.is_object()).ok_or_else(||anyhow::anyhow!("invalid stored tool receipt"))?.clone();
        let receipt=completed(q,result.clone());
        // Full successful replies (including create without a task) are stable.
        // A child-unknown/full-reply failure is only a projection; consult the
        // original child action/committed ask instead of freezing that failure.
        if receipt["status"]!="unknown" && result["outcome"]["receipt"]["result"]["delegation"]["ok"]!=false {return Ok(receipt);}
        result
    } else {
        let Some(response)=&action.response else {
            // Durable evidence, not a synthesized business response. Captured
            // identities/phases do not prove readiness or authorize a retry.
            // Keep free-form failure detail, paths and routing authority out of
            // the caller projection; references identify the existing journal.
            let matching_failures = snapshot.failure_events.values()
                .filter(|failure| failure.action_id == action.action_id
                    && failure.project_id == action.project_id
                    && failure.operation == action.operation);
            let failure_count = matching_failures.clone().count();
            let failures: Vec<_> = matching_failures.take(16)
                .map(|failure| {
                    let mut reference = json!({"eventId":failure.event_id,"code":failure.code,
                        "createdAt":failure.created_at});
                    // lifecycle_error writes this exact code prefix. Expose
                    // only the allowlisted constant, never its diagnostic tail.
                    if failure.code == "owner_action_required" {
                        if let Some((reason, retry)) = failure_reason(&failure.detail) {
                            reference["reasonCode"] = json!(reason);
                            if retry { reference["retry"] = json!("exact"); }
                        }
                    }
                    reference
                }).collect();
            let mut context = json!({
                "phase":action.phase,"phaseSequence":action.phase_sequence,
                "knownSuccessorCutexSession":action.known_successor_cutex_session,
                "knownNativeSessionId":action.known_native_session_id,
                "failureCount":failure_count,"failures":failures
            });
            // The agent exists but its runtime did not start (r276): say how
            // to finish the create instead of leaving an offline agent.
            let latest = context["failures"].as_array()
                .and_then(|failures| failures.iter().max_by_key(|f| f["createdAt"].as_str().unwrap_or_default().to_string()));
            if action.operation == crate::agent_management::AgentOperationKind::Create
                && latest.is_some_and(|failure| failure["retry"] == "exact") {
                context["retry"] = json!("exact");
                context["hint"] = json!(RETRY_CREATE_HINT);
            }
            return Ok(envelope(q,"unknown",true,Some(json!({"durableContext":context}))));
        };
        serde_json::to_value(response)?
    };
    if let Some(sparse)=snapshot.sparse_requests.get(&action.action_id) {
        if let Some(digest)=result.pointer_mut("/outcome/receipt/request_sha256") {*digest=json!(sparse.sparse_sha256.as_str());}
    }
    if result["outcome"]["status"]=="complete" {
        let child=Query {action_id:crate::asks::create_task::delegation_action_id(&q.action_id),tool:"agent_delegate".into()};
        let receipt=read_child(&child)?;
        if let Some(child_result)=receipt.get("result") {
            result["outcome"]["receipt"]["result"]["delegation"]=child_result.clone();
            if receipt["outcomeUnknown"]==true {
                result["outcome"]["receipt"]["result"]["delegation"]["outcomeUnknown"]=json!(true);
            }
            result["taskReceiptAvailable"]=json!(true);
        } else {
            crate::asks::create_task::normalize_uncertain_attachment(&mut result,&q.action_id);
            result["taskReceiptAvailable"]=json!(false);result["completionScope"]=json!("creationOnly");
            // A persisted explicit pre-effect child refusal remains a refusal;
            // an absent/unfinished child cannot turn unknown into no_write.
            if stored.is_some() && !crate::asks::create_task::is_uncertain(&result["outcome"]["receipt"]["result"]["delegation"]) {
                return Ok(completed(q,result));
            }
            return Ok(envelope(q,"unknown",true,Some(result)));
        }
    }
    Ok(completed(q,result))
}

/// Final reply attached to the original provider action through the existing
/// additive extension map. Older store writers preserve it. This is receipt
/// data only, never a new request, queue, action identity or recovery runner.
pub fn persist_creation_reply(caller: &str, action_id: &str, expected_digest: Option<&str>, result: Value) -> anyhow::Result<Value> {
    let store=crate::agent_management::AgentManagementStore::open_default()?;
    persist_creation_reply_in(&store,caller,action_id,expected_digest,result)
}

fn persist_creation_reply_in(store: &crate::agent_management::AgentManagementStore, caller: &str,
    action_id: &str, expected_digest: Option<&str>, result: Value) -> anyhow::Result<Value> {
    // Refusals and uncertain recovery responses must not poison an existing
    // successful action's receipt, especially on changed-payload retries.
    if result["outcome"]["status"] != "complete" { return Ok(result); }
    let id=crate::agent_management::AgentActionId::new(action_id)?;
    Ok(store.with_state(true,|mut state| {
        let Some(action)=state.actions.get(&id) else {
            return Ok((state,result,false)); // refusal before provider admission
        };
        if action.caller_cutex_session.as_str()!=caller || action.operation!=crate::agent_management::AgentOperationKind::Create {
            return Err(crate::agent_management::AgentManagementError::Unauthorized);
        }
        let digest=action.request_sha256.as_str().to_owned();
        if expected_digest != Some(digest.as_str()) {
            return Err(crate::agent_management::AgentManagementError::InvalidStore);
        }
        // One row per receipt (058 S4); a non-object legacy value is damage.
        if state.extra.get(RECEIPTS_EXTENSION).is_some_and(|receipts|!receipts.is_object()) {
            return Err(crate::agent_management::AgentManagementError::InvalidStore);
        }
        if let Some(previous)=state.tool_action_receipt(action_id) {
            if previous["caller"] != caller || previous["request_sha256"] != digest {
                return Err(crate::agent_management::AgentManagementError::InvalidStore);
            }
            // An explicit exact retry may repair the original task attachment
            // through create_task's existing durable child action. Queries do
            // not repair anything. Preserve the first successful full reply.
            let query=Query {tool:"agent_create".into(),action_id:action_id.into()};
            if completed(&query,previous["result"].clone())["status"]!="unknown"
                && previous["result"]["outcome"]["receipt"]["result"]["delegation"]["ok"] != false {
                let previous=previous["result"].clone();
                return Ok((state,previous,false));
            }
        }
        state.tool_action_receipts.insert(action_id.into(),json!({"caller":caller,"request_sha256":digest,"result":result}));
        Ok((state,result,true))
    })?)
}

fn delegation_query(store: &crate::asks::store::AskStore, caller: &str, q: &Query) -> anyhow::Result<Value> {
    let state = store.peek()?.unwrap_or_default();
    let action = state.actions.get(&format!("{caller}\u{1f}{}",q.action_id)).filter(|a| a.tool == q.tool);
    let mut waiting=None;
    if let Some(result) = action.and_then(|a| a.result.as_ref()) {
        let receipt=completed(q,result.clone());
        // A reply that waited for the owner's review is not final: the
        // delegation's live state answers.
        match receipt["status"].as_str() {
            Some("unknown") => {}
            Some("awaitingApproval") => waiting=Some(receipt),
            _ => return Ok(receipt),
        }
    }
    if let Some(mut result) = crate::asks::tools::live_delegation_view(store,caller,&q.action_id)? {
        result["ok"] = json!(true);
        result["created"] = json!(false);
        return Ok(completed(q, crate::asks::tools::delegation_terms(result)));
    }
    if let Some(waiting)=waiting {return Ok(waiting);}
    Ok(envelope(q,if action.is_some() {"unknown"} else {"notFound"},true,None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_worker_is_fast_scoped_and_never_replayed() {
        let q = Query {tool:"agent_create".into(),action_id:format!("test-{}",uuid::Uuid::new_v4())};
        let cache: SharedCache = Default::default();
        let (release, wait) = std::sync::mpsc::channel();
        let result = start_in(cache.clone(),Duration::ZERO,"caller",q.clone(),"digest".into(),move || {
            wait.recv().unwrap(); Ok(json!({"outcome":{"status":"complete"}}))
        });
        assert_eq!(result["status"],"running");
        assert_eq!(start_in(cache.clone(),Duration::ZERO,"caller",q.clone(),"digest".into(),|| panic!("replayed"))["status"],"running");
        assert_eq!(start_in(cache.clone(),Duration::ZERO,"caller",q.clone(),"different".into(),|| panic!("changed payload"))["result"]["code"],"idempotency_conflict");
        assert!(!cache.lock().unwrap().rows.contains_key(&key("stranger",&q)));
        release.send(()).unwrap();
    }
    #[test]
    fn status_does_not_confuse_approval_unknown_or_failure_with_success() {
        let q=Query {tool:"agent_create".into(),action_id:"a".into()};
        for (result,status) in [(json!({"outcome":{"status":"approval_required"}}),"awaitingApproval"),
            (json!({"outcome":{"status":"response_uncertain"}}),"unknown"),
            (json!({"outcome":{"status":"no_write"}}),"failed"),
            (json!({"outcome":{"status":"complete"}}),"completed")] {
            assert_eq!(completed(&q,result)["status"],status);
        }
        assert!(serde_json::from_value::<Query>(json!({"tool":"agent_create","action_id":"a","caller":"other"})).is_err());
        assert_eq!(completed(&q,json!("malformed"))["status"],"unknown");
        let mut receipt=completed(&q,json!({"outcome":{"status":"complete"}}));
        assert!(valid_response(&receipt,"agent_create","a"));
        assert!(!valid_response(&receipt,"agent_create","foreign"));
        receipt["outcomeUnknown"]=json!(true);
        assert!(!valid_response(&receipt,"agent_create","a"));
        receipt["outcomeUnknown"]=json!(false);
        receipt.as_object_mut().unwrap().remove("result");
        assert!(!valid_response(&receipt,"agent_create","a"));
    }

    #[test]
    fn durable_delegation_query_recovers_commit_without_action_reply_and_does_not_write() {
        use crate::asks::store::{AskStore, tests::{new_ask,REQUESTER}};
        let root = std::env::temp_dir().join(format!("receipt-{}",uuid::Uuid::new_v4()));
        let store = AskStore::at(root.join("asks"));
        store.with_state(|s| s.create_ask(new_ask("committed","private brief"),chrono::Utc::now())).unwrap().unwrap();
        let q=Query {tool:"agent_delegate".into(),action_id:"committed".into()};
        let before=serde_json::to_value(store.snapshot().unwrap()).unwrap();
        let receipt=delegation_query(&store,REQUESTER,&q).unwrap();
        assert_eq!(receipt["status"],"completed");
        assert_eq!(receipt["result"]["created"],false);
        assert_eq!(delegation_query(&store,"stranger",&q).unwrap()["status"],"notFound");
        assert_eq!(serde_json::to_value(store.snapshot().unwrap()).unwrap(),before);
        // Dropping/reopening the handle simulates loss of the response cache.
        drop(store);
        assert_eq!(delegation_query(&AskStore::at(root.join("asks")),REQUESTER,&q).unwrap(),receipt);
    }

    #[test]
    fn reserved_delegation_is_unknown_and_query_never_releases_or_restarts_it() {
        let root=std::env::temp_dir().join(format!("receipt-{}",uuid::Uuid::new_v4()));
        let store=crate::asks::store::AskStore::at(root.join("asks"));
        store.with_state(|s| s.begin_action("caller","reserved","agent_delegate","digest",chrono::Utc::now())).unwrap();
        let q=Query {tool:"agent_delegate".into(),action_id:"reserved".into()};
        let before=serde_json::to_value(store.snapshot().unwrap()).unwrap();
        assert_eq!(delegation_query(&store,"caller",&q).unwrap()["status"],"unknown");
        assert_eq!(serde_json::to_value(store.snapshot().unwrap()).unwrap(),before);
    }

    #[test]
    fn creation_full_receipt_survives_reopen_and_rejects_conflicting_digest() {
        let root=std::env::temp_dir().join(format!("receipt-{}",uuid::Uuid::new_v4()));
        let store=crate::agent_management::AgentManagementStore::open(&root).unwrap();
        let record: crate::agent_management::AgentActionRecord=serde_json::from_value(json!({
            "action_id":"full-create","request_sha256":"0".repeat(64),"operation":"create","project_id":"p",
            "caller_cutex_session":"cutex.00000000-0000-0000-0000-0000000000a1","phase":"prepared",
            "created_at":"2026-10-26T00:00:00Z","updated_at":"2026-10-26T00:00:00Z"
        })).unwrap();
        let caller=record.caller_cutex_session.as_str().to_owned();
        store.with_state(true,|mut s| {s.actions.insert(record.action_id.clone(),record);Ok((s,(),true))}).unwrap();
        let digest="0".repeat(64);
        let partial=json!({"outcome":{"status":"complete","receipt":{"result":{"delegation":{"ok":false}}}}});
        let full=json!({"outcome":{"status":"complete","receipt":{"result":{"delegation":{"ok":true,"delegation":"delegation-one"}}}}});
        assert!(persist_creation_reply_in(&store,&caller,"full-create",Some("wrong"),full.clone()).is_err());
        assert!(!store.snapshot().unwrap().extra.contains_key(RECEIPTS_EXTENSION));
        persist_creation_reply_in(&store,&caller,"full-create",Some(&digest),partial).unwrap();
        persist_creation_reply_in(&store,&caller,"full-create",Some(&digest),full.clone()).unwrap();
        let refusal=json!({"outcome":{"status":"no_write","code":"idempotency_conflict"}});
        assert_eq!(persist_creation_reply_in(&store,&caller,"full-create",Some("wrong"),refusal.clone()).unwrap(),refusal);
        drop(store);
        let snapshot=crate::agent_management::AgentManagementStore::open(&root).unwrap().snapshot().unwrap();
        let q=Query {tool:"agent_create".into(),action_id:"full-create".into()};
        assert_eq!(management_query(&snapshot,&caller,&q).unwrap()["result"],full);
        assert_eq!(management_query(&snapshot,"stranger",&q).unwrap()["status"],"notFound");
    }

    #[test]
    fn creation_query_is_bound_to_original_caller_even_after_role_change() {
        let mut snapshot=crate::agent_management::AgentManagementSnapshot::empty();
        let record: crate::agent_management::AgentActionRecord=serde_json::from_value(json!({
            "action_id":"create-a","request_sha256":"0".repeat(64),"operation":"create","project_id":"p",
            "caller_cutex_session":"cutex.00000000-0000-0000-0000-0000000000a1","phase":"prepared",
            "created_at":"2026-10-26T00:00:00Z","updated_at":"2026-10-26T00:00:00Z"
        })).unwrap();
        let caller=record.caller_cutex_session.as_str().to_owned();
        snapshot.actions.insert(record.action_id.clone(),record);
        let q=Query {tool:"agent_create".into(),action_id:"create-a".into()};
        assert_eq!(management_query(&snapshot,&caller,&q).unwrap()["status"],"unknown");
        assert_eq!(management_query(&snapshot,"new-director",&q).unwrap()["status"],"notFound");
        let before=serde_json::to_value(&snapshot).unwrap();
        management_query(&snapshot,&caller,&q).unwrap();
        assert_eq!(serde_json::to_value(&snapshot).unwrap(),before);
    }
}

#[cfg(test)]
#[path = "action_receipts_p1_tests.rs"]
mod p1_tests;

#[cfg(test)]
#[path = "action_receipts_context_tests.rs"]
mod context_tests;

#[cfg(test)]
#[path = "action_receipts_wait_tests.rs"]
mod wait_tests;

#[cfg(test)]
#[path = "action_receipts_live_tests.rs"]
mod live_tests;
