//! Production work run by the Bus-owned worker after revalidating the MCP
//! occurrence. No transport, secondary queue, or new authority domain.
use std::sync::{Arc, Mutex};
use serde_json::{json, Value};
use crate::agent_bus::{server::AgentBusRequestHandlers, store::AgentBusState};
use crate::agent_management::{AgentManagementInvocation, AgentManagementRequest, SparseAgentManagementRequest};

pub fn is_create(payload: &AgentManagementRequest, sparse: Option<&SparseAgentManagementRequest>) -> bool {
    sparse.map_or_else(|| matches!(payload.operation,crate::agent_management::AgentOperation::Create { .. }),
        |request| matches!(request.operation,crate::agent_management::SparseOperation::Create { .. }))
}

pub fn create(
    state: &Arc<Mutex<AgentBusState>>, invocation: AgentManagementInvocation,
    payload: AgentManagementRequest, sparse: Option<SparseAgentManagementRequest>,
    handlers: AgentBusRequestHandlers, facade: u32,
    launch_policy: impl FnOnce(&AgentManagementInvocation, &AgentManagementRequest) -> Option<Value>,
) -> anyhow::Result<Value> {
    // r274.1: allow_unlisted holds for this create's launch review.
    let _model_scope = crate::profiles::model_check::RequestScope::enter(
        sparse.as_ref().is_some_and(|sparse| sparse.operation.agent().allow_unlisted));
    let (payload, sparse_digest) = if let Some(sparse) = &sparse {
        match crate::agent_management::admit(&invocation.caller_cutex_session,sparse) {
            Ok(admitted) => (admitted.request,Some(admitted.sparse_sha256)),
            Err(refusal) => {
                let mut response = json!({"schema":"cutex/agent-management/v1","action_id":payload.action_id,
                    "outcome":{"status":"no_write","code":refusal.code,"detail":refusal.detail}});
                crate::agent_management::attach_name_hint(&mut response,&sparse.operation.agent().name);
                return Ok(response);
            }
        }
    } else { (payload,None) };
    if let Some(refusal) = launch_policy(&invocation,&payload) { return Ok(refusal); }
    let delegated = sparse.as_ref().and_then(|s| crate::agent_management::delegated_task(s,&payload)).map(str::to_owned);
    // r276: the task outlives a create whose runtime start fails, so finishing
    // the create later (exact retry, member_runtime online) still delivers it.
    if let Some(task) = &delegated {
        let store = crate::agent_management::AgentManagementStore::open_default()?;
        crate::asks::create_task::remember_pending(&store,payload.action_id.as_str(),crate::asks::create_task::PendingTask {
            caller:invocation.caller_cutex_session.as_str().to_owned(),task:task.clone(),facade,
            recorded_at:chrono::Utc::now().to_rfc3339()})?;
    }
    finish(state,invocation,payload,sparse_digest,delegated,handlers,facade)
}

/// Runs the admitted create and attaches its task (shared by the tool call
/// and [`resume_unfinished_create`]).
#[allow(clippy::too_many_arguments)]
fn finish(
    state: &Arc<Mutex<AgentBusState>>, invocation: AgentManagementInvocation,
    payload: AgentManagementRequest, sparse_digest: Option<crate::role_revision::Sha256>,
    delegated: Option<String>, handlers: AgentBusRequestHandlers, facade: u32,
) -> anyhow::Result<Value> {
    let requested_name = match &payload.operation {
        crate::agent_management::AgentOperation::Create { spec, .. } => Some(spec.name.clone()),
        _ => None,
    };
    let full_digest = crate::agent_management::full_sha256(&payload);
    let caller = invocation.caller_cutex_session.clone();
    let action_id = payload.action_id.clone();
    let mut response = (handlers.agent_management)(state,invocation,payload)?;
    if let Some(task) = &delegated {
        crate::asks::create_task::attach(&mut response,caller.as_str(),task,action_id.as_str(),facade);
    }
    if delegated.is_some() {
        let settled = crate::agent_management::AgentManagementStore::open_default()
            .and_then(|store| crate::asks::create_task::settle_pending(&store,action_id.as_str(),&response));
        if let Err(error) = settled {
            eprintln!("\x1b[33mwarning:\x1b[0m agent_create {}: pending task not cleared: {error:#}", action_id.as_str());
        }
    }
    crate::management::v2::profile_auto_pick::attach_to_reply(&mut response,action_id.as_str());
    crate::profiles::model_check::attach_warning(&mut response);
    if let Some(name) = &requested_name {
        crate::agent_management::attach_name_hint(&mut response,name);
    }
    if let Some(sparse_digest) = sparse_digest {
        if let Some(digest) = response.pointer_mut("/outcome/receipt/request_sha256") {
            if full_digest.as_ref().is_some_and(|full| digest.as_str() == Some(full.as_str())) { *digest=json!(sparse_digest.as_str()); }
        }
    }
    // Normalize even legacy/provider cached attachment responses before storing a full reply.
    crate::asks::create_task::normalize_uncertain_attachment(&mut response,action_id.as_str());
    super::persist_creation_reply(caller.as_str(),action_id.as_str(),
        full_digest.as_ref().map(|d|d.as_str()),response)
}

/// r276: the creator's `member_runtime online` of an agent whose create never
/// finished (its runtime start failed after the agent existed: bus timeout)
/// finishes that create, so its task is delivered. Returns what became of it,
/// for the online reply; None when there is no such create.
pub fn resume_unfinished_create(
    state: &Arc<Mutex<AgentBusState>>, invocation: &AgentManagementInvocation,
    agent: &crate::role_revision::CutexSessionId, handlers: AgentBusRequestHandlers,
    launch_policy: impl FnOnce(&AgentManagementInvocation, &AgentManagementRequest) -> Option<Value>,
) -> Option<Value> {
    let store = crate::agent_management::AgentManagementStore::open_default().ok()?;
    let snapshot = store.snapshot().ok()?;
    let (action, sparse) = unfinished_create(&snapshot, invocation.caller_cutex_session.as_str(), agent)?;
    let action_id = action.as_str().to_owned();
    let pending = crate::asks::create_task::pending_task(&snapshot, &action_id);
    let request = sparse.request.clone();
    if let Some(refusal) = launch_policy(invocation, &request) {
        return Some(json!({"action_id":action_id,"status":"not_resumed","code":refusal["outcome"]["code"].clone()}));
    }
    let facade = pending.as_ref().map_or(crate::asks::tools::FACADE_DELEGATIONS, |task| task.facade);
    let result = finish(state, invocation.clone(), request, Some(sparse.sparse_sha256.clone()),
        pending.map(|task| task.task), handlers, facade);
    let query = super::Query { action_id: action_id.clone(), tool: "agent_create".into() };
    Some(match result {
        Ok(response) => {
            let status = super::completed(&query, response.clone())["status"].clone();
            json!({"action_id":action_id,"status":status,
                "delegation":response["outcome"]["receipt"]["result"]["delegation"].clone()})
        }
        Err(error) => json!({"action_id":action_id,"status":"unknown","detail":crate::asks::truncate_chars(&format!("{error:#}"), 300)}),
    })
}

/// The latest create by `caller` that made `agent` but never finished, with
/// its frozen request (sparse creates only).
fn unfinished_create<'a>(
    snapshot: &'a crate::agent_management::AgentManagementSnapshot, caller: &str,
    agent: &crate::role_revision::CutexSessionId,
) -> Option<(&'a crate::agent_management::AgentActionId, &'a crate::agent_management::SparseRequestRecord)> {
    use crate::agent_management::AgentActionPhase as Phase;
    snapshot.actions.values()
        .filter(|action| action.operation == crate::agent_management::AgentOperationKind::Create
            && action.response.is_none()
            && action.caller_cutex_session.as_str() == caller
            && action.known_successor_cutex_session.as_ref() == Some(agent)
            && matches!(action.phase, Phase::Configured | Phase::Online | Phase::Ready | Phase::MessagePending | Phase::MessageQueued))
        .filter_map(|action| snapshot.sparse_requests.get(&action.action_id)
            .filter(|sparse| sparse.caller_cutex_session.as_str() == caller)
            .map(|sparse| (action, sparse)))
        .max_by(|(a, _), (b, _)| a.created_at.cmp(&b.created_at))
        .map(|(action, sparse)| (&action.action_id, sparse))
}

pub fn delegate(caller: &str, body: &[u8], facade: u32) -> anyhow::Result<Value> {
    // agent_delegate does not use send/wake hooks; reject any accidental use
    // rather than expanding this async route to additional mutating tools.
    let payload: Value = serde_json::from_slice(body)?;
    anyhow::ensure!(payload["tool"] == "agent_delegate", "async tool is not agent_delegate");
    let store = crate::asks::store::AskStore::open_default()?;
    let world = crate::asks::scheduler::HostWorld::load();
    let send = |_: &crate::asks::tools::TellSend<'_>| anyhow::bail!("unexpected send in agent_delegate");
    let wake = |_: &str, _: &str, _: &str| None;
    let clock = chrono::Utc::now;
    let sleep = || std::thread::sleep(std::time::Duration::from_millis(250));
    let ctx = crate::asks::tools::ToolContext { store:&store,world:&world,caller,now:clock(),
        send:&send,wake_chain:&wake,clock:&clock,sleep:&sleep,
        wait:crate::agent_bus::tool_policy::delegation_wait_enabled(caller),facade };
    Ok(crate::asks::tools::handle(&ctx,body))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_create_is_selected_before_placeholder_resolution() {
        let sparse: SparseAgentManagementRequest=serde_json::from_value(json!({
            "schema":"cutex/agent-management-sparse/v1","action_id":"create-sparse","operation":"create","agent":{"name":"fixture"}}
        )).unwrap();
        let placeholder=AgentManagementRequest {schema:crate::agent_management::AgentManagementSchema::V1,
            action_id:sparse.action_id.clone(),project_id:None,operation:crate::agent_management::AgentOperation::QueryManaged};
        assert!(is_create(&placeholder,Some(&sparse)));
        assert!(!is_create(&placeholder,None));
    }

    const CALLER: &str = "cutex.00000000-0000-0000-0000-0000000000a1";
    const AGENT: &str = "cutex.00000000-0000-0000-0000-0000000000a2";

    fn action(id: &str, caller: &str, phase: &str, response: bool, at: &str) -> crate::agent_management::AgentActionRecord {
        let mut record: crate::agent_management::AgentActionRecord = serde_json::from_value(json!({
            "action_id":id, "request_sha256":"0".repeat(64), "operation":"create", "project_id":"p",
            "caller_cutex_session":caller, "phase":phase, "phase_sequence":7,
            "known_native_session_id":"native", "known_successor_cutex_session":AGENT,
            "created_at":at, "updated_at":at
        })).unwrap();
        if response {
            record.response = Some(serde_json::from_value(json!({"schema":"cutex/agent-management/v1","action_id":id,
                "outcome":{"status":"no_write","code":"x","detail":"x"}})).unwrap());
        }
        record
    }
    fn sparse(id: &str, caller: &str) -> crate::agent_management::SparseRequestRecord {
        serde_json::from_value(json!({"sparse_sha256":"1".repeat(64), "caller_cutex_session":caller,
            "request":{"schema":"cutex/agent-management/v1","action_id":id,"operation":"create","spec":{
                "name":"s5-owner-input","cwd":"/private","profile":"Jasmine","runtime_backend":"host","model":"m",
                "reasoning":"default","permissions":"danger-full-access","approval_policy":"never",
                "sandbox_mode":"danger-full-access","groups":[],"expose_to_im":false,"pin":false},
                "start_mode":"bootstrap_only","frozen_message":null},
            "recorded_at":"2026-11-04T05:35:25Z"})).unwrap()
    }

    /// r276 (EVA 2026-11-04): member_runtime online by the creator finds the
    /// create that made the agent but never finished; nothing else.
    #[test]
    fn online_resumes_only_the_creators_unfinished_create_of_that_agent() {
        let mut snapshot = crate::agent_management::AgentManagementSnapshot::empty();
        let agent = crate::role_revision::CutexSessionId::new(AGENT).unwrap();
        let add = |snapshot: &mut crate::agent_management::AgentManagementSnapshot, record: crate::agent_management::AgentActionRecord, with_sparse: bool| {
            if with_sparse { snapshot.sparse_requests.insert(record.action_id.clone(), sparse(record.action_id.as_str(), record.caller_cutex_session.as_str())); }
            snapshot.actions.insert(record.action_id.clone(), record);
        };
        assert!(unfinished_create(&snapshot, CALLER, &agent).is_none());
        add(&mut snapshot, action("finished", CALLER, "complete", true, "2026-11-04T05:40:00Z"), true);
        add(&mut snapshot, action("before-agent", CALLER, "adopted", false, "2026-11-04T05:40:00Z"), true);
        add(&mut snapshot, action("legacy-full", CALLER, "configured", false, "2026-11-04T05:41:00Z"), false);
        add(&mut snapshot, action("other-caller", "cutex.00000000-0000-0000-0000-0000000000a3", "configured", false, "2026-11-04T05:42:00Z"), true);
        assert!(unfinished_create(&snapshot, CALLER, &agent).is_none());
        add(&mut snapshot, action("older", CALLER, "configured", false, "2026-11-04T05:35:25Z"), true);
        add(&mut snapshot, action("s5", CALLER, "online", false, "2026-11-04T05:36:00Z"), true);
        let (id, found) = unfinished_create(&snapshot, CALLER, &agent).unwrap();
        assert_eq!((id.as_str(), found.request.action_id.as_str()), ("s5", "s5"));
        let other = crate::role_revision::CutexSessionId::new("cutex.00000000-0000-0000-0000-0000000000a9").unwrap();
        assert!(unfinished_create(&snapshot, CALLER, &other).is_none());
    }

    static RESUME_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn resume_handlers() -> AgentBusRequestHandlers {
        AgentBusRequestHandlers {
            reconcile_registration_agent: |_, _| anyhow::bail!("unused"),
            reconcile_agent: |_| anyhow::bail!("unused"),
            redrive_ordinary_messages: |_| Ok(0),
            send_payload_response: |_, _| anyhow::bail!("unused"),
            // The provider finishing the frozen create request.
            agent_management: |_, invocation, request| {
                RESUME_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(invocation.caller_cutex_session.as_str(), CALLER);
                assert!(matches!(request.operation, crate::agent_management::AgentOperation::Create { .. }));
                Ok(json!({"schema":"cutex/agent-management/v1","action_id":request.action_id,"outcome":{"status":"complete","receipt":{
                    "action_id":request.action_id,"operation":"create","request_sha256":crate::agent_management::full_sha256(&request).unwrap().as_str(),
                    "result":{"kind":"created","agent":{"cutex_session_id":AGENT}}}}}))
            },
            managed_archive: |_, _| panic!("unused"),
            automatic_managed_online: |_, _| anyhow::bail!("unused"),
            job_service_completion: |_, _, _| anyhow::bail!("unused"),
            job_service_completion_query: |_, _| anyhow::bail!("unused"),
            management_start_acked: |_, _| {},
        }
    }

    /// r276: online by the creator finishes the unfinished create on the live
    /// store path: the frozen request runs, the kept task is attached, the
    /// reply is recorded for action_status. A policy refusal leaves it alone.
    #[test]
    fn online_finishes_the_create_and_tries_its_kept_task() {
        use std::sync::atomic::Ordering::SeqCst;
        let store = crate::agent_management::AgentManagementStore::open_default().unwrap();
        let id = "resume-e2e";
        let sparse = sparse(id, CALLER);
        let mut record = action(id, CALLER, "configured", false, "2026-11-04T05:35:25Z");
        record.request_sha256 = crate::agent_management::full_sha256(&sparse.request).unwrap();
        let action_id = record.action_id.clone();
        store.with_state(true, |mut state| {
            state.sparse_requests.insert(action_id.clone(), sparse.clone());
            state.actions.insert(action_id.clone(), record.clone());
            Ok((state, (), true))
        }).unwrap();
        crate::asks::create_task::remember_pending(&store, id, crate::asks::create_task::PendingTask {
            caller: CALLER.into(), task: "Map the repo".into(), facade: 2, recorded_at: "2026-11-04T05:35:25Z".into() }).unwrap();
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let invocation = AgentManagementInvocation {
            caller_cutex_session: crate::role_revision::CutexSessionId::new(CALLER).unwrap(),
            caller_runtime_agent_id: "codex.caller".into(),
        };
        let agent = crate::role_revision::CutexSessionId::new(AGENT).unwrap();
        // Another caller, or the launch policy refusing: nothing runs.
        let stranger = AgentManagementInvocation {
            caller_cutex_session: crate::role_revision::CutexSessionId::new("cutex.00000000-0000-0000-0000-0000000000a3").unwrap(),
            caller_runtime_agent_id: "codex.stranger".into(),
        };
        assert!(resume_unfinished_create(&state, &stranger, &agent, resume_handlers(), |_, _| None).is_none());
        let refused = resume_unfinished_create(&state, &invocation, &agent, resume_handlers(),
            |_, _| Some(json!({"outcome":{"status":"no_write","code":"approval_required"}}))).unwrap();
        assert_eq!((refused["status"].as_str(), refused["code"].as_str()), (Some("not_resumed"), Some("approval_required")));
        assert_eq!(RESUME_CALLS.load(SeqCst), 0);
        assert!(crate::asks::create_task::pending_task(&store.snapshot().unwrap(), id).is_some());

        let resumed = resume_unfinished_create(&state, &invocation, &agent, resume_handlers(), |_, _| None).unwrap();
        assert_eq!(RESUME_CALLS.load(SeqCst), 1);
        assert_eq!(resumed["action_id"], id);
        assert!(resumed["delegation"].is_object(), "the kept task was tried: {resumed}");
        let snapshot = store.snapshot().unwrap();
        let receipt = snapshot.tool_action_receipt(id).expect("reply recorded for action_status");
        assert_eq!(receipt["caller"], CALLER);
        assert_eq!(receipt["result"]["outcome"]["receipt"]["result"]["delegation"], resumed["delegation"]);
        // A definite answer settles the task; an uncertain one keeps it.
        assert_eq!(crate::asks::create_task::pending_task(&snapshot, id).is_some(),
            crate::asks::create_task::is_uncertain(&resumed["delegation"]));
    }

    /// The task of a create is kept until its delegation is made, so a create
    /// finished later still delivers it; it survives a restart.
    #[test]
    fn a_create_task_is_kept_until_it_is_delegated() {
        use crate::asks::create_task::{pending_task, remember_pending, settle_pending, PendingTask};
        let root = std::env::temp_dir().join(format!("pending-task-{}", uuid::Uuid::new_v4()));
        let store = crate::agent_management::AgentManagementStore::open(&root).unwrap();
        let task = |text: &str| PendingTask { caller: CALLER.into(), task: text.into(), facade: 2, recorded_at: "2026-11-04T05:35:25Z".into() };
        remember_pending(&store, "s5", task("Map the repo")).unwrap();
        remember_pending(&store, "s5", task("a changed retry")).unwrap();
        remember_pending(&store, "s7", task("Other")).unwrap();
        let created = |delegation: Value| json!({"outcome":{"status":"complete","receipt":{"result":{"kind":"created",
            "agent":{"cutex_session_id":AGENT},"delegation":delegation}}}});
        // Unfinished or uncertain: kept.
        for response in [json!({"outcome":{"status":"owner_action_required"}}), json!({"status":"response_uncertain","outcomeUnknown":true}),
            created(json!({"ok":false,"status":"response_uncertain","outcomeUnknown":true}))] {
            settle_pending(&store, "s5", &response).unwrap();
        }
        drop(store);
        let store = crate::agent_management::AgentManagementStore::open(&root).unwrap();
        assert_eq!(pending_task(&store.snapshot().unwrap(), "s5").unwrap().task, "Map the repo");
        // Delegated: gone; a refusal without an agent too.
        settle_pending(&store, "s5", &created(json!({"ok":true,"delegation":"delegation-1"}))).unwrap();
        assert!(pending_task(&store.snapshot().unwrap(), "s5").is_none());
        assert!(pending_task(&store.snapshot().unwrap(), "s7").is_some());
        settle_pending(&store, "s7", &json!({"outcome":{"status":"no_write","code":"x"}})).unwrap();
        assert!(!store.snapshot().unwrap().extra.contains_key(crate::asks::create_task::PENDING_TASKS_EXTENSION));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
