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

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::model::ExecutionState;
use super::store::AskStore;
use super::tools::{TellSend, ToolContext};

/// How long a create result waits for the delegation to leave the queue
/// (admitted into a turn, held, or final), like the start message's wait.
const DELEGATION_WAIT: Duration = Duration::from_secs(8);
/// The MCP adapter's 30 s budget for the whole create call, with slack.
const CALL_BUDGET: Duration = Duration::from_secs(25);
const POLL: Duration = Duration::from_millis(250);

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
    call_started: Instant,
) {
    let wait = DELEGATION_WAIT.min(CALL_BUDGET.saturating_sub(call_started.elapsed()));
    let store = match AskStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            if let Some(agent) = created_agent(response) {
                set(response, not_delegated(&agent, "persistence_unavailable", &format!("{error:#}")));
            }
            return;
        }
    };
    let world = super::scheduler::HostWorld::load();
    let sleep = || std::thread::sleep(POLL);
    attach_with(
        response,
        &store,
        &world,
        caller,
        task,
        create_action_id,
        crate::agent_bus::tool_policy::delegation_wait_enabled(caller),
        facade,
        wait,
        &sleep,
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
    wait: Duration,
    sleep: &dyn Fn(),
) {
    let Some(agent) = created_agent(response) else {
        return;
    };
    let send = |_: &TellSend<'_>| -> anyhow::Result<Value> {
        anyhow::bail!("agent_create delegations send no notes")
    };
    let wake_chain = |_: &str, _: &str, _: &str| None;
    let clock = chrono::Utc::now;
    let ctx = ToolContext {
        store,
        world,
        caller,
        now: chrono::Utc::now(),
        send: &send,
        wake_chain: &wake_chain,
        clock: &clock,
        sleep,
        wait: wait_tool,
        facade,
    };
    let action_id = delegation_action_id(create_action_id);
    // The agent_delegate tool itself (same checks, record and reply).
    let body = json!({
        "tool": super::tools::AGENT_DELEGATE,
        "arguments": {"target": agent, "text": task},
        "action_id": action_id,
    });
    let mut delegation = super::tools::handle(&ctx, body.to_string().as_bytes());
    if delegation["ok"] != true {
        let code = delegation["code"].as_str().unwrap_or("delegation_failed").to_string();
        let detail = delegation["detail"].as_str().unwrap_or_default().to_string();
        set(response, not_delegated(&agent, &code, &detail));
        return;
    }
    refresh(&mut delegation, store, caller, &action_id, wait, sleep);
    set(response, delegation);
}

/// Wait up to `wait` for the delegation to leave the queue, then refresh
/// its state fields in the reply.
fn refresh(delegation: &mut Value, store: &AskStore, caller: &str, action_id: &str, wait: Duration, sleep: &dyn Fn()) {
    let deadline = Instant::now() + wait;
    loop {
        let Ok(state) = store.snapshot() else { return };
        let Some(ask) = state
            .asks
            .values()
            .find(|ask| ask.action_id == action_id && ask.requester.session() == Some(caller))
        else {
            return;
        };
        let waiting = matches!(ask.execution, ExecutionState::Queued) && ask.hold.is_none();
        if !waiting || Instant::now() >= deadline {
            let view = super::view::requester_view(&state, ask);
            for field in ["state", "hold", "hold_since", "queue_position", "attempt", "activity"] {
                if let (Some(object), Some(value)) = (delegation.as_object_mut(), view.get(field)) {
                    object.insert(field.to_string(), value.clone());
                }
            }
            return;
        }
        sleep();
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
    use crate::asks::model::Requester;
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
            crate::asks::tools::FACADE_DELEGATIONS, Duration::ZERO, &|| {});
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
        assert!(delegation["delegation"].as_str().unwrap().starts_with("delegation-"), "{delegation}");
        assert!(delegation["detail"].as_str().unwrap().contains("Queued for"), "{delegation}");
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks.len(), 1);
        let ask = state.asks.values().next().unwrap().clone();
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
