//! Blocking HTTP route handling for the local cutex agent bus.

use std::collections::BTreeSet;
use std::net::TcpStream;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Context;
use chrono::Utc;
use serde_json::Value;

use crate::agent_bus::audit::append_agent_bus_audit_record;
use crate::agent_bus::groups::agent_from_register_request;
use crate::agent_bus::groups::update_agent_groups;
use crate::agent_bus::identity::normalize_agent_groups;
use crate::agent_bus::model::AgentBusAckRequest;
use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentBusGroupUpdateRequest;
use crate::agent_bus::model::AgentBusHeartbeatRequest;
use crate::agent_bus::model::AgentBusMessage;
use crate::agent_bus::model::AgentBusPollResponse;
use crate::agent_bus::model::AgentBusSendRequest;
use crate::agent_bus::model::AgentBusUnregisterRequest;
use crate::agent_bus::model::TASK_SERVICE_REMOVED_MESSAGE;
use crate::agent_bus::model::{
    JobServiceCompletionQuery, JobServiceCompletionReceipt, JOB_SERVICE_COMPLETION_MAX_BODY_BYTES,
};
use crate::agent_bus::queue::ack_agent_messages;
use crate::agent_bus::queue::poll_agent_messages;
use crate::agent_bus::routing::project_current_durable_session_ids;
use crate::agent_bus::routing::visible_agents_for_request;
use crate::agent_bus::routing::AgentTargetResolutionCode;
use crate::agent_bus::routing::AgentTargetResolutionError;
use crate::agent_bus::store::agent_is_local_to_bus;
use crate::agent_bus::store::persist_agent_bus_registry;
use crate::agent_bus::store::prune_stale_agents;
use crate::agent_bus::store::save_agent_bus_registry_locked;
use crate::agent_bus::store::AgentBusState;
use crate::agent_management::{
    AgentActionId, AgentManagementInvocation, AgentManagementOutcome, AgentManagementRequest,
    AgentManagementResponse, AgentManagementSchema, AGENT_MANAGEMENT_MAX_BODY_BYTES,
};
use crate::http::query::query_bool;
use crate::http::query::query_value;
use crate::http::server::read_simple_http_request;
use crate::http::server::require_service_bridge_token;
use crate::http::server::write_http_response;
use crate::http::server::write_json_response;
use crate::platform::host::current_host_name;
use crate::platform::now_epoch_secs;
use crate::role_revision::RuntimeAgentId;
use crate::session::store::load_cutex_session_store_shared;

const RESET: &str = "\x1b[0m";
const YELLOW: &str = "\x1b[33m";
const MAX_POLL_WAIT: Duration = Duration::from_secs(5);

struct AgentBusPollSignal {
    generation: Mutex<u64>,
    available: Condvar,
}

static AGENT_BUS_POLL_SIGNAL: OnceLock<AgentBusPollSignal> = OnceLock::new();

fn agent_bus_poll_signal() -> &'static AgentBusPollSignal {
    AGENT_BUS_POLL_SIGNAL.get_or_init(|| AgentBusPollSignal {
        generation: Mutex::new(0),
        available: Condvar::new(),
    })
}

pub fn notify_agent_bus_message_available() {
    let signal = agent_bus_poll_signal();
    if let Ok(mut generation) = signal.generation.lock() {
        *generation = generation.wrapping_add(1);
        signal.available.notify_all();
    }
}

#[derive(Clone, Copy)]
pub struct AgentBusRequestHandlers {
    pub reconcile_registration_agent: fn(&mut AgentBusAgent) -> anyhow::Result<()>,
    pub reconcile_agent: fn(&AgentBusAgent) -> anyhow::Result<()>,
    pub redrive_ordinary_messages: fn(&Arc<Mutex<AgentBusState>>) -> anyhow::Result<usize>,
    pub send_payload_response:
        fn(&Arc<Mutex<AgentBusState>>, AgentBusSendRequest) -> anyhow::Result<Value>,
    pub agent_management: fn(
        &Arc<Mutex<AgentBusState>>,
        AgentManagementInvocation,
        AgentManagementRequest,
    ) -> anyhow::Result<Value>,
    pub job_service_completion: fn(
        &Arc<Mutex<AgentBusState>>,
        &crate::agent_bus::identity::JobServiceSystemPrincipal,
        crate::agent_bus::job_completion::IncomingCompletion,
    ) -> anyhow::Result<JobServiceCompletionReceipt>,
    pub job_service_completion_query: fn(
        &crate::agent_bus::identity::JobServiceSystemPrincipal,
        JobServiceCompletionQuery,
    ) -> anyhow::Result<JobServiceCompletionReceipt>,
    /// Agent Management start messages just ACKed (delivered, or finished
    /// undelivered): tells the requester when one ended undelivered.
    pub management_start_acked: fn(&Arc<Mutex<AgentBusState>>, &[String]),
}

pub fn handle_agent_bus_request(
    stream: &mut TcpStream,
    state: &Arc<Mutex<AgentBusState>>,
    token: Option<&str>,
    job_service_token: Option<&str>,
    handlers: AgentBusRequestHandlers,
) -> anyhow::Result<()> {
    let request = read_simple_http_request(stream)?;
    // Removed routes answer before authentication and before any state access.
    if let Some(message) = removed_route(&request.path) {
        return write_json_response(
            stream,
            410,
            "Gone",
            &serde_json::json!({
                "ok": false,
                "code": "tool_removed",
                "message": message,
                "retryable": false,
            }),
        );
    }
    let request = match crate::agent_bus::agent_auth::BusRequest::authenticate(request, token) {
        Ok(request) => request,
        Err(rejection) => {
            return write_http_response(
                stream,
                401,
                "Unauthorized",
                "text/plain",
                rejection.message().as_bytes(),
            )
        }
    };
    // A per-agent credential reaches only the MCP adapter's fenced routes.
    if !request.agent_route_permitted() {
        return write_http_response(
            stream,
            403,
            "Forbidden",
            "text/plain",
            b"agent credentials are limited to the cutex MCP routes",
        );
    }
    // Route checks compare against the verified agent credential for agent
    // callers and the root token otherwise.
    let token = request.route_token(token);
    let path_only = request
        .path
        .split('?')
        .next()
        .unwrap_or(request.path.as_str());
    match (request.method.as_str(), path_only) {
        ("GET", "/") => write_http_response(stream, 200, "OK", "text/plain", b"ok"),
        ("GET", "/api/agents") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            if let Some(invocation) = validate_mcp_caller_fence(&request, state)? {
                if request.path != "/api/agents?all_groups=false&all_hosts=false" {
                    anyhow::bail!("unsupported MCP list scope");
                }
                let sessions = load_cutex_session_store_shared()?;
                let boundary = crate::agent_bus::boundary::AgentBoundary::load()?;
                let state = state.lock().map_err(|_| anyhow!("Bus unavailable"))?;
                let mut agents = mcp_visible_agents(&state, &invocation, &boundary, &sessions);
                agents.retain(|a| agent_is_local_to_bus(a, &current_host_name()));
                let mut result = crate::agent_bus::mcp::list::project(
                    &invocation.caller_runtime_agent_id,
                    agents,
                    &sessions,
                );
                mark_external_grants(
                    &mut result,
                    &boundary,
                    invocation.caller_cutex_session.as_str(),
                );
                return write_json_response(stream, 200, "OK", &result);
            }
            if prune_stale_agents(state)? {
                persist_agent_bus_registry(state)?;
            }
            let requester = query_value(&request.path, "agent_id");
            let all_groups =
                query_bool(&request.path, "all_groups") || query_bool(&request.path, "allGroups");
            let mut agents = {
                let state = state
                    .lock()
                    .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
                let mut agents =
                    visible_agents_for_request(&state, requester.as_deref(), all_groups);
                agents.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
                agents
            };
            if let Ok(sessions) = load_cutex_session_store_shared() {
                project_current_durable_session_ids(&mut agents, &sessions);
            }
            // Job Service adapter identity lookup under a per-agent
            // credential: the caller's own entry only, never the roster.
            if let Some(principal) = request.job_identity_lookup() {
                let own = crate::agent_bus::agent_auth::job_identity_view(agents, principal);
                return write_json_response(stream, 200, "OK", &serde_json::to_value(own)?);
            }
            write_json_response(stream, 200, "OK", &serde_json::to_value(agents)?)
        }
        // Role-filtered tool list for the MCP adapter (`tools/list`).
        ("POST", crate::agent_bus::agent_auth::TOOLSET_PATH) => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            write_json_response(stream, 200, "OK", &mcp_toolset_response(&request))
        }
        // No relay state is touched in the Bus process. Management forwards
        // the original cxa1 bearer, never a privileged replacement credential.
        ("POST", crate::agent_bus::agent_auth::BROWSER_FENCE_PATH) => {
            let valid_body = serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|v| v.as_object().is_some_and(|v| v.is_empty()));
            if request.agent().is_none() || !valid_body {
                return write_json_response(stream,403,"Forbidden",&serde_json::json!({"error":{"code":"permission_denied"}}));
            }
            match validate_mcp_caller_fence_for(&request,state,true) {
                Ok(Some(invocation)) => write_json_response(stream,200,"OK",&serde_json::json!({
                    "cutexSessionId":invocation.caller_cutex_session.as_str(),
                    "runtimeAgentId":invocation.caller_runtime_agent_id,
                    "threadId":request.headers.get(crate::agent_bus::mcp::THREAD_HEADER),
                    "runtimeGeneration":request.headers.get(crate::agent_bus::mcp::GENERATION_HEADER).and_then(|v|v.parse::<u64>().ok()),
                })),
                _ => write_json_response(stream,403,"Forbidden",&serde_json::json!({"error":{"code":"permission_denied"}})),
            }
        }
        // Asks (委托) MCP tools (asks design §6): the fence gives the caller.
        ("POST", crate::asks::tools::PATH | crate::asks::short_ids::PATH) => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let result = match validate_mcp_caller_fence(&request, state) {
                Ok(Some(invocation)) => {
                    let caller = invocation.caller_cutex_session.as_str().to_string();
                    let runtime = invocation.caller_runtime_agent_id.clone();
                    match crate::asks::store::AskStore::open_default() {
                        Ok(store) => {
                            let world = crate::asks::scheduler::HostWorld::load();
                            let send = |tell: &crate::asks::tools::TellSend<'_>| -> anyhow::Result<serde_json::Value> {
                                let payload = crate::agent_bus::model::AgentBusSendRequest {
                                    to: tell.to.to_string(),
                                    all_groups: true,
                                    kind: crate::agent_bus::model::AgentBusEnvelopeKind::Message,
                                    from: None,
                                    from_agent_id: Some(runtime.clone()),
                                    from_session_id: Some(caller.clone()),
                                    to_session_id: None,
                                    content: tell.text.to_string(),
                                    // A note starts no turn; a wake starts the
                                    // recipient's next turn (Codex and Claude).
                                    delivery_mode: Some(if tell.wake {
                                        crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
                                    } else {
                                        crate::agent_bus::delivery::AgentDeliveryMode::Passive
                                    }),
                                    queue_only: None,
                                    trigger_turn: None,
                                    sender_kind: None,
                                    display_source: None,
                                    submit_mode: None,
                                    control_type: None,
                                    control_payload: None,
                                    external_action_id: None,
                                    external_message_id: Some(tell.external_id.to_string()),
                                    sender_facts: Some(tell.facts.clone()),
                                };
                                (handlers.send_payload_response)(state, payload)
                            };
                            let wake_chain = |from: &str, to: &str, turn: &str| {
                                crate::management::v2::agent_bus_state::agent_bus_message_repository()
                                    .and_then(|repository| repository.wake_into_turn(from, to, turn))
                                    .ok()
                                    .flatten()
                            };
                            let clock = chrono::Utc::now;
                            let sleep =
                                || std::thread::sleep(std::time::Duration::from_millis(250));
                            let ctx = crate::asks::tools::ToolContext {
                                store: &store,
                                world: &world,
                                caller: &caller,
                                now: chrono::Utc::now(),
                                send: &send,
                                wake_chain: &wake_chain,
                                clock: &clock,
                                sleep: &sleep,
                                wait: crate::agent_bus::tool_policy::delegation_wait_enabled(
                                    &caller,
                                ),
                                facade: crate::asks::tools::facade_level(
                                    request
                                        .headers
                                        .get(crate::asks::tools::FACADE_HEADER)
                                        .map(String::as_str),
                                ),
                            };
                            if request.path == crate::asks::short_ids::PATH {
                                crate::asks::short_ids::handle(&ctx, &request.body)
                            } else {
                                crate::asks::tools::handle(&ctx, &request.body)
                            }
                        }
                        Err(_) => {
                            serde_json::json!({"ok": false, "status": "no_write", "code": "persistence_unavailable",
                            "detail": "the delegation store is unavailable; nothing was done"})
                        }
                    }
                }
                _ => serde_json::json!({"ok": false, "status": "no_write", "code": "unauthorized",
                    "detail": "delegation tools require the current persistent agent runtime"}),
            };
            write_json_response(stream, 200, "OK", &result)
        }
        // web_search (profile-search-tool): one search-only request to the
        // caller's own profile provider. Agents and the owner's quick chats.
        ("POST", crate::agent_bus::web_search::PATH) => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let result = match validate_mcp_caller_fence_for(&request, state, true) {
                Ok(Some(invocation)) => {
                    let thread = request.headers.get(crate::agent_bus::mcp::THREAD_HEADER).cloned().unwrap_or_default();
                    let generation = request
                        .headers
                        .get(crate::agent_bus::mcp::GENERATION_HEADER)
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or_default();
                    crate::agent_bus::web_search::handle(invocation.caller_cutex_session.as_str(), &thread, generation, &request.body)
                }
                _ => serde_json::json!({"ok": false, "status": "no_write", "code": "unauthorized",
                    "detail": "web_search requires the current persistent runtime"}),
            };
            write_json_response(stream, 200, "OK", &result)
        }
        // service_* MCP tools: the authenticated caller owns what it registers.
        ("POST", "/api/services/v1") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let result = match validate_mcp_caller_fence(&request, state) {
                Ok(Some(invocation)) => crate::services::agent_api::handle(
                    invocation.caller_cutex_session.as_str(),
                    &request.body,
                    approval_waiter(&request, stream, &invocation),
                ),
                _ => serde_json::json!({"ok": false, "code": "unauthorized",
                    "detail": "services tools require the current persistent agent runtime"}),
            };
            write_json_response(stream, 200, "OK", &result)
        }
        ("POST", "/api/agents/register") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let payload: crate::agent_bus::model::AgentBusRegisterRequest =
                serde_json::from_slice(&request.body)
                    .context("Failed to parse agent register JSON")?;
            let now = now_epoch_secs();
            let agent = agent_from_register_request(payload, now);
            let refresh = state
                .lock()
                .map_err(|_| anyhow!("agent bus state lock poisoned"))?
                .agents
                .get(&agent.id)
                .is_some_and(|known| known.pid == agent.pid);
            register_agent_with_reconciliation(
                state,
                agent,
                handlers.reconcile_registration_agent,
                save_agent_bus_registry_locked,
            )?;
            if registration_redrive_due(refresh, now) {
                if let Err(error) = (handlers.redrive_ordinary_messages)(state) {
                    eprintln!("warning: durable ordinary-message redrive failed: {error:#}");
                }
            }
            write_json_response(stream, 200, "OK", &serde_json::json!({"ok": true}))
        }
        ("POST", "/api/agents/groups") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            if prune_stale_agents(state)? {
                persist_agent_bus_registry(state)?;
            }
            let payload: AgentBusGroupUpdateRequest = serde_json::from_slice(&request.body)
                .context("Failed to parse agent group update JSON")?;
            let groups = normalize_agent_groups(payload.groups);
            let (agent_id, agent_name, groups, agent) =
                update_agent_groups(state, &payload.target, &groups, payload.mode)?;
            persist_agent_bus_registry(state)?;
            if let Err(err) = (handlers.reconcile_agent)(&agent) {
                eprintln!(
                    "{YELLOW}warning:{RESET} failed to reconcile cutex session registry: {err:#}"
                );
            }
            write_json_response(
                stream,
                200,
                "OK",
                &serde_json::json!({
                    "ok": true,
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "groups": groups,
                }),
            )
        }
        ("POST", "/api/agents/heartbeat") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let payload: AgentBusHeartbeatRequest = serde_json::from_slice(&request.body)
                .context("Failed to parse agent heartbeat JSON")?;
            let now = now_epoch_secs();
            {
                let mut state = state
                    .lock()
                    .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
                if let Some(agent) = state.agents.get_mut(&payload.id) {
                    agent.last_seen_epoch_secs = now;
                }
            }
            write_json_response(stream, 200, "OK", &serde_json::json!({"ok": true}))
        }
        ("POST", "/api/agents/unregister") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            #[cfg(feature = "stock-launch-test-hook")]
            crate::app_server::private_restart_phase("bus.unregister.lock.begin");
            let payload: AgentBusUnregisterRequest = serde_json::from_slice(&request.body)
                .context("Failed to parse agent unregister JSON")?;
            let removed = {
                let mut state = state
                    .lock()
                    .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
                #[cfg(feature = "stock-launch-test-hook")]
                crate::app_server::private_restart_phase("bus.unregister.lock.acquired");
                state.messages.remove(&payload.id);
                let removed = state.agents.remove(&payload.id).is_some();
                if removed {
                    save_agent_bus_registry_locked(&state)?;
                }
                removed
            };
            if removed {
                notify_agent_bus_message_available();
            }
            #[cfg(feature = "stock-launch-test-hook")]
            crate::app_server::private_restart_phase("bus.unregister.reply");
            write_json_response(
                stream,
                200,
                "OK",
                &serde_json::json!({"ok": true, "removed": removed}),
            )
        }
        ("POST", "/api/messages/send") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            if prune_stale_agents(state)? {
                persist_agent_bus_registry(state)?;
            }
            let mut payload: AgentBusSendRequest = serde_json::from_slice(&request.body)
                .context("Failed to parse agent message JSON")?;
            // Sender facts are established in process, never by an HTTP caller.
            payload.sender_facts = None;
            if let Some(invocation) = validate_mcp_caller_fence(&request, state)? {
                if payload.from_agent_id.as_ref() != request.headers.get("x-cutex-agent-id")
                    || payload.from.is_some()
                    || payload.from_session_id.is_some()
                    || payload.to_session_id.is_some()
                    || payload
                        .sender_kind
                        .as_ref()
                        .is_some_and(|kind| !kind.is_agent())
                    || payload.control_type.is_some()
                    || payload.control_payload.is_some()
                    || payload.submit_mode.is_some()
                    || payload.display_source.is_some()
                    || payload.external_action_id.is_some()
                    || payload.kind != crate::agent_bus::model::AgentBusEnvelopeKind::Message
                {
                    anyhow::bail!("unauthorized MCP sender or operation");
                }
                // Reuse the existing identity-bound send validation. A runtime
                // rebind cannot reinterpret this Core caller as another Agent.
                payload.from_session_id =
                    Some(invocation.caller_cutex_session.as_str().to_string());
                // Cooperation boundary: inside the caller's project / creator
                // tree (or a linked domain, or an external agent grant in
                // either direction) groups no longer restrict delivery;
                // outside it nothing is sent.
                let boundary = crate::agent_bus::boundary::AgentBoundary::load()?;
                if !boundary.host_wide() {
                    let caller = invocation.caller_cutex_session.as_str();
                    let inside =
                        crate::agent_bus::routing::is_full_durable_cutex_session_id(&payload.to)
                            && boundary.may_message(caller, &payload.to);
                    if inside && !boundary.may_see(&payload.to, caller) {
                        // A granted external agent cannot list the project's
                        // agents: tell it whom to answer.
                        payload.sender_facts = Some(crate::agent_bus::model::AgentSenderFacts {
                            reply_only: true,
                            ..Default::default()
                        });
                    }
                    if !inside {
                        return write_json_response(
                            stream,
                            403,
                            "Forbidden",
                            &serde_json::json!({
                                "ok": false,
                                "code": "recipient_outside_boundary",
                                "message": "Message was not sent: the recipient is outside your project. Agents of other projects are isolated unless the owner links the projects.",
                            }),
                        );
                    }
                    payload.all_groups = true;
                }
            }
            let response = match (handlers.send_payload_response)(state, payload) {
                Ok(response) => response,
                Err(error) => {
                    if let Some(response) = write_agent_target_resolution_error(stream, &error) {
                        return response;
                    }
                    return Err(error);
                }
            };
            write_json_response(stream, 200, "OK", &response)
        }
        // Owner stuck-message support (Management `agent-bus-stuck-messages`):
        // root bus credential only (per-agent credentials never reach here).
        ("GET", "/api/messages/queued") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let queued = crate::agent_bus::queue::queued_agent_messages(state)?;
            write_json_response(
                stream,
                200,
                "OK",
                &serde_json::json!({"ok": true, "messages": queued}),
            )
        }
        ("POST", "/api/messages/withdraw") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let payload: Value = serde_json::from_slice(&request.body)
                .context("Failed to parse message withdraw JSON")?;
            let message_ids = payload
                .get("messageIds")
                .and_then(Value::as_array)
                .filter(|ids| !ids.is_empty() && ids.len() <= 256)
                .and_then(|ids| {
                    ids.iter()
                        .map(|id| {
                            id.as_str()
                                .filter(|id| !id.is_empty() && id.len() <= 512)
                                .map(str::to_string)
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| anyhow!("messageIds: 1-256 nonempty message ids are required"))?;
            let reason = payload
                .get("reason")
                .and_then(Value::as_str)
                .map(|reason| crate::agent_bus::audit::content_preview(reason, 200));
            let withdrawn = crate::agent_bus::queue::withdraw_agent_messages(state, &message_ids)?;
            if let Err(err) = append_agent_bus_audit_record(serde_json::json!({
                "event": "withdrawn",
                "timestamp": Utc::now().to_rfc3339(),
                "message_ids": message_ids,
                "withdrawn": withdrawn
                    .iter()
                    .map(|(agent_id, message_id)| serde_json::json!({"agent_id": agent_id, "message_id": message_id}))
                    .collect::<Vec<_>>(),
                "reason": reason,
            })) {
                eprintln!("{YELLOW}warning:{RESET} failed to write agent audit log: {err:#}");
            }
            write_json_response(
                stream,
                200,
                "OK",
                &serde_json::json!({
                    "ok": true,
                    "withdrawn": withdrawn
                        .iter()
                        .map(|(agent_id, message_id)| serde_json::json!({"agentId": agent_id, "messageId": message_id}))
                        .collect::<Vec<_>>(),
                }),
            )
        }
        ("POST", "/api/messages/redrive") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let redriven = (handlers.redrive_ordinary_messages)(state)?;
            write_json_response(
                stream,
                200,
                "OK",
                &serde_json::json!({"ok": true, "redriven": redriven}),
            )
        }
        ("POST", "/api/job-service/v1/completions") => {
            let Some(route_token) = job_service_token.filter(|value| !value.is_empty()) else {
                return write_http_response(
                    stream,
                    503,
                    "Service Unavailable",
                    "text/plain",
                    b"Job Service completion credential unavailable",
                );
            };
            if require_service_bridge_token(&request, Some(route_token), "Job Service completion")
                .is_err()
            {
                return write_http_response(
                    stream,
                    401,
                    "Unauthorized",
                    "text/plain",
                    b"Unauthorized Job Service completion request",
                );
            }
            if request.body.len() > JOB_SERVICE_COMPLETION_MAX_BODY_BYTES {
                return write_http_response(
                    stream,
                    413,
                    "Payload Too Large",
                    "text/plain",
                    b"Job Service completion request exceeds route limit",
                );
            }
            let payload: crate::agent_bus::job_completion::IncomingCompletion =
                serde_json::from_slice(&request.body)
                    .context("strict Job Service completion parsing failed")?;
            let principal = crate::agent_bus::identity::job_service_system_principal();
            let response = (handlers.job_service_completion)(state, &principal, payload)?;
            write_json_response(stream, 200, "OK", &serde_json::to_value(response)?)
        }
        ("POST", "/api/job-service/v1/completions/query") => {
            let Some(route_token) = job_service_token.filter(|value| !value.is_empty()) else {
                return write_http_response(
                    stream,
                    503,
                    "Service Unavailable",
                    "text/plain",
                    b"Job Service completion credential unavailable",
                );
            };
            if require_service_bridge_token(&request, Some(route_token), "Job Service completion")
                .is_err()
            {
                return write_http_response(
                    stream,
                    401,
                    "Unauthorized",
                    "text/plain",
                    b"Unauthorized Job Service completion request",
                );
            }
            if request.body.len() > JOB_SERVICE_COMPLETION_MAX_BODY_BYTES {
                return write_http_response(
                    stream,
                    413,
                    "Payload Too Large",
                    "text/plain",
                    b"Job Service completion query exceeds route limit",
                );
            }
            let payload: JobServiceCompletionQuery = serde_json::from_slice(&request.body)
                .context("strict Job Service completion query parsing failed")?;
            let principal = crate::agent_bus::identity::job_service_system_principal();
            let response = (handlers.job_service_completion_query)(&principal, payload)?;
            write_json_response(stream, 200, "OK", &serde_json::to_value(response)?)
        }
        ("POST", "/api/agent-management/v1/actions") => {
            let route_started = std::time::Instant::now();
            if request.body.len() > AGENT_MANAGEMENT_MAX_BODY_BYTES {
                let response = agent_management_no_write(
                    AgentActionId::new("invalid-body").expect("fixed action ID"),
                    "body_too_large",
                    "Agent Management request exceeds route limit",
                );
                return write_json_response(stream, 200, "OK", &serde_json::to_value(response)?);
            }
            // Sparse agent_create / member_replace (P2): resolved into the full
            // request below, once the MCP caller is known.
            let sparse = if crate::agent_management::is_sparse_body(&request.body) {
                match serde_json::from_slice::<crate::agent_management::SparseAgentManagementRequest>(
                    &request.body,
                ) {
                    Ok(sparse) => Some(sparse),
                    Err(error) => {
                        let response = agent_management_no_write(
                            AgentActionId::new("invalid-body").expect("fixed action ID"),
                            "invalid_arguments",
                            &bounded_detail(format!("invalid agent arguments: {error}")),
                        );
                        return write_json_response(
                            stream,
                            200,
                            "OK",
                            &serde_json::to_value(response)?,
                        );
                    }
                }
            } else {
                None
            };
            let parsed = match &sparse {
                // Placeholder until resolution; never executed as is.
                Some(sparse) => Ok(sparse_placeholder(sparse)),
                None => serde_json::from_slice::<AgentManagementRequest>(&request.body),
            };
            let payload: AgentManagementRequest = match parsed {
                Ok(payload) => payload,
                Err(_) => {
                    let response = agent_management_no_write(
                        AgentActionId::new("invalid-body").expect("fixed action ID"),
                        "invalid_body",
                        "strict Agent Management request parsing failed",
                    );
                    return write_json_response(
                        stream,
                        200,
                        "OK",
                        &serde_json::to_value(response)?,
                    );
                }
            };
            if require_agent_management_bridge_token(&request, token).is_err() {
                let response = agent_management_no_write(
                    payload.action_id.clone(),
                    "unauthorized",
                    "Agent Management requires authenticated Agent Bus access",
                );
                return write_json_response(stream, 200, "OK", &serde_json::to_value(response)?);
            }
            // An agent on an older facade may still send director_rotate.
            if let Some(response) = removed_agent_management_operation(&payload) {
                return write_json_response(stream, 200, "OK", &serde_json::to_value(response)?);
            }
            match validate_mcp_caller_fence(&request, state) {
                Ok(Some(invocation)) => {
                    if !matches!(
                        payload.operation,
                        crate::agent_management::AgentOperation::QueryManaged
                            | crate::agent_management::AgentOperation::Create { .. }
                            | crate::agent_management::AgentOperation::Online { .. }
                            | crate::agent_management::AgentOperation::Offline { .. }
                            | crate::agent_management::AgentOperation::Restart { .. }
                            | crate::agent_management::AgentOperation::Close { .. }
                            | crate::agent_management::AgentOperation::Replace { .. }
                    ) {
                        return write_json_response(
                            stream,
                            200,
                            "OK",
                            &serde_json::to_value(agent_management_no_write(
                                payload.action_id.clone(),
                                "unauthorized",
                                "MCP operation is not enabled",
                            ))?,
                        );
                    }
                    // Sparse: resolve, authorize and bind the resolution to
                    // the action id first (R2), so the launch policy and its
                    // approval gate see the frozen first resolution (R4).
                    let mut sparse_sha256 = None;
                    let payload = match &sparse {
                        Some(sparse) => match crate::agent_management::admit(
                            &invocation.caller_cutex_session,
                            sparse,
                        ) {
                            Ok(admitted) => {
                                sparse_sha256 = Some(admitted.sparse_sha256);
                                admitted.request
                            }
                            Err(refusal) => {
                                let response = agent_management_no_write(
                                    payload.action_id.clone(),
                                    refusal.code,
                                    &bounded_detail(refusal.detail),
                                );
                                return write_json_response(
                                    stream,
                                    200,
                                    "OK",
                                    &serde_json::to_value(response)?,
                                );
                            }
                        },
                        None => payload,
                    };
                    let waiter = approval_waiter(&request, stream, &invocation);
                    // Q21: the owner approved this launch while the call held.
                    let mut approval_note = None;
                    if let Some(refusal) = agent_launch_policy_refusal(
                        &invocation,
                        &payload,
                        waiter.as_ref(),
                        &mut approval_note,
                    ) {
                        return write_json_response(
                            stream,
                            200,
                            "OK",
                            &serde_json::to_value(refusal)?,
                        );
                    }
                    // Only façades whose result parser takes the note get it
                    // (older ones refuse unknown fields on a receipt).
                    let approval_note = approval_note.filter(|_| {
                        crate::approvals::facade_takes_approval_note(
                            request
                                .headers
                                .get(crate::asks::tools::FACADE_HEADER)
                                .map(String::as_str),
                        )
                    });
                    let Some(sparse_sha256) = sparse_sha256 else {
                        let mut response = (handlers.agent_management)(state, invocation, payload)?;
                        attach_launch_approval_note(&mut response, approval_note);
                        return write_json_response(stream, 200, "OK", &response);
                    };
                    let full_sha256 = crate::agent_management::full_sha256(&payload);
                    // agent-create-task-delegation: the task goes to the new
                    // agent as a delegation from the caller, not a start message.
                    let delegated = sparse
                        .as_ref()
                        .and_then(|sparse| crate::agent_management::delegated_task(sparse, &payload))
                        .map(str::to_string);
                    let caller = invocation.caller_cutex_session.clone();
                    let action_id = payload.action_id.clone();
                    let mut response = (handlers.agent_management)(state, invocation, payload)?;
                    if let Some(task) = delegated {
                        crate::asks::create_task::attach(
                            &mut response,
                            caller.as_str(),
                            &task,
                            action_id.as_str(),
                            crate::asks::tools::facade_level(
                                request
                                    .headers
                                    .get(crate::asks::tools::FACADE_HEADER)
                                    .map(String::as_str),
                            ),
                            route_started,
                        );
                    }
                    // The caller binds the receipt to the request it sent.
                    if let Some(digest) = response.pointer_mut("/outcome/receipt/request_sha256") {
                        if full_sha256.is_some_and(|full| digest.as_str() == Some(full.as_str())) {
                            *digest = serde_json::Value::String(sparse_sha256.as_str().to_string());
                        }
                    }
                    attach_launch_approval_note(&mut response, approval_note);
                    return write_json_response(stream, 200, "OK", &response);
                }
                Err(_) => {
                    return write_json_response(
                        stream,
                        200,
                        "OK",
                        &serde_json::to_value(agent_management_no_write(
                            payload.action_id.clone(),
                            "unauthorized",
                            "MCP current runtime binding rejected",
                        ))?,
                    )
                }
                _ => {}
            }
            if sparse.is_some() {
                // Sparse requests resolve from the fenced MCP caller only.
                let response = agent_management_no_write(
                    payload.action_id.clone(),
                    "unauthorized",
                    "sparse agent arguments require the cutex MCP tools",
                );
                return write_json_response(stream, 200, "OK", &serde_json::to_value(response)?);
            }
            let sender = match agent_management_sender(&request, state) {
                Ok(sender) => sender,
                Err(_) => {
                    let response = agent_management_no_write(
                        payload.action_id.clone(),
                        "unauthorized",
                        "Agent Bus sender authentication failed",
                    );
                    return write_json_response(
                        stream,
                        200,
                        "OK",
                        &serde_json::to_value(response)?,
                    );
                }
            };
            let invocation = match resolve_agent_management_invocation(&sender) {
                Ok(invocation) => invocation,
                Err(error) => {
                    let response = agent_management_no_write(
                        payload.action_id.clone(),
                        error.code(),
                        error.detail(),
                    );
                    return write_json_response(
                        stream,
                        200,
                        "OK",
                        &serde_json::to_value(response)?,
                    );
                }
            };
            let response = (handlers.agent_management)(state, invocation, payload)?;
            write_json_response(stream, 200, "OK", &response)
        }
        ("GET", "/api/messages/poll") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let agent_id = query_value(&request.path, "agent_id")
                .ok_or_else(|| anyhow!("Missing agent_id query parameter"))?;
            let ack_mode = query_value(&request.path, "ack")
                .as_deref()
                .is_some_and(|value| matches!(value, "1" | "true" | "yes"));
            let wait = poll_wait_duration(&request.path);
            let (agent_name, messages) =
                poll_agent_messages_with_wait(state, &agent_id, ack_mode, wait)?;
            if !messages.is_empty() {
                // Identical repeated polls (un-ACKed messages re-polled every
                // few seconds) are coalesced (`agent_bus_audit`).
                let message_ids = messages
                    .iter()
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                if let Err(err) = crate::agent_bus::audit::record_agent_bus_poll(
                    &agent_id,
                    &agent_name,
                    ack_mode,
                    &message_ids,
                    crate::platform::now_epoch_secs(),
                ) {
                    eprintln!("{YELLOW}warning:{RESET} failed to write agent audit log: {err:#}");
                }
            }
            write_json_response(
                stream,
                200,
                "OK",
                &serde_json::to_value(AgentBusPollResponse { messages })?,
            )
        }
        ("POST", "/api/messages/ack") => {
            require_service_bridge_token(&request, token, "Agent Bus")?;
            let payload: AgentBusAckRequest = serde_json::from_slice(&request.body)
                .context("Failed to parse agent message ack JSON")?;
            let management_starts =
                queued_management_starts(state, &payload.agent_id, &payload.message_ids);
            let acked = ack_agent_messages(state, &payload.agent_id, &payload.message_ids)?;
            if acked > 0 && !management_starts.is_empty() {
                (handlers.management_start_acked)(state, &management_starts);
            }
            if acked > 0 {
                crate::agent_bus::audit::reset_agent_bus_poll_coalescing(&payload.agent_id);
                if let Err(err) = append_agent_bus_audit_record(serde_json::json!({
                    "event": "acked",
                    "timestamp": Utc::now().to_rfc3339(),
                    "agent_id": payload.agent_id,
                    "count": acked,
                    "message_ids": payload.message_ids,
                })) {
                    eprintln!("{YELLOW}warning:{RESET} failed to write agent audit log: {err:#}");
                }
            }
            write_json_response(
                stream,
                200,
                "OK",
                &serde_json::json!({
                    "ok": true,
                    "acked": acked,
                }),
            )
        }
        _ => write_http_response(stream, 404, "Not Found", "text/plain", b"not found"),
    }
}

/// Ids among `ids` of Agent Management start messages still queued for
/// `agent_id` (read before the ACK removes them).
fn queued_management_starts(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
    ids: &[String],
) -> Vec<String> {
    let Ok(state) = state.lock() else {
        return Vec::new();
    };
    state
        .messages
        .get(agent_id)
        .map(|queue| {
            queue
                .iter()
                .filter(|message| {
                    ids.contains(&message.id)
                        && message.from == crate::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER
                        && message.control_type.as_deref()
                            == Some(crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE)
                })
                .map(|message| message.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn write_agent_target_resolution_error(
    stream: &mut TcpStream,
    error: &anyhow::Error,
) -> Option<anyhow::Result<()>> {
    let target = error.downcast_ref::<AgentTargetResolutionError>()?;
    let (status, reason) = match target.code() {
        AgentTargetResolutionCode::NotFound => (404, "Not Found"),
        AgentTargetResolutionCode::Ambiguous | AgentTargetResolutionCode::TargetUnavailable => {
            (409, "Conflict")
        }
    };
    Some(write_json_response(
        stream,
        status,
        reason,
        &serde_json::json!({
            "ok": false,
            "code": target.code().label(),
            "message": target.to_string(),
        }),
    ))
}

/// Routes removed in r219 that still answer for one release (HTTP 410,
/// code `tool_removed`, not retryable). Drop in r220.
fn removed_route(path: &str) -> Option<&'static str> {
    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/api/rotation/v1/release" => Some("release rotation was removed in r219"),
        // Task Service: `/api/task/actions`, `/api/task/actions/reconcile`,
        // `/api/task/v2/*` and anything else under `/api/task`.
        "/api/task" => Some(TASK_SERVICE_REMOVED_MESSAGE),
        _ if path.starts_with("/api/task/") => Some(TASK_SERVICE_REMOVED_MESSAGE),
        _ => None,
    }
}

/// r219: operations removed from Agent Management. Answered `tool_removed`
/// (no write) before the caller fence, launch policy or provider run, so an
/// agent on an older MCP facade gets the same refusal as the new tombstone.
/// Drop in r220 with the other tombstones.
fn removed_agent_management_operation(
    request: &AgentManagementRequest,
) -> Option<AgentManagementResponse> {
    matches!(
        request.operation,
        crate::agent_management::AgentOperation::DirectorRotate { .. }
    )
    .then(|| {
        agent_management_no_write(
            request.action_id.clone(),
            "tool_removed",
            crate::agent_management::DIRECTOR_ROTATE_REMOVED,
        )
    })
}

fn agent_management_no_write(
    action_id: AgentActionId,
    code: &str,
    detail: &str,
) -> AgentManagementResponse {
    AgentManagementResponse {
        schema: AgentManagementSchema::V1,
        action_id,
        outcome: AgentManagementOutcome::NoWrite {
            code: code.to_string(),
            detail: detail.to_string(),
        },
    }
}

#[derive(Clone, Debug)]
struct AgentManagementRosterSender {
    runtime_agent_id: RuntimeAgentId,
    roster_session_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentManagementInvocationError {
    PersistenceUnavailable,
    StaleRuntimeIdentity,
}

impl AgentManagementInvocationError {
    fn code(self) -> &'static str {
        match self {
            Self::PersistenceUnavailable => "persistence_unavailable",
            Self::StaleRuntimeIdentity => "stale_runtime_identity",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::PersistenceUnavailable => "Agent Management durable session state is unavailable",
            Self::StaleRuntimeIdentity => {
                "ambient runtime does not resolve to one active durable Cutex session"
            }
        }
    }
}

fn require_agent_management_bridge_token(
    request: &crate::http::server::SimpleHttpRequest,
    token: Option<&str>,
) -> anyhow::Result<()> {
    let route_token = token
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Agent Management requires a configured Agent Bus token"))?;
    require_service_bridge_token(request, Some(route_token), "Agent Bus")
}

/// Agents an MCP caller may see: its cooperation boundary (project, creator
/// tree, linked domains; see `agent_bus::boundary`) or, when the owner chose
/// host-wide visibility, its Agent Bus groups as before.
fn mcp_visible_agents(
    state: &AgentBusState,
    invocation: &AgentManagementInvocation,
    boundary: &crate::agent_bus::boundary::AgentBoundary,
    sessions: &crate::session::model::CutexSessionStore,
) -> Vec<AgentBusAgent> {
    if boundary.host_wide() {
        return visible_agents_for_request(state, Some(&invocation.caller_runtime_agent_id), false);
    }
    let mut agents: Vec<AgentBusAgent> = state.agents.values().cloned().collect();
    project_current_durable_session_ids(&mut agents, sessions);
    let caller = invocation.caller_cutex_session.as_str();
    agents.retain(|agent| {
        agent.id == invocation.caller_runtime_agent_id
            || agent
                .cutex_session_id
                .as_deref()
                .is_some_and(|session| boundary.may_see(caller, session))
    });
    agents
}

/// Rows of external agents granted to the caller's project carry
/// `"external_grant": true` (they are not project members).
fn mark_external_grants(
    listing: &mut serde_json::Value,
    boundary: &crate::agent_bus::boundary::AgentBoundary,
    caller: &str,
) {
    let Some(rows) = listing["agents"].as_array_mut() else {
        return;
    };
    for row in rows {
        let granted = row["cutex_session_id"]
            .as_str()
            .is_some_and(|session| boundary.external_grant(caller, session));
        if granted {
            row["external_grant"] = serde_json::Value::Bool(true);
        }
    }
}

/// Durable session of the runtime a root-token caller declares (adapters
/// launched before per-agent credentials).
fn session_for_declared_runtime(
    request: &crate::agent_bus::agent_auth::BusRequest,
) -> anyhow::Result<Option<String>> {
    let Some(runtime) = request
        .headers
        .get("x-cutex-agent-id")
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let sessions = load_cutex_session_store_shared()?;
    let mut matches = sessions.sessions.values().filter(|record| {
        record.is_active() && record.current_runtime_agent_id.as_deref() == Some(runtime.as_str())
    });
    Ok(match (matches.next(), matches.next()) {
        (Some(record), None) => Some(record.cutex_session_id.clone()),
        _ => None,
    })
}

/// `POST /api/mcp/v1/toolset`: the caller's roles and the cutex tools it is
/// shown (`agent_bus::tool_policy`).
fn mcp_toolset_response(request: &crate::agent_bus::agent_auth::BusRequest) -> serde_json::Value {
    let session = match request.agent() {
        Some(principal) => Some(principal.cutex_session_id.clone()),
        None => session_for_declared_runtime(request).ok().flatten(),
    };
    let Some(session) = session else {
        return serde_json::json!({"ok": false, "code": "caller_unresolved"});
    };
    match crate::agent_bus::tool_policy::roles_for_session(&session) {
        Ok(roles) => crate::agent_bus::tool_policy::toolset_response(&session, roles),
        Err(_) => serde_json::json!({"ok": false, "code": "roles_unavailable"}),
    }
}

fn bounded_detail(mut detail: String) -> String {
    const MAX: usize = 4000;
    if detail.len() > MAX {
        let mut end = MAX;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail.push('…');
    }
    detail
}

/// Owner decisions 2026-10-07 for agent-initiated launches (create,
/// replace): the new agent never exceeds the caller's permission
/// envelope (`agent_management::envelope::clamp_or_refuse`), and a profile
/// the owner marked as needing approval waits for that approval. Returns the
/// no-write response to answer instead of executing. Exact replays of an
/// admitted action and Human-reviewed bootstrap intents keep their admission.
/// A complete Agent Management result after an approval-wait carries the
/// approved-after-wait note (Q21), so the agent knows the owner reviewed
/// the launch and does not retry it. Failed / refused results do not.
fn attach_launch_approval_note(response: &mut serde_json::Value, note: Option<serde_json::Value>) {
    if let Some(note) = note {
        if response.pointer("/outcome/status").and_then(serde_json::Value::as_str) == Some("complete") {
            crate::approvals::attach_approval_note(response, note);
        }
    }
}

/// `approval_note` receives the approved-after-wait note when the owner
/// approved the launch while this call held for the decision.
fn agent_launch_policy_refusal(
    invocation: &AgentManagementInvocation,
    request: &AgentManagementRequest,
    waiter: Option<&crate::approvals::Waiter>,
    approval_note: &mut Option<serde_json::Value>,
) -> Option<AgentManagementResponse> {
    use crate::agent_management::envelope::{
        clamp_or_refuse, requested_from_spec, PermissionEnvelope, PermissionExceedsCreator,
    };
    use crate::agent_management::AgentOperation as Operation;
    let (operation, spec) = match &request.operation {
        Operation::Create { spec, .. } => ("create", spec),
        Operation::Replace { successor, .. } => ("replace", successor),
        _ => return None,
    };
    if request.operation.bootstrap_intent().is_some() {
        return None;
    }
    let no_write = |code: &str, detail: String| {
        Some(agent_management_no_write(
            request.action_id.clone(),
            code,
            &bounded_detail(detail),
        ))
    };
    match crate::agent_management::AgentManagementProvider::open_default()
        .and_then(|provider| Ok(provider.store().snapshot()?))
    {
        Ok(snapshot) if snapshot.actions.contains_key(&request.action_id) => return None,
        Ok(_) => {}
        Err(_) => {
            return no_write(
                "persistence_unavailable",
                "Agent Management state is unavailable; nothing was created".into(),
            )
        }
    }
    let Ok(sessions) = load_cutex_session_store_shared() else {
        return no_write(
            "persistence_unavailable",
            "session state is unavailable; nothing was created".into(),
        );
    };
    let Some(caller) = sessions
        .sessions
        .get(invocation.caller_cutex_session.as_str())
    else {
        return no_write(
            "stale_runtime_identity",
            "the calling agent has no durable session".into(),
        );
    };
    let envelope = match PermissionEnvelope::from_record(caller) {
        Ok(envelope) => envelope,
        Err(detail) => return no_write("creator_permissions_unknown", detail),
    };
    if let Err(refusal) = clamp_or_refuse(&envelope, &requested_from_spec(spec)) {
        return no_write(
            PermissionExceedsCreator::CODE,
            format!(
                "{} Offending fields: {}",
                refusal.detail(),
                serde_json::to_string(&refusal.violations).unwrap_or_default()
            ),
        );
    }
    let profile = spec.profile.as_deref().unwrap_or_default();
    let store = crate::profiles::settings_api::load_store().ok();
    // No aliases: a renamed profile's old name is unknown. Answer with the
    // current names so the caller can retry with one of them.
    if let Some(store) = &store {
        if !profile.trim().is_empty()
            && crate::profiles::settings_api::find(store, profile.trim()).is_none()
        {
            return no_write(
                "unknown_profile",
                crate::profiles::identity::unknown_profile_message(
                    profile,
                    &crate::profiles::identity::available_profile_names(store),
                ),
            );
        }
    }
    let needs_approval = store
        .as_ref()
        .and_then(|store| crate::profiles::settings_api::find(store, profile).cloned())
        .is_some_and(|account| {
            crate::profiles::settings_api::agent_use_requires_approval(&account)
        });
    if !needs_approval {
        return None;
    }
    let digest = crate::approvals::request_digest(&serde_json::json!({
        "project_id": request.project_id,
        "operation": request.operation,
    }));
    let gate = crate::approvals::ApprovalStore::open_default().and_then(|store| {
        crate::approvals::gate_with_wait(
            &store,
            crate::approvals::ApprovalRequest {
                kind: crate::approvals::ApprovalKind::AgentProfile,
                reason: "profile_requires_approval".into(),
                requester_cutex_session_id: invocation.caller_cutex_session.as_str().to_string(),
                requester_name: caller.formal_agent_name.clone(),
                project_id: request
                    .project_id
                    .as_ref()
                    .map(|id| id.as_str().to_string()),
                summary: format!(
                    "{operation} agent {} with profile {profile} ({} / {})",
                    spec.name, spec.sandbox_mode, spec.approval_policy
                ),
                details: serde_json::json!({
                    "operation": operation,
                    "name": spec.name,
                    "profile": profile,
                    "model": spec.model,
                    "cwd": spec.cwd,
                    "sandboxMode": spec.sandbox_mode,
                    "approvalPolicy": spec.approval_policy,
                }),
                request_sha256: digest,
            },
            waiter,
            |record| crate::management::v2::approvals::publish_changed(record, "requested"),
            crate::management::v2::approvals::queue_decision_notice,
        )
    });
    match gate {
        Ok(outcome @ crate::approvals::GateOutcome::Approved(_)) => {
            *approval_note = outcome.approved_after_wait();
            None
        }
        Ok(outcome) => {
            let refusal = outcome.refusal().unwrap_or_default();
            no_write(
                refusal["code"].as_str().unwrap_or("approval_required"),
                format!(
                    "{} (approval_id {})",
                    refusal["detail"].as_str().unwrap_or_default(),
                    refusal["approval_id"].as_str().unwrap_or_default()
                ),
            )
        }
        Err(_) => no_write(
            "persistence_unavailable",
            "owner approval state is unavailable; nothing was created".into(),
        ),
    }
}

/// The hold an MCP call asked for (approval-wait: header
/// [`crate::approvals::APPROVAL_WAIT_HEADER`], sent only by façades cutex
/// opted in), bound to this connection and the caller's current runtime.
/// `None` answers `approval_required` at once, as before.
fn approval_waiter(
    request: &crate::agent_bus::agent_auth::BusRequest,
    stream: &TcpStream,
    invocation: &AgentManagementInvocation,
) -> Option<crate::approvals::Waiter> {
    use crate::agent_bus::mcp::{GENERATION_HEADER, THREAD_HEADER};
    let secs = crate::approvals::parse_wait_secs(
        request
            .headers
            .get(crate::approvals::APPROVAL_WAIT_HEADER)
            .map(String::as_str),
    )?;
    let thread = request.headers.get(THREAD_HEADER)?.clone();
    let generation: u64 = request.headers.get(GENERATION_HEADER)?.parse().ok()?;
    let turn = request
        .headers
        .get(crate::approvals::TURN_HEADER)
        .map(|turn| turn.trim().to_string())
        .filter(|turn| {
            !turn.is_empty() && turn.len() <= 200 && !turn.chars().any(char::is_control)
        });
    let peer = stream.try_clone().ok()?;
    let session = invocation.caller_cutex_session.as_str().to_string();
    let runtime = invocation.caller_runtime_agent_id.clone();
    let fence_thread = thread.clone();
    Some(crate::approvals::Waiter::new(
        secs,
        Some(thread),
        turn,
        move || connection_open(&peer),
        move || caller_still_current(&session, &runtime, generation, &fence_thread),
    ))
}

/// The client still waits for the answer: it sent its whole request and
/// reads, so a readable end of stream (or an error) means it went away.
fn connection_open(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let mut byte = [0_u8; 1];
    let open = match stream.peek(&mut byte) {
        Ok(0) => false,
        Ok(_) => true,
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ),
    };
    let _ = stream.set_nonblocking(false);
    open
}

/// The runtime that asked is still the session's current one (same runtime
/// id, generation and thread; not retired). Checked once, before a holding
/// call uses an approval.
fn caller_still_current(session: &str, runtime: &str, generation: u64, thread: &str) -> bool {
    load_cutex_session_store_shared().is_ok_and(|store| {
        store.sessions.get(session).is_some_and(|record| {
            record.runtime_generation == generation
                && record.current_runtime_agent_id.as_deref() == Some(runtime)
                && record.app_server_thread_id() == Some(thread)
                && !record.is_retired()
        })
    })
}

/// Stand-in for a sparse request until it is resolved: carries its action and
/// project for early refusals. Never executed (the route resolves or refuses).
fn sparse_placeholder(
    sparse: &crate::agent_management::SparseAgentManagementRequest,
) -> AgentManagementRequest {
    AgentManagementRequest {
        schema: crate::agent_management::AgentManagementSchema::V1,
        action_id: sparse.action_id.clone(),
        project_id: sparse.project_id.clone(),
        operation: crate::agent_management::AgentOperation::QueryManaged,
    }
}

/// Additional fail-closed occurrence fence for the opt-in MCP adapter. Native
/// routes retain their existing contract; this grants no new principal/role.
/// For a per-agent credential the fenced session must also be the session the
/// credential was issued for.
pub(crate) fn validate_mcp_caller_fence(
    request: &crate::agent_bus::agent_auth::BusRequest,
    state: &Arc<Mutex<AgentBusState>>,
) -> anyhow::Result<Option<AgentManagementInvocation>> {
    validate_mcp_caller_fence_for(request, state, false)
}

/// [`validate_mcp_caller_fence`]; `owned_sessions` also admits the current
/// runtime of an owned session (a quick chat: local-only, no Agent identity).
/// Only the browser fence passes true: a quick chat holds browser tools but
/// never the Agent Bus tools.
fn validate_mcp_caller_fence_for(
    request: &crate::agent_bus::agent_auth::BusRequest,
    state: &Arc<Mutex<AgentBusState>>,
    owned_sessions: bool,
) -> anyhow::Result<Option<AgentManagementInvocation>> {
    use crate::agent_bus::mcp::{GENERATION_HEADER, THREAD_HEADER};
    if !request.headers.contains_key(THREAD_HEADER)
        && !request.headers.contains_key(GENERATION_HEADER)
    {
        return Ok(None);
    }
    let thread = request
        .headers
        .get(THREAD_HEADER)
        .context("MCP thread missing")?;
    let generation: u64 = request
        .headers
        .get(GENERATION_HEADER)
        .context("MCP generation missing")?
        .parse()?;
    let sender = agent_management_sender(request, state)?;
    let roster = state
        .lock()
        .map_err(|_| anyhow!("Bus unavailable"))?
        .agents
        .get(sender.runtime_agent_id.as_str())
        .cloned()
        .context("MCP occurrence missing")?;
    let sessions = load_cutex_session_store_shared()?;
    let matches: Vec<_> = sessions
        .sessions
        .iter()
        .filter(|(_, r)| r.app_server_thread_id() == Some(thread.as_str()))
        .collect();
    let [(key, record)] = matches.as_slice() else {
        anyhow::bail!("MCP native mapping absent or ambiguous");
    };
    let caller_cutex_session =
        crate::role_revision::CutexSessionId::new(record.cutex_session_id.clone())
            .map_err(|_| anyhow!("invalid durable identity"))?;
    if *key != &record.cutex_session_id
        || sender.roster_session_id != *thread
        || generation == 0
        || record.runtime_generation != generation
        || record.current_runtime_agent_id.as_deref() != Some(sender.runtime_agent_id.as_str())
        || record.is_retired()
        || (record.explicit_launch.is_some() && record.app_server_launch_claim_id.is_some())
        || !(persistent_agent_occurrence(record, &roster)
            || (owned_sessions && owned_session_occurrence(record, &roster)))
    {
        anyhow::bail!("MCP occurrence is not the current persistent runtime");
    }
    if let Some(principal) = request.agent() {
        if principal.cutex_session_id != record.cutex_session_id
            || principal.runtime_agent_id != sender.runtime_agent_id.as_str()
        {
            anyhow::bail!("MCP occurrence does not match the agent credential");
        }
    }
    Ok(Some(AgentManagementInvocation {
        caller_cutex_session,
        caller_runtime_agent_id: sender.runtime_agent_id.as_str().to_string(),
    }))
}

fn persistent_agent_occurrence(
    record: &crate::session::model::CutexSessionRecord,
    roster: &crate::agent_bus::model::AgentBusAgent,
) -> bool {
    use crate::agent_bus::model::AgentRegistrationClass::Persistent;
    record.agent_enabled && record.registration_class == Persistent && roster.registration_class == Persistent
}

/// An owned session (quick chat) registers local-only and stays local-only.
fn owned_session_occurrence(
    record: &crate::session::model::CutexSessionRecord,
    roster: &crate::agent_bus::model::AgentBusAgent,
) -> bool {
    record.is_owned_session()
        && roster.registration_class == crate::agent_bus::model::AgentRegistrationClass::LocalOnly
}

fn agent_management_sender(
    request: &crate::http::server::SimpleHttpRequest,
    state: &Arc<Mutex<AgentBusState>>,
) -> anyhow::Result<AgentManagementRosterSender> {
    let sender_id = request
        .headers
        .get("x-cutex-agent-id")
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Agent Management sender header is missing"))?;
    let runtime_agent_id = RuntimeAgentId::new(sender_id.to_string())
        .map_err(|_| anyhow!("Agent Management sender is invalid"))?;
    let roster = state
        .lock()
        .map_err(|_| anyhow!("Agent Bus state lock is unavailable"))?
        .agents
        .get(sender_id)
        .cloned()
        .ok_or_else(|| anyhow!("Agent Management sender is not registered"))?;
    if roster.id != sender_id
        || !agent_is_local_to_bus(&roster, &current_host_name())
        || !crate::platform::process::process_is_running(roster.pid)
    {
        return Err(anyhow!("Agent Management sender is not current and local"));
    }
    let cutoff =
        now_epoch_secs().saturating_sub(crate::agent_bus::store::AGENT_BUS_STALE_HEARTBEAT_SECS);
    if roster.last_seen_epoch_secs < cutoff {
        return Err(anyhow!("Agent Management sender heartbeat is stale"));
    }
    let roster_session_id = roster
        .session_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Agent Management sender has no durable session"))?;
    Ok(AgentManagementRosterSender {
        runtime_agent_id,
        roster_session_id,
    })
}

fn resolve_agent_management_invocation(
    sender: &AgentManagementRosterSender,
) -> Result<AgentManagementInvocation, AgentManagementInvocationError> {
    let store = crate::session::store::load_cutex_session_store_shared()
        .map_err(|_| AgentManagementInvocationError::PersistenceUnavailable)?;
    resolve_agent_management_invocation_from_store(sender, &store)
}

fn resolve_agent_management_invocation_from_store(
    sender: &AgentManagementRosterSender,
    store: &crate::session::model::CutexSessionStore,
) -> Result<AgentManagementInvocation, AgentManagementInvocationError> {
    // Bus session IDs are protocol thread IDs, not names or native Claude UUIDs.
    let mut matches = store.sessions.iter().filter(|(_, record)| {
        record.app_server_thread_id() == Some(sender.roster_session_id.as_str())
            || (record.claude_session.is_none()
                && record.cutex_session_id == sender.roster_session_id)
    });
    let (key, session) = matches
        .next()
        .ok_or(AgentManagementInvocationError::StaleRuntimeIdentity)?;
    if matches.next().is_some() || key != &session.cutex_session_id {
        return Err(AgentManagementInvocationError::StaleRuntimeIdentity);
    }
    if session.archive_state != crate::session::model::CutexSessionArchiveState::Active
        || session.current_runtime_agent_id.as_deref() != Some(sender.runtime_agent_id.as_str())
    {
        return Err(AgentManagementInvocationError::StaleRuntimeIdentity);
    }
    Ok(AgentManagementInvocation {
        caller_cutex_session: crate::role_revision::CutexSessionId::new(
            session.cutex_session_id.clone(),
        )
        .map_err(|_| AgentManagementInvocationError::StaleRuntimeIdentity)?,
        caller_runtime_agent_id: sender.runtime_agent_id.as_str().to_string(),
    })
}

/// Minimum spacing of durable-message redrives triggered by registration
/// refreshes. A new registration (unknown id, or a new process for it) always
/// redrives; each agent also refreshes every 30 s, and redriving on every
/// refresh re-read the whole durable message state and session store several
/// times a second with many agents (Linux host, 2026-10-09).
const REFRESH_REDRIVE_INTERVAL_SECS: u64 = 60;

static LAST_REFRESH_REDRIVE_EPOCH_SECS: AtomicU64 = AtomicU64::new(0);

fn registration_redrive_due(refresh: bool, now_epoch_secs: u64) -> bool {
    registration_redrive_due_at(&LAST_REFRESH_REDRIVE_EPOCH_SECS, refresh, now_epoch_secs)
}

fn registration_redrive_due_at(last: &AtomicU64, refresh: bool, now_epoch_secs: u64) -> bool {
    if !refresh {
        return true;
    }
    let previous = last.load(Ordering::Acquire);
    if now_epoch_secs >= previous && now_epoch_secs - previous < REFRESH_REDRIVE_INTERVAL_SECS {
        return false;
    }
    last.compare_exchange(
        previous,
        now_epoch_secs,
        Ordering::AcqRel,
        Ordering::Acquire,
    )
    .is_ok()
}

fn register_agent_with_reconciliation(
    state: &Arc<Mutex<AgentBusState>>,
    mut agent: AgentBusAgent,
    reconcile_registration_agent: impl FnOnce(&mut AgentBusAgent) -> anyhow::Result<()>,
    persist_registry: impl FnOnce(&AgentBusState) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // The registration callback does not re-enter Agent Bus. Keeping the state
    // lock across its store CAS and roster persistence prevents unregister or
    // roster observation from entering the post-CAS/pre-visibility gap.
    #[cfg(feature = "stock-launch-test-hook")]
    crate::app_server::private_restart_phase("bus.register.lock.begin");
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    #[cfg(feature = "stock-launch-test-hook")]
    crate::app_server::private_restart_phase("bus.register.reconcile.begin");
    reconcile_registration_agent(&mut agent)?;
    #[cfg(feature = "stock-launch-test-hook")]
    crate::app_server::private_restart_phase("bus.register.reconcile.end");
    state.agents.insert(agent.id.clone(), agent.clone());
    persist_registry(&state)?;
    #[cfg(feature = "stock-launch-test-hook")]
    crate::app_server::private_restart_phase("bus.register.persist.end");
    Ok(())
}

fn poll_wait_duration(path: &str) -> Duration {
    query_value(path, "wait_ms")
        .or_else(|| query_value(path, "waitMs"))
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_default()
        .min(MAX_POLL_WAIT)
}

fn poll_agent_messages_with_wait(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
    ack_mode: bool,
    wait: Duration,
) -> anyhow::Result<(String, Vec<AgentBusMessage>)> {
    let deadline = Instant::now() + wait;
    loop {
        let signal = agent_bus_poll_signal();
        let observed_generation = *signal
            .generation
            .lock()
            .map_err(|_| anyhow!("agent bus poll signal lock poisoned"))?;
        let result = poll_messages(state, agent_id, ack_mode)?;
        if !result.1.is_empty() || wait.is_zero() || !agent_is_registered(state, agent_id)? {
            return Ok(result);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(result);
        }
        let generation = signal
            .generation
            .lock()
            .map_err(|_| anyhow!("agent bus poll signal lock poisoned"))?;
        if *generation != observed_generation {
            continue;
        }
        let (_generation, wait_result) = signal
            .available
            .wait_timeout(generation, remaining)
            .map_err(|_| anyhow!("agent bus poll signal lock poisoned"))?;
        if wait_result.timed_out() {
            return poll_messages(state, agent_id, ack_mode);
        }
    }
}

/// Peek, then (without `ack`) remove exactly the returned messages: a message
/// enqueued between the two locks stays queued for the next poll.
fn poll_messages(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
    ack_mode: bool,
) -> anyhow::Result<(String, Vec<AgentBusMessage>)> {
    let (agent_name, messages) = poll_agent_messages(state, agent_id, true, now_epoch_secs())?;
    if !ack_mode && !messages.is_empty() {
        let ids = messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut bus = state
            .lock()
            .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
        if let Some(queue) = bus.messages.get_mut(agent_id) {
            queue.retain(|message| !ids.contains(message.id.as_str()));
        }
    }
    Ok((agent_name, messages))
}

fn agent_is_registered(state: &Arc<Mutex<AgentBusState>>, agent_id: &str) -> anyhow::Result<bool> {
    state
        .lock()
        .map(|state| state.agents.contains_key(agent_id))
        .map_err(|_| anyhow!("agent bus state lock poisoned"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn launch_results_carry_the_approved_after_wait_note_only_when_complete() {
        let note = serde_json::json!({"decision":"approved","waitedSecs":3,"approvalId":"a",
            "detail":"The owner approved this request after 3 s; it has been carried out."});
        let mut complete = serde_json::json!({"schema":"s","action_id":"x","outcome":{"status":"complete","receipt":{}}});
        attach_launch_approval_note(&mut complete, Some(note.clone()));
        assert_eq!(complete["approval"], note);
        assert_eq!(complete["detail"], note["detail"]);
        let mut refused = serde_json::json!({"schema":"s","action_id":"x","outcome":{"status":"no_write","code":"c","detail":"d"}});
        let before = refused.clone();
        attach_launch_approval_note(&mut refused, Some(note));
        assert_eq!(refused, before);
        let mut plain = before.clone();
        attach_launch_approval_note(&mut plain, None);
        assert_eq!(plain, before);
    }

    #[test]
    fn approval_hold_notices_a_caller_that_went_away() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.write_all(b"x").unwrap();
        // Unread request bytes still mean "waiting"; so does nothing to read.
        assert!(super::connection_open(&server));
        let mut byte = [0_u8; 1];
        std::io::Read::read_exact(&mut &server, &mut byte).unwrap();
        assert!(super::connection_open(&server));
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(2);
        while super::connection_open(&server) {
            assert!(Instant::now() < deadline, "close never seen");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The socket is blocking again for the answer.
        let mut server = server;
        assert!(server.write_all(b"").is_ok());
    }

    #[test]
    fn registration_refresh_redrive_is_rate_limited_but_new_registrations_are_not() {
        let last = AtomicU64::new(0);
        assert!(registration_redrive_due_at(&last, true, 1_000));
        assert!(!registration_redrive_due_at(&last, true, 1_001));
        assert!(!registration_redrive_due_at(&last, true, 1_059));
        // A new agent (or a new process for it) always redrives and does not
        // consume the refresh slot.
        assert!(registration_redrive_due_at(&last, false, 1_030));
        assert_eq!(last.load(Ordering::Acquire), 1_000);
        assert!(registration_redrive_due_at(&last, true, 1_060));
        assert!(!registration_redrive_due_at(&last, true, 1_100));
        // A clock step backwards does not block redrives forever.
        assert!(registration_redrive_due_at(&last, true, 500));
        assert!(!registration_redrive_due_at(&last, true, 520));
    }

    #[test]
    fn external_grant_reply_hint_names_the_sender_and_listing_marks_grants() {
        const MEMBER: &str = "cutex.00000000-0000-0000-0000-0000000000a1";
        const EXTERNAL: &str = "cutex.00000000-0000-0000-0000-000000000005";
        let security = crate::config::agent_security::AgentSecurityConfig {
            external_agents: vec![crate::config::agent_security::ExternalAgentGrant {
                project: "alpha".into(),
                agent: EXTERNAL.into(),
            }],
            ..Default::default()
        };
        let boundary = crate::agent_bus::boundary::AgentBoundary::new(
            &security,
            [(MEMBER.to_string(), "alpha".to_string())]
                .into_iter()
                .collect(),
            Default::default(),
        );
        let mut listing = serde_json::json!({"agents": [
            {"cutex_session_id": MEMBER},
            {"cutex_session_id": EXTERNAL},
            {"cutex_session_id": null},
        ]});
        mark_external_grants(&mut listing, &boundary, MEMBER);
        assert!(listing["agents"][0].get("external_grant").is_none());
        assert_eq!(listing["agents"][1]["external_grant"], true);
        assert!(listing["agents"][2].get("external_grant").is_none());
    }
    use std::fs;
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use crate::agent_bus::delivery::AgentDeliveryMode;
    use crate::agent_bus::model::AgentBusEnvelopeKind;
    use crate::agent_bus::model::AgentMessageKind;
    use crate::agent_bus::model::AgentRegistrationClass;
    use crate::session::archive::commit_retire;
    use crate::session::archive::record_has_runtime_claim;
    use crate::session::archive::validate_retire_preconditions;
    use crate::session::archive::CutexSessionArchiveError;
    use crate::session::model::CutexSessionRecord;
    use crate::session::model::CutexSessionStore;
    use crate::session::runtime_reconciliation::reconcile_cutex_session_store_for_registration;
    use crate::session::store::load_cutex_session_store_from_path;
    use crate::session::store::save_cutex_session_store_to_path;
    use crate::session::store::CutexSessionStoreRevisionConflict;

    #[test]
    fn poll_wait_duration_is_optional_and_bounded() {
        assert_eq!(
            poll_wait_duration("/api/messages/poll?agent_id=one"),
            Duration::ZERO
        );
        assert_eq!(
            poll_wait_duration("/api/messages/poll?wait_ms=2000"),
            Duration::from_secs(2)
        );
        assert_eq!(
            poll_wait_duration("/api/messages/poll?waitMs=60000"),
            MAX_POLL_WAIT
        );
    }

    #[test]
    fn target_resolution_errors_are_typed_4xx_and_internal_errors_remain_unmapped() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let target_error = crate::agent_bus::routing::resolve_agent_target_for_sender(
            &state,
            "cutex.018e0000-0000-7000-8000-0000a000037f",
            None,
            true,
        )
        .expect_err("unknown durable target");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let mut client =
            TcpStream::connect(listener.local_addr().expect("address")).expect("connect client");
        let (mut server, _) = listener.accept().expect("accept server");
        write_agent_target_resolution_error(&mut server, &target_error)
            .expect("target error should map")
            .expect("write target error");
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read response");
        let response = String::from_utf8(response).expect("utf8 response");
        assert!(response.starts_with("HTTP/1.1 404 Not Found"));
        assert!(response.contains("\"code\":\"not_found\""));

        assert!(write_agent_target_resolution_error(
            &mut server,
            &anyhow::anyhow!("registry persistence unavailable"),
        )
        .is_none());
    }

    #[test]
    fn long_poll_wakes_when_a_message_is_enqueued() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        state
            .lock()
            .expect("agent bus state")
            .agents
            .insert("target".to_string(), test_agent("target"));
        let poll_state = Arc::clone(&state);
        let started = Instant::now();
        let poller = std::thread::spawn(move || {
            poll_agent_messages_with_wait(&poll_state, "target", true, Duration::from_secs(1))
                .expect("long poll should complete")
        });

        std::thread::sleep(Duration::from_millis(25));
        state
            .lock()
            .expect("agent bus state")
            .messages
            .entry("target".to_string())
            .or_default()
            .push_back(AgentBusMessage {
                id: "message-1".to_string(),
                kind: AgentBusEnvelopeKind::Message,
                from: "sender".to_string(),
                to: "target".to_string(),
                from_cutex_session_id: None,
                to_cutex_session_id: None,
                content: "hello".to_string(),
                delivery_mode: AgentDeliveryMode::Passive,
                trigger_turn: false,
                created_at_epoch_secs: 1,
                sender_kind: AgentMessageKind::Agent,
                display_source: None,
                submit_mode: None,
                control_type: None,
                control_payload: None,
                external_action_id: None,
                external_message_id: None,
            });
        notify_agent_bus_message_available();

        let (_, messages) = poller.join().expect("long poll thread");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "message-1");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn long_poll_wakes_when_an_agent_is_unregistered() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        state
            .lock()
            .expect("agent bus state")
            .agents
            .insert("target".to_string(), test_agent("target"));
        let poll_state = state.clone();
        let started = Instant::now();
        let poller = std::thread::spawn(move || {
            poll_agent_messages_with_wait(&poll_state, "target", true, Duration::from_secs(1))
                .expect("long poll should complete")
        });

        std::thread::sleep(Duration::from_millis(25));
        state
            .lock()
            .expect("agent bus state")
            .agents
            .remove("target");
        notify_agent_bus_message_available();

        let (_, messages) = poller.join().expect("long poll thread");
        assert!(messages.is_empty());
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn retire_store_write_wins_before_new_registration_becomes_visible() {
        let (root, path) = initialized_session_store();
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let registration_state = Arc::clone(&state);
        let registration_path = path.clone();
        let (loaded_tx, loaded_rx) = mpsc::sync_channel(1);
        let (continue_tx, continue_rx) = mpsc::sync_channel(1);
        let registration = std::thread::spawn(move || {
            register_agent_with_reconciliation(
                &registration_state,
                test_agent("target"),
                move |agent| {
                    let mut store = load_cutex_session_store_from_path(&registration_path)?;
                    let reconciliation = reconcile_cutex_session_store_for_registration(
                        &mut store,
                        agent,
                        "host",
                        "2026-08-10T00:00:30Z",
                    )?;
                    assert!(reconciliation.store_fence_required);
                    assert!(reconciliation.outcome.is_some());
                    loaded_tx.send(()).expect("signal registration load");
                    continue_rx.recv().expect("continue registration save");
                    save_cutex_session_store_to_path(&registration_path, &store)
                },
                |_| Ok(()),
            )
        });

        loaded_rx.recv().expect("registration loaded old revision");
        let mut archive = load_cutex_session_store_from_path(&path).expect("load archive writer");
        let record = archive
            .sessions
            .get_mut("cutex.registration-race")
            .expect("archive record");
        commit_retire(record, 1, 0, true, "2026-08-10T00:01:00Z".to_string())
            .expect("commit retire winner");
        save_cutex_session_store_to_path(&path, &archive).expect("save retire winner");
        continue_tx.send(()).expect("release registration save");

        let error = registration
            .join()
            .expect("registration thread")
            .expect_err("stale registration reconciliation must fail");
        assert!(error
            .downcast_ref::<CutexSessionStoreRevisionConflict>()
            .is_some());
        assert!(state.lock().expect("agent bus state").agents.is_empty());
        let persisted = load_cutex_session_store_from_path(&path).expect("reload retired store");
        assert!(persisted
            .sessions
            .get("cutex.registration-race")
            .expect("persisted record")
            .is_retired());
        fs::remove_dir_all(root).expect("remove test store");
    }

    #[test]
    fn new_registration_store_write_wins_before_roster_visibility() {
        let (root, path) = initialized_session_store();
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let registration_state = Arc::clone(&state);
        let registration_path = path.clone();
        let (saved_tx, saved_rx) = mpsc::sync_channel(1);
        let (continue_tx, continue_rx) = mpsc::sync_channel(1);
        let (visible_tx, visible_rx) = mpsc::sync_channel(1);
        let registration = std::thread::spawn(move || {
            register_agent_with_reconciliation(
                &registration_state,
                test_agent("target"),
                move |agent| {
                    let mut store = load_cutex_session_store_from_path(&registration_path)?;
                    let reconciliation = reconcile_cutex_session_store_for_registration(
                        &mut store,
                        agent,
                        "host",
                        "2026-08-10T00:00:30Z",
                    )?;
                    assert!(reconciliation.store_fence_required);
                    assert!(reconciliation.outcome.is_some());
                    save_cutex_session_store_to_path(&registration_path, &store)?;
                    saved_tx.send(()).expect("signal registration save");
                    continue_rx.recv().expect("continue roster persistence");
                    Ok(())
                },
                move |_| {
                    visible_tx.send(()).expect("signal roster persistence");
                    Ok(())
                },
            )
        });

        saved_rx
            .recv()
            .expect("registration reconciliation saved first");
        assert!(matches!(
            visible_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let persisted =
            load_cutex_session_store_from_path(&path).expect("load registration winner");
        let record = persisted
            .sessions
            .get("cutex.registration-race")
            .expect("registration record");
        assert_eq!(record.current_runtime_agent_id.as_deref(), Some("target"));
        assert!(matches!(
            validate_retire_preconditions(record, 1, 0),
            Err(CutexSessionArchiveError::StaleRevision {
                expected: 1,
                actual: 2
            })
        ));
        continue_tx.send(()).expect("release roster persistence");
        registration
            .join()
            .expect("registration thread")
            .expect("registration winner");
        visible_rx.recv().expect("roster persisted after release");
        assert!(state
            .lock()
            .expect("agent bus state")
            .agents
            .contains_key("target"));
        assert!(load_cutex_session_store_from_path(&path)
            .expect("reload active store")
            .sessions
            .get("cutex.registration-race")
            .expect("active record")
            .is_active());
        fs::remove_dir_all(root).expect("remove test store");
    }

    #[test]
    fn failed_refresh_reconciliation_preserves_prior_roster_entry() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        state
            .lock()
            .expect("agent bus state")
            .agents
            .insert("target".to_string(), test_agent("target"));
        let mut refreshed = test_agent("target");
        refreshed.pid = 99;

        let error = register_agent_with_reconciliation(
            &state,
            refreshed,
            |_| anyhow::bail!("injected registration reconciliation failure"),
            |_| panic!("failed reconciliation must not persist the roster"),
        )
        .expect_err("refresh reconciliation must fail closed");

        assert!(error
            .to_string()
            .contains("registration reconciliation failure"));
        assert_eq!(
            state
                .lock()
                .expect("agent bus state")
                .agents
                .get("target")
                .expect("prior roster entry")
                .pid,
            42
        );
    }

    #[test]
    fn unregister_cannot_enter_between_registration_cas_and_roster_write() {
        let (root, path) = initialized_session_store();
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        state
            .lock()
            .expect("agent bus state")
            .agents
            .insert("target".to_string(), test_agent("target"));
        let registration_state = Arc::clone(&state);
        let registration_path = path.clone();
        let (saved_tx, saved_rx) = mpsc::sync_channel(1);
        let (continue_tx, continue_rx) = mpsc::sync_channel(1);
        let (order_tx, order_rx) = mpsc::channel();
        let visible_order_tx = order_tx.clone();
        let registration = std::thread::spawn(move || {
            register_agent_with_reconciliation(
                &registration_state,
                test_agent("target"),
                move |agent| {
                    let mut store = load_cutex_session_store_from_path(&registration_path)?;
                    let reconciliation = reconcile_cutex_session_store_for_registration(
                        &mut store,
                        agent,
                        "host",
                        "2026-08-10T00:00:30Z",
                    )?;
                    assert!(reconciliation.store_fence_required);
                    assert!(reconciliation.outcome.is_some());
                    save_cutex_session_store_to_path(&registration_path, &store)?;
                    saved_tx.send(()).expect("signal refresh save");
                    continue_rx.recv().expect("continue roster write");
                    Ok(())
                },
                move |_| {
                    visible_order_tx
                        .send("visible")
                        .expect("record roster visibility");
                    Ok(())
                },
            )
        });

        saved_rx.recv().expect("refresh store CAS completed");
        let unregister_state = Arc::clone(&state);
        let (attempted_tx, attempted_rx) = mpsc::sync_channel(1);
        let unregister = std::thread::spawn(move || {
            attempted_tx.send(()).expect("signal unregister attempt");
            let removed = unregister_state
                .lock()
                .expect("agent bus state")
                .agents
                .remove("target")
                .is_some();
            order_tx
                .send("unregistered")
                .expect("record unregister completion");
            removed
        });

        attempted_rx
            .recv()
            .expect("unregister attempted state lock");
        assert!(matches!(
            order_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        continue_tx.send(()).expect("release roster write");
        assert_eq!(order_rx.recv().expect("first ordered action"), "visible");
        registration
            .join()
            .expect("registration thread")
            .expect("refresh registration");
        assert_eq!(
            order_rx.recv().expect("second ordered action"),
            "unregistered"
        );
        assert!(unregister.join().expect("unregister thread"));
        assert!(state.lock().expect("agent bus state").agents.is_empty());
        assert!(load_cutex_session_store_from_path(&path)
            .expect("reload registration store")
            .sessions
            .get("cutex.registration-race")
            .expect("active record")
            .is_active());
        fs::remove_dir_all(root).expect("remove test store");
    }

    #[test]
    fn retired_session_rejects_matching_sessionless_registration() {
        let (root, path) = initialized_sessionless_store();
        let mut archive = load_cutex_session_store_from_path(&path).expect("load archive writer");
        let record = archive
            .sessions
            .get_mut("cutex.registration-race")
            .expect("archive record");
        commit_retire(record, 1, 0, true, "2026-08-10T00:01:00Z".to_string())
            .expect("commit retire winner");
        save_cutex_session_store_to_path(&path, &archive).expect("save retire winner");
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let registration_path = path.clone();
        let error = register_agent_with_reconciliation(
            &state,
            sessionless_test_agent("target"),
            move |agent| {
                let mut store = load_cutex_session_store_from_path(&registration_path)?;
                let reconciliation = reconcile_cutex_session_store_for_registration(
                    &mut store,
                    agent,
                    "host",
                    "2026-08-10T00:01:30Z",
                )?;
                if reconciliation.store_fence_required {
                    save_cutex_session_store_to_path(&registration_path, &store)?;
                }
                Ok(())
            },
            |_| panic!("retired registration must not persist roster"),
        )
        .expect_err("retired sessionless registration must fail");

        assert!(error.to_string().contains("retired cutex session"));
        assert!(state.lock().expect("agent bus state").agents.is_empty());
        assert!(load_cutex_session_store_from_path(&path)
            .expect("reload retired store")
            .sessions
            .get("cutex.registration-race")
            .expect("retired record")
            .is_retired());
        fs::remove_dir_all(root).expect("remove test store");
    }

    #[test]
    fn matching_sessionless_runtime_claim_wins_before_roster_write() {
        let (root, path) = initialized_sessionless_store();
        let mut stale_archive =
            load_cutex_session_store_from_path(&path).expect("load stale archive writer");
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let registration_state = Arc::clone(&state);
        let registration_path = path.clone();
        let (saved_tx, saved_rx) = mpsc::sync_channel(1);
        let (continue_tx, continue_rx) = mpsc::sync_channel(1);
        let (visible_tx, visible_rx) = mpsc::sync_channel(1);
        let registration = std::thread::spawn(move || {
            register_agent_with_reconciliation(
                &registration_state,
                sessionless_test_agent("target"),
                move |agent| {
                    let mut store = load_cutex_session_store_from_path(&registration_path)?;
                    let reconciliation = reconcile_cutex_session_store_for_registration(
                        &mut store,
                        agent,
                        "host",
                        "2026-08-10T00:00:30Z",
                    )?;
                    assert!(reconciliation.store_fence_required);
                    assert!(reconciliation.outcome.is_none());
                    save_cutex_session_store_to_path(&registration_path, &store)?;
                    saved_tx.send(()).expect("signal sessionless save");
                    continue_rx.recv().expect("continue roster write");
                    Ok(())
                },
                move |_| {
                    visible_tx.send(()).expect("signal roster persistence");
                    Ok(())
                },
            )
        });

        saved_rx.recv().expect("sessionless store fence saved");
        assert!(matches!(
            visible_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let persisted = load_cutex_session_store_from_path(&path)
            .expect("load sessionless registration winner");
        let claimed = persisted
            .sessions
            .get("cutex.registration-race")
            .expect("claimed sessionless record");
        assert_eq!(claimed.current_runtime_agent_id.as_deref(), Some("target"));
        assert_eq!(claimed.runtime_generation, 1);
        assert!(record_has_runtime_claim(claimed));
        assert!(matches!(
            validate_retire_preconditions(claimed, 1, 0),
            Err(CutexSessionArchiveError::StaleRuntimeFence {
                expected: 0,
                actual: 1
            })
        ));
        let record = stale_archive
            .sessions
            .get_mut("cutex.registration-race")
            .expect("archive record");
        commit_retire(record, 1, 0, true, "2026-08-10T00:01:00Z".to_string())
            .expect("prepare stale retire");
        let error = save_cutex_session_store_to_path(&path, &stale_archive)
            .expect_err("registration store fence must stale retire writer");
        assert!(error
            .downcast_ref::<CutexSessionStoreRevisionConflict>()
            .is_some());
        continue_tx.send(()).expect("release roster write");
        registration
            .join()
            .expect("registration thread")
            .expect("sessionless registration winner");
        visible_rx.recv().expect("roster persisted after release");
        assert!(state
            .lock()
            .expect("agent bus state")
            .agents
            .contains_key("target"));
        assert!(load_cutex_session_store_from_path(&path)
            .expect("reload active store")
            .sessions
            .get("cutex.registration-race")
            .expect("active record")
            .is_active());
        fs::remove_dir_all(root).expect("remove test store");
    }

    fn initialized_session_store() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "cutex-registration-retire-race-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("cutex-sessions.json");
        let mut store = CutexSessionStore::default();
        store.sessions.insert(
            "cutex.registration-race".to_string(),
            CutexSessionRecord::new_at(
                "cutex.registration-race".to_string(),
                Some("thread-1".to_string()),
                "host".to_string(),
                "/tmp".to_string(),
                None,
                "2026-08-10T00:00:00Z".to_string(),
            )
            .expect("session record"),
        );
        save_cutex_session_store_to_path(&path, &store).expect("save initial store");
        (root, path)
    }

    fn initialized_sessionless_store() -> (PathBuf, PathBuf) {
        let (root, path) = initialized_session_store();
        let mut store = load_cutex_session_store_from_path(&path).expect("load sessionless store");
        store
            .sessions
            .get_mut("cutex.registration-race")
            .expect("sessionless record")
            .last_runtime_agent_id = Some("target".to_string());
        save_cutex_session_store_to_path(&path, &store).expect("save sessionless runtime identity");
        (root, path)
    }

    fn active_route_session(
        cutex_session_id: &str,
        roster_session_id: &str,
        runtime_id: &str,
        revision: u64,
        generation: u64,
    ) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            cutex_session_id.to_string(),
            Some(roster_session_id.to_string()),
            current_host_name(),
            format!("/tmp/{runtime_id}"),
            Some("orchid".to_string()),
            "2026-08-22T00:00:00Z".to_string(),
        )
        .expect("active route session");
        record.revision = revision;
        record.current_runtime_agent_id = Some(runtime_id.to_string());
        record.runtime_generation = generation;
        record.agent_enabled = true;
        record
    }

    fn active_route_roster(runtime_id: &str, roster_session_id: &str) -> AgentBusAgent {
        let mut roster = test_agent(runtime_id);
        roster.session_id = Some(roster_session_id.to_string());
        roster.host_id = Some(current_host_name());
        roster.pid = std::process::id();
        roster.last_seen_epoch_secs = now_epoch_secs();
        roster
    }

    #[test]
    fn agent_management_sender_discards_project_chat_groups() {
        let runtime_id = "runtime-director-r10";
        let roster_session_id = "018e0000-0000-7000-8000-0000b0009ae4";
        let mut roster = active_route_roster(runtime_id, roster_session_id);
        roster.groups = vec![
            "project:legacy-chat-label".to_string(),
            "project:automatic-routing-label".to_string(),
        ];
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        state
            .lock()
            .unwrap()
            .agents
            .insert(runtime_id.to_string(), roster);
        let request = crate::http::server::SimpleHttpRequest {
            method: "POST".to_string(),
            path: "/api/agent-management/v1/actions".to_string(),
            headers: std::collections::HashMap::from([(
                "x-cutex-agent-id".to_string(),
                runtime_id.to_string(),
            )]),
            body: Vec::new(),
        };

        let sender = agent_management_sender(&request, &state).unwrap();
        assert_eq!(sender.runtime_agent_id.as_str(), runtime_id);
        assert_eq!(sender.roster_session_id, roster_session_id);
    }

    #[test]
    fn agent_management_caller_resolution_binds_current_runtime_to_durable_session() {
        let mut store = CutexSessionStore::default();
        let record = active_route_session(
            "cutex.director-r10",
            "018e0000-0000-7000-8000-0000b0009ae4",
            "runtime-director-r10",
            3,
            7,
        );
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        let sender = AgentManagementRosterSender {
            runtime_agent_id: RuntimeAgentId::new("runtime-director-r10").unwrap(),
            roster_session_id: "018e0000-0000-7000-8000-0000b0009ae4".to_string(),
        };
        let invocation = resolve_agent_management_invocation_from_store(&sender, &store).unwrap();
        assert_eq!(
            invocation.caller_cutex_session.as_str(),
            "cutex.director-r10"
        );
        assert_eq!(invocation.caller_runtime_agent_id, "runtime-director-r10");

        store
            .sessions
            .get_mut("cutex.director-r10")
            .unwrap()
            .current_runtime_agent_id = Some("superseded-runtime".to_string());
        assert_eq!(
            resolve_agent_management_invocation_from_store(&sender, &store),
            Err(AgentManagementInvocationError::StaleRuntimeIdentity)
        );
        let record = store.sessions.get_mut("cutex.director-r10").unwrap();
        record.current_runtime_agent_id = Some("runtime-director-r10".to_string());
        record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        assert_eq!(
            resolve_agent_management_invocation_from_store(&sender, &store),
            Err(AgentManagementInvocationError::StaleRuntimeIdentity)
        );
    }

    #[test]
    fn claude_caller_uses_adapter_identity_and_rejects_ambiguous_or_old_runtime() {
        let thread = "018e0000-0000-7000-8000-0000b0009ae4";
        let mut record =
            active_route_session("cutex.claude-test", thread, "claude.connection", 1, 1);
        record.codex_session_id = None;
        record.claude_session = Some(serde_json::from_value(serde_json::json!({
            "deployment": {"python":"/fixture/python", "adapter_source":"/fixture/adapter",
                "claude_cli":"/fixture/claude", "claude_home":"/fixture/home", "tui_binary":"/fixture/tui"},
            "thread_id":thread, "state_dir":"/fixture/state", "model":"sonnet",
            "native_session_id":"different-native-uuid"
        })).unwrap());
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        let mut sender = AgentManagementRosterSender {
            runtime_agent_id: RuntimeAgentId::new("claude.connection").unwrap(),
            roster_session_id: thread.into(),
        };
        assert_eq!(
            resolve_agent_management_invocation_from_store(&sender, &store)
                .unwrap()
                .caller_cutex_session
                .as_str(),
            "cutex.claude-test"
        );
        sender.roster_session_id = "different-native-uuid".into();
        assert!(resolve_agent_management_invocation_from_store(&sender, &store).is_err());
        sender.roster_session_id = thread.into();
        sender.runtime_agent_id = RuntimeAgentId::new("claude.old-connection").unwrap();
        assert!(resolve_agent_management_invocation_from_store(&sender, &store).is_err());
        sender.runtime_agent_id = RuntimeAgentId::new("claude.connection").unwrap();
        record.cutex_session_id = "cutex.duplicate".into();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        assert!(resolve_agent_management_invocation_from_store(&sender, &store).is_err());
    }

    static ROUTE_MANAGEMENT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static ROUTE_CHAT_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counted_noop_reconcile(_agent: &AgentBusAgent) -> anyhow::Result<()> {
        ROUTE_MANAGEMENT_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn counted_unreachable_send(
        _state: &Arc<Mutex<AgentBusState>>,
        _request: AgentBusSendRequest,
    ) -> anyhow::Result<Value> {
        ROUTE_CHAT_CALLS.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("this route must not invoke chat/Management send")
    }

    fn counted_unreachable_agent_management(
        _state: &Arc<Mutex<AgentBusState>>,
        _invocation: AgentManagementInvocation,
        _request: AgentManagementRequest,
    ) -> anyhow::Result<Value> {
        anyhow::bail!("this route must not invoke Agent Management")
    }

    /// Handlers for route tests that must not reach chat send, Agent
    /// Management or the Job Service lane.
    fn route_test_handlers() -> AgentBusRequestHandlers {
        AgentBusRequestHandlers {
            reconcile_registration_agent: |agent| counted_noop_reconcile(agent),
            reconcile_agent: counted_noop_reconcile,
            redrive_ordinary_messages: |_| Ok(0),
            send_payload_response: counted_unreachable_send,
            agent_management: counted_unreachable_agent_management,
            job_service_completion: |_, _, _| anyhow::bail!("unreachable Job Service submit"),
            job_service_completion_query: |_, _| anyhow::bail!("unreachable Job Service query"),
            management_start_acked: |_, _| {},
        }
    }

    /// In-memory bus state of a route test (no Task Service or other store).
    fn route_state() -> Arc<Mutex<AgentBusState>> {
        Arc::new(Mutex::new(AgentBusState::default()))
    }

    #[test]
    fn browser_live_fence_http_requires_current_live_local_persistent_runtime() {
        use crate::config::instance::{Selection,set_test_selection};
        let previous=crate::config::instance::selected().unwrap();let path=std::env::temp_dir().join(format!("browser-fence-http-{}",uuid::Uuid::new_v4()));set_test_selection(Selection::Custom(path.clone()));
        let session="cutex.browser-fence";let thread="018e0000-0000-7000-8000-0000b0009ae4";let runtime="browser-runtime";
        let mut record=active_route_session(session,thread,runtime,1,7);record.registration_class=AgentRegistrationClass::Persistent;
        let mut store=CutexSessionStore::default();store.sessions.insert(session.into(),record);crate::session::store::save_cutex_session_store(&store).unwrap();
        let state=route_state();let valid=active_route_roster(runtime,thread);state.lock().unwrap().agents.insert(runtime.into(),valid.clone());
        let credential=crate::agent_bus::agent_auth::agent_credential("route-token",runtime,session);
        let call=|token:&str,thread_header:&str,generation:u64,body:&str| {
            let raw=format!("POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nX-Cutex-Agent-Id: {runtime}\r\nX-Cutex-Mcp-Thread-Id: {thread_header}\r\nX-Cutex-Mcp-Generation: {generation}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",crate::agent_bus::agent_auth::BROWSER_FENCE_PATH,body.len());
            invoke_route(raw.into_bytes(),Arc::clone(&state))
        };
        let response=call(&credential,thread,7,"{}");assert!(response.starts_with(b"HTTP/1.1 200 "),"fixture response: {}",String::from_utf8_lossy(&response));assert_eq!(http_json(&response)["runtimeGeneration"],7);
        // The fence is read-only: observing it must not refresh heartbeat.
        assert_eq!(state.lock().unwrap().agents[runtime].last_seen_epoch_secs,valid.last_seen_epoch_secs);
        for (token,header,generation,body) in [("route-token",thread,7,"{}"),(&credential,"old-thread",7,"{}"),(&credential,thread,6,"{}"),(&credential,thread,7,"{\"runtimeGeneration\":7}")] {assert!(call(token,header,generation,body).starts_with(b"HTTP/1.1 403 "));}
        for invalid in 0..4 {
            let mut roster=valid.clone();match invalid{0=>roster.pid=u32::MAX,1=>roster.last_seen_epoch_secs=0,2=>roster.host_id=Some("unrelated-remote-host".into()),_=>roster.registration_class=AgentRegistrationClass::Ephemeral};state.lock().unwrap().agents.insert(runtime.into(),roster);
            assert!(call(&credential,thread,7,"{}").starts_with(b"HTTP/1.1 403 "),"negative {invalid}");
        }
        state.lock().unwrap().agents.insert(runtime.into(),valid);
        let mut stored=crate::session::store::load_cutex_session_store().unwrap();stored.sessions.get_mut(session).unwrap().current_runtime_agent_id=Some("replacement".into());crate::session::store::save_cutex_session_store(&stored).unwrap();assert!(call(&credential,thread,7,"{}").starts_with(b"HTTP/1.1 403 "));
        set_test_selection(previous);let _=std::fs::remove_dir_all(path);
    }

    #[test]
    fn browser_fence_admits_a_quick_chat_runtime_but_other_mcp_routes_do_not() {
        use crate::config::instance::{Selection,set_test_selection};
        let previous=crate::config::instance::selected().unwrap();let path=std::env::temp_dir().join(format!("browser-fence-owned-{}",uuid::Uuid::new_v4()));set_test_selection(Selection::Custom(path.clone()));
        let session="cutex.quick-chat";let thread="018e0000-0000-7000-8000-0000b0009ae5";let runtime="stock.quick-chat";
        // A quick chat: owned session, local-only, no Agent identity.
        let mut record=active_route_session(session,thread,runtime,1,3);
        record.agent_enabled=false;record.registration_class=AgentRegistrationClass::LocalOnly;
        record.explicit_launch=Some(crate::agent_management::ExplicitLaunchContract{version:999,migration_action_id:None,native_id:thread.into(),native_home:"/unavailable".into(),bundle_manifest:"/unavailable/bundle".into(),bundle_sha256:crate::role_revision::Sha256::new("a".repeat(64)).unwrap()});
        assert!(record.is_owned_session());
        let mut store=CutexSessionStore::default();store.sessions.insert(session.into(),record);crate::session::store::save_cutex_session_store(&store).unwrap();
        let state=route_state();let mut roster=active_route_roster(runtime,thread);roster.registration_class=AgentRegistrationClass::LocalOnly;state.lock().unwrap().agents.insert(runtime.into(),roster.clone());
        let credential=crate::agent_bus::agent_auth::agent_credential("route-token",runtime,session);
        let call=|path:&str,body:&str| {
            let raw=format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {credential}\r\nX-Cutex-Agent-Id: {runtime}\r\nX-Cutex-Mcp-Thread-Id: {thread}\r\nX-Cutex-Mcp-Generation: 3\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            invoke_route(raw.into_bytes(),Arc::clone(&state))
        };
        let response=call(crate::agent_bus::agent_auth::BROWSER_FENCE_PATH,"{}");
        assert!(response.starts_with(b"HTTP/1.1 200 "),"owned fence: {}",String::from_utf8_lossy(&response));
        assert_eq!(http_json(&response)["cutexSessionId"],session);
        // Delegation tools stay closed to a quick chat.
        let asks=call(crate::asks::tools::PATH,r#"{"tool":"agent_list","arguments":{}}"#);
        assert_eq!(http_json(&asks)["code"],"unauthorized","{}",String::from_utf8_lossy(&asks));
        // A local-only roster entry of an agent-enabled session is no quick chat.
        let mut stored=crate::session::store::load_cutex_session_store().unwrap();stored.sessions.get_mut(session).unwrap().agent_enabled=true;crate::session::store::save_cutex_session_store(&stored).unwrap();
        assert!(call(crate::agent_bus::agent_auth::BROWSER_FENCE_PATH,"{}").starts_with(b"HTTP/1.1 403 "));
        stored.sessions.get_mut(session).unwrap().agent_enabled=false;stored.sessions.get_mut(session).unwrap().archive_state=crate::session::model::CutexSessionArchiveState::Retired;crate::session::store::save_cutex_session_store(&stored).unwrap();
        assert!(call(crate::agent_bus::agent_auth::BROWSER_FENCE_PATH,"{}").starts_with(b"HTTP/1.1 403 "));
        set_test_selection(previous);let _=std::fs::remove_dir_all(path);
    }

    #[test]
    fn owner_queue_routes_list_and_withdraw_with_the_root_token_only() {
        let state = route_state();
        let enqueue = |target: &str, content: &str| {
            crate::agent_bus::queue::enqueue_agent_bus_message_once(
                &state,
                "sender",
                target,
                "target-name",
                content,
                AgentBusEnvelopeKind::Message,
                AgentDeliveryMode::AfterTurn,
                AgentMessageKind::Agent,
                None,
                None,
                None,
                None,
                None,
                None,
                1,
            )
            .unwrap()
            .record
            .id
        };
        let stuck = enqueue("runtime-withdraw-a", "stuck");
        let kept = enqueue("runtime-withdraw-a", "kept");
        let get = |token: Option<&str>| {
            let auth = token
                .map(|token| format!("Authorization: Bearer {token}\r\n"))
                .unwrap_or_default();
            format!(
                "GET /api/messages/queued HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"
            )
            .into_bytes()
        };
        let unauthenticated = invoke_route(get(None), Arc::clone(&state));
        // Rejected (no root credential): never a listing.
        assert!(!unauthenticated.starts_with(b"HTTP/1.1 200"));
        let wrong = invoke_route(get(Some("nope")), Arc::clone(&state));
        assert!(!wrong.starts_with(b"HTTP/1.1 200"));
        let listed = http_json(&invoke_route(get(Some("route-token")), Arc::clone(&state)));
        let ids = listed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["agentId"] == "runtime-withdraw-a")
            .map(|m| m["messageId"].as_str().unwrap().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        // Same creation second: ordered by (random) message id.
        assert_eq!(ids, [stuck.clone(), kept.clone()].into_iter().collect());

        let body =
            serde_json::json!({"messageIds": [stuck.clone()], "reason": "owner"}).to_string();
        let post = format!(
            "POST /api/messages/withdraw HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer route-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let withdrawn = http_json(&invoke_route(post, Arc::clone(&state)));
        assert_eq!(withdrawn["ok"], true);
        assert_eq!(
            withdrawn["withdrawn"],
            serde_json::json!([{"agentId": "runtime-withdraw-a", "messageId": stuck}])
        );
        let queue = state.lock().unwrap().messages["runtime-withdraw-a"].clone();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, kept);
    }

    #[test]
    fn non_ack_poll_removes_only_the_returned_messages() {
        let state = route_state();
        state
            .lock()
            .unwrap()
            .agents
            .insert("target".to_string(), test_agent("target"));
        let enqueue = |content: &str| {
            crate::agent_bus::queue::enqueue_agent_bus_message_once(
                &state,
                "sender",
                "target",
                "target",
                content,
                AgentBusEnvelopeKind::Message,
                AgentDeliveryMode::Passive,
                AgentMessageKind::Agent,
                None,
                None,
                None,
                None,
                None,
                None,
                1,
            )
            .unwrap()
            .record
            .id
        };
        let first = enqueue("first");
        let (_, peeked) = poll_messages(&state, "target", true).unwrap();
        assert_eq!(peeked.len(), 1);
        assert_eq!(state.lock().unwrap().messages["target"].len(), 1);
        let (_, polled) = poll_messages(&state, "target", false).unwrap();
        assert_eq!(
            polled.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            [first]
        );
        assert!(state.lock().unwrap().messages["target"].is_empty());
        // A Task Service message an older release queued is an ordinary queue
        // entry now: no completion fence, no seat lookup.
        state
            .lock()
            .unwrap()
            .messages
            .entry("target".to_string())
            .or_default()
            .push_back(AgentBusMessage {
                id: "legacy-task-message".to_string(),
                kind: AgentBusEnvelopeKind::Message,
                from: "cutex-task-service".to_string(),
                to: "target".to_string(),
                from_cutex_session_id: None,
                to_cutex_session_id: None,
                content: "legacy completion".to_string(),
                delivery_mode: AgentDeliveryMode::AfterTurn,
                trigger_turn: true,
                created_at_epoch_secs: 1,
                sender_kind: AgentMessageKind::TaskServiceSystem,
                display_source: None,
                submit_mode: None,
                control_type: Some("cutex.task_service.completion.v1".to_string()),
                control_payload: Some(serde_json::json!({"forged": true})),
                external_action_id: None,
                external_message_id: None,
            });
        let (_, legacy) = poll_messages(&state, "target", true).unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].id, "legacy-task-message");
    }

    /// Observable effects of a route that must write nothing: the in-memory
    /// roster, queues, send dedupe state and the chat / Management handlers.
    #[derive(Debug, PartialEq, Eq)]
    struct RouteNoWriteObserver {
        state: String,
        management_calls: usize,
        chat_calls: usize,
    }

    fn route_no_write_observer(state: &Arc<Mutex<AgentBusState>>) -> RouteNoWriteObserver {
        let state = state.lock().unwrap();
        let agents = state.agents.iter().collect::<BTreeMap<_, _>>();
        let messages = state.messages.iter().collect::<BTreeMap<_, _>>();
        let recent_sends = state.recent_sends.keys().collect::<BTreeSet<_>>();
        let reservations = state.send_reservations.iter().collect::<BTreeMap<_, _>>();
        RouteNoWriteObserver {
            state: serde_json::to_string(&(agents, messages, recent_sends, reservations))
                .expect("serialize route observer"),
            management_calls: ROUTE_MANAGEMENT_CALLS.load(Ordering::SeqCst),
            chat_calls: ROUTE_CHAT_CALLS.load(Ordering::SeqCst),
        }
    }

    fn assert_route_no_write_observer(
        state: &Arc<Mutex<AgentBusState>>,
        before: &RouteNoWriteObserver,
        control: &str,
    ) {
        assert_eq!(&route_no_write_observer(state), before, "{control}");
    }

    fn raw_route_request(path: &str, token: &str, sender: &str, body: &[u8]) -> Vec<u8> {
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nX-Cutex-Agent-Id: {sender}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        request
    }

    fn invoke_route(request: Vec<u8>, state: Arc<Mutex<AgentBusState>>) -> Vec<u8> {
        invoke_route_with_handlers(request, state, route_test_handlers())
    }

    fn invoke_route_with_handlers(
        request: Vec<u8>,
        state: Arc<Mutex<AgentBusState>>,
        handlers: AgentBusRequestHandlers,
    ) -> Vec<u8> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(&request).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        if let Err(error) = handle_agent_bus_request(
            &mut server,
            &state,
            Some("route-token"),
            Some("job-service-route-token"),
            handlers,
        ) {
            write_http_response(
                &mut server,
                500,
                "Internal Server Error",
                "text/plain",
                error.to_string().as_bytes(),
            )
            .unwrap();
        }
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        response
    }

    fn http_json(response: &[u8]) -> Value {
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        serde_json::from_slice(&response[split + 4..]).unwrap()
    }

    #[test]
    fn job_service_completion_route_rejects_the_agent_bus_credential() {
        let state = route_state();
        let before = route_no_write_observer(&state);
        let request = raw_route_request(
            "/api/job-service/v1/completions",
            "route-token",
            "runtime-worker",
            b"{}",
        );
        let response = invoke_route(request, Arc::clone(&state));
        assert!(response.starts_with(b"HTTP/1.1 401 Unauthorized"));
        assert_route_no_write_observer(&state, &before, "job service wrong credential");
    }

    #[test]
    fn job_service_completion_route_mints_principal_only_after_dedicated_auth() {
        fn accepted(
            _state: &Arc<Mutex<AgentBusState>>,
            principal: &crate::agent_bus::identity::JobServiceSystemPrincipal,
            request: crate::agent_bus::job_completion::IncomingCompletion,
        ) -> anyhow::Result<JobServiceCompletionReceipt> {
            assert!(principal.authenticate());
            let crate::agent_bus::job_completion::IncomingCompletion::V1(request) = request else {
                anyhow::bail!("expected v1 fixture")
            };
            Ok(JobServiceCompletionReceipt {
                schema: crate::agent_bus::model::JOB_SERVICE_COMPLETION_SCHEMA.to_string(),
                status: "committed".to_string(),
                event_id: request.event_id,
                message_id: Some("jsc_fixture".to_string()),
                disposition: crate::agent_bus::model::JobServiceCompletionDisposition::Pending,
                deduplicated: false,
                a4_receipt: None,
                error_code: None,
            })
        }

        let state = route_state();
        let mut handlers = route_test_handlers();
        handlers.job_service_completion = accepted;
        let body = serde_json::to_vec(&serde_json::json!({
            "schema": "cutex.job_service.completion.v1",
            "eventId": "event-1",
            "jobId": "job-1",
            "jobRevision": 1,
            "terminalStatus": "exited",
            "resultSha256": "a".repeat(64),
            "targetCutexSessionId": "cutex.11111111-1111-4111-8111-111111111111"
        }))
        .unwrap();
        let request = raw_route_request(
            "/api/job-service/v1/completions",
            "job-service-route-token",
            "ignored-model-sender",
            &body,
        );
        let response = invoke_route_with_handlers(request, Arc::clone(&state), handlers);
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert_eq!(http_json(&response)["eventId"], "event-1");
    }

    /// r219: a director_rotate request from an older facade is refused with
    /// `tool_removed`; other operations go on to the normal checks.
    #[test]
    fn removed_director_rotate_operation_answers_tool_removed() {
        let spec = serde_json::json!({"name":"Next","cwd":"/w","profile":"p","runtime_backend":"host",
            "model":"m","reasoning":"low","permissions":"read-only","approval_policy":"on-request",
            "sandbox_mode":"read-only","groups":[]});
        let rotate: AgentManagementRequest = serde_json::from_value(serde_json::json!({
            "schema":"cutex/agent-management/v1","action_id":"rotate-old-facade","operation":"director_rotate",
            "expected_predecessor_cutex_session":"cutex.director","expected_authority_epoch":1,
            "mode":"retain_predecessor_bootstrap_only","successor":spec.clone()}))
        .unwrap();
        let response =
            serde_json::to_value(removed_agent_management_operation(&rotate).unwrap()).unwrap();
        assert_eq!(response["action_id"], "rotate-old-facade");
        assert_eq!(response["outcome"]["status"], "no_write");
        assert_eq!(response["outcome"]["code"], "tool_removed");
        let create: AgentManagementRequest = serde_json::from_value(serde_json::json!({
            "schema":"cutex/agent-management/v1","action_id":"create","operation":"create",
            "spec":spec,"start_mode":"bootstrap_only"}))
        .unwrap();
        assert!(removed_agent_management_operation(&create).is_none());
    }

    /// Sparse agent_create arguments resolve only for a fenced MCP caller;
    /// malformed ones and unfenced callers never reach Agent Management
    /// (`route_test_handlers` fails the test if they did).
    #[test]
    fn sparse_requests_need_the_mcp_fence_and_valid_arguments() {
        let path = "/api/agent-management/v1/actions";
        let body = serde_json::json!({"schema":crate::agent_management::SPARSE_SCHEMA,"action_id":"sparse-route",
            "operation":"create","agent":{"name":"quokka"},"task":"t"});
        let response = http_json(&invoke_route(
            raw_route_request(
                path,
                "route-token",
                "runtime-a",
                body.to_string().as_bytes(),
            ),
            route_state(),
        ));
        assert_eq!(response["action_id"], "sparse-route");
        assert_eq!(response["outcome"]["code"], "unauthorized", "{response}");
        let mut bad = body.clone();
        bad["agent"]["groups"] = serde_json::json!(["default"]);
        let response = http_json(&invoke_route(
            raw_route_request(path, "route-token", "runtime-a", bad.to_string().as_bytes()),
            route_state(),
        ));
        assert_eq!(
            response["outcome"]["code"], "invalid_arguments",
            "{response}"
        );
        assert!(
            response["outcome"]["detail"]
                .as_str()
                .unwrap()
                .contains("groups"),
            "{response}"
        );
    }

    fn unauthenticated_route_request(method: &str, path: &str, body: &[u8]) -> Vec<u8> {
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        request
    }

    /// Invokes `request` while the test holds the bus state lock: a route that
    /// touched the state could not answer within the bound.
    fn invoke_route_without_state_access(
        request: Vec<u8>,
        state: &Arc<Mutex<AgentBusState>>,
    ) -> Vec<u8> {
        let guard = state.lock().unwrap();
        let route_state = Arc::clone(state);
        let (sent, received) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sent.send(invoke_route(request, route_state));
        });
        let response = received
            .recv_timeout(Duration::from_secs(10))
            .expect("a removed route answers without touching the bus state");
        drop(guard);
        response
    }

    fn assert_tool_removed(response: &[u8], expected_message: &str) {
        assert!(
            response.starts_with(b"HTTP/1.1 410 Gone\r\n"),
            "{}",
            String::from_utf8_lossy(response)
        );
        let response = http_json(response);
        assert_eq!(response["ok"], false);
        assert_eq!(response["code"], "tool_removed");
        assert_eq!(response["message"], expected_message);
        assert_eq!(response["retryable"], false);
    }

    #[test]
    fn unauthenticated_federation_routes_are_absent_without_state_access() {
        let state = route_state();
        let before = route_no_write_observer(&state);
        for (method, path, body) in [
            ("GET", "/api/federation/agents?all_groups=true", &b""[..]),
            (
                "POST",
                "/api/federation/messages/send",
                &br#"{"to":"worker","content":"must not send"}"#[..],
            ),
        ] {
            let request = unauthenticated_route_request(method, path, body);
            let response = invoke_route_without_state_access(request, &state);
            assert!(response.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
        }
        assert_route_no_write_observer(&state, &before, "removed federation routes");
    }

    /// r219: Release rotation is gone. The old route answers 410
    /// `tool_removed` before authentication and before any state access.
    #[test]
    fn removed_release_rotation_route_answers_gone_without_auth() {
        let state = route_state();
        let before = route_no_write_observer(&state);
        let body =
            br#"{"schema":"cutex/release-rotation-command/v1","action_id":"rotate-release"}"#;
        for request in [
            unauthenticated_route_request("POST", "/api/rotation/v1/release", body),
            raw_route_request(
                "/api/rotation/v1/release",
                "wrong-token",
                "route-runtime",
                body,
            ),
        ] {
            let response = invoke_route_without_state_access(request, &state);
            assert_tool_removed(&response, "release rotation was removed in r219");
        }
        assert_route_no_write_observer(&state, &before, "removed release rotation route");
    }

    /// r219: every Task Service route (`/api/task/actions`, its reconcile
    /// route, `/api/task/v2/*` and anything else under `/api/task`) answers
    /// 410 `tool_removed` before authentication and before any state access,
    /// whatever the credential, and writes nothing.
    #[test]
    fn removed_task_service_routes_answer_gone_without_auth_or_writes() {
        let state = route_state();
        state
            .lock()
            .unwrap()
            .agents
            .insert("route-runtime".to_string(), test_agent("route-runtime"));
        let before = route_no_write_observer(&state);
        let body = br#"{"schema":"cutex/task-service-action/v2","action_id":"old-facade"}"#;
        for path in [
            "/api/task",
            "/api/task/actions",
            "/api/task/actions/reconcile",
            "/api/task/v2/actions",
            "/api/task/v2/worker-prepare",
            "/api/task/v2/worker-context",
            "/api/task/v2/director-action",
            "/api/task/v2/coordinator",
            "/api/task/v2/terminal-semantic",
            "/api/task/v2/terminal",
            "/api/task/v2/query",
            "/api/task/v3/unknown?agent_id=route-runtime",
        ] {
            for request in [
                unauthenticated_route_request("POST", path, body),
                unauthenticated_route_request("GET", path, b""),
                raw_route_request(path, "wrong-token", "route-runtime", body),
                raw_route_request(path, "route-token", "route-runtime", body),
            ] {
                let response = invoke_route_without_state_access(request, &state);
                assert_tool_removed(&response, TASK_SERVICE_REMOVED_MESSAGE);
            }
        }
        assert_route_no_write_observer(&state, &before, "removed Task Service routes");
        // Neighbouring paths are not caught by the prefix.
        assert_eq!(removed_route("/api/tasks"), None);
        assert_eq!(removed_route("/api/taskx/actions"), None);
        assert_eq!(removed_route("/api/messages/poll?x=/api/task/"), None);
    }

    fn test_agent(id: &str) -> AgentBusAgent {
        AgentBusAgent {
            id: id.to_string(),
            name: id.to_string(),
            base_name: Some(id.to_string()),
            thread_name: None,
            path_key: None,
            session_id: Some("thread-1".to_string()),
            cutex_session_id: None,
            profile: "profile".to_string(),
            cwd: "/tmp".to_string(),
            pid: 42,
            host_id: Some("host".to_string()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 1,
        }
    }

    fn sessionless_test_agent(id: &str) -> AgentBusAgent {
        AgentBusAgent {
            session_id: None,
            ..test_agent(id)
        }
    }
}
