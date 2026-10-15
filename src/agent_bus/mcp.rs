//! Narrow outbound MCP protocol adapter. Authority and receipts remain in Cutex.
use crate::role_revision::RuntimeAgentId;
use anyhow::{bail, Context};
use serde_json::{json, Value};

#[path = "mcp_list.rs"]
pub(crate) mod list;
#[path = "mcp_management.rs"]
mod management;
#[path = "mcp_services.rs"]
mod services;

pub const THREAD_HEADER: &str = "x-cutex-mcp-thread-id";
pub const GENERATION_HEADER: &str = "x-cutex-mcp-generation";

#[derive(Clone, Debug)]
pub struct CallerFence {
    pub thread_id: String,
    pub generation: u64,
}

/// A refusal decided before anything was sent, with a model-facing code.
#[derive(Debug)]
struct ToolRefusal {
    code: &'static str,
    detail: String,
}

impl std::fmt::Display for ToolRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

const OUTCOME_UNKNOWN_DETAIL: &str = "The request may or may not have taken effect: the Agent Bus answer was lost. Retrying the same call with the same action_id / alarm_id is deduplicated; otherwise check first (agent_list, alarm_list, service_list) before repeating it.";

/// Model-facing result for a tool call that failed before a Bus answer:
/// `no_write` when nothing was sent (fix the arguments or retry later),
/// `response_uncertain` when the outcome is unknown. Transport diagnostics
/// are never shown.
fn call_failure(error: &anyhow::Error) -> Value {
    if let Some(refusal) = error.downcast_ref::<ToolRefusal>() {
        return json!({"ok":false,"status":"no_write","code":refusal.code,"detail":refusal.detail});
    }
    if error
        .downcast_ref::<crate::agent_bus::client::BusUnreachable>()
        .is_some()
    {
        return json!({"ok":false,"status":"no_write","code":"bus_unavailable",
            "detail":"Nothing was sent: the local cutex Agent Bus is not reachable. Retry later; repeating the same call is safe."});
    }
    if error
        .downcast_ref::<crate::agent_bus::client::ResponseUncertain>()
        .is_some()
    {
        return json!({"ok":false,"status":"response_uncertain","code":"outcome_unknown","detail":OUTCOME_UNKNOWN_DETAIL});
    }
    let detail: String = format!("Nothing was sent: fix the arguments and retry. {error:#}")
        .chars()
        .take(600)
        .collect();
    json!({"ok":false,"status":"no_write","code":"invalid_arguments","detail":detail})
}

/// Turn a raw Bus HTTP rejection (`{http_status, error}`) into the same
/// shape: 4xx means nothing was done, 5xx/408 means the outcome is unknown.
fn normalize_bus_rejection(result: Value) -> Value {
    let Some(status) = result["http_status"].as_u64() else {
        return result;
    };
    if status >= 500 || status == 408 {
        return json!({"ok":false,"status":"response_uncertain","code":"outcome_unknown","detail":OUTCOME_UNKNOWN_DETAIL});
    }
    let error = &result["error"];
    let code = ["code", "error"]
        .iter()
        .find_map(|key| error[*key].as_str())
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 64
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .unwrap_or("rejected");
    let detail: String = ["message", "detail"]
        .iter()
        .find_map(|key| error[*key].as_str())
        .unwrap_or("The Agent Bus rejected the request.")
        .chars()
        .take(1000)
        .collect();
    json!({"ok":false,"status":"no_write","code":code,"http_status":status,
        "detail":format!("Nothing was done: {detail}")})
}

/// The short-ID preflight is read-only: a raw Bus rejection gets the usual
/// shape, and even a 5xx/408 means nothing was written (the tool itself was
/// never called), so it is never reported as an uncertain outcome.
fn preflight_rejection(result: Value) -> Value {
    if result.get("http_status").is_none() {
        return result;
    }
    let mut normalized = normalize_bus_rejection(result);
    if normalized["status"] == "response_uncertain" {
        normalized = json!({"ok":false,"status":"no_write","code":"id_resolution_unavailable",
            "detail":"Nothing was done: the Agent Bus could not resolve the IDs in this call. Retry shortly."});
    }
    normalized
}

/// `tools/list` for this agent: only the tools its roles admit (asked from
/// the Agent Bus once per list). If the Bus cannot answer (an older daemon,
/// or not reachable yet), every tool is listed: calls are still authorised
/// server-side, so this only affects visibility.
fn listed_tools(port: u16, token: &str, runtime: &RuntimeAgentId) -> Value {
    let mut list = tools();
    for _ in 0..2 {
        let Ok(toolset) = crate::agent_bus::client::fetch_mcp_toolset(port, token, runtime) else {
            continue;
        };
        if let (true, Some(names)) = (toolset["ok"] == true, toolset["tools"].as_array()) {
            let allowed: Vec<String> = names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect();
            // This server lists for Codex runtimes (Claude reads its
            // descriptor); the toolset omits delegation_wait when the
            // profile turned it off, and the descriptions follow.
            let wait = allowed.iter().any(|name| name == crate::asks::tools::DELEGATION_WAIT);
            list = tools_for(crate::asks::tools::ToolRuntime::Codex, wait);
            crate::agent_bus::tool_policy::retain_tools(&mut list, &allowed);
            return list;
        }
    }
    // Fail closed for opt-in tools when the Bus cannot say.
    super::mcp_browser::filter_listing(&mut list, false);
    crate::agent_bus::tool_policy::retain_tools_except(&mut list, &[super::web_search::TOOL]);
    list
}

const ALARM_SET: &str = "alarm_set";
const ALARM_LIST: &str = "alarm_list";
const ALARM_CANCEL: &str = "alarm_cancel";

pub fn tools() -> Value {
    let mut tools = crate::agent_bus::alarms::tools();
    tools.extend(management::tools());
    tools.extend(services::tools());
    // Delegations (委托): agent_list / agent_delegate / delegation_* (asks design §6).
    tools.extend(crate::asks::tools::definitions());
    // Agent → owner messages (served on the delegation route).
    tools.push(crate::owner_notify::tool::definition());
    // Artifacts (served on the delegation route too).
    tools.extend(crate::artifacts::tools::definitions(crate::asks::tools::ToolRuntime::Codex));
    tools.extend(super::mcp_browser::definitions());
    // Workflows (served on the delegation route too).
    tools.extend(crate::workflows::tools::definitions());
    // Search-only provider request (profile `searchTool`).
    tools.push(super::web_search::definition());
    json!({ "tools": tools })
}

/// [`tools`] with the delegation tools worded for `runtime`; `wait` lists
/// `delegation_wait`.
pub fn tools_for(runtime: crate::asks::tools::ToolRuntime, wait: bool) -> Value {
    let mut list = tools();
    if let Some(tools) = list["tools"].as_array_mut() {
        tools.retain(|tool| {
            !tool["name"].as_str().is_some_and(|name| crate::asks::tools::is_tool(name) || crate::artifacts::tools::is_tool(name))
        });
        tools.extend(crate::asks::tools::definitions_for(runtime, wait));
        tools.extend(crate::artifacts::tools::definitions(runtime));
    }
    list
}

/// Old tool names from before the 2026-10-07 rename
/// (docs/research/cutex-tool-naming.md, Rollout). `tools/call` still accepts
/// them for one release; `tools/list` never shows them. Renamed tools run the
/// new tool; removed tools answer `no_write` naming the replacement.
///
/// DROP this table (and [`legacy_call`]) in the first release after the
/// rename has shipped and every agent has restarted on the new names.
mod legacy {
    pub(super) enum Legacy {
        /// Same arguments, new name.
        Renamed(&'static str),
        /// `cutex_agent_management`: the tool is chosen by `operation`.
        Management,
        /// Gone; the detail names the replacement.
        Removed(&'static str),
    }

    const LIST: &str = "This tool was removed: use agent_list.";
    const TASKS: &str = "The Task Service tools were removed: use agent_delegate to hand work to another agent.";

    pub(super) const TOOLS: &[(&str, Legacy)] = &[
        ("set_alarm", Legacy::Renamed(super::ALARM_SET)),
        ("list_alarms", Legacy::Renamed(super::ALARM_LIST)),
        ("cancel_alarm", Legacy::Renamed(super::ALARM_CANCEL)),
        ("cutex_service_register", Legacy::Renamed(super::services::REGISTER)),
        ("cutex_service_update", Legacy::Renamed(super::services::UPDATE)),
        ("cutex_service_unregister", Legacy::Renamed(super::services::UNREGISTER)),
        ("cutex_service_list", Legacy::Renamed(super::services::LIST)),
        ("cutex_agent_management", Legacy::Management),
        ("send", Legacy::Removed("The send tool was removed: use agent_delegate when the other agent should act on it (notify: passive if you don't need the result), or agent_tell for a note.")),
        ("query_managed", Legacy::Removed(LIST)),
        ("cutex_agent_list", Legacy::Removed(LIST)),
        ("cutex_task_service", Legacy::Removed(TASKS)),
        ("cutex_task_service_read", Legacy::Removed(TASKS)),
        ("cutex_task_service_director", Legacy::Removed(TASKS)),
        ("cutex_task_service_terminal", Legacy::Removed(TASKS)),
        // r219: drop in r220 with the other tombstones.
        ("director_rotate", Legacy::Removed(DIRECTOR_ROTATE)),
    ];

    pub(super) const DIRECTOR_ROTATE: &str = crate::agent_management::DIRECTOR_ROTATE_REMOVED;

    pub(super) const MANAGEMENT_QUERY: &str = LIST;
    pub(super) const MANAGEMENT_OTHER: &str = "cutex_agent_management was split: use agent_create, agent_archive, member_runtime or member_replace.";
}

/// The tool a `tools/call` name runs: itself, or the new tool behind an old
/// name ([`legacy::TOOLS`]). Removed tools are refused before anything is sent.
fn legacy_call<'a>(name: &'a str, args: Value) -> Result<(&'a str, Value), ToolRefusal> {
    let Some((_, legacy)) = legacy::TOOLS.iter().find(|(old, _)| *old == name) else {
        return Ok((name, args));
    };
    let removed = |detail: &str| ToolRefusal { code: "tool_removed", detail: detail.to_string() };
    match legacy {
        legacy::Legacy::Renamed(new) => Ok((new, args)),
        legacy::Legacy::Removed(detail) => Err(removed(detail)),
        legacy::Legacy::Management => match management::legacy_call(&args) {
            Some(call) => Ok(call),
            None if args["operation"] == "query_managed" => Err(removed(legacy::MANAGEMENT_QUERY)),
            None if args["operation"] == "director_rotate" => Err(removed(legacy::DIRECTOR_ROTATE)),
            None => Err(removed(legacy::MANAGEMENT_OTHER)),
        },
    }
}

/// Alarm by complete id to an agent the caller may message without seeing it
/// (an external agent granted to a project answering that project's agents;
/// see `agent_bus::boundary`). Host-wide mode keeps the group roster rule.
/// Fails closed when the boundary cannot be loaded.
fn reply_alarm_allowed(caller: &str, target: &str) -> bool {
    crate::agent_bus::boundary::AgentBoundary::load()
        .is_ok_and(|boundary| !boundary.host_wide() && boundary.may_message(caller, target))
}

/// External clients need not implement cute-codex's per-call metadata extension.
/// A launcher may pin the protocol thread once; supplied metadata must agree.
fn caller_thread(params: &Value, bound_thread: Option<&str>) -> anyhow::Result<String> {
    let supplied = params.get("_meta").and_then(|meta| meta.get("threadId"));
    let supplied = supplied.map(|value| {
        value.as_str().context("invalid MCP threadId")
            .and_then(crate::session::identity::normalize_codex_session_id)
    }).transpose()?;
    let bound = bound_thread.map(crate::session::identity::normalize_codex_session_id).transpose()?;
    if let (Some(supplied), Some(bound)) = (&supplied, &bound) {
        anyhow::ensure!(supplied == bound, "MCP threadId differs from launcher binding");
    }
    bound.or(supplied).context("MCP threadId missing: use launcher binding or per-call metadata")
}

/// The default `action_id` of an ask tool call (asks §6.1), known before the
/// first side effect: (runtime, generation, thread, turn, tool, canonical
/// arguments). The turn comes from cute-codex's per-call metadata
/// (`x-codex-turn-metadata.turn_id`), else the call id (`callId`, Claude
/// Code's `claudecode/toolUseId`), else `call` = this one JSON-RPC call
/// (`rpc_call_key`: the outer request id within this MCP process), so
/// distinct calls without metadata never share an id while a retransmit of
/// the same request keeps it. A model-supplied `action_id` argument wins on
/// the bus side.
pub(crate) fn derive_action_id(params: &Value, call: &str, runtime: &RuntimeAgentId, generation: u64, thread: &str, tool: &str, args: &Value) -> String {
    let meta = &params["_meta"];
    fn given(value: Option<&Value>) -> Option<&str> {
        value.and_then(Value::as_str).filter(|key| !key.trim().is_empty())
    }
    let turn = given(meta.pointer("/x-codex-turn-metadata/turn_id"))
        .or_else(|| given(meta.get("callId")))
        .or_else(|| given(meta.get("claudecode/toolUseId")))
        .map(str::to_string)
        .unwrap_or_else(|| format!("rpc:{call}"));
    let digest = crate::approvals::request_digest(&json!({
        "runtime": runtime.as_str(),
        "generation": generation,
        "thread": thread,
        "turn": turn,
        "tool": tool,
        "arguments": args,
    }));
    format!("auto-{}", &digest[..32])
}

/// The per-call fallback key of `derive_action_id`: the outer JSON-RPC
/// request id, scoped to this MCP process (`process` is random per run, so
/// a client that restarts its id counter after a reconnect gets new ids).
fn rpc_call_key(process: &str, request_id: &Value) -> String {
    format!("{process}:{request_id}")
}

/// The turn of a tool call from cute-codex's per-call metadata, if any.
fn call_turn_id(params: &Value) -> Option<String> {
    params["_meta"]
        .pointer("/x-codex-turn-metadata/turn_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|turn| !turn.is_empty() && turn.len() <= 200)
        .map(str::to_string)
}

pub fn run() -> anyhow::Result<()> {
    use std::io::{BufRead, Read, Write};
    let runtime = RuntimeAgentId::new(std::env::var("CUTEX_AGENT_ID")?)
        .map_err(|_| anyhow::anyhow!("invalid runtime ID"))?;
    let generation: u64 = std::env::var("CUTEX_RUNTIME_GENERATION")?.parse()?;
    if generation == 0 {
        bail!("current runtime generation required");
    }
    let bound_thread = std::env::var("CUTEX_MCP_THREAD_ID").ok()
        .map(|value| crate::session::identity::normalize_codex_session_id(&value))
        .transpose()?;
    let token = std::env::var("CUTEX_AGENT_BUS_TOKEN")?;
    let url = url::Url::parse(&std::env::var("CUTEX_AGENT_BUS_URL")?)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.path() != "/"
        || url.query().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("MCP requires the configured local Cutex Bus endpoint");
    }
    let port = url.port().context("explicit local Bus port required")?;
    let mut input = std::io::stdin().lock();
    // Calls run concurrently (asks §5.8: a long agent_wait must not block a
    // parallel agent_tell); responses are written whole under a lock.
    let output = std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
    let token = std::sync::Arc::new(token);
    let mut calls: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let process = std::sync::Arc::new(uuid::Uuid::new_v4().simple().to_string());
    loop {
        let mut bytes = Vec::new();
        let count = input.by_ref().take(262145).read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        if count > 262144 || bytes.last() != Some(&b'\n') {
            bail!("MCP frame exceeds limit");
        }
        let message: Value = serde_json::from_slice(&bytes)?;
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let concurrent = message["method"] == "tools/call";
        let respond = {
            let (token, runtime, bound_thread, output, process) = (token.clone(), runtime.clone(), bound_thread.clone(), output.clone(), process.clone());
            move || -> anyhow::Result<()> {
        let id = &id;
        let result = match message["method"].as_str() {
            Some("initialize") => Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"cutex-outbound-prototype","version":"1"}}),
            ),
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(listed_tools(port, &token, &runtime)),
            Some("tools/call") => (|| -> anyhow::Result<Value> {
                let thread = caller_thread(&message["params"], bound_thread.as_deref())?;
                let name = message["params"]["name"]
                    .as_str()
                    .context("tool name missing")?;
                let args = message["params"]["arguments"].clone();
                let fence = CallerFence {
                    thread_id: thread,
                    generation,
                };
                let (name, mut args) = legacy_call(name, args).map_err(anyhow::Error::msg)?;
                if super::mcp_browser::is_tool(name) {
                    let request = super::mcp_browser::request_id(&process, id)?;
                    return Ok(super::mcp_browser::invoke(&token, &runtime, &fence, &request, name, &args));
                }
                if crate::asks::tools::is_tool(name) || management::is_tool(name) || name == ALARM_SET {
                    let resolved = crate::agent_bus::client::submit_mcp_control(
                        port, &token, &runtime, &fence, crate::asks::short_ids::PATH,
                        &json!({"tool":name,"arguments":args}),
                    )?;
                    if resolved["ok"] != true {
                        return Ok(preflight_rejection(resolved));
                    }
                    args = resolved["arguments"].clone();
                }
                let mut result = if crate::asks::tools::is_tool(name) || name == crate::owner_notify::tool::OWNER_NOTIFY || crate::workflows::tools::is_tool(name) {
                    let action_id = derive_action_id(&message["params"], &rpc_call_key(&process, id), &runtime, generation, &fence.thread_id, name, &args);
                    crate::agent_bus::client::submit_mcp_control(
                        port, &token, &runtime, &fence, crate::asks::tools::PATH,
                        &json!({"tool": name, "arguments": args, "action_id": action_id}),
                    )?
                } else if crate::artifacts::tools::is_tool(name) {
                    // Artifacts also carry the call's turn (provenance).
                    let action_id = derive_action_id(&message["params"], &rpc_call_key(&process, id), &runtime, generation, &fence.thread_id, name, &args);
                    let mut body = json!({"tool": name, "arguments": args, "action_id": action_id});
                    if let Some(turn) = call_turn_id(&message["params"]) {
                        body["turn_id"] = json!(turn);
                    }
                    crate::agent_bus::client::submit_mcp_control(port, &token, &runtime, &fence, crate::asks::tools::PATH, &body)?
                } else if management::is_tool(name) {
                    // action_id is optional: the same call in the same turn
                    // derives the same id (exact replay); a model-supplied one wins.
                    if let Some(object) = args.as_object_mut() {
                        if object.get("action_id").is_none_or(Value::is_null) {
                            let mut keyed = Value::Object(object.clone());
                            keyed.as_object_mut().map(|keyed| keyed.remove("action_id"));
                            let action_id = derive_action_id(&message["params"], &rpc_call_key(&process, id), &runtime, generation, &fence.thread_id, name, &keyed);
                            object.insert("action_id".into(), json!(action_id));
                        }
                    }
                    // Management results keep their own receipt shape.
                    return Ok(management::invoke(management::operation_args(name, args), |body| {
                        crate::agent_bus::client::submit_mcp_control(
                            port,
                            &token,
                            &runtime,
                            &fence,
                            "/api/agent-management/v1/actions",
                            body,
                        )
                    }));
                } else if super::web_search::is_tool(name) {
                    let mut body = json!({"arguments": args});
                    if let Some(turn) = call_turn_id(&message["params"]) {
                        body["turn_id"] = json!(turn);
                    }
                    crate::agent_bus::client::submit_mcp_control(port, &token, &runtime, &fence, super::web_search::PATH, &body)?
                } else if services::is_tool(name) {
                    match services::request_body(name, args) {
                        Ok(body) => crate::agent_bus::client::submit_mcp_control(
                            port, &token, &runtime, &fence, services::PATH, &body,
                        )?,
                        Err(error) => json!({"ok":false,"code":"invalid_arguments","detail":error.to_string()}),
                    }
                } else if matches!(name, ALARM_SET | ALARM_LIST | ALARM_CANCEL) {
                    let roster = crate::agent_bus::client::submit_mcp_control(
                        port, &token, &runtime, &fence,
                        crate::agent_bus::agent_auth::MCP_AGENT_LIST_PATH, &json!({}))?;
                    let outcome = (|| -> anyhow::Result<Value> {
                        anyhow::ensure!(roster["ok"] == true && roster["scope"] == "local_group_visible", "Caller registration unavailable");
                        let owner = crate::agent_bus::alarms::caller_owner(runtime.as_str())?;
                        match name {
                            ALARM_SET => {
                                let a: crate::agent_bus::alarms::SetArgs=serde_json::from_value(args)?;
                                if crate::agent_bus::routing::is_full_durable_cutex_session_id(&a.to) {
                                    let listed = roster["agents"].as_array().is_some_and(|rows| rows.iter().any(|r| r["cutex_session_id"].as_str()==Some(&a.to)));
                                    anyhow::ensure!(listed || reply_alarm_allowed(&owner, &a.to), "Target not visible to caller");
                                } else {
                                    anyhow::bail!("Canonical alarm target unavailable");
                                }
                                crate::agent_bus::alarms::set(&owner,a)
                            }
                            ALARM_LIST => {
                                anyhow::ensure!(args.as_object().is_some_and(|a|a.is_empty()),"Expected empty arguments");
                                crate::agent_bus::alarms::list(Some(&owner))
                            }
                            _ => {
                                anyhow::ensure!(args.as_object().is_some_and(|a|a.len()==1),"Expected alarm_id only");
                                crate::agent_bus::alarms::cancel(Some(&owner),args["alarm_id"].as_str().context("alarm_id missing")?)
                            }
                        }
                    })();
                    match outcome { Ok(value)=>json!({"ok":true,"result":value}),Err(e)=>json!({"ok":false,"code":"alarm_rejected","detail":e.to_string()}) }
                } else {
                    return Err(anyhow::Error::msg(ToolRefusal {
                        code: "unknown_tool",
                        detail: format!("Nothing was sent: {} is not a cutex tool; use the tools in tools/list.", name.chars().take(80).collect::<String>()),
                    }));
                };
                if matches!(name, ALARM_SET | ALARM_LIST | ALARM_CANCEL) {
                    if let Ok(index)=crate::short_ids::session_index() {
                        crate::short_ids::render_value(&mut result,&index,crate::short_ids::settings());
                    }
                }
                Ok(normalize_bus_rejection(result))
            })()
            .or_else(|error| Ok(if message["params"]["name"].as_str().is_some_and(super::mcp_browser::is_tool) {
                super::mcp_browser::failure("permission_denied")
            } else { call_failure(&error) }))
            .map(|result: Value| {
                if message["params"]["name"].as_str().is_some_and(super::mcp_browser::is_tool) {
                    return super::mcp_browser::validate_result(result).unwrap_or_else(|_| super::mcp_browser::failure("invalid_result"));
                }
                let failed = result.get("http_status").is_some()
                    || result["outcome"]["status"] == "no_write"
                    || result["outcome"]["status"] == "owner_action_required"
                    || matches!(
                        result["status"].as_str(),
                        Some("no_write" | "conflict" | "response_uncertain" | "approval_required")
                    )
                    || result["ok"] == false;
                json!({"isError":failed,"content":[{"type":"text","text":result.to_string()}]})
            }),
            _ => Err(anyhow::anyhow!("unsupported MCP method")),
        };
        // Never echo transport errors/credentials into model-visible output.
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(_) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"Cutex rejected operation, caller metadata, or authenticated service request"}})
            }
        };
        let mut output = output.lock().map_err(|_| anyhow::anyhow!("MCP output lock poisoned"))?;
        serde_json::to_writer(&mut *output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
        Ok(())
            }
        };
        if concurrent {
            calls.retain(|call| !call.is_finished());
            calls.push(std::thread::spawn(move || {
                let _ = respond();
            }));
        } else {
            respond()?;
        }
    }
    for call in calls {
        let _ = call.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strict function-declaration enum rules (Google / Vertex via
    /// OpenAI-compatible gateways): every `enum` / `const` sits next to a
    /// single `type` and all values share that one primitive type (integers
    /// count as `number`). Returns `tool: path: problem` lines.
    fn strict_enum_violations(tool: &str, schema: &Value, path: &str, out: &mut Vec<String>) {
        fn kind(value: &Value) -> &'static str {
            match value {
                Value::String(_) => "string",
                Value::Bool(_) => "boolean",
                Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
                Value::Number(_) => "number",
                Value::Null => "null",
                _ => "unsupported",
            }
        }
        let Some(map) = schema.as_object() else {
            if let Some(items) = schema.as_array() {
                for (i, item) in items.iter().enumerate() {
                    strict_enum_violations(tool, item, &format!("{path}[{i}]"), out);
                }
            }
            return;
        };
        let mut values: Vec<Value> = map.get("enum").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(value) = map.get("const") {
            values.push(value.clone());
        }
        if map.contains_key("enum") || map.contains_key("const") {
            let mut kinds: Vec<&str> = values.iter().map(kind).collect();
            if kinds.contains(&"number") {
                kinds.iter_mut().for_each(|k| if *k == "integer" { *k = "number" });
            }
            kinds.sort();
            kinds.dedup();
            let declared = map.get("type");
            let problem = if kinds.iter().any(|k| matches!(*k, "null" | "unsupported")) {
                Some("null or non-primitive value")
            } else if kinds.len() > 1 {
                Some("mixed value types")
            } else if declared.is_none() {
                Some("no type")
            } else if !declared.is_some_and(Value::is_string) {
                Some("type is not a single string")
            } else {
                let declared = declared.and_then(Value::as_str).unwrap_or_default();
                kinds.first().filter(|k| !(**k == declared || (declared == "number" && **k == "integer"))).map(|_| "type mismatch")
            };
            if let Some(problem) = problem {
                out.push(format!("{tool}: {path}: {problem}: {}", Value::Object(map.clone())));
            }
        }
        for key in ["properties", "$defs", "definitions", "patternProperties"] {
            if let Some(children) = map.get(key).and_then(Value::as_object) {
                for (name, child) in children {
                    strict_enum_violations(tool, child, &format!("{path}/{key}/{name}"), out);
                }
            }
        }
        for key in ["items", "additionalProperties", "anyOf", "oneOf", "allOf", "prefixItems", "not"] {
            if let Some(child) = map.get(key) {
                strict_enum_violations(tool, child, &format!("{path}/{key}"), out);
            }
        }
    }

    /// Every facade tool schema must pass the strict enum rules, for both
    /// runtimes' wordings.
    #[test]
    fn facade_tool_schemas_use_single_typed_enums() {
        use crate::asks::tools::ToolRuntime;
        let mut out = Vec::new();
        let mut listed = 0;
        for list in [tools(), tools_for(ToolRuntime::Codex, true), tools_for(ToolRuntime::Codex, false),
                     tools_for(ToolRuntime::Claude, true), tools_for(ToolRuntime::Claude, false)] {
            for tool in list["tools"].as_array().expect("tools") {
                listed += 1;
                let name = tool["name"].as_str().unwrap_or("?");
                strict_enum_violations(name, &tool["inputSchema"], "", &mut out);
            }
        }
        assert!(listed > 20, "{listed}");
        out.sort();
        out.dedup();
        assert!(out.is_empty(), "{} strict enum violations:\n{}", out.len(), out.join("\n"));
    }

    #[test]
    fn external_mcp_thread_binding_is_optional_but_cannot_be_overridden() {
        let thread = "018e0000-0000-7000-8000-0000a000037e";
        let other = "018e0000-0000-7000-8000-0000a000037f";
        assert_eq!(super::caller_thread(&serde_json::json!({}), Some(thread)).unwrap(), thread);
        let params = serde_json::json!({"_meta":{"threadId":thread}});
        assert_eq!(super::caller_thread(&params, None).unwrap(), thread);
        assert_eq!(super::caller_thread(&params, Some(thread)).unwrap(), thread);
        assert!(super::caller_thread(&params, Some(other)).is_err());
        assert!(super::caller_thread(&serde_json::json!({}), None).is_err());
        assert!(super::caller_thread(&serde_json::json!({"_meta":{"threadId":3}}), Some(thread)).is_err());
        assert!(super::caller_thread(&serde_json::json!({}), Some("../bad-thread")).is_err());
    }

    /// R3 (review 2026-10-11): without turn / call metadata (a launcher-bound
    /// external client, Claude Code without a tool-use id) the derived
    /// action_id comes from the outer JSON-RPC call, so an online, offline,
    /// online round trip with the same arguments gets three ids, while a
    /// retransmit of one request keeps its id. Metadata, when present, wins.
    #[test]
    fn derived_action_ids_fall_back_to_the_json_rpc_call() {
        let runtime = RuntimeAgentId::new("r3-fixture").unwrap();
        let online = serde_json::json!({"change":"online","target":"x"});
        let offline = serde_json::json!({"change":"offline","target":"x"});
        let id = |params: &Value, request: Value, args: &Value| {
            derive_action_id(params, &rpc_call_key("proc-a", &request), &runtime, 1, "thread", "member_runtime", args)
        };
        let bare = serde_json::json!({"name":"member_runtime"});
        let first = id(&bare, serde_json::json!(1), &online);
        let _ = id(&bare, serde_json::json!(2), &offline);
        let third = id(&bare, serde_json::json!(3), &online);
        assert_ne!(first, third, "distinct calls must not share an action id");
        assert_eq!(first, id(&bare, serde_json::json!(1), &online), "retransmit stays stable");
        assert_ne!(id(&bare, serde_json::json!(1), &online), id(&bare, serde_json::json!("1"), &online));
        // Another MCP process (client reconnect restarting its counter).
        assert_ne!(
            first,
            derive_action_id(&bare, &rpc_call_key("proc-b", &serde_json::json!(1)), &runtime, 1, "thread", "member_runtime", &online)
        );
        // Turn metadata / call ids keep the per-turn identity across requests.
        for meta in [
            serde_json::json!({"_meta":{"x-codex-turn-metadata":{"turn_id":"t-1"}}}),
            serde_json::json!({"_meta":{"callId":"call-1"}}),
            serde_json::json!({"_meta":{"claudecode/toolUseId":"toolu_1"}}),
        ] {
            assert_eq!(id(&meta, serde_json::json!(1), &online), id(&meta, serde_json::json!(9), &online));
            assert_ne!(id(&meta, serde_json::json!(1), &online), first);
        }
        // An empty turn id is no metadata.
        let empty = serde_json::json!({"_meta":{"x-codex-turn-metadata":{"turn_id":""}}});
        assert_ne!(id(&empty, serde_json::json!(1), &online), id(&empty, serde_json::json!(3), &online));
    }

    #[test]
    fn nonbrowser_claude_envelope_keeps_launcher_thread_and_rpc_action_identity() {
        let thread = "018e0000-0000-7000-8000-0000a000037e";
        let runtime = RuntimeAgentId::new("claude-envelope-fixture").unwrap();
        let params = json!({"name":"member_runtime","arguments":{"change":"online","target":"x"}});
        assert_eq!(caller_thread(&params, Some(thread)).unwrap(), thread);
        assert!(caller_thread(&params, None).is_err(), "non-browser adapter uses launcher binding");
        let id = |params: &Value, process: &str| derive_action_id(params, &rpc_call_key(process, &json!(1)),
            &runtime, 1, thread, "member_runtime", &params["arguments"]);
        assert_eq!(id(&params, "one"), id(&params, "one"));
        assert_ne!(id(&params, "one"), id(&params, "two"), "one-shot facade processes keep separate calls");
        let mut thread_only = params.clone();
        thread_only["_meta"] = json!({"threadId":thread});
        assert_eq!(id(&params, "one"), id(&thread_only, "one"), "thread metadata is not action-id input");
        // This is precisely why the adapter must NOT inject browser callId into
        // existing non-browser tools: the established metadata precedence differs.
        thread_only["_meta"]["callId"] = json!("native-call");
        assert_ne!(id(&params, "one"), id(&thread_only, "one"));
        assert_eq!(id(&thread_only, "one"), id(&thread_only, "two"));
    }

    const AUTHORITY_KEYS: [&str; 6] = [
        "caller",
        "caller_cutex_session_id",
        "role",
        "runtime_generation",
        "token",
        "threadId",
    ];

    #[test]
    fn mcp_schema_and_parser_do_not_accept_caller_authority() {
        for tool in tools()["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{}", tool["name"]);
            for key in AUTHORITY_KEYS {
                assert!(tool["inputSchema"]["properties"].get(key).is_none(), "{} {key}", tool["name"]);
            }
        }
        // Management tools refuse such fields before anything is sent.
        for tool in management::tools() {
            let name = tool["name"].as_str().unwrap();
            for key in AUTHORITY_KEYS {
                let args = json!({"action_id":"a","cutex_session_id":"cutex.target","change":"online",key:"spoof"});
                let result = management::invoke(management::operation_args(name, args), |_| {
                    panic!("{name}: spoofed {key} transported")
                });
                assert_eq!(result["outcome"]["status"], "no_write", "{name} {key}");
            }
        }
        // So do the services tools.
        for tool in services::tools() {
            let name = tool["name"].as_str().unwrap();
            for key in AUTHORITY_KEYS {
                assert!(services::request_body(name, json!({"slug":"web",key:"spoof"})).is_err());
            }
        }
    }

    #[test]
    fn old_names_are_never_listed_and_renamed_ones_run_the_new_tool() {
        let listed: Vec<String> = tools()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        for (old, legacy) in legacy::TOOLS {
            assert!(!listed.iter().any(|name| name == old), "{old} is listed");
            if let legacy::Legacy::Renamed(new) = legacy {
                assert!(listed.iter().any(|name| name == new), "{old} -> unlisted {new}");
            }
        }
        for (old, new) in [
            ("set_alarm", "alarm_set"),
            ("list_alarms", "alarm_list"),
            ("cancel_alarm", "alarm_cancel"),
            ("cutex_service_register", "service_register"),
            ("cutex_service_update", "service_update"),
            ("cutex_service_unregister", "service_unregister"),
            ("cutex_service_list", "service_list"),
        ] {
            let args = json!({"alarm_id":"x"});
            let (name, mapped) = legacy_call(old, args.clone()).unwrap();
            assert_eq!((name, mapped), (new, args));
        }
        // New and unknown names pass through unchanged.
        for name in ["alarm_set", "agent_create", "agent_tell", "no_such_tool"] {
            assert_eq!(legacy_call(name, json!({})).unwrap().0, name);
        }
    }

    #[test]
    fn old_management_operations_map_to_the_split_tools() {
        let spec = json!({"name":"A","cwd":"/w","profile":"p","runtime_backend":"host","model":"m",
            "reasoning":"low","permissions":"read-only","approval_policy":"on-request",
            "sandbox_mode":"read-only","groups":[]});
        for (old, tool, operation) in [
            (json!({"operation":"create","action_id":"a","spec":spec,"start_mode":"bootstrap_only"}), "agent_create", "create"),
            (json!({"operation":"close","action_id":"a","cutex_session_id":"cutex.t"}), "agent_archive", "close"),
            (json!({"operation":"restart","action_id":"a","cutex_session_id":"cutex.t"}), "member_runtime", "restart"),
            (json!({"operation":"replace","action_id":"a","predecessor_cutex_session_id":"cutex.t","policy":"keep_old","successor":spec,"start_mode":"bootstrap_only"}), "member_replace", "replace"),
        ] {
            let (name, args) = legacy_call("cutex_agent_management", old.clone()).unwrap();
            assert_eq!(name, tool);
            // The new tool sends the same provider request the old one did.
            let mut sent = Value::Null;
            management::invoke(management::operation_args(name, args), |body| {
                sent = body.clone();
                anyhow::bail!("stop")
            });
            assert_eq!(sent["operation"], operation);
            let mut expected = old;
            expected["schema"] = json!("cutex/agent-management/v1");
            let expected: crate::agent_management::AgentManagementRequest = serde_json::from_value(expected).unwrap();
            assert_eq!(sent, json!(expected));
        }
        let query = legacy_call("cutex_agent_management", json!({"operation":"query_managed","action_id":"q"})).unwrap_err();
        assert_eq!(query.code, "tool_removed");
        assert!(query.detail.contains("agent_list"));
        let other = legacy_call("cutex_agent_management", json!({"operation":"grant_operator"})).unwrap_err();
        assert!(other.detail.contains("agent_create") && other.detail.contains("member_runtime"));
        assert!(!other.detail.contains("director_rotate"));
        // r219: the old director_rotate operation is refused before anything is
        // sent; its recorded request shape still decodes (history), but a new
        // request is refused with tool_removed.
        let old = json!({"operation":"director_rotate","action_id":"a","expected_predecessor_cutex_session":"cutex.d",
            "expected_authority_epoch":1,"mode":"retain_predecessor_bootstrap_only","successor":spec});
        let rotate = legacy_call("cutex_agent_management", old.clone()).unwrap_err();
        assert_eq!(rotate.code, "tool_removed");
        assert!(rotate.detail.contains("owner assigns"), "{}", rotate.detail);
        let mut recorded = old;
        recorded["schema"] = json!("cutex/agent-management/v1");
        let recorded: crate::agent_management::AgentManagementRequest = serde_json::from_value(recorded).unwrap();
        assert_eq!(recorded.validate().unwrap_err().code(), "tool_removed");
    }

    #[test]
    fn removed_tools_answer_no_write_naming_the_replacement() {
        for (old, replacement) in [
            ("send", "agent_delegate"),
            ("send", "agent_tell"),
            ("query_managed", "agent_list"),
            ("cutex_agent_list", "agent_list"),
            ("cutex_task_service", "agent_delegate"),
            ("cutex_task_service_read", "agent_delegate"),
            ("cutex_task_service_director", "agent_delegate"),
            ("cutex_task_service_terminal", "agent_delegate"),
            ("director_rotate", "owner assigns the project Director"),
        ] {
            let refusal = legacy_call(old, json!({})).unwrap_err();
            let result = call_failure(&anyhow::Error::msg(refusal));
            assert_eq!(result["status"], "no_write", "{old}");
            assert_eq!(result["code"], "tool_removed", "{old}");
            assert!(result["detail"].as_str().unwrap().contains(replacement), "{old}: {result}");
        }
    }

    #[test]
    fn failures_distinguish_not_sent_from_outcome_unknown() {
        use crate::agent_bus::client::{BusUnreachable, ResponseUncertain};
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let unreachable = anyhow::Error::new(refused).context(BusUnreachable("connect".into()));
        assert_eq!(call_failure(&unreachable)["status"], "no_write");
        assert_eq!(call_failure(&unreachable)["code"], "bus_unavailable");
        let lost = anyhow::anyhow!("read timed out with token-ish detail").context(ResponseUncertain);
        let lost = call_failure(&lost);
        assert_eq!(lost["status"], "response_uncertain");
        assert!(!lost.to_string().contains("token-ish"));
        for stale in ["cutex_agent_list", "query_managed", "cutex_service_list", "external_message_id"] {
            assert!(!lost["detail"].as_str().unwrap().contains(stale), "{stale}");
        }
        let Err(invalid) = serde_json::from_value::<crate::agent_bus::alarms::SetArgs>(json!({"to":"x"})) else {
            panic!("alarm arguments without alarm_id parsed");
        };
        let invalid = call_failure(&anyhow::Error::new(invalid));
        assert_eq!(invalid["code"], "invalid_arguments");
        assert_eq!(invalid["status"], "no_write");

        let rejected = normalize_bus_rejection(json!({"http_status":403,"error":{"ok":false,"code":"recipient_outside_boundary","message":"outside"}}));
        assert_eq!(rejected["status"], "no_write");
        assert_eq!(rejected["code"], "recipient_outside_boundary");
        let odd = normalize_bus_rejection(json!({"http_status":400,"error":{"code":"has space"}}));
        assert_eq!(odd["code"], "rejected");
        let server = normalize_bus_rejection(json!({"http_status":502,"error":{"message":"internal path C:/x"}}));
        assert_eq!(server["status"], "response_uncertain");
        assert!(!server.to_string().contains("C:/x"));
        let fine = json!({"ok":true});
        assert_eq!(normalize_bus_rejection(fine.clone()), fine);
    }

    #[test]
    fn short_id_preflight_rejections_are_never_uncertain() {
        let old_bus = preflight_rejection(json!({"http_status":404,"error":{"error":"not_found"}}));
        assert_eq!((old_bus["status"].clone(), old_bus["code"].clone()), (json!("no_write"), json!("not_found")));
        let down = preflight_rejection(json!({"http_status":503,"error":{"message":"internal path C:/x"}}));
        assert_eq!((down["status"].clone(), down["code"].clone()), (json!("no_write"), json!("id_resolution_unavailable")));
        assert!(!down.to_string().contains("C:/x"));
        let resolver = json!({"ok":false,"code":"id_ambiguous","candidates":[]});
        assert_eq!(preflight_rejection(resolver.clone()), resolver);
    }
}
