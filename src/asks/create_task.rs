//! `agent_create` / `member_replace` tasks as delegations
//! (`agent-create-task-delegation`, owner decision 2026-10-15).
//!
//! A sparse create / replace with a `task` (default `task_mode`) starts the
//! new agent idle; the Agent Bus then delegates the task from the caller to
//! it through the `agent_delegate` tool itself ([`super::tools::handle`]),
//! so gate, boundary, idempotency, questions, progress and the result all
//! work exactly as for an `agent_delegate` call. The delegation's action id
//! is derived from the create's action id: an exact create retry finds the
//! same delegation (and creates it if the first call could not).

use serde_json::{json, Value};

use super::store::AskStore;
use super::tools::{TellSend, ToolContext};

/// The delegation action id of the create / replace action `create_action_id`.
/// Stable per action (fits `agent_delegate`'s action id alphabet).
pub fn delegation_action_id(create_action_id: &str) -> String {
    let digest = super::sha256_hex(create_action_id.as_bytes());
    format!("agent-create-task:{}", digest.get(..32).unwrap_or_default())
}

/// The agent a completed create / replace response created.
pub fn created_agent(response: &Value) -> Option<String> {
    if response["outcome"]["status"] != "complete" {
        return None;
    }
    let result = &response["outcome"]["receipt"]["result"];
    match result["kind"].as_str() {
        Some("created") => result["agent"]["cutex_session_id"].as_str(),
        Some("replaced") => result["successor"]["cutex_session_id"].as_str(),
        _ => None,
    }
    .map(str::to_string)
}

/// Delegate `task` from `caller` to the agent a completed create / replace
/// `response` reports, and put the delegation (or why it could not be made)
/// at `outcome.receipt.result.delegation`. Nothing happens for a response
/// that created no agent. Live stores; see [`attach_with`].
pub fn attach(
    response: &mut Value,
    caller: &str,
    task: &str,
    create_action_id: &str,
    facade: u32,
) {
    let store = match AskStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            if created_agent(response).is_some() {
                set(response, uncertain_delegation(delegation_action_id(create_action_id), json!({"ok":false,"code":"persistence_unavailable","detail":format!("{error:#}")})));
            }
            return;
        }
    };
    let world = super::scheduler::HostWorld::load();
    attach_with(
        response,
        &store,
        &world,
        caller,
        task,
        create_action_id,
        crate::agent_bus::tool_policy::delegation_wait_enabled(caller),
        facade,
    );
}

/// [`attach`] on the given stores.
#[allow(clippy::too_many_arguments)]
pub fn attach_with(
    response: &mut Value,
    store: &AskStore,
    world: &dyn super::scheduler::SchedulerWorld,
    caller: &str,
    task: &str,
    create_action_id: &str,
    wait_tool: bool,
    facade: u32,
) {
    let Some(agent) = created_agent(response) else {
        return;
    };
    let send = |_: &TellSend<'_>| -> anyhow::Result<Value> {
        anyhow::bail!("agent_create delegations send no notes")
    };
    let wake_chain = |_: &str, _: &str, _: &str| None;
    let clock = chrono::Utc::now;
    let sleep = || {};
    let ctx = ToolContext {
        store,
        world,
        caller,
        now: chrono::Utc::now(),
        send: &send,
        wake_chain: &wake_chain,
        clock: &clock,
        sleep: &sleep,
        wait: wait_tool,
        facade,
    };
    let action_id = delegation_action_id(create_action_id);
    // The agent_delegate tool itself: this includes routing approval + one-use
    // reviewer grant registration and independent owner gate. Ordinary local
    // creation captures provisional authority without waiting for lifecycle;
    // final scheduler admission fences/revalidates and alone spends the grant.
    // Do not call AskState.create_ask directly as a create+task fast path.
    let body = json!({
        "tool": super::tools::AGENT_DELEGATE,
        "arguments": {"target": agent, "text": task},
        "action_id": action_id,
    });
    let delegation = super::tools::handle(&ctx, body.to_string().as_bytes());
    if is_uncertain(&delegation) {
        set(response, uncertain_delegation(action_id, delegation));
        return;
    }
    if delegation["ok"] != true {
        let code = delegation["code"].as_str().unwrap_or("delegation_failed").to_string();
        let detail = delegation["detail"].as_str().unwrap_or_default().to_string();
        set(response, not_delegated(&agent, &code, &detail));
        return;
    }
    set(response, delegation);
}

/// Shared with receipt reconstruction: an uncertain child must never become a
/// definite "not delivered" parent failure, even for legacy cached replies.
pub(crate) fn is_uncertain(value: &Value) -> bool {
    value["outcomeUnknown"]==true || matches!(value["status"].as_str(),Some("response_uncertain"|"unknown"))
        || matches!(value["code"].as_str(),Some("outcome_unknown"|"response_uncertain"|"persistence_unavailable"))
}
fn uncertain_delegation(action_id: String, mut value: Value) -> Value {
    value["ok"]=json!(false);value["status"]=json!("response_uncertain");value["outcomeUnknown"]=json!(true);
    value["action_id"]=json!(action_id);
    value["note"]=json!("The agent was created; its task attachment outcome is unknown. Query the original action ID. Any explicit retry must keep the original action ID; do not send a new delegation.");
    value
}
pub(crate) fn normalize_uncertain_attachment(response: &mut Value, create_action_id: &str) {
    if let Some(child)=response.pointer_mut("/outcome/receipt/result/delegation") {
        if is_uncertain(child) {*child=uncertain_delegation(delegation_action_id(create_action_id),child.clone());}
    }
}

fn set(response: &mut Value, delegation: Value) {
    response["outcome"]["receipt"]["result"]["task_mode"] = json!("delegation");
    response["outcome"]["receipt"]["result"]["delegation"] = delegation;
}

/// The agent exists but the task was not delegated: nothing was sent to
/// it. An exact retry of the create tries the delegation again.
fn not_delegated(agent: &str, code: &str, detail: &str) -> Value {
    json!({
        "ok": false,
        "code": code,
        "detail": super::truncate_chars(detail, 300),
        "note": format!("The agent was created, but its task was not delegated and it never got it. Retry this call with the same action_id, or send the task with agent_delegate(target=\"{agent}\")."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::model::{ExecutionState, Requester};
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::store::tests::{temp_store, REQUESTER, TARGET};

    fn created(agent: &str) -> Value {
        json!({"schema":"cutex/agent-management/v1","action_id":"create-1","outcome":{"status":"complete","receipt":{
            "schema":"cutex/agent-management-receipt/v1","action_id":"create-1","request_sha256":"0","operation":"create",
            "project_id":"alpha","completed_at":"2026-10-15T00:00:00Z",
            "result":{"kind":"created","agent":{"cutex_session_id":agent}}}}})
    }

    fn run(world: &crate::asks::scheduler::tests::FakeWorld, store: &AskStore, response: &mut Value, action: &str) {
        attach_with(response, store, world, REQUESTER, "Map the repo and report", action, true,
            crate::asks::tools::FACADE_DELEGATIONS);
    }

    fn read_tool(world: &crate::asks::scheduler::tests::FakeWorld, store: &AskStore, caller: &str, id: &str) -> Value {
        let send = |_: &TellSend<'_>| -> anyhow::Result<Value> { panic!("read must not send") };
        let wake = |_: &str, _: &str, _: &str| -> Option<u32> { panic!("read must not wake") };
        let clock = chrono::Utc::now;
        let sleep = || panic!("read must not wait");
        let ctx = ToolContext { store, world, caller, now: clock(), send: &send,
            wake_chain: &wake, clock: &clock, sleep: &sleep, wait: true,
            facade: crate::asks::tools::FACADE_DELEGATIONS };
        super::super::tools::handle(&ctx, json!({"tool":"delegation_read",
            "arguments":{"delegation":id},"action_id":"read-created-task"}).to_string().as_bytes())
    }

    /// The task becomes a delegation from the creator to the new agent, an
    /// exact retry finds it again, and its result reaches the creator.
    #[test]
    fn task_is_delegated_once_and_its_result_reaches_the_creator() {
        let (world, dir) = world("create-task");
        let (store, store_dir) = temp_store("create-task");
        let mut response = created(TARGET);
        run(&world, &store, &mut response, "create-1");
        let result = &response["outcome"]["receipt"]["result"];
        assert_eq!(result["task_mode"], "delegation");
        let delegation = &result["delegation"];
        assert_eq!(delegation["ok"], true, "{delegation}");
        assert_eq!(delegation["created"], true);
        assert_eq!(delegation["state"], "queued");
        assert!(delegation["delegation"].is_string(), "{delegation}");
        assert!(delegation["detail"].as_str().unwrap().contains("Queued for"), "{delegation}");
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks.len(), 1);
        let ask = state.asks.values().next().unwrap().clone();
        let displayed_id = delegation["delegation"].as_str().unwrap();
        assert!(displayed_id.starts_with("delegation-"));
        // MCP returns a display selector, not a storage key. Exercise the real
        // tool resolver now and again after the result has been produced.
        assert_eq!(read_tool(&world, &store, REQUESTER, displayed_id)["code"], "answer_not_found");
        assert_eq!(ask.requester.session(), Some(REQUESTER), "the creator is the requester");
        assert_eq!(ask.target_cutex_session_id, TARGET);
        assert_eq!(ask.action_id, delegation_action_id("create-1"));

        // An exact create retry (same action id) makes no second delegation.
        let mut again = created(TARGET);
        run(&world, &store, &mut again, "create-1");
        let repeated = &again["outcome"]["receipt"]["result"]["delegation"];
        assert_eq!(repeated["delegation"], delegation["delegation"]);
        assert_eq!(repeated["created"], false);
        assert_eq!(store.snapshot().unwrap().asks.len(), 1);

        // The handler's turn ends: the result is the creator's to claim.
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = chrono::Utc::now();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.submitted.borrow()[0].0, TARGET, "the brief went to the new agent");
        let turn = store.snapshot().unwrap().leases[TARGET].turn_id.clone().unwrap();
        crate::asks::observe::observe(&store, TARGET, "item/completed", &json!({"turnId": turn, "item": {"type": "agentMessage", "text": "report"}}), now).unwrap();
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": turn, "status": "completed"}}), now).unwrap();
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask.ask_id];
        assert_eq!(ask.execution, ExecutionState::Answered);
        assert!(state.deliveries.values().any(|delivery| delivery.requester_cutex_session_id == REQUESTER && delivery.ask_id == ask.ask_id && delivery.kind == crate::asks::model::DeliveryKind::Answer),
            "a result delivery to the creator: {:?}", state.deliveries);
        assert!(matches!(ask.requester, Requester::Agent { .. }));
        for reader in [REQUESTER, TARGET] {
            let read = read_tool(&world, &store, reader, displayed_id);
            assert_eq!(read["ok"], true, "{read}");
            assert_eq!(read["text"], "report");
            assert_eq!(read["delegation"], delegation["delegation"]);
            let canonical_read = read_tool(&world, &store, reader, &ask.ask_id);
            assert_eq!(read, canonical_read, "both selectors must reach the same result");
        }
        let before = serde_json::to_value(store.snapshot().unwrap()).unwrap();
        let denied = read_tool(&world, &store, "cutex.00000000-0000-0000-0000-0000000000ff", displayed_id);
        assert_eq!(denied["ok"], false, "{denied}");
        assert_eq!(denied["code"], "id_not_found");
        assert!(denied.get("text").is_none());
        assert_eq!(serde_json::to_value(store.snapshot().unwrap()).unwrap(), before);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// The delegation cannot be made (here: the agent is outside the
    /// creator's boundary): the reply says so, nothing is left behind.
    #[test]
    fn a_refused_delegation_is_reported_without_a_record() {
        let (world, dir) = world("create-task-refused");
        let (store, store_dir) = temp_store("create-task-refused");
        let mut response = created("cutex.00000000-0000-0000-0000-0000000000ff");
        run(&world, &store, &mut response, "create-2");
        let delegation = &response["outcome"]["receipt"]["result"]["delegation"];
        assert_eq!(delegation["ok"], false, "{delegation}");
        assert!(delegation["note"].as_str().unwrap().contains("agent_delegate"), "{delegation}");
        assert!(store.snapshot().unwrap().asks.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn no_agent_no_delegation() {
        let (world, dir) = world("create-task-none");
        let (store, store_dir) = temp_store("create-task-none");
        let mut refused = json!({"outcome":{"status":"no_write","code":"x","detail":"y"}});
        run(&world, &store, &mut refused, "create-3");
        assert!(refused["outcome"].get("receipt").is_none());
        assert!(store.snapshot().unwrap().asks.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn action_ids_are_stable_and_valid() {
        let id = delegation_action_id("quokka/create@1");
        assert_eq!(id, delegation_action_id("quokka/create@1"));
        assert_ne!(id, delegation_action_id("quokka/create@2"));
        assert!(id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)), "{id}");
    }
}
