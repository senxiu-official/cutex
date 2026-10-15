//! Explicit browser-only authority; never broadens ordinary Management routes.
use super::{model::*, state::DesktopIdentity};
use crate::http::server::SimpleHttpRequest;
use crate::management::server::ManagementRequestContext;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct AgentContext {
    pub session: String,
    pub runtime: String,
    pub thread: String,
    pub generation: u64,
    pub turn: String,
    pub credential_identity: String,
}

pub fn desktop(request: &SimpleHttpRequest, root: Option<&str>) -> Result<DesktopIdentity> {
    if !crate::management::v2::server::authenticated_root_request(request, root) {
        return Err(RelayError::new("permission_denied"));
    }
    if let Some(token) = crate::management::listeners::current_client_token() {
        if !token.still_valid_uncached() {
            return Err(RelayError::new("permission_denied"));
        }
        return Ok(DesktopIdentity {
            id: format!("client:{}", token.token_id),
            valid: Arc::new(move || token.still_valid_uncached()),
        });
    }
    // Identity is a digest, never a raw Management credential. The running
    // listener's root is immutable; restarting it resets every registration.
    let id = format!(
        "root:{:x}",
        Sha256::digest(root.unwrap_or_default().as_bytes())
    );
    Ok(DesktopIdentity {
        id,
        valid: Arc::new(|| true),
    })
}

pub fn agent(
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    require_turn: bool,
) -> Result<AgentContext> {
    use crate::agent_bus::agent_auth::{authenticate, BusCaller};
    let denied = || RelayError::new("permission_denied");
    let config = crate::config::store::load_codez_config_checked()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    let caller =
        authenticate(&request.headers, config.agent_bus_token.as_deref()).map_err(|_| denied())?;
    let BusCaller::Agent(principal) = &caller else {
        return Err(denied());
    };
    let principal = principal.clone();
    let thread = request
        .headers
        .get(crate::agent_bus::mcp::THREAD_HEADER)
        .ok_or_else(denied)?
        .clone();
    let generation = request
        .headers
        .get(crate::agent_bus::mcp::GENERATION_HEADER)
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(denied)?;
    // Heartbeats are live Bus memory, not the persisted startup registry.
    // Ask its narrow fence endpoint using exactly this cxa1 proof.
    let proof = live_fence(request, crate::agent_bus::service::agent_bus_port(&config))?;
    if proof["cutexSessionId"] != principal.cutex_session_id
        || proof["runtimeAgentId"] != principal.runtime_agent_id
        || proof["threadId"] != thread
        || proof["runtimeGeneration"] != generation
    {
        return Err(denied());
    }
    let status = (context.load_runtime_status)(&principal.cutex_session_id)
        .map_err(|_| denied())?
        .ok_or_else(denied)?;
    if !status.connected
        || status.cutex_session_id != principal.cutex_session_id
        || status.thread_id != thread
        || status.runtime_generation != generation
    {
        return Err(denied());
    }
    let turn = status.active_turn_id.unwrap_or_default();
    if require_turn && !identifier(&turn) {
        return Err(denied());
    }
    Ok(AgentContext {
        session: principal.cutex_session_id,
        runtime: principal.runtime_agent_id.clone(),
        thread,
        generation,
        turn,
        credential_identity: format!(
            "agent:{:x}",
            Sha256::digest(
                request
                    .headers
                    .get("authorization")
                    .ok_or_else(denied)?
                    .as_bytes()
            )
        ),
    })
}

fn live_fence(request: &SimpleHttpRequest, port: u16) -> Result<serde_json::Value> {
    let denied = || RelayError::new("permission_denied");
    let unavailable = || RelayError::new("browser_unavailable");
    let mut headers = String::new();
    for key in [
        "authorization",
        "x-cutex-agent-id",
        crate::agent_bus::mcp::THREAD_HEADER,
        crate::agent_bus::mcp::GENERATION_HEADER,
    ] {
        let value = request
            .headers
            .get(key)
            .filter(|v| !v.is_empty() && v.len() <= 1024 && !v.contains(['\r', '\n']))
            .ok_or_else(denied)?;
        headers.push_str(&format!("{key}: {value}\r\n"));
    }
    let mut socket = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(500),
    )
    .map_err(|_| unavailable())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|_| unavailable())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|_| unavailable())?;
    let wire=format!("POST {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",crate::agent_bus::agent_auth::BROWSER_FENCE_PATH);
    socket
        .write_all(wire.as_bytes())
        .map_err(|_| unavailable())?;
    let mut bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut buffer = [0u8; 1024];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(unavailable)?;
        socket
            .set_read_timeout(Some(remaining))
            .map_err(|_| unavailable())?;
        let n = socket.read(&mut buffer).map_err(|_| unavailable())?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..n]);
        if bytes.len() > 16384 {
            return Err(unavailable());
        }
    }
    if bytes.len() > 16384 {
        return Err(unavailable());
    }
    let split = bytes
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .ok_or_else(unavailable)?;
    if bytes.starts_with(b"HTTP/1.1 403 ") || bytes.starts_with(b"HTTP/1.1 401 ") {
        return Err(denied());
    }
    if split > 8192 || !bytes.starts_with(b"HTTP/1.1 200 ") {
        return Err(unavailable());
    }
    serde_json::from_slice(&bytes[split + 4..]).map_err(|_| unavailable())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    fn request(bearer: Option<&str>) -> SimpleHttpRequest {
        let mut headers = HashMap::new();
        if let Some(bearer) = bearer {
            headers.insert("authorization".into(), format!("Bearer {bearer}"));
        }
        SimpleHttpRequest {
            method: "POST".into(),
            path: "/v2/browser/devices/register".into(),
            body: b"{}".to_vec(),
            headers,
        }
    }
    #[test]
    fn browser_desktop_auth_requires_configured_root_and_never_accepts_cxa1() {
        assert!(desktop(&request(None), None).is_err());
        assert!(desktop(&request(Some("root")), None).is_err());
        assert!(desktop(&request(Some("root")), Some("")).is_err());
        assert!(desktop(&request(Some("cxa1.fake.session")), Some("root")).is_err());
        let identity = desktop(&request(Some("root")), Some("root")).unwrap();
        assert!(identity.id.starts_with("root:"));
        assert_eq!(identity.id.len(), 69);
        assert_ne!(identity.id, "root");
    }
    #[test]
    fn browser_live_fence_does_not_dial_without_every_original_header() {
        // Port zero cannot be a listening destination. Missing/unsafe headers
        // must be rejected before any connection is attempted.
        let mut request = request(Some("cxa1.fake.session"));
        assert_eq!(
            live_fence(&request, 0).unwrap_err().code,
            "permission_denied"
        );
        request
            .headers
            .insert("x-cutex-agent-id".into(), "runtime".into());
        request
            .headers
            .insert(crate::agent_bus::mcp::THREAD_HEADER.into(), "thread".into());
        request.headers.insert(
            crate::agent_bus::mcp::GENERATION_HEADER.into(),
            "1\r\nInjected: x".into(),
        );
        assert_eq!(
            live_fence(&request, 0).unwrap_err().code,
            "permission_denied"
        );
    }
    #[test]
    fn browser_live_fence_closed_bus_is_unavailable_not_authorization_denial() {
        let mut request = request(Some("cxa1.fake.session"));
        request
            .headers
            .insert("x-cutex-agent-id".into(), "runtime".into());
        request
            .headers
            .insert(crate::agent_bus::mcp::THREAD_HEADER.into(), "thread".into());
        request
            .headers
            .insert(crate::agent_bus::mcp::GENERATION_HEADER.into(), "1".into());
        assert_eq!(
            live_fence(&request, 0).unwrap_err().code,
            "browser_unavailable"
        );
    }
}
