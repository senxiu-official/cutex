//! Blocking HTTP client helpers for the local cutex agent bus.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;

use crate::agent_bus::delivery::AgentDeliveryMode;
use crate::agent_bus::model::AgentBusAckRequest;
use crate::agent_bus::model::AgentBusAckResponse;
use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentBusEnvelopeKind;
use crate::agent_bus::model::AgentBusGroupUpdateRequest;
use crate::agent_bus::model::AgentBusGroupUpdateResponse;
use crate::agent_bus::model::AgentBusHeartbeatRequest;
use crate::agent_bus::model::AgentBusMessage;
use crate::agent_bus::model::AgentBusPollResponse;
use crate::agent_bus::model::AgentBusRegisterRequest;
use crate::agent_bus::model::AgentBusSendRequest;
use crate::agent_bus::model::AgentBusSendResponse;
use crate::agent_bus::model::AgentBusUnregisterRequest;
use crate::agent_bus::model::AgentBusUnregisterResponse;
use crate::agent_bus::model::AgentGroupUpdateMode;
use crate::agent_bus::model::AgentMessageKind;
use crate::agent_bus::routing::agent_sender_label;
use crate::agent_bus::service::agent_bus_base_url;
use crate::agent_bus::service::agent_bus_port;
use crate::config::env::CUTEX_AGENT_ID_ENV_VAR;
use crate::config::env::CUTEX_AGENT_NAME_ENV_VAR;
use crate::http::client::http_json_request;
use crate::http::client::http_local_root_status_ok;
use crate::http::client::HttpJsonRequest;
use crate::profiles::model::CodezConfig;
use crate::role_revision::RuntimeAgentId;

const AGENT_BUS_POLL_WAIT_MS: &str = "2000";
const AGENT_BUS_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
// Reviewed registration verifies the frozen artifact set before publication.
// This is one bounded request, never an automatic registration retry.
const AGENT_REGISTRATION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const AGENT_MANAGEMENT_ACTION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBusHttpClient {
    base_url: String,
    token: Option<String>,
}

impl AgentBusHttpClient {
    pub fn from_config(config: &CodezConfig) -> Self {
        Self::new(
            agent_bus_base_url(agent_bus_port(config)),
            config.agent_bus_token.clone(),
        )
    }

    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        }
    }

    pub fn register(&self, request: &AgentBusRegisterRequest) -> anyhow::Result<()> {
        let body = serde_json::to_vec(request)?;
        let response = agent_bus_http_json_with_timeout(
            &self.base_url,
            "POST",
            "/api/agents/register",
            self.token.as_deref(),
            Some(&body),
            AGENT_REGISTRATION_RESPONSE_TIMEOUT,
        )?;
        require_ok_response(&response, "agent registration")
    }

    pub fn heartbeat(&self, agent_id: &str) -> anyhow::Result<()> {
        let body = serde_json::to_vec(&AgentBusHeartbeatRequest {
            id: agent_id.to_string(),
        })?;
        let response = self.request("POST", "/api/agents/heartbeat", Some(&body))?;
        require_ok_response(&response, "agent heartbeat")
    }

    pub fn unregister(&self, agent_id: &str) -> anyhow::Result<bool> {
        let body = serde_json::to_vec(&AgentBusUnregisterRequest {
            id: agent_id.to_string(),
        })?;
        let response = self.request("POST", "/api/agents/unregister", Some(&body))?;
        let response: AgentBusUnregisterResponse = serde_json::from_value(response)
            .context("Failed to parse agent unregister response")?;
        if !response.ok {
            anyhow::bail!("cutex agent bus rejected runtime unregister");
        }
        Ok(response.removed)
    }

    pub fn poll(&self, agent_id: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
        let response = self.request("GET", &agent_bus_poll_path(agent_id), None)?;
        serde_json::from_value::<AgentBusPollResponse>(response)
            .map(|response| response.messages)
            .context("Failed to parse agent poll response")
    }

    pub fn ack(&self, agent_id: &str, message_ids: &[String]) -> anyhow::Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let body = serde_json::to_vec(&AgentBusAckRequest {
            agent_id: agent_id.to_string(),
            message_ids: message_ids.to_vec(),
        })?;
        let response = self.request("POST", "/api/messages/ack", Some(&body))?;
        let response: AgentBusAckResponse = serde_json::from_value(response)
            .context("Failed to parse agent acknowledgement response")?;
        if !response.ok {
            anyhow::bail!("cutex agent bus rejected message acknowledgement");
        }
        Ok(response.acked)
    }

    pub fn redrive_durable_messages(&self) -> anyhow::Result<usize> {
        let response = self.request("POST", "/api/messages/redrive", Some(b"{}"))?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            anyhow::bail!("cutex agent bus rejected durable message redrive");
        }
        response
            .get("redriven")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .context("agent bus redrive response omitted count")
    }

    /// Every message in the bus's in-memory runtime queues (root token).
    pub fn queued_messages(
        &self,
    ) -> anyhow::Result<Vec<crate::agent_bus::queue::QueuedAgentBusMessage>> {
        let response = self.request("GET", "/api/messages/queued", None)?;
        require_ok_response(&response, "queued message listing")?;
        serde_json::from_value(response.get("messages").cloned().unwrap_or(Value::Null))
            .context("agent bus queued message listing is malformed")
    }

    /// Remove message ids from every in-memory runtime queue (root token);
    /// returns `(agent_id, message_id)` pairs removed.
    pub fn withdraw_messages(
        &self,
        message_ids: &[String],
        reason: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let body = serde_json::to_vec(&serde_json::json!({
            "messageIds": message_ids,
            "reason": reason,
        }))?;
        let response = self.request("POST", "/api/messages/withdraw", Some(&body))?;
        require_ok_response(&response, "message withdraw")?;
        Ok(response
            .get("withdrawn")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some((
                    entry.get("agentId")?.as_str()?.to_string(),
                    entry.get("messageId")?.as_str()?.to_string(),
                ))
            })
            .collect())
    }

    fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> anyhow::Result<Value> {
        agent_bus_http_json(&self.base_url, method, path, self.token.as_deref(), body)
    }
}

fn agent_bus_poll_path(agent_id: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("agent_id", agent_id)
        .append_pair("ack", "1")
        .append_pair("wait_ms", AGENT_BUS_POLL_WAIT_MS)
        .finish();
    format!("/api/messages/poll?{query}")
}

fn require_ok_response(response: &Value, operation: &str) -> anyhow::Result<()> {
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    anyhow::bail!("cutex agent bus rejected {operation}")
}

pub fn agent_bus_fetch_agents(config: &CodezConfig) -> anyhow::Result<Vec<AgentBusAgent>> {
    agent_bus_fetch_agents_scoped(config, None)
}

pub fn agent_bus_healthy(port: u16, token: Option<&str>) -> bool {
    http_local_root_status_ok(port, token, Duration::from_millis(250))
}

pub fn agent_bus_fetch_agents_if_healthy(config: &CodezConfig) -> Vec<AgentBusAgent> {
    if agent_bus_healthy(agent_bus_port(config), config.agent_bus_token.as_deref()) {
        agent_bus_fetch_agents(config).unwrap_or_default()
    } else {
        Vec::new()
    }
}

pub fn agent_bus_fetch_agents_scoped(
    config: &CodezConfig,
    requester_agent_id: Option<&str>,
) -> anyhow::Result<Vec<AgentBusAgent>> {
    agent_bus_fetch_agents_scoped_with_groups(config, requester_agent_id, false)
}

pub fn agent_bus_fetch_agents_scoped_with_groups(
    config: &CodezConfig,
    requester_agent_id: Option<&str>,
    all_groups: bool,
) -> anyhow::Result<Vec<AgentBusAgent>> {
    let path = requester_agent_id
        .filter(|value| !value.trim().is_empty())
        .map(|agent_id| format!("/api/agents?agent_id={agent_id}"))
        .unwrap_or_else(|| "/api/agents".to_string());
    let separator = if path.contains('?') { '&' } else { '?' };
    let path = if all_groups {
        format!(
            "{path}{separator}all_groups={}",
            if all_groups { "true" } else { "false" },
        )
    } else {
        path
    };
    let value = agent_bus_http_json_with_timeout(
        &agent_bus_base_url(agent_bus_port(config)),
        "GET",
        &path,
        config.agent_bus_token.as_deref(),
        None,
        AGENT_BUS_HTTP_TIMEOUT,
    )?;
    serde_json::from_value(value).context("Failed to parse agent list response")
}

pub fn agent_bus_send_agent_message(
    config: &CodezConfig,
    target: &str,
    message: &str,
    delivery_mode: AgentDeliveryMode,
    all_groups: bool,
    explicit_from: Option<&str>,
    external_message_id: Option<&str>,
) -> anyhow::Result<AgentBusSendResponse> {
    let external_message_id = normalize_external_message_id(external_message_id)?;
    let sender = resolve_agent_sender_name(config, explicit_from);
    let from_agent_id = std::env::var(CUTEX_AGENT_ID_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let request = agent_message_send_request(
        target,
        message,
        delivery_mode,
        all_groups,
        sender,
        from_agent_id,
        external_message_id,
    );
    let body = serde_json::to_vec(&request)?;
    let response = agent_bus_http_json(
        &agent_bus_base_url(agent_bus_port(config)),
        "POST",
        "/api/messages/send",
        config.agent_bus_token.as_deref(),
        Some(&body),
    )?;
    serde_json::from_value(response).context("Failed to parse agent send response")
}

/// Alarm targets were resolved locally at creation. Never consult remote peers
/// or inherit an interactive caller identity from the service environment.
pub fn send_local_alarm(
    config: &CodezConfig,
    target: &str,
    message: &str,
    message_id: &str,
) -> anyhow::Result<AgentBusSendResponse> {
    let mut request = agent_message_send_request(
        target,
        message,
        AgentDeliveryMode::AfterTurn,
        true,
        crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into(),
        None,
        Some(message_id.into()),
    );
    request.sender_kind = Some(AgentMessageKind::AlarmSystem);
    let response = agent_bus_http_json(
        &agent_bus_base_url(agent_bus_port(config)),
        "POST",
        "/api/messages/send",
        config.agent_bus_token.as_deref(),
        Some(&serde_json::to_vec(&request)?),
    )?;
    serde_json::from_value(response).context("Failed to parse alarm send response")
}

/// Enqueue a cross-host tell (peers): a passive note with the reserved peer
/// control record and no local sender identity (`agent_bus::peer_tell`).
pub fn send_peer_tell(
    config: &CodezConfig,
    target: &str,
    message: &str,
    facts: &crate::agent_bus::peer_tell::PeerTellFacts,
    message_id: &str,
) -> anyhow::Result<Value> {
    let mut request = agent_message_send_request(
        target,
        message,
        AgentDeliveryMode::Passive,
        true,
        facts.sender_label(),
        None,
        Some(message_id.into()),
    );
    request.control_type = Some(crate::agent_bus::peer_tell::CONTROL_TYPE.into());
    request.control_payload = Some(serde_json::to_value(facts)?);
    agent_bus_http_json(
        &agent_bus_base_url(agent_bus_port(config)),
        "POST",
        "/api/messages/send",
        config.agent_bus_token.as_deref(),
        Some(&serde_json::to_vec(&request)?),
    )
}

pub fn agent_bus_update_agent_groups(
    config: &CodezConfig,
    target: &str,
    groups: &[String],
    mode: AgentGroupUpdateMode,
) -> anyhow::Result<AgentBusGroupUpdateResponse> {
    let request = agent_group_update_request(target, groups, mode);
    let body = serde_json::to_vec(&request)?;
    let response = agent_bus_http_json(
        &agent_bus_base_url(agent_bus_port(config)),
        "POST",
        "/api/agents/groups",
        config.agent_bus_token.as_deref(),
        Some(&body),
    )?;
    serde_json::from_value(response).context("Failed to parse agent group update response")
}

pub fn agent_bus_submit_agent_management(
    config: &CodezConfig,
    request: &crate::agent_management::AgentManagementRequest,
) -> anyhow::Result<crate::agent_management::AgentManagementResponse> {
    let response = submit_authenticated_agent_control(
        config,
        "/api/agent-management/v1/actions",
        request,
        crate::agent_management::AGENT_MANAGEMENT_MAX_BODY_BYTES,
        "Agent Management",
        AGENT_MANAGEMENT_ACTION_TIMEOUT,
    )?;
    serde_json::from_slice(&response)
        .context("Failed to parse typed Cutex Agent Management response")
}

fn submit_authenticated_agent_control(
    config: &CodezConfig,
    path: &str,
    request: &impl serde::Serialize,
    max_body_bytes: usize,
    label: &str,
    response_timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let sender = std::env::var(CUTEX_AGENT_ID_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("CUTEX_AGENT_ID is required for {label}"))?;
    let sender = RuntimeAgentId::new(sender)
        .map_err(|_| anyhow::anyhow!("CUTEX_AGENT_ID is not a valid runtime agent ID"))?;
    let body = serde_json::to_vec(request)?;
    if body.len() > max_body_bytes {
        anyhow::bail!("{label} request exceeds the local route size limit");
    }
    let port = agent_bus_port(config);
    let route_token = config
        .agent_bus_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .with_context(|| format!("{label} requires a configured local agent bus bridge token"))?;
    submit_authenticated_agent_control_request(
        port,
        route_token,
        &sender,
        path,
        &body,
        label,
        response_timeout,
    )
}

#[allow(clippy::too_many_arguments)]
fn submit_authenticated_agent_control_request(
    port: u16,
    route_token: &str,
    sender: &RuntimeAgentId,
    path: &str,
    body: &[u8],
    label: &str,
    response_timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    submit_authenticated_agent_control_with_fence(
        port,
        route_token,
        sender,
        path,
        body,
        label,
        response_timeout,
        None,
    )
}

/// The local Agent Bus refused the connection: nothing was sent. Attached as
/// error context, so callers can tell "not sent" from "outcome unknown".
#[derive(Debug, Clone)]
pub struct BusUnreachable(pub String);

impl std::fmt::Display for BusUnreachable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The request may have reached the Agent Bus but its answer was lost or
/// unreadable: the outcome is unknown.
#[derive(Debug, Clone, Copy)]
pub struct ResponseUncertain;

impl std::fmt::Display for ResponseUncertain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Agent Bus response lost; outcome unknown")
    }
}

/// Role-filtered tool names for the calling agent (`POST` [`TOOLSET_PATH`],
/// no MCP caller fence: the native thread is unknown at `tools/list`).
///
/// [`TOOLSET_PATH`]: crate::agent_bus::agent_auth::TOOLSET_PATH
pub fn fetch_mcp_toolset(port: u16, token: &str, sender: &RuntimeAgentId) -> anyhow::Result<Value> {
    if token.trim().is_empty() || token.contains(['\r', '\n']) {
        anyhow::bail!("invalid MCP request configuration");
    }
    let response = submit_authenticated_agent_control_with_fence(
        port,
        token,
        sender,
        crate::agent_bus::agent_auth::TOOLSET_PATH,
        b"{}",
        "MCP toolset",
        Duration::from_secs(5),
        None,
    )?;
    Ok(serde_json::from_slice(&response)?)
}

pub fn submit_mcp_control(
    port: u16,
    token: &str,
    sender: &RuntimeAgentId,
    fence: &crate::agent_bus::mcp::CallerFence,
    path: &str,
    body: &Value,
) -> anyhow::Result<Value> {
    submit_mcp_control_with_wait(port, token, sender, fence, path, body, None)
}

/// A façade call that may hold on the Agent Bus for the owner's decision
/// (approval-wait): seconds to hold and the calling turn, when known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalWaitRequest {
    pub secs: u64,
    pub turn_id: Option<String>,
}

impl ApprovalWaitRequest {
    fn headers(&self) -> String {
        let mut headers = format!("X-Cutex-Approval-Wait: {}\r\n", self.secs);
        if let Some(turn) = self.turn_id.as_deref().filter(|turn| {
            !turn.is_empty() && turn.len() <= 200 && !turn.chars().any(char::is_control)
        }) {
            headers.push_str(&format!("X-Cutex-Mcp-Turn-Id: {turn}\r\n"));
        }
        headers
    }
}

/// How long a façade waits for the Agent Bus's answer on `path`, plus the
/// approval hold it asked for (approval-wait).
fn mcp_response_timeout(path: &str, wait: Option<&ApprovalWaitRequest>) -> Duration {
    let hold = wait.map_or(Duration::ZERO, |wait| Duration::from_secs(wait.secs));
    hold + if path == "/api/agent-management/v1/actions" {
        // Fixed native Management wrapper budget; timeout is uncertain,
        // never permission to issue a replacement action identity.
        Duration::from_secs(30)
    } else if path == crate::agent_bus::web_search::PATH {
        // One provider request (its own timeout) plus slack.
        crate::agent_bus::web_search::BUS_TIMEOUT
    } else if path == crate::asks::tools::PATH {
        // agent_wait blocks up to 120 s on the bus (asks §5.8).
        Duration::from_millis(crate::asks::WAIT_MAX_MS + 30_000)
    } else {
        AGENT_MANAGEMENT_ACTION_TIMEOUT
    }
}

/// [`submit_mcp_control`]; with `wait` the request carries the hold headers
/// and its read timeout grows by the hold.
pub fn submit_mcp_control_with_wait(
    port: u16,
    token: &str,
    sender: &RuntimeAgentId,
    fence: &crate::agent_bus::mcp::CallerFence,
    path: &str,
    body: &Value,
    wait: Option<&ApprovalWaitRequest>,
) -> anyhow::Result<Value> {
    if !crate::agent_bus::agent_auth::MCP_ROUTE_PATHS.contains(&path) {
        anyhow::bail!("unsupported MCP route");
    }
    let body = serde_json::to_vec(body)?;
    if body.len() > 262144 || token.trim().is_empty() || token.contains(['\r', '\n']) {
        anyhow::bail!("invalid MCP request configuration");
    }
    let response = match submit_authenticated_agent_control_with_headers(
        port,
        token,
        sender,
        path,
        &body,
        "MCP",
        mcp_response_timeout(path, wait),
        Some(fence),
        &wait.map(ApprovalWaitRequest::headers).unwrap_or_default(),
    ) {
        Ok(response) => response,
        Err(error) if error.downcast_ref::<BusUnreachable>().is_some() => return Err(error),
        Err(error) => return Err(error.context(ResponseUncertain)),
    };
    serde_json::from_slice(&response)
        .map_err(|error| anyhow::Error::new(error).context(ResponseUncertain))
}

#[allow(clippy::too_many_arguments)]
fn submit_authenticated_agent_control_with_fence(
    port: u16,
    route_token: &str,
    sender: &RuntimeAgentId,
    path: &str,
    body: &[u8],
    label: &str,
    response_timeout: Duration,
    fence: Option<&crate::agent_bus::mcp::CallerFence>,
) -> anyhow::Result<Vec<u8>> {
    submit_authenticated_agent_control_with_headers(
        port,
        route_token,
        sender,
        path,
        body,
        label,
        response_timeout,
        fence,
        "",
    )
}

/// `fenced_headers`: extra header lines (each ending in CRLF), sent only
/// with a fence.
#[allow(clippy::too_many_arguments)]
fn submit_authenticated_agent_control_with_headers(
    port: u16,
    route_token: &str,
    sender: &RuntimeAgentId,
    path: &str,
    body: &[u8],
    label: &str,
    response_timeout: Duration,
    fence: Option<&crate::agent_bus::mcp::CallerFence>,
    fenced_headers: &str,
) -> anyhow::Result<Vec<u8>> {
    let extra = if let Some(fence) = fence {
        let thread = crate::session::identity::normalize_codex_session_id(&fence.thread_id)?;
        format!(
            "X-Cutex-Mcp-Thread-Id: {thread}\r\nX-Cutex-Mcp-Generation: {}\r\nX-Cutex-Facade: delegations={}, {}\r\n{fenced_headers}",
            fence.generation,
            crate::asks::tools::FACADE_DELEGATIONS,
            crate::approvals::FACADE_APPROVAL_NOTE
        )
    } else {
        String::new()
    };
    let mut stream = crate::config::instance::ensure_port_allowed(port)
        .and_then(|()| TcpStream::connect(("127.0.0.1", port)))
        .map_err(|error| {
            anyhow::Error::new(error).context(BusUnreachable(format!(
                "Failed to connect local cutex agent bus {label} route"
            )))
        })?;
    stream.set_write_timeout(Some(AGENT_BUS_HTTP_TIMEOUT)).ok();
    stream.set_read_timeout(Some(response_timeout)).ok();
    let method = if fence.is_some() && path == crate::agent_bus::agent_auth::MCP_AGENT_LIST_PATH {
        "GET"
    } else {
        "POST"
    };
    let headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAuthorization: Bearer {route_token}\r\n{extra}X-Cutex-Agent-Id: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sender.as_str(),
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(&body)?;
    let mut response = Vec::new();
    let bounded_mcp = fence.is_some()
        && (path == "/api/agent-management/v1/actions"
            || path == crate::agent_bus::agent_auth::MCP_AGENT_LIST_PATH);
    // Native Management adapter limit: 1 MiB body. Bound framing too before allocating
    // an unbounded response; other existing callers retain their limits.
    let limit = if bounded_mcp {
        1024 * 1024 + 65536 + 1
    } else {
        u64::MAX
    };
    if bounded_mcp {
        response = mcp_http_response::read_response(&mut stream)
            .with_context(|| format!("Failed to read Cutex {label} response"))?;
    } else {
        Read::by_ref(&mut stream)
            .take(limit)
            .read_to_end(&mut response)
            .with_context(|| format!("Failed to read Cutex {label} response"))?;
    }
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .with_context(|| format!("Cutex {label} response has no HTTP header boundary"))?;
    let header = String::from_utf8_lossy(&response[..split]);
    if bounded_mcp && (split > 65536 || response.len() - split - 4 > 1024 * 1024) {
        return Ok(b"{\"mcp_transport_invalid_response\":true}".to_vec());
    }
    if !header.starts_with("HTTP/1.1 2") {
        let body = String::from_utf8_lossy(&response[split + 4..]);
        if fence.is_some() {
            let status = header
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(500);
            let error = serde_json::from_str::<Value>(&body)
                .unwrap_or_else(|_| serde_json::json!({"message":body.trim()}));
            return Ok(serde_json::to_vec(
                &serde_json::json!({"http_status":status,"error":error}),
            )?);
        }
        anyhow::bail!("Cutex {label} route returned non-success: {header}\n{body}");
    }
    Ok(response[split + 4..].to_vec())
}

fn agent_group_update_request(
    target: &str,
    groups: &[String],
    mode: AgentGroupUpdateMode,
) -> AgentBusGroupUpdateRequest {
    AgentBusGroupUpdateRequest {
        target: target.to_string(),
        groups: groups.to_vec(),
        mode,
    }
}

fn agent_message_send_request(
    target: &str,
    message: &str,
    delivery_mode: AgentDeliveryMode,
    all_groups: bool,
    sender: String,
    from_agent_id: Option<String>,
    external_message_id: Option<String>,
) -> AgentBusSendRequest {
    let trigger_turn = delivery_mode.trigger_turn();
    AgentBusSendRequest {
        to: target.to_string(),
        all_groups,
        kind: AgentBusEnvelopeKind::Message,
        from: Some(sender),
        from_agent_id,
        from_session_id: None,
        to_session_id: None,
        content: message.to_string(),
        delivery_mode: Some(delivery_mode),
        queue_only: None,
        trigger_turn: Some(trigger_turn),
        sender_kind: Some(AgentMessageKind::Agent),
        display_source: None,
        submit_mode: None,
        control_type: None,
        control_payload: None,
        external_action_id: None,
        external_message_id,
        sender_facts: None,
    }
}

fn normalize_external_message_id(value: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("External message ID cannot be empty or whitespace-only");
    }
    Ok(Some(value.to_string()))
}

fn resolve_agent_sender_name(config: &CodezConfig, explicit_from: Option<&str>) -> String {
    if let Some(from) = explicit_from.filter(|value| !value.trim().is_empty()) {
        return from.to_string();
    }
    if let Ok(agent_id) = std::env::var(CUTEX_AGENT_ID_ENV_VAR) {
        if let Ok(agents) = agent_bus_fetch_agents(config) {
            if let Some(agent) = agents.iter().find(|agent| agent.id == agent_id) {
                return agent_sender_label(agent);
            }
        }
        return std::env::var(CUTEX_AGENT_NAME_ENV_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(agent_id);
    }
    std::env::var(CUTEX_AGENT_NAME_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "cutex".to_string())
}

pub fn agent_bus_http_json(
    base_url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> anyhow::Result<Value> {
    agent_bus_http_json_with_timeout(base_url, method, path, token, body, AGENT_BUS_HTTP_TIMEOUT)
}

fn agent_bus_http_json_with_timeout(
    base_url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let base_url = base_url.trim_end_matches('/');
    let url = format!("{base_url}{path}");
    http_json_request(HttpJsonRequest {
        url: &url,
        method,
        token,
        body,
        timeout,
        invalid_url_context: &format!("Invalid agent bus URL: {url}"),
        only_http_message: "Only http:// agent bus URLs are supported",
        missing_host_message: &format!("Agent bus URL has no host: {url}"),
        connect_context: "Failed to connect cutex agent bus",
        read_context: "Failed to read agent bus response",
        non_success_prefix: "cutex agent bus returned non-success",
        parse_context: "Failed to parse agent bus JSON response",
        ok_text_as_null: false,
    })
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    use serde_json::json;

    use super::*;
    use crate::agent_bus::model::AgentRegistrationClass;
    use crate::http::server::read_simple_http_request;
    use crate::http::server::write_json_response;

    #[test]
    fn mcp_control_reaches_http_with_fence_and_full_response() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = json!({"schema":"cutex/task-service-query/v2","query":{"operation":"read_contract","body":{"assignment_id":"assignment-1"}}});
        let response = json!({"outcome":{"kind":"contract","body":{"opaque_contract":"完整合同\n".repeat(10000)}}});
        let expected_request = body.clone();
        let expected_response = response.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_simple_http_request(&mut stream).unwrap();
            assert_eq!(request.path, "/api/agent-management/v1/actions");
            assert_eq!(request.headers["x-cutex-mcp-generation"], "3");
            assert_eq!(request.headers["x-cutex-agent-id"], "stock.worker");
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                expected_request
            );
            write_json_response(&mut stream, 200, "OK", &expected_response).unwrap();
        });
        let result = submit_mcp_control(
            port,
            "private-test-token",
            &RuntimeAgentId::new("stock.worker").unwrap(),
            &crate::agent_bus::mcp::CallerFence {
                thread_id: "11111111-1111-4111-8111-111111111111".into(),
                generation: 3,
            },
            "/api/agent-management/v1/actions",
            &body,
        )
        .unwrap();
        assert_eq!(result, response);
        server.join().unwrap();
        // Routes of removed tools are refused before connecting.
        for removed in [
            "/api/messages/send",
            "/api/task/v2/query",
            "/api/task/v2/actions",
        ] {
            let error = submit_mcp_control(
                port,
                "private-test-token",
                &RuntimeAgentId::new("stock.worker").unwrap(),
                &crate::agent_bus::mcp::CallerFence {
                    thread_id: "11111111-1111-4111-8111-111111111111".into(),
                    generation: 3,
                },
                removed,
                &json!({}),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unsupported MCP route"),
                "{removed}"
            );
        }
    }

    #[test]
    fn approval_wait_headers_and_timeout_only_when_opted_in() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_simple_http_request(&mut stream).unwrap();
                seen.push((
                    request.headers.get(crate::approvals::APPROVAL_WAIT_HEADER).cloned(),
                    request.headers.get(crate::approvals::TURN_HEADER).cloned(),
                ));
                write_json_response(&mut stream, 200, "OK", &json!({"ok": true})).unwrap();
            }
            seen
        });
        let fence = crate::agent_bus::mcp::CallerFence {
            thread_id: "11111111-1111-4111-8111-111111111111".into(),
            generation: 3,
        };
        let sender = RuntimeAgentId::new("stock.worker").unwrap();
        let wait = ApprovalWaitRequest { secs: 120, turn_id: Some("turn-7".into()) };
        let body = json!({"op": "list", "args": {}});
        submit_mcp_control_with_wait(port, "private-test-token", &sender, &fence, "/api/services/v1", &body, Some(&wait)).unwrap();
        submit_mcp_control(port, "private-test-token", &sender, &fence, "/api/services/v1", &body).unwrap();
        assert_eq!(
            server.join().unwrap(),
            [(Some("120".to_string()), Some("turn-7".to_string())), (None, None)]
        );
        // The read timeout grows by the hold; a turn id with a line break is dropped.
        let path = "/api/agent-management/v1/actions";
        assert_eq!(mcp_response_timeout(path, None), Duration::from_secs(30));
        assert_eq!(mcp_response_timeout(path, Some(&wait)), Duration::from_secs(150));
        assert_eq!(mcp_response_timeout("/api/services/v1", Some(&wait)), AGENT_MANAGEMENT_ACTION_TIMEOUT + Duration::from_secs(120));
        let injected = ApprovalWaitRequest { secs: 5, turn_id: Some("t\r\nX-Evil: 1".into()) };
        assert_eq!(injected.headers(), "X-Cutex-Approval-Wait: 5\r\n");
    }

    #[test]
    fn mcp_complete_receipt_returns_while_peer_remains_open() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done, wait) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_simple_http_request(&mut stream).unwrap();
            write_json_response(&mut stream, 200, "OK", &json!({"receipt":"complete"})).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        let result = submit_authenticated_agent_control_with_fence(
            port,
            "test-token",
            &RuntimeAgentId::new("stock.worker").unwrap(),
            "/api/agent-management/v1/actions",
            b"{}",
            "test",
            Duration::from_millis(100),
            Some(&crate::agent_bus::mcp::CallerFence {
                thread_id: "11111111-1111-4111-8111-111111111111".into(),
                generation: 3,
            }),
        );
        done.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&result.unwrap()).unwrap(),
            json!({"receipt":"complete"})
        );
    }

    #[test]
    fn agent_message_send_request_sets_agent_wire_fields() {
        let request = agent_message_send_request(
            "worker",
            "hello",
            AgentDeliveryMode::Passive,
            true,
            "sender".to_string(),
            Some("agent-1".to_string()),
            None,
        );

        assert_eq!(request.to, "worker");
        assert_eq!(request.content, "hello");
        assert!(request.all_groups);
        assert!(serde_json::to_value(&request)
            .expect("serialize request")
            .get("all_hosts")
            .is_none());
        assert_eq!(request.kind, AgentBusEnvelopeKind::Message);
        assert_eq!(request.from.as_deref(), Some("sender"));
        assert_eq!(request.from_agent_id.as_deref(), Some("agent-1"));
        assert_eq!(request.delivery_mode, Some(AgentDeliveryMode::Passive));
        assert_eq!(request.trigger_turn, Some(false));
        assert_eq!(request.sender_kind, Some(AgentMessageKind::Agent));
        assert!(request.queue_only.is_none());
        assert!(request.from_session_id.is_none());
        assert!(request.to_session_id.is_none());
        assert!(request.display_source.is_none());
        assert!(request.submit_mode.is_none());
        assert!(request.control_type.is_none());
        assert!(request.control_payload.is_none());
        assert!(request.external_action_id.is_none());
        assert!(request.external_message_id.is_none());
    }

    #[test]
    fn agent_message_send_request_serializes_normalized_external_message_id() {
        let external_message_id = normalize_external_message_id(Some("  upstream-42  "))
            .expect("external message ID should normalize");
        let request = agent_message_send_request(
            "worker",
            "hello",
            AgentDeliveryMode::Soon,
            false,
            "sender".to_string(),
            None,
            external_message_id,
        );

        assert_eq!(request.external_message_id.as_deref(), Some("upstream-42"));
        assert!(request.external_action_id.is_none());
        let serialized = serde_json::to_value(request).expect("request should serialize");
        assert_eq!(
            serialized
                .get("external_message_id")
                .and_then(Value::as_str),
            Some("upstream-42")
        );
        assert!(serialized
            .get("external_action_id")
            .is_some_and(Value::is_null));
    }

    #[test]
    fn external_message_id_normalization_rejects_blank_values() {
        assert_eq!(
            normalize_external_message_id(None).expect("omitted ID should remain valid"),
            None
        );
        for blank in ["", "   ", "\t\n"] {
            let error = normalize_external_message_id(Some(blank))
                .expect_err("blank external message ID should fail");
            assert!(error.to_string().contains("cannot be empty"));
        }
    }

    #[test]
    fn agent_group_update_request_sets_group_wire_fields() {
        let request = agent_group_update_request(
            "worker",
            &["project:abc".to_string(), "waveline".to_string()],
            AgentGroupUpdateMode::Add,
        );

        assert_eq!(request.target, "worker");
        assert_eq!(
            request.groups,
            vec!["project:abc".to_string(), "waveline".to_string()]
        );
        assert_eq!(request.mode, AgentGroupUpdateMode::Add);
    }

    #[test]
    fn poll_path_percent_encodes_agent_id_and_requests_explicit_ack() {
        assert_eq!(
            agent_bus_poll_path("agent id/one"),
            "/api/messages/poll?agent_id=agent+id%2Fone&ack=1&wait_ms=2000"
        );
    }

    #[test]
    fn agent_management_response_can_arrive_after_the_ordinary_route_budget() {
        let ordinary_test_budget = Duration::from_millis(20);
        let action_test_budget = Duration::from_secs(2);
        let response_delay = Duration::from_millis(60);
        assert!(AGENT_MANAGEMENT_ACTION_TIMEOUT > AGENT_BUS_HTTP_TIMEOUT);

        let action_id = crate::agent_management::AgentActionId::new("slow-action").unwrap();
        let request = crate::agent_management::AgentManagementRequest {
            schema: crate::agent_management::AgentManagementSchema::V1,
            action_id: action_id.clone(),
            project_id: Some(crate::agent_management::ProjectId::new("timeout-project").unwrap()),
            operation: crate::agent_management::AgentOperation::QueryManaged,
        };
        let expected_response = crate::agent_management::AgentManagementResponse {
            schema: crate::agent_management::AgentManagementSchema::V1,
            action_id,
            outcome: crate::agent_management::AgentManagementOutcome::NoWrite {
                code: "fixture".to_string(),
                detail: "delayed typed response".to_string(),
            },
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind delayed action route");
        let port = listener.local_addr().unwrap().port();
        let expected_request = request.clone();
        let response_value = serde_json::to_value(&expected_response).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept delayed action request");
            let received = read_simple_http_request(&mut stream).expect("read delayed action");
            assert_eq!(received.path, "/api/agent-management/v1/actions");
            assert_eq!(
                received.headers.get("authorization").map(String::as_str),
                Some("Bearer route-token")
            );
            assert_eq!(
                received.headers.get("x-cutex-agent-id").map(String::as_str),
                Some("runtime-timeout")
            );
            assert_eq!(
                serde_json::from_slice::<crate::agent_management::AgentManagementRequest>(
                    &received.body
                )
                .unwrap(),
                expected_request
            );
            thread::sleep(response_delay);
            write_json_response(&mut stream, 200, "OK", &response_value)
                .expect("write delayed typed response");
        });

        let body = serde_json::to_vec(&request).unwrap();
        let sender = RuntimeAgentId::new("runtime-timeout").unwrap();
        let started = Instant::now();
        let response = submit_authenticated_agent_control_request(
            port,
            "route-token",
            &sender,
            "/api/agent-management/v1/actions",
            &body,
            "Agent Management",
            action_test_budget,
        )
        .expect("route-specific budget must admit the delayed response");
        let elapsed = started.elapsed();
        server.join().expect("delayed action server");
        assert!(elapsed > ordinary_test_budget);
        assert_eq!(
            serde_json::from_slice::<crate::agent_management::AgentManagementResponse>(&response)
                .unwrap(),
            expected_response
        );
    }

    #[test]
    fn runtime_client_registers_polls_in_ack_mode_and_acknowledges() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test bus");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("test bus address")
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept registration");
            let request = read_simple_http_request(&mut stream).expect("read registration");
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/api/agents/register");
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer test-token")
            );
            let registration: AgentBusRegisterRequest =
                serde_json::from_slice(&request.body).expect("parse registration");
            assert_eq!(registration.id, "runtime id/one");
            write_json_response(&mut stream, 200, "OK", &json!({ "ok": true }))
                .expect("write registration response");

            let (mut stream, _) = listener.accept().expect("accept poll");
            let request = read_simple_http_request(&mut stream).expect("read poll");
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.path,
                "/api/messages/poll?agent_id=runtime+id%2Fone&ack=1&wait_ms=2000"
            );
            write_json_response(
                &mut stream,
                200,
                "OK",
                &json!({
                    "messages": [{
                        "id": "message-1",
                        "kind": "message",
                        "from": "sender",
                        "to": "runtime id/one",
                        "content": "hello",
                        "deliveryMode": "soon",
                        "triggerTurn": true,
                        "createdAtEpochSecs": 1,
                        "senderKind": "agent"
                    }]
                }),
            )
            .expect("write poll response");

            let (mut stream, _) = listener.accept().expect("accept acknowledgement");
            let request = read_simple_http_request(&mut stream).expect("read acknowledgement");
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/api/messages/ack");
            let acknowledgement: AgentBusAckRequest =
                serde_json::from_slice(&request.body).expect("parse acknowledgement");
            assert_eq!(acknowledgement.agent_id, "runtime id/one");
            assert_eq!(acknowledgement.message_ids, vec!["message-1"]);
            write_json_response(&mut stream, 200, "OK", &json!({ "ok": true, "acked": 1 }))
                .expect("write acknowledgement response");

            let (mut stream, _) = listener.accept().expect("accept unregister");
            let request = read_simple_http_request(&mut stream).expect("read unregister");
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/api/agents/unregister");
            let unregister: AgentBusUnregisterRequest =
                serde_json::from_slice(&request.body).expect("parse unregister");
            assert_eq!(unregister.id, "runtime id/one");
            write_json_response(
                &mut stream,
                200,
                "OK",
                &json!({ "ok": true, "removed": true }),
            )
            .expect("write unregister response");
        });

        let client = AgentBusHttpClient::new(base_url, Some(" test-token ".to_string()));
        client
            .register(&AgentBusRegisterRequest {
                id: "runtime id/one".to_string(),
                name: "runtime".to_string(),
                base_name: None,
                thread_name: None,
                path_key: None,
                session_id: Some("thread-1".to_string()),
                profile: "profile".to_string(),
                cwd: "/tmp".to_string(),
                pid: 42,
                host_id: Some("host".to_string()),
                groups: Vec::new(),
                registration_class: AgentRegistrationClass::Persistent,
            })
            .expect("register runtime");
        let messages = client.poll("runtime id/one").expect("poll messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].delivery_mode, AgentDeliveryMode::Soon);
        assert_eq!(
            client
                .ack("runtime id/one", &["message-1".to_string()])
                .expect("acknowledge message"),
            1
        );
        assert!(client
            .unregister("runtime id/one")
            .expect("unregister runtime"));
        server.join().expect("test bus server");
    }
}

#[path = "mcp_http_response.rs"]
mod mcp_http_response;
