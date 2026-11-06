//! Connection-local background inventory. No cutex store, journal or admission lane.
//! Notification and snapshot mutations run on the same transport actor, in wire
//! order. In particular an exit after a list response cannot be overwritten by
//! that response, even when the durable event consumer is backlogged.
use super::client::{AppServerEvent, AppServerHandle};
use super::manager::{
    AppServerManagedRuntimeStatus, AppServerRuntimeEventContext, AppServerRuntimeEventSink,
};
use super::protocol::RpcResponseOutcome;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub const METHOD: &str = "cutex/session/backgroundWork/changed";
const TIMEOUT: Duration = Duration::from_secs(5);
const RECENT_TASKS: usize = 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BackgroundWork {
    pub running: bool,
    pub subagents: usize,
    pub shells: usize,
    pub terminals: usize,
    pub complete: bool,
}

#[derive(Debug, Default)]
struct Inventory {
    tasks: HashMap<String, bool>, // true = subagent, false = shell
    ended_tasks: VecDeque<String>,
    task_fence_overflow: bool,
    terminals: HashMap<String, HashSet<String>>, // thread -> item identities
    revision: u64,
    seeds: HashMap<String, u64>,
    task_revisions: HashMap<String, u64>,
    terminal_revisions: HashMap<(String, String), u64>,
    requested: HashSet<String>,
    complete: bool,
}
impl Inventory {
    pub fn summary(&self) -> BackgroundWork {
        let subagents = self.tasks.values().filter(|v| **v).count();
        let shells = self.tasks.len() - subagents;
        let terminals = self.terminals.values().map(HashSet::len).sum::<usize>();
        BackgroundWork {
            running: subagents + shells + terminals > 0,
            subagents,
            shells,
            terminals,
            complete: self.complete
                && !self.task_fence_overflow
                && self.requested.is_empty()
                && self.seeds.is_empty(),
        }
    }
    fn task(&mut self, task: &Value) {
        let Some(id) = task
            .get("taskId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let background = task.get("background").and_then(Value::as_bool) == Some(true);
        let active = matches!(
            task.get("status").and_then(Value::as_str),
            Some("running" | "pending")
        );
        if !active {
            if (self.tasks.remove(id).is_some() || background)
                && !self.ended_tasks.iter().any(|t| t == id)
            {
                if self.ended_tasks.len() == RECENT_TASKS {
                    self.ended_tasks.pop_front();
                    self.task_fence_overflow = true;
                }
                self.ended_tasks.push_back(id.into());
            }
            return;
        }
        let kind = task.get("taskType").and_then(Value::as_str).unwrap_or("");
        let shell = matches!(kind, "local_bash" | "local_shell" | "bash");
        if background
            && !(shell && task.get("ownedBySubagent").and_then(Value::as_bool) == Some(true))
            && !self.ended_tasks.iter().any(|t| t == id)
        {
            self.tasks.insert(id.into(), !shell);
        } else {
            self.tasks.remove(id);
        }
    }
    fn terminal(&mut self, thread: &str, method: &str, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("commandExecution") {
            return;
        }
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        self.revision += 1;
        if self.seeds.contains_key(thread) {
            self.terminal_revisions
                .insert((thread.into(), id.into()), self.revision);
        }
        let rows = self.terminals.entry(thread.into()).or_default();
        if method == "item/completed" {
            rows.remove(id);
        } else if method == "item/started"
            && item.get("processId").and_then(Value::as_str).is_some()
        {
            rows.insert(id.into());
        }
        if rows.is_empty() {
            self.terminals.remove(thread);
        }
    }
}

type Status = Arc<Mutex<AppServerManagedRuntimeStatus>>;
struct ConnectionState {
    status: Status,
    inventory: Mutex<Inventory>,
}
type Observation = Arc<ConnectionState>;

fn affects_inventory(context: &AppServerRuntimeEventContext, event: &AppServerEvent) -> bool {
    let AppServerEvent::Notification(n) = event else {
        return matches!(event, AppServerEvent::Disconnected { .. });
    };
    let Some(p) = &n.params else { return false };
    if context.runtime_backend.starts_with("claude") {
        return n.method == "claude/task/updated"
            && p.get("threadId").and_then(Value::as_str) == Some(&context.thread_id)
            && p.get("task").is_some();
    }
    if !matches!(
        n.method.as_str(),
        "item/started" | "item/completed" | "thread/started"
    ) {
        return false;
    }
    // Spawn edges are useful even without a command; learning them needs no
    // status lock, summary traversal or task/terminal inventory mutation.
    crate::management::v2::subagent_lineage::learn_from_notification(
        &context.cutex_session_id,
        &context.thread_id,
        &n.raw,
    );
    matches!(n.method.as_str(), "item/started" | "item/completed")
        && p.pointer("/item/type").and_then(Value::as_str) == Some("commandExecution")
}
fn mutate(
    observation: &Observation,
    context: &AppServerRuntimeEventContext,
    sink: &Arc<dyn AppServerRuntimeEventSink>,
    f: impl FnOnce(&mut Inventory),
) {
    let mut status = observation.status.lock().unwrap_or_else(|e| e.into_inner());
    if !status.connected {
        return;
    }
    let mut inventory = observation
        .inventory
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f(&mut inventory);
    let after = Some(inventory.summary());
    if status.background_work != after {
        status.background_work = after.clone();
        // This callback is memory-only and must not acquire the runtime-manager lock.
        let _ = sink.handle_background_work(context, after);
    }
}

/// Install before the event worker starts. RPCs run off the actor; their response
/// observers apply the snapshot at the exact wire position. No persistence API.
pub fn attach(
    handle: AppServerHandle,
    status: Status,
    context: AppServerRuntimeEventContext,
    sink: Arc<dyn AppServerRuntimeEventSink>,
) -> anyhow::Result<()> {
    attach_observation(handle, status, context, sink).map(|_| ())
}

fn attach_observation(
    handle: AppServerHandle,
    status: Status,
    context: AppServerRuntimeEventContext,
    sink: Arc<dyn AppServerRuntimeEventSink>,
) -> anyhow::Result<Option<Observation>> {
    let claude = context.runtime_backend.starts_with("claude");
    if !claude
        && !matches!(
            context.runtime_backend.as_str(),
            "host" | "host_foreground" | "docker" | "cute_alden" | "codex"
        )
    {
        return Ok(None);
    }
    let status = Arc::new(ConnectionState {
        status,
        inventory: Mutex::new(Inventory::default()),
    });
    mutate(&status, &context, &sink, |_| {});
    let (tx, rx) = mpsc::channel::<String>();
    let (event_status, event_context, event_sink) = (status.clone(), context.clone(), sink.clone());
    handle.observe_notifications(Box::new(move |event| {
        if !affects_inventory(&event_context, event) {
            return;
        }
        if matches!(event, AppServerEvent::Disconnected { .. }) {
            let mut s = event_status
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if s.connected {
                s.connected = false;
                s.mask_disconnected_turn();
                *event_status
                    .inventory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Inventory::default();
                let _ = event_sink.handle_background_work(&event_context, None);
            }
            return;
        }
        mutate(&event_status, &event_context, &event_sink, |s| {
            let AppServerEvent::Notification(n) = event else {
                return;
            };
            let Some(p) = &n.params else { return };
            let root = &event_context.thread_id;
            if claude {
                if p.get("threadId").and_then(Value::as_str) == Some(root)
                    && n.method == "claude/task/updated"
                {
                    if let Some(task) = p.get("task") {
                        let i = &mut *s;
                        i.revision += 1;
                        if i.seeds.contains_key(root) {
                            if let Some(id) = task.get("taskId").and_then(Value::as_str) {
                                i.task_revisions.insert(id.into(), i.revision);
                            }
                        }
                        i.task(task);
                    }
                }
                return;
            }
            let Some(thread) = p.get("threadId").and_then(Value::as_str) else {
                return;
            };
            if !matches!(n.method.as_str(), "item/started" | "item/completed") {
                return;
            }
            let Some(item) = p.get("item") else { return };
            if item.get("type").and_then(Value::as_str) != Some("commandExecution") {
                return;
            }
            if thread == root
                || crate::management::v2::subagent_lineage::is_descendant(
                    &event_context.cutex_session_id,
                    root,
                    thread,
                )
            {
                s.terminal(thread, &n.method, item);
            } else if s.requested.insert(thread.into()) {
                let _ = tx.send(thread.into());
            }
        });
    }))?;
    let observation = status.clone();
    std::thread::Builder::new()
        .name("cutex-background-inventory".into())
        .spawn(move || {
            let mut ok = seed(
                &handle,
                &status,
                &context,
                &sink,
                &context.thread_id,
                claude,
            );
            if !claude {
                // loaded/list is an in-memory inventory, unlike thread/list's DB/rollout scan.
                let mut cursor = Value::Null;
                let mut seen = HashSet::new();
                loop {
                    let response = handle.request_with_timeout(
                        "thread/loaded/list",
                        json!({"cursor":cursor}),
                        TIMEOUT,
                    );
                    let Ok(response) = response else {
                        ok = false;
                        break;
                    };
                    let Some(rows) = response.get("data").and_then(Value::as_array) else {
                        ok = false;
                        break;
                    };
                    for thread in rows
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|id| *id != context.thread_id)
                    {
                        match descendant(&handle, &context, thread) {
                            Ok(true) => {
                                ok &= seed(&handle, &status, &context, &sink, thread, false);
                            }
                            Ok(false) => {}
                            Err(()) => ok = false,
                        }
                    }
                    cursor = response.get("nextCursor").cloned().unwrap_or(Value::Null);
                    if cursor.is_null() {
                        break;
                    }
                    if !seen.insert(cursor.to_string()) {
                        ok = false;
                        break;
                    }
                }
                mutate(&status, &context, &sink, |s| s.complete = ok);
            }
            while let Ok(thread) = rx.recv() {
                if !status
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .connected
                {
                    break;
                }
                match descendant(&handle, &context, &thread) {
                    Ok(true) => {
                        if !seed(&handle, &status, &context, &sink, &thread, false) {
                            mutate(&status, &context, &sink, |s| s.complete = false);
                        }
                    }
                    Ok(false) => {}
                    Err(()) => {
                        mutate(&status, &context, &sink, |s| {
                            s.requested.remove(&thread);
                            s.complete = false;
                        });
                    }
                }
                mutate(&status, &context, &sink, |i| {
                    i.requested.remove(&thread);
                });
            }
        })?;
    Ok(Some(observation))
}

fn descendant(
    handle: &AppServerHandle,
    c: &AppServerRuntimeEventContext,
    thread: &str,
) -> Result<bool, ()> {
    crate::management::v2::subagent_lineage::verify_descendant(
        &c.cutex_session_id,
        &c.thread_id,
        thread,
        |id| {
            handle
                .request_with_timeout(
                    "thread/read",
                    json!({"threadId":id,"includeTurns":false}),
                    TIMEOUT,
                )
                .map(|v| v.get("thread").cloned())
                .map_err(|e| {
                    crate::management::v2::subagent_lineage::LineageReadError::Unavailable(
                        e.to_string(),
                    )
                })
        },
    )
    .map_err(|_| ())
}

fn seed(
    handle: &AppServerHandle,
    status: &Observation,
    context: &AppServerRuntimeEventContext,
    sink: &Arc<dyn AppServerRuntimeEventSink>,
    thread: &str,
    claude: bool,
) -> bool {
    let mark = {
        let mut s = status.inventory.lock().unwrap_or_else(|e| e.into_inner());
        let i = &mut *s;
        i.revision += 1;
        let mark = i.revision;
        i.seeds.insert(thread.into(), mark);
        mark
    };
    let success = seed_pages(handle, status, context, sink, thread, claude, mark);
    mutate(status, context, sink, |s| {
        let i = &mut *s;
        i.seeds.remove(thread);
        i.terminal_revisions.retain(|(t, _), _| t != thread);
        if claude {
            i.task_revisions.clear();
        }
        if !success {
            i.complete = false;
        }
    });
    success
}

fn seed_pages(
    handle: &AppServerHandle,
    status: &Observation,
    context: &AppServerRuntimeEventContext,
    sink: &Arc<dyn AppServerRuntimeEventSink>,
    thread: &str,
    claude: bool,
    mark: u64,
) -> bool {
    let method = if claude {
        "claude/tasks/list"
    } else {
        "thread/backgroundTerminals/list"
    };
    let mut cursor = Value::Null;
    let mut first = true;
    let mut cursors = HashSet::new();
    loop {
        let (s, c, sink, thread) = (
            status.clone(),
            context.clone(),
            sink.clone(),
            thread.to_owned(),
        );
        let response = handle.request_observed_with_timeout(
            method,
            json!({"threadId":thread,"cursor":cursor}),
            TIMEOUT,
            Box::new(move |r| {
                mutate(&s, &c, &sink, |s| {
                    if s.seeds.get(&thread) != Some(&mark) {
                        return;
                    }
                    let RpcResponseOutcome::Result(v) = &r.outcome else {
                        s.complete = false;
                        return;
                    };
                    if claude {
                        let Some(tasks) = v.get("tasks").and_then(Value::as_array) else {
                            s.complete = false;
                            return;
                        };
                        let i = &mut *s;
                        i.tasks
                            .retain(|id, _| i.task_revisions.get(id).is_some_and(|r| *r > mark));
                        for task in tasks {
                            if task
                                .get("taskId")
                                .and_then(Value::as_str)
                                .is_some_and(|id| {
                                    i.task_revisions.get(id).is_some_and(|r| *r > mark)
                                })
                            {
                                continue;
                            }
                            i.task(task);
                        }
                        i.complete =
                            v.get("completeInventory").and_then(Value::as_bool) == Some(true);
                    } else if let Some(rows) = v.get("data").and_then(Value::as_array) {
                        let inventory = &mut *s;
                        let ids: Vec<_> = rows
                            .iter()
                            .filter_map(|row| row.get("itemId").and_then(Value::as_str))
                            .filter(|id| {
                                inventory
                                    .terminal_revisions
                                    .get(&(thread.clone(), (*id).into()))
                                    .is_none_or(|r| *r <= mark)
                            })
                            .map(str::to_owned)
                            .collect();
                        let current = inventory.terminals.entry(thread.clone()).or_default();
                        if first {
                            current.retain(|id| {
                                inventory
                                    .terminal_revisions
                                    .get(&(thread.clone(), id.clone()))
                                    .is_some_and(|r| *r > mark)
                            });
                        }
                        current.extend(ids);
                    }
                });
            }),
        );
        let Ok(response) = response else { return false };
        if claude {
            return response.get("tasks").is_some_and(Value::is_array);
        }
        let Some(rows) = response.get("data").and_then(Value::as_array) else {
            return false;
        };
        if rows
            .iter()
            .any(|row| row.get("itemId").and_then(Value::as_str).is_none())
        {
            return false;
        }
        cursor = response.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            return true;
        }
        if !cursors.insert(cursor.to_string()) {
            return false;
        }
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::{AppServerClient, AppServerClientOptions, AppServerEndpoint};
    use super::*;
    use std::time::Instant;
    use tungstenite::Message;

    #[test]
    fn text_output_and_noncommand_items_never_enter_inventory_mutation() {
        for backend in ["host", "claude_subscription"] {
            let c = context(backend);
            for method in [
                "item/agentMessage/delta",
                "item/reasoning/textDelta",
                "item/commandExecution/outputDelta",
                "turn/started",
                "turn/completed",
                "item/started",
                "item/completed",
            ] {
                let params = json!({"threadId":"root","delta":"token","item":{"type":"agentMessage","id":"text"}});
                let event = AppServerEvent::Notification(super::super::protocol::RpcNotification {
                    method: method.into(),
                    params: Some(params.clone()),
                    raw: json!({"method":method,"params":params}),
                });
                assert!(!affects_inventory(&c, &event), "{backend}: {method}");
            }
        }
    }

    #[test]
    fn claude_counts_background_agents_and_own_shells_and_never_reopens_lost() {
        let mut i = Inventory::default();
        for task in [
            json!({"taskId":"agent","taskType":"local_agent","status":"running","background":true,"ownedBySubagent":true}),
            json!({"taskId":"own","taskType":"local_bash","status":"pending","background":true}),
            json!({"taskId":"nested","taskType":"local_bash","status":"running","background":true,"ownedBySubagent":true}),
            json!({"taskId":"foreground","taskType":"local_agent","status":"running","background":false}),
        ] {
            i.task(&task);
        }
        assert_eq!(
            i.summary(),
            BackgroundWork {
                running: true,
                subagents: 1,
                shells: 1,
                terminals: 0,
                complete: false
            }
        );
        for state in ["lost", "running", "pending"] {
            i.task(&json!({"taskId":"agent","taskType":"local_agent","status":state,"background":true}));
        }
        assert_eq!(i.summary().subagents, 0);
        for state in [
            "completed",
            "failed",
            "stopped",
            "killed",
            "cancelled",
            "unknown",
        ] {
            i.task(&json!({"taskId":"own","status":state}));
            assert!(!i.summary().running);
        }
    }

    #[test]
    fn terminal_identity_includes_thread_and_exit_can_belong_to_an_old_turn() {
        let mut i = Inventory::default();
        let item = json!({"id":"item","type":"commandExecution","processId":"42"});
        i.terminal("parent", "item/started", &item);
        i.terminal("child", "item/started", &item);
        i.terminal("child", "item/started", &item);
        assert_eq!(i.summary().terminals, 2);
        i.terminal(
            "parent",
            "item/completed",
            &json!({"id":"item","type":"commandExecution"}),
        );
        assert_eq!(i.summary().terminals, 1);
        i.terminal("child", "item/completed", &item);
        assert!(!i.summary().running);
        for n in 0..1024 {
            i.terminal(
                "parent",
                "item/started",
                &json!({"id":n.to_string(),"type":"commandExecution","processId":n.to_string()}),
            );
        }
        assert_eq!(
            i.summary().terminals,
            1024,
            "active inventory is never silently capped"
        );
    }

    #[test]
    fn completed_foreground_commands_leave_no_identity_metadata_and_background_task_fences_are_bounded(
    ) {
        let mut i = Inventory::default();
        for n in 0..10_000 {
            let item =
                json!({"id":n.to_string(),"type":"commandExecution","processId":n.to_string()});
            i.terminal("root", "item/started", &item);
            i.terminal("root", "item/completed", &item);
            i.task(&json!({"taskId":n.to_string(),"background":false,"status":"completed"}));
        }
        assert!(i.terminals.is_empty());
        assert!(i.terminal_revisions.is_empty());
        assert!(i.task_revisions.is_empty());
        assert!(i.ended_tasks.is_empty());
        i.complete = true;
        for n in 0..RECENT_TASKS + 1 {
            i.task(&json!({"taskId":n.to_string(),"background":true,"status":"lost"}));
        }
        assert_eq!(i.ended_tasks.len(), RECENT_TASKS);
        assert!(
            !i.summary().complete,
            "eviction is explicit, never silently authoritative"
        );
    }

    fn status(context: &AppServerRuntimeEventContext) -> Status {
        Arc::new(Mutex::new(AppServerManagedRuntimeStatus {
            background_work: Default::default(),
            cutex_session_id: context.cutex_session_id.clone(),
            thread_id: context.thread_id.clone(),
            runtime_generation: 1,
            connected: true,
            active_turn_id: None,
            active_turn_observed_at: None,
            thread_status: None,
            thread_status_observed_at: None,
            thread_settings: None,
            thread_settings_source: None,
            thread_settings_complete: false,
            thread_settings_observed_at: None,
            runtime_workspace_roots: None,
            instruction_sources: None,
            resume_snapshot_observed_at: None,
            event_method_counts: Default::default(),
            initialized_user_agent: None,
            last_event_at: None,
            last_error: None,
        }))
    }

    #[test]
    fn status_snapshots_copy_only_fixed_size_summary_for_a_large_inventory() {
        let c = context("host");
        let status = status(&c);
        let observation = Arc::new(ConnectionState {
            status: status.clone(),
            inventory: Mutex::new(Inventory::default()),
        });
        let sink: Arc<dyn AppServerRuntimeEventSink> = Arc::new(Sink::default());
        mutate(&observation, &c, &sink, |i| {
            for n in 0..10_000 {
                i.terminal("root","item/started",&json!({"id":format!("long-private-item-identity-{n}"),"type":"commandExecution","processId":n.to_string()}));
            }
        });
        // Inventory intentionally has no Clone implementation. Public status
        // contains only this inline, allocation-free type, never IDs or maps.
        assert!(std::mem::size_of::<Option<BackgroundWork>>() <= 48);
        for _ in 0..1000 {
            let snapshot = status.lock().unwrap().clone();
            let work = snapshot.background_work.unwrap();
            assert_eq!(work.terminals, 10_000);
            assert!(serde_json::to_string(&work).unwrap().len() < 128);
        }
        assert_eq!(
            observation.inventory.lock().unwrap().terminals["root"].len(),
            10_000
        );
    }
    fn context(backend: &str) -> AppServerRuntimeEventContext {
        AppServerRuntimeEventContext {
            cutex_session_id: uuid::Uuid::new_v4().to_string(),
            thread_id: "root".into(),
            runtime_generation: 1,
            runtime_backend: backend.into(),
            full_text_item_terminal: false,
            launched_profile: None,
            launched_profile_id: None,
            usage: Default::default(),
            schema: Default::default(),
        }
    }
    #[derive(Default)]
    struct Sink(Mutex<Vec<Option<BackgroundWork>>>);
    impl AppServerRuntimeEventSink for Sink {
        fn handle_event(
            &self,
            _: &AppServerRuntimeEventContext,
            _: &AppServerEvent,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn handle_background_work(
            &self,
            _: &AppServerRuntimeEventContext,
            work: Option<BackgroundWork>,
        ) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(work);
            Ok(())
        }
    }
    fn wait_for(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !f() {
            assert!(Instant::now() < deadline, "background inventory timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Real transport, deliberately never drain the durable consumer queue.
    /// Seed parent + loaded child, then child exit after response and transport
    /// loss. Tests the response barrier and disconnect independently of journaling.
    #[test]
    fn reconnect_inventory_rolls_up_loaded_children_and_orders_exits_before_disconnect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (close_tx, close_rx) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            loop {
                let Ok(Message::Text(text)) = socket.read() else {
                    break;
                };
                let request: Value = serde_json::from_str(text.as_ref()).unwrap();
                let Some(method) = request.get("method").and_then(Value::as_str) else {
                    continue;
                };
                if method == "initialized" {
                    continue;
                }
                let result = match method {
                    "initialize" => json!({}),
                    "thread/backgroundTerminals/list" => {
                        json!({"data":[{"itemId":if request["params"]["threadId"]=="root" {"p"} else {"c"},"processId":"42"}],"nextCursor":null})
                    }
                    "thread/loaded/list" => {
                        json!({"data":["root","child","unrelated"],"nextCursor":null})
                    }
                    "thread/read" => {
                        json!({"thread":{"id":request["params"]["threadId"],"parentThreadId":if request["params"]["threadId"]=="child" {json!("root")} else {Value::Null}}})
                    }
                    _ => panic!("unexpected own-client request: {method}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                if method == "thread/read" && request["params"]["threadId"] == "unrelated" {
                    socket.send(Message::Text(json!({"method":"item/completed","params":{"threadId":"child","turnId":"old-turn","item":{"id":"c","type":"commandExecution"}}}).to_string().into())).unwrap();
                    close_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                    break;
                }
            }
        });
        let client = AppServerClient::connect(AppServerClientOptions::new(
            AppServerEndpoint::LoopbackWebSocket {
                url: format!("ws://{address}"),
                bearer_token: None,
            },
        ))
        .unwrap();
        let c = context("host");
        let s = status(&c);
        let sink = Arc::new(Sink::default());
        attach(client.handle(), s.clone(), c.clone(), sink.clone()).unwrap();
        wait_for(|| {
            let s = s.lock().unwrap();
            s.background_work
                .as_ref()
                .is_some_and(|w| w.complete && w.terminals == 1)
        });
        assert!(
            sink.0
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .any(|w| w.terminals == 2),
            "child inventory was included before its exit"
        );
        close_tx.send(()).unwrap();
        wait_for(|| !s.lock().unwrap().connected);
        assert!(s.lock().unwrap().background_work.is_none());
        assert!(sink.0.lock().unwrap().last().unwrap().is_none());
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn late_child_lineage_and_inflight_snapshot_keep_new_start_and_reject_old_exit() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let event = |method: &str, id: &str| {
                json!({"method":method,"params":{"threadId":"late-child","turnId":"old-turn",
                "item":{"id":id,"type":"commandExecution","processId":"42"}}})
            };
            loop {
                let Ok(Message::Text(text)) = socket.read() else {
                    break;
                };
                let request: Value = serde_json::from_str(text.as_ref()).unwrap();
                let method = request["method"].as_str().unwrap();
                if method == "initialized" {
                    continue;
                }
                let result = match method {
                    "initialize" => json!({}),
                    "thread/loaded/list" => json!({"data":["root"],"nextCursor":null}),
                    "thread/read" => {
                        socket
                            .send(Message::Text(
                                event("item/started", "new-before-lineage")
                                    .to_string()
                                    .into(),
                            ))
                            .unwrap();
                        socket
                            .send(Message::Text(
                                event("item/completed", "first").to_string().into(),
                            ))
                            .unwrap();
                        json!({"thread":{"id":"late-child","parentThreadId":"root"}})
                    }
                    "thread/backgroundTerminals/list"
                        if request["params"]["threadId"] == "root" =>
                    {
                        json!({"data":[],"nextCursor":null})
                    }
                    "thread/backgroundTerminals/list" => {
                        socket
                            .send(Message::Text(
                                event("item/completed", "new-before-lineage")
                                    .to_string()
                                    .into(),
                            ))
                            .unwrap();
                        socket
                            .send(Message::Text(
                                event("item/started", "new-during-list").to_string().into(),
                            ))
                            .unwrap();
                        json!({"data":[{"itemId":"new-before-lineage","processId":"42"}],"nextCursor":null})
                    }
                    _ => panic!("unexpected request {method}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                if method == "thread/loaded/list" {
                    socket
                        .send(Message::Text(
                            event("item/started", "first").to_string().into(),
                        ))
                        .unwrap();
                }
                if method == "thread/backgroundTerminals/list"
                    && request["params"]["threadId"] == "late-child"
                {
                    done_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                    break;
                }
            }
        });
        let client = AppServerClient::connect(AppServerClientOptions::new(
            AppServerEndpoint::LoopbackWebSocket {
                url: format!("ws://{address}"),
                bearer_token: None,
            },
        ))
        .unwrap();
        let c = context("host");
        let s = status(&c);
        let observation =
            attach_observation(client.handle(), s.clone(), c, Arc::new(Sink::default()))
                .unwrap()
                .unwrap();
        wait_for(|| {
            observation
                .inventory
                .lock()
                .unwrap()
                .terminals
                .get("late-child")
                .is_some_and(|ids| ids.len() == 1 && ids.contains("new-during-list"))
        });
        assert!(s.lock().unwrap().background_work.as_ref().unwrap().running);
        wait_for(|| observation.inventory.lock().unwrap().seeds.is_empty());
        assert!(observation
            .inventory
            .lock()
            .unwrap()
            .terminal_revisions
            .is_empty());
        done_tx.send(()).unwrap();
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn failed_claude_inventory_marks_incomplete_without_clearing_observed_work() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            loop {
                let Ok(Message::Text(text)) = socket.read() else {
                    break;
                };
                let request: Value = serde_json::from_str(text.as_ref()).unwrap();
                match request["method"].as_str().unwrap() {
                    "initialized" => {}
                    "initialize" => socket
                        .send(Message::Text(
                            json!({"id":request["id"],"result":{}}).to_string().into(),
                        ))
                        .unwrap(),
                    "claude/tasks/list" => {
                        socket.send(Message::Text(json!({"method":"claude/task/updated","params":{"threadId":"root","task":{
                            "taskId":"a","taskType":"local_agent","status":"running","background":true}}}).to_string().into())).unwrap();
                        socket.send(Message::Text(json!({"id":request["id"],"error":{"code":-32601,"message":"unavailable"}}).to_string().into())).unwrap();
                        done_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                        break;
                    }
                    other => panic!("unexpected request {other}"),
                }
            }
        });
        let client = AppServerClient::connect(AppServerClientOptions::new(
            AppServerEndpoint::LoopbackWebSocket {
                url: format!("ws://{address}"),
                bearer_token: None,
            },
        ))
        .unwrap();
        let c = context("claude_subscription");
        let s = status(&c);
        let observation =
            attach_observation(client.handle(), s.clone(), c, Arc::new(Sink::default()))
                .unwrap()
                .unwrap();
        wait_for(|| {
            let s = s.lock().unwrap();
            s.background_work
                .as_ref()
                .is_some_and(|w| w.running && !w.complete)
        });
        wait_for(|| observation.inventory.lock().unwrap().seeds.is_empty());
        let work = s.lock().unwrap().background_work.clone().unwrap();
        assert!(work.running);
        assert!(!work.complete);
        done_tx.send(()).unwrap();
        drop(client);
        peer.join().unwrap();
    }
}
