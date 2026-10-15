//! Per-agent Agent Bus authentication (owner decision 2026-10-07).
//!
//! Every managed agent runtime authenticates with its own credential, never
//! with the daemon's root `agent_bus_token`:
//!
//! ```text
//! cxa1.<hex HMAC-SHA256(root, label | runtime_agent_id | 0 | cutex_session_id)>.<cutex_session_id>
//! ```
//!
//! The runtime agent id travels in `x-cutex-agent-id` (as before) and the
//! durable session id inside the credential. The bus recomputes the MAC with
//! the root token it already holds, so both identities are authenticated
//! rather than self-declared. Derivation needs no storage: a daemon restart
//! keeps credentials valid and rotating the root token revokes all of them.
//! Stock runtime ids are a fresh `stock.<uuid>` per launch (Claude:
//! `claude.<connection>`), so a credential is per runtime occurrence.
//!
//! An agent credential is accepted only on the MCP adapter's route set
//! ([`agent_route_allowed`]). Registration, heartbeat, polling, acks, groups,
//! redrive stays root-only (the removed Release rotation and
//! Task Service routes answer 410 before authentication).
//!
//! Requests presenting the root token keep their previous contract: the root
//! credential belongs to the daemon and the owner's tooling. That is also the
//! upgrade path: runtimes launched before per-agent credentials carry the
//! root token and keep working until they are relaunched.
//!
//! One non-MCP caller also holds the runtime's credential: the Job Service
//! MCP adapter (`review-sources/job-service`, `cutex_job` server), which
//! inherits `CUTEX_AGENT_BUS_TOKEN` and resolves its caller with
//! `GET /api/agents?agent_id=<runtime>&all_hosts=false` and no
//! `x-cutex-agent-id` header. Its binary is pinned by digest in the reviewed
//! Job descriptor, so the bus accepts exactly that request shape
//! ([`job_identity_lookup_runtime`]): the runtime id comes from the query,
//! the MAC binds it, and the answer is only the caller's own roster entry
//! ([`job_identity_view`]).
//!
//! New bus routes reuse this module: wrap the request with
//! [`BusRequest::authenticate`], then read [`BusRequest::agent`] for the
//! authenticated caller.

use std::collections::HashMap;
use std::ops::Deref;

use sha2::Digest;
use sha2::Sha256;

use crate::http::server::SimpleHttpRequest;
use crate::profiles::model::CodezConfig;

/// Prefix of a per-agent credential. Versioned so the derivation can change.
pub const AGENT_CREDENTIAL_PREFIX: &str = "cxa1.";
const DERIVATION_LABEL: &[u8] = b"cutex-agent-bus-credential-v1\0";

/// Environment variable that carries the Agent Bus credential to the cutex
/// MCP adapter process.
pub const BUS_TOKEN_ENV: &str = "CUTEX_AGENT_BUS_TOKEN";

/// `-c` override that blanks the credential in the environment cute-codex
/// builds for shell/exec tools. `shell_environment_policy.set` is applied
/// after inheritance and excludes, so this hides the value without replacing
/// the profile's own `exclude` list (a `-c ...exclude=[...]` override would
/// drop it). MCP servers get their environment separately (`env_vars`).
pub const SHELL_ENV_BLANK_OVERRIDE: &str = "shell_environment_policy.set.CUTEX_AGENT_BUS_TOKEN";

/// Role lookup used by the MCP adapter at `tools/list`. It carries no MCP
/// caller fence: the native thread is not known before the first call.
pub const TOOLSET_PATH: &str = "/api/mcp/v1/toolset";
/// Read-only current-occurrence proof for Management's browser relay.
pub const BROWSER_FENCE_PATH: &str = "/api/mcp/v1/browser-fence";

/// The Agent Bus routes the cutex MCP adapter calls
/// (`client::submit_mcp_control`). The agent list is a GET, the rest POST.
pub const MCP_ROUTE_PATHS: &[&str] = &[
    MCP_AGENT_LIST_PATH,
    "/api/agent-management/v1/actions",
    "/api/services/v1",
    BROWSER_FENCE_PATH,
    // Asks (委托): agent_list / agent_ask / agent_wait / … (asks §6).
    "/api/asks/v1",
    "/api/asks/v1/resolve-ids",
    // web_search (profile-search-tool).
    crate::agent_bus::web_search::PATH,
];

/// The fenced, local, own-groups agent list the MCP adapter reads.
pub const MCP_AGENT_LIST_PATH: &str = "/api/agents?all_groups=false&all_hosts=false";

/// The runtime id of a Job Service adapter identity lookup:
/// `GET /api/agents?agent_id=<runtime>&all_hosts=false`, exactly as the
/// deployed adapter sends it (it percent-encodes everything but RFC 3986
/// unreserved bytes, which every runtime id consists of).
pub fn job_identity_lookup_runtime<'a>(method: &str, path: &'a str) -> Option<&'a str> {
    if method != "GET" {
        return None;
    }
    let runtime = path
        .strip_prefix("/api/agents?agent_id=")?
        .strip_suffix("&all_hosts=false")?;
    (!runtime.is_empty()
        && runtime
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
        && crate::role_revision::RuntimeAgentId::new(runtime.to_string()).is_ok())
    .then_some(runtime)
}

/// What a per-agent credential sees on the Job identity lookup: its own
/// roster entry, and only while that entry projects the credential's durable
/// session. Anything else is an empty list, which the adapter refuses.
pub fn job_identity_view(
    agents: Vec<crate::agent_bus::model::AgentBusAgent>,
    principal: &AgentPrincipal,
) -> Vec<crate::agent_bus::model::AgentBusAgent> {
    agents
        .into_iter()
        .filter(|agent| {
            agent.id == principal.runtime_agent_id
                && agent.cutex_session_id.as_deref() == Some(principal.cutex_session_id.as_str())
        })
        .collect()
}

/// The authenticated identity behind a per-agent credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPrincipal {
    pub runtime_agent_id: String,
    pub cutex_session_id: String,
}

/// Who sent a bus request, judged from its credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusCaller {
    /// The root Agent Bus token (daemon, owner tooling, pre-upgrade runtimes).
    /// Identity headers remain self-declared under the root contract.
    Root,
    /// A verified per-agent credential.
    Agent(AgentPrincipal),
    /// No credential, or a non-agent value that is not the root token. Route
    /// checks decide (the health route is open; others reject).
    Unauthenticated,
}

/// A presented agent credential that does not verify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthRejection(&'static str);

impl AuthRejection {
    pub fn message(self) -> &'static str {
        self.0
    }
}

fn hmac_sha256(key: &[u8], message: &[&[u8]]) -> [u8; 32] {
    let mut block = [0_u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    for part in message {
        inner.update(part);
    }
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn mac_hex(root: &str, runtime_agent_id: &str, cutex_session_id: &str) -> String {
    hex(&hmac_sha256(
        root.as_bytes(),
        &[
            DERIVATION_LABEL,
            runtime_agent_id.as_bytes(),
            b"\0",
            cutex_session_id.as_bytes(),
        ],
    ))
}

/// The per-agent credential for one runtime of one durable session.
pub fn agent_credential(root: &str, runtime_agent_id: &str, cutex_session_id: &str) -> String {
    format!(
        "{AGENT_CREDENTIAL_PREFIX}{}.{cutex_session_id}",
        mac_hex(root, runtime_agent_id, cutex_session_id)
    )
}

/// The Agent Bus credential to hand a managed runtime launched as
/// `runtime_agent_id` for `cutex_session_id`. Always per-agent; only a debug
/// build with [`crate::config::agent_security::DEBUG_SHARED_BUS_TOKEN_ENV`]
/// set hands out the root token. `None` when no root token is configured.
pub fn launch_credential(
    config: &CodezConfig,
    runtime_agent_id: &str,
    cutex_session_id: &str,
) -> Option<String> {
    let root = config
        .agent_bus_token
        .as_deref()
        .filter(|token| !token.trim().is_empty())?;
    if crate::config::agent_security::debug_shared_bus_token() {
        return Some(root.to_string());
    }
    Some(agent_credential(root, runtime_agent_id, cutex_session_id))
}

fn bearer(headers: &HashMap<String, String>) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Classify a request by its credential. Only a presented agent credential
/// that fails verification is an error; everything else is left to the
/// route's own checks, exactly as before per-agent credentials existed.
pub fn authenticate(
    headers: &HashMap<String, String>,
    root: Option<&str>,
) -> Result<BusCaller, AuthRejection> {
    authenticate_with_runtime_fallback(headers, root, None)
}

/// [`authenticate`], taking the runtime id from `fallback_runtime` when the
/// request carries no `x-cutex-agent-id` (the Job identity lookup).
fn authenticate_with_runtime_fallback(
    headers: &HashMap<String, String>,
    root: Option<&str>,
    fallback_runtime: Option<&str>,
) -> Result<BusCaller, AuthRejection> {
    let Some(root) = root.filter(|root| !root.is_empty()) else {
        // No root token configured: the bus is unauthenticated and nothing
        // can be verified (stock launches refuse to run in this state).
        return Ok(BusCaller::Unauthenticated);
    };
    let Some(presented) = bearer(headers) else {
        return Ok(BusCaller::Unauthenticated);
    };
    if constant_time_eq(presented.as_bytes(), root.as_bytes()) {
        return Ok(BusCaller::Root);
    }
    let Some(rest) = presented.strip_prefix(AGENT_CREDENTIAL_PREFIX) else {
        return Ok(BusCaller::Unauthenticated);
    };
    let (mac, session) = rest
        .split_once('.')
        .ok_or(AuthRejection("malformed agent credential"))?;
    if mac.len() != 64 || !mac.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(AuthRejection("malformed agent credential"));
    }
    crate::role_revision::CutexSessionId::new(session.to_string())
        .map_err(|_| AuthRejection("malformed agent credential"))?;
    let runtime = headers
        .get("x-cutex-agent-id")
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .or(fallback_runtime)
        .ok_or(AuthRejection("agent credential requires x-cutex-agent-id"))?;
    crate::role_revision::RuntimeAgentId::new(runtime.to_string())
        .map_err(|_| AuthRejection("invalid x-cutex-agent-id"))?;
    if !constant_time_eq(mac.as_bytes(), mac_hex(root, runtime, session).as_bytes()) {
        return Err(AuthRejection("agent credential does not match this agent"));
    }
    Ok(BusCaller::Agent(AgentPrincipal {
        runtime_agent_id: runtime.to_string(),
        cutex_session_id: session.to_string(),
    }))
}

/// Routes an agent credential may call: the MCP adapter's routes, each with
/// the MCP caller fence headers (so the bus applies its current-runtime
/// checks), plus the unfenced toolset lookup.
pub fn agent_route_allowed(method: &str, path: &str, headers: &HashMap<String, String>) -> bool {
    use crate::agent_bus::mcp::{GENERATION_HEADER, THREAD_HEADER};
    if method == "POST" && path == TOOLSET_PATH {
        return true;
    }
    let fenced = headers.contains_key(THREAD_HEADER) && headers.contains_key(GENERATION_HEADER);
    let expected_method = if path == MCP_AGENT_LIST_PATH { "GET" } else { "POST" };
    fenced && method == expected_method && MCP_ROUTE_PATHS.contains(&path)
}

/// An Agent Bus request together with its authenticated caller. Derefs to the
/// raw request so existing route code keeps reading headers and body.
#[derive(Debug)]
pub struct BusRequest {
    http: SimpleHttpRequest,
    caller: BusCaller,
}

impl BusRequest {
    pub fn authenticate(http: SimpleHttpRequest, root: Option<&str>) -> Result<Self, AuthRejection> {
        let caller = authenticate_with_runtime_fallback(
            &http.headers,
            root,
            job_identity_lookup_runtime(&http.method, &http.path),
        )?;
        Ok(Self { http, caller })
    }

    /// Test/in-process construction with an explicit caller.
    pub fn with_caller(http: SimpleHttpRequest, caller: BusCaller) -> Self {
        Self { http, caller }
    }

    pub fn caller(&self) -> &BusCaller {
        &self.caller
    }

    /// The verified agent behind a per-agent credential, if any.
    pub fn agent(&self) -> Option<&AgentPrincipal> {
        match &self.caller {
            BusCaller::Agent(principal) => Some(principal),
            _ => None,
        }
    }

    /// Whether an agent credential may use this route at all.
    pub fn agent_route_permitted(&self) -> bool {
        self.agent().is_none()
            || agent_route_allowed(&self.http.method, &self.http.path, &self.http.headers)
            || self.job_identity_lookup().is_some()
    }

    /// The caller of a Job Service adapter identity lookup: an agent
    /// credential asking about its own runtime id.
    pub fn job_identity_lookup(&self) -> Option<&AgentPrincipal> {
        let principal = self.agent()?;
        (job_identity_lookup_runtime(&self.http.method, &self.http.path)
            == Some(principal.runtime_agent_id.as_str()))
        .then_some(principal)
    }

    /// The token existing route checks compare against: the presented agent
    /// credential (already verified) for agent callers, else the root token.
    pub fn route_token<'a>(&'a self, root: Option<&'a str>) -> Option<&'a str> {
        match &self.caller {
            BusCaller::Agent(_) => bearer(&self.http.headers),
            _ => root,
        }
    }
}

impl Deref for BusRequest {
    type Target = SimpleHttpRequest;

    fn deref(&self) -> &SimpleHttpRequest {
        &self.http
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "cutex.00000000-0000-0000-0000-000000000001";
    const OTHER_SESSION: &str = "cutex.00000000-0000-0000-0000-000000000002";

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Keys longer than one block are hashed first (RFC 4231 case 6).
        let mac = hmac_sha256(
            &[0xaa; 131],
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"],
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn credential_binds_runtime_session_and_root() {
        let credential = agent_credential("root", "stock.a", SESSION);
        assert!(credential.starts_with(AGENT_CREDENTIAL_PREFIX));
        assert!(credential.ends_with(SESSION));
        assert!(!credential.contains("root"));
        assert_ne!(credential, agent_credential("root", "stock.b", SESSION));
        assert_ne!(credential, agent_credential("root", "stock.a", OTHER_SESSION));
        assert_ne!(credential, agent_credential("other-root", "stock.a", SESSION));
        let bearer = format!("Bearer {credential}");
        assert_eq!(
            authenticate(&headers(&[("authorization", &bearer), ("x-cutex-agent-id", "stock.a")]), Some("root")),
            Ok(BusCaller::Agent(AgentPrincipal {
                runtime_agent_id: "stock.a".into(),
                cutex_session_id: SESSION.into(),
            }))
        );
        // The same credential claiming another runtime (e.g. the Director's)
        // is refused: the header is authenticated, not self-declared.
        assert!(authenticate(
            &headers(&[("authorization", &bearer), ("x-cutex-agent-id", "stock.director")]),
            Some("root"),
        )
        .is_err());
        // Swapping the embedded session breaks the MAC.
        let swapped = format!("Bearer {}", credential.replace(SESSION, OTHER_SESSION));
        assert!(authenticate(
            &headers(&[("authorization", &swapped), ("x-cutex-agent-id", "stock.a")]),
            Some("root"),
        )
        .is_err());
        assert!(authenticate(&headers(&[("authorization", &bearer)]), Some("root")).is_err());
        // Under another root nothing verifies.
        assert!(authenticate(
            &headers(&[("authorization", &bearer), ("x-cutex-agent-id", "stock.a")]),
            Some("rotated-root"),
        )
        .is_err());
    }

    #[test]
    fn root_and_other_tokens_keep_their_contract() {
        let root = headers(&[("authorization", "Bearer root"), ("x-cutex-agent-id", "anyone")]);
        assert_eq!(authenticate(&root, Some("root")), Ok(BusCaller::Root));
        let wrong = headers(&[("authorization", "Bearer guess")]);
        assert_eq!(authenticate(&wrong, Some("root")), Ok(BusCaller::Unauthenticated));
        assert_eq!(authenticate(&headers(&[]), Some("root")), Ok(BusCaller::Unauthenticated));
        let agentish = headers(&[("authorization", "Bearer cxa1.zz"), ("x-cutex-agent-id", "a")]);
        assert!(authenticate(&agentish, Some("root")).is_err());
        // Without a configured root nothing is verified (open bus).
        assert_eq!(authenticate(&agentish, None), Ok(BusCaller::Unauthenticated));
    }

    #[test]
    fn agent_routes_are_the_fenced_mcp_set() {
        let fenced = headers(&[
            (crate::agent_bus::mcp::THREAD_HEADER, "t"),
            (crate::agent_bus::mcp::GENERATION_HEADER, "1"),
        ]);
        let bare = headers(&[]);
        assert!(agent_route_allowed("POST", "/api/asks/v1", &fenced));
        assert!(agent_route_allowed("POST", crate::agent_bus::web_search::PATH, &fenced));
        assert!(!agent_route_allowed("POST", crate::agent_bus::web_search::PATH, &bare));
        assert!(!agent_route_allowed("POST", "/api/asks/v1", &bare));
        assert!(!agent_route_allowed("GET", "/api/asks/v1", &fenced));
        assert!(agent_route_allowed("GET", MCP_AGENT_LIST_PATH, &fenced));
        assert!(!agent_route_allowed("POST", MCP_AGENT_LIST_PATH, &fenced));
        assert!(!agent_route_allowed("GET", "/api/agents?all_groups=true", &fenced));
        assert!(agent_route_allowed("POST", TOOLSET_PATH, &bare));
        for root_only in [
            "/api/agents/register",
            "/api/agents/heartbeat",
            "/api/agents/unregister",
            "/api/agents/groups",
            "/api/messages/redrive",
            "/api/messages/ack",
            "/api/federation/messages/send",
            "/api/rotation/v1/release",
            // Served only the removed `send` tool.
            "/api/messages/send",
        ] {
            assert!(!agent_route_allowed("POST", root_only, &fenced), "{root_only}");
        }
        assert!(!agent_route_allowed("GET", "/api/messages/poll?agent_id=x", &fenced));
        assert!(!agent_route_allowed("GET", "/", &fenced));
    }

    #[test]
    fn browser_live_fence_is_only_the_exact_fenced_post() {
        let fenced = headers(&[
            (crate::agent_bus::mcp::THREAD_HEADER, "thread"),
            (crate::agent_bus::mcp::GENERATION_HEADER, "1"),
        ]);
        assert!(agent_route_allowed("POST", BROWSER_FENCE_PATH, &fenced));
        assert!(!agent_route_allowed("GET", BROWSER_FENCE_PATH, &fenced));
        assert!(!agent_route_allowed("POST", BROWSER_FENCE_PATH, &headers(&[])));
        assert!(!agent_route_allowed("POST", &format!("{BROWSER_FENCE_PATH}?session=other"), &fenced));
        assert!(!agent_route_allowed("POST", "/v2/browser/devices/register", &fenced));
    }

    #[test]
    fn bus_request_routes_compare_against_the_presented_agent_credential() {
        let credential = agent_credential("root", "stock.a", SESSION);
        let http = SimpleHttpRequest {
            method: "POST".into(),
            path: "/api/messages/send".into(),
            headers: headers(&[
                ("authorization", &format!("Bearer {credential}")),
                ("x-cutex-agent-id", "stock.a"),
            ]),
            body: Vec::new(),
        };
        let request = BusRequest::authenticate(http, Some("root")).unwrap();
        assert_eq!(request.agent().unwrap().cutex_session_id, SESSION);
        assert_eq!(request.route_token(Some("root")), Some(credential.as_str()));
        // Without the fence headers an agent may not use the route.
        assert!(!request.agent_route_permitted());
        assert!(crate::http::server::require_service_bridge_token(
            &request,
            request.route_token(Some("root")),
            "Agent Bus"
        )
        .is_ok());
    }

    fn job_lookup(runtime_in_path: &str, extra: &[(&str, &str)], credential: &str) -> SimpleHttpRequest {
        let bearer = format!("Bearer {credential}");
        let mut pairs = vec![("authorization", bearer.as_str())];
        pairs.extend_from_slice(extra);
        SimpleHttpRequest {
            method: "GET".into(),
            path: format!("/api/agents?agent_id={runtime_in_path}&all_hosts=false"),
            headers: headers(&pairs),
            body: Vec::new(),
        }
    }

    #[test]
    fn job_identity_lookup_shape_is_exact() {
        let ok = "/api/agents?agent_id=stock.3a71b74b-253f-4701-b99b-6a7e6a29af28&all_hosts=false";
        assert_eq!(
            job_identity_lookup_runtime("GET", ok),
            Some("stock.3a71b74b-253f-4701-b99b-6a7e6a29af28")
        );
        assert_eq!(job_identity_lookup_runtime("POST", ok), None);
        for path in [
            "/api/agents?agent_id=stock.a",
            "/api/agents?agent_id=stock.a&all_hosts=true",
            "/api/agents?agent_id=stock.a&all_hosts=false&all_groups=true",
            "/api/agents?all_groups=true&agent_id=stock.a&all_hosts=false",
            "/api/agents?agent_id=&all_hosts=false",
            "/api/agents?agent_id=stock.a%26all_groups%3Dtrue&all_hosts=false",
            "/api/agents?agent_id=stock.a&all_groups=true&all_hosts=false",
            MCP_AGENT_LIST_PATH,
        ] {
            assert_eq!(job_identity_lookup_runtime("GET", path), None, "{path}");
        }
    }

    #[test]
    fn job_adapter_lookup_authenticates_from_the_query_runtime() {
        // The deployed Job adapter sends only Authorization (no
        // x-cutex-agent-id); the runtime id in the query is MAC-checked.
        let credential = agent_credential("root", "stock.a", SESSION);
        let request =
            BusRequest::authenticate(job_lookup("stock.a", &[], &credential), Some("root")).unwrap();
        let principal = AgentPrincipal {
            runtime_agent_id: "stock.a".into(),
            cutex_session_id: SESSION.into(),
        };
        assert_eq!(request.agent(), Some(&principal));
        assert!(request.agent_route_permitted());
        assert_eq!(request.job_identity_lookup(), Some(&principal));
        assert!(crate::http::server::require_service_bridge_token(
            &request,
            request.route_token(Some("root")),
            "Agent Bus"
        )
        .is_ok());

        // Asking about another runtime with this credential fails the MAC.
        assert!(
            BusRequest::authenticate(job_lookup("stock.director", &[], &credential), Some("root"))
                .is_err()
        );
        // A header naming this runtime cannot widen the lookup to another one.
        let other = BusRequest::authenticate(
            job_lookup("stock.director", &[("x-cutex-agent-id", "stock.a")], &credential),
            Some("root"),
        )
        .unwrap();
        assert_eq!(other.job_identity_lookup(), None);
        assert!(!other.agent_route_permitted());
        // The fallback applies to the lookup shape only.
        let mut post = job_lookup("stock.a", &[], &credential);
        post.method = "POST".into();
        assert!(BusRequest::authenticate(post, Some("root")).is_err());
        let mut wider = job_lookup("stock.a", &[], &credential);
        wider.path = "/api/agents?agent_id=stock.a".into();
        assert!(BusRequest::authenticate(wider, Some("root")).is_err());
        // Root callers keep the old contract (no job-lookup narrowing).
        let root = BusRequest::authenticate(job_lookup("stock.a", &[], "root"), Some("root")).unwrap();
        assert_eq!(root.caller(), &BusCaller::Root);
        assert_eq!(root.job_identity_lookup(), None);
    }

    #[test]
    fn job_identity_view_is_only_the_callers_current_entry() {
        let agent = |id: &str, session: Option<&str>| -> crate::agent_bus::model::AgentBusAgent {
            serde_json::from_value(serde_json::json!({
                "id": id, "name": id, "session_id": "thread", "cutex_session_id": session,
                "profile": "p", "cwd": "/", "pid": 1, "last_seen_epoch_secs": 0
            }))
            .unwrap()
        };
        let principal = AgentPrincipal {
            runtime_agent_id: "stock.a".into(),
            cutex_session_id: SESSION.into(),
        };
        let view = job_identity_view(
            vec![
                agent("stock.b", Some(OTHER_SESSION)),
                agent("stock.a", Some(SESSION)),
                agent("stock.c", Some(SESSION)),
            ],
            &principal,
        );
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, "stock.a");
        // A stale or unprojected occurrence yields nothing.
        assert!(job_identity_view(vec![agent("stock.a", None)], &principal).is_empty());
        assert!(job_identity_view(vec![agent("stock.a", Some(OTHER_SESSION))], &principal).is_empty());
    }

    #[test]
    fn launch_credential_is_per_agent() {
        let mut config = CodezConfig {
            agent_bus_token: Some("root-secret".into()),
            ..CodezConfig::default()
        };
        if !crate::config::agent_security::debug_shared_bus_token() {
            let credential = launch_credential(&config, "stock.a", SESSION).unwrap();
            assert_eq!(credential, agent_credential("root-secret", "stock.a", SESSION));
            assert!(!credential.contains("root-secret"));
        }
        config.agent_bus_token = Some("  ".into());
        assert!(launch_credential(&config, "stock.a", SESSION).is_none());
    }
}
