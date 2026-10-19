//! Management v2 HTTP server loop and app-layer callback boundary.

use std::net::{IpAddr, TcpStream};
use std::sync::OnceLock;

use crate::http::server::read_next_http_request;
use crate::http::server::{
    keep_alive, set_extra_response_headers, set_keep_alive, KeepAlive, SimpleHttpRequest,
};
use crate::management::listeners::{ConnectionInfo, ConnectionSlot, ListenSpec, KEEP_ALIVE_IDLE};

use crate::app_server::manager::AppServerManagedRuntimeStatus;
use crate::http::server::read_simple_http_request_with_body_limit;
use crate::http::server::write_http_response;
use crate::http::server::HttpRequestBodyTooLarge;
use crate::im::registry::ImRegistry;
use crate::management::v2::session::MAX_REQUEST_BYTES;
use crate::management::v2::user_input::UserInputExecutionError;
use crate::management::v2::user_input::UserInputSubmitCommand;
use crate::management::v2::user_input::UserInputSubmitExecution;


pub type ManagementHandoffRegistrar = fn(IpAddr, u16);
pub type ManagementHumanCreateAgentHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
    &crate::management::v2::human_agents::HumanCreateAgentRequest,
) -> Result<String, UserInputExecutionError>;
pub type ManagementHumanCreateSessionHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
    &crate::management::v2::human_sessions::HumanCreateSessionRequest,
) -> Result<String, UserInputExecutionError>;
pub type ManagementRegistryLoader = fn() -> anyhow::Result<ImRegistry>;
pub type ManagementRuntimeStatusLoader =
    fn(&str) -> anyhow::Result<Option<AppServerManagedRuntimeStatus>>;
pub type ManagementNativeRequestHandler =
    fn(&str, serde_json::Value) -> Result<serde_json::Value, ManagementNativeForwardError>;
pub type ManagementNativeServerResponseHandler =
    fn(&str, u64, serde_json::Value) -> Result<(), ManagementNativeForwardError>;
pub type ManagementUserInputHandler =
    fn(UserInputSubmitCommand) -> Result<UserInputSubmitExecution, UserInputExecutionError>;
/// (cutexSessionId, maxItems, ignorePause) -> items sent.
pub type ManagementUserInputQueueFlusher = fn(&str, usize, bool) -> anyhow::Result<usize>;
/// `cutex/turn/stop` backend: (cutexSessionId, threadId) -> interrupted turn id
/// (None: no turn was running). Interrupts and waits for the turn to stop.
pub type ManagementTurnInterruptHandler =
    fn(&str, &str) -> Result<Option<String>, UserInputExecutionError>;
/// `cutex/session/history/revert` backend: (cutexSessionId, beforeTurnId,
/// rewindFiles) -> result.
pub type ManagementHistoryRevertHandler =
    fn(&str, &str, bool) -> Result<serde_json::Value, UserInputExecutionError>;
/// `cutex/session/delete` backend (also the delete-empty step of
/// `cutex/session/retire`): (cutexSessionId, request) -> delete receipt.
pub type ManagementSessionDeleteHandler = fn(
    &str,
    &crate::management::v2::session_delete::SessionDeleteRequest,
) -> Result<serde_json::Value, UserInputExecutionError>;
/// `cutex/session/fork` backend: (owner, source cutexSessionId, request) ->
/// fork facts including `newCutexSessionId`.
pub type ManagementSessionForkHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
    &str,
    &crate::management::v2::session_fork::SessionForkRequest,
) -> Result<serde_json::Value, UserInputExecutionError>;
/// Spawn a read-only, initialized native app-server on an offline session's
/// own native home; returns it with that home (`GET /v2/sessions/{id}/history`).
pub type OfflineHistoryEndpointOpener = fn(
    &str,
) -> anyhow::Result<(
    Box<dyn crate::catalog::CatalogEndpoint>,
    std::path::PathBuf,
)>;
pub type ManagementBootstrapStateLoader = fn(&str, u64) -> anyhow::Result<serde_json::Value>;
/// A cutex session's `cutexState.latestPlan` (`launch::native_plan`); `null`
/// when it has none or it cannot be derived (Claude sessions, other hosts).
pub type ManagementLatestPlanLoader = fn(&str) -> serde_json::Value;
/// A cutex session's `cutexState.latestTokenUsage`
/// (`launch::native_token_usage`); `null` when it has none or it cannot be
/// derived (Claude sessions, other hosts).
pub type ManagementLatestTokenUsageLoader = fn(&str) -> serde_json::Value;
pub type ManagementSessionMutationHandler =
    fn(&str, &str, serde_json::Value) -> Result<serde_json::Value, UserInputExecutionError>;
pub type ManagementProjectAuthorityHandler =
    fn(crate::agent_management::ProjectAuthorityRequest) -> serde_json::Value;
pub type ManagementLegacyDirectorOwnershipImportHandler =
    fn(crate::agent_management::LegacyDirectorOwnershipImportRequest) -> serde_json::Value;
pub type ManagementAgentReservationReconciliationHandler =
    fn(crate::agent_management::AgentReservationReconciliationRequest) -> serde_json::Value;
pub type HumanManagementProjectCollectionHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
) -> Result<
    crate::management::control_plane::HumanManagementProjectCollection,
    crate::agent_management::AgentManagementError,
>;
pub type HumanManagementProjectReadHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
    &crate::agent_management::ProjectId,
) -> Result<
    crate::management::control_plane::HumanManagementProjectWorkspace,
    crate::agent_management::AgentManagementError,
>;
pub type HumanManagementPresentationUpdateHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
    &crate::management::control_plane::HumanManagementPresentationUpdateRequest,
) -> Result<
    crate::agent_management::ProjectPresentationSettings,
    crate::agent_management::AgentManagementError,
>;
pub type HumanManagementOperatorActionHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
    &crate::management::control_plane::HumanManagementOperatorActionRequest,
) -> Result<
    crate::management::control_plane::HumanManagementOperatorReceipt,
    crate::agent_management::AgentManagementError,
>;
pub type HumanManagementProjectMutationHandler = fn(
    &crate::management::control_plane::HumanManagementPrincipal,
    &crate::management::control_plane::HumanManagementProjectMutationRequest,
) -> Result<
    crate::management::control_plane::HumanManagementProjectMutationReceipt,
    crate::agent_management::AgentManagementError,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagementNativeForwardError {
    BeforeForward(String),
    NativeRequestIdInUse(String),
    OutcomeUnknown(String),
    StaleRuntimeGeneration { expected: u64, actual: Option<u64> },
}

#[derive(Clone, Copy)]
pub struct ManagementRequestContext {
    pub explicit_launch_action:
        fn(
            &crate::management::control_plane::HumanManagementPrincipal,
            &crate::agent_management::ExplicitLaunchRequest,
        ) -> Result<serde_json::Value, crate::agent_management::AgentManagementError>,
    pub adopt_saved_native: fn(
        &crate::management::control_plane::HumanManagementPrincipal,
        &crate::agent_management::HumanAdoptRequest,
    ) -> Result<
        crate::agent_management::HumanAdoptResult,
        crate::agent_management::AgentManagementError,
    >,
    pub review_agent_archive: fn(
        &crate::management::control_plane::HumanManagementPrincipal,
        &crate::agent_management::AgentArchiveReviewRequest,
    ) -> Result<
        crate::agent_management::AgentArchiveReview,
        crate::agent_management::AgentManagementError,
    >,
    pub execute_agent_archive: fn(
        &crate::management::control_plane::HumanManagementPrincipal,
        &crate::agent_management::AgentArchiveRequest,
    ) -> Result<
        crate::agent_management::AgentArchiveReceipt,
        crate::agent_management::AgentManagementError,
    >,
    pub durable_agent_candidates: fn(
        &crate::management::control_plane::HumanManagementPrincipal,
    ) -> Result<
        Vec<crate::agent_management::DurableAgentCandidate>,
        crate::agent_management::AgentManagementError,
    >,
    pub import_durable_agent: fn(
        &crate::management::control_plane::HumanManagementPrincipal,
        &crate::agent_management::DurableImportRequest,
    ) -> Result<
        crate::agent_management::DurableImportReceipt,
        crate::agent_management::AgentManagementError,
    >,
    pub load_registry: ManagementRegistryLoader,
    pub load_runtime_status: ManagementRuntimeStatusLoader,
    pub forward_native_request: ManagementNativeRequestHandler,
    pub respond_native_server_request: ManagementNativeServerResponseHandler,
    pub handle_user_input: ManagementUserInputHandler,
    pub flush_user_input_queue: ManagementUserInputQueueFlusher,
    pub steer_user_input_queue: crate::management::v2::queue_order::QueueSteerHandler,
    pub interrupt_active_turn: ManagementTurnInterruptHandler,
    pub load_bootstrap_state: ManagementBootstrapStateLoader,
    pub load_latest_plan: ManagementLatestPlanLoader,
    pub load_latest_token_usage: ManagementLatestTokenUsageLoader,
    pub revert_session_history: ManagementHistoryRevertHandler,
    pub delete_session: ManagementSessionDeleteHandler,
    pub fork_session: ManagementSessionForkHandler,
    pub side_chat: crate::management::v2::side_chats::SideChatHandler,
    pub open_offline_history: OfflineHistoryEndpointOpener,
    pub mutate_session: ManagementSessionMutationHandler,
    pub online_session: fn(
        Option<&crate::management::control_plane::HumanManagementPrincipal>,
        &str,
        serde_json::Value,
    ) -> Result<serde_json::Value, UserInputExecutionError>,
    pub bind_project_authority: ManagementProjectAuthorityHandler,
    pub import_legacy_director_ownership: ManagementLegacyDirectorOwnershipImportHandler,
    pub reconcile_agent_reservation: ManagementAgentReservationReconciliationHandler,
    pub list_management_projects: HumanManagementProjectCollectionHandler,
    pub read_management_project: HumanManagementProjectReadHandler,
    pub update_management_project_presentation: HumanManagementPresentationUpdateHandler,
    pub execute_management_operator_action: HumanManagementOperatorActionHandler,
    pub execute_management_project_mutation: HumanManagementProjectMutationHandler,
    /// Root-only `POST /v2/human/agents`; returns the new cutex session ID.
    pub create_human_agent: ManagementHumanCreateAgentHandler,
    /// Root-only `POST /v2/human/sessions`; returns the new cutex session ID.
    pub create_human_session: ManagementHumanCreateSessionHandler,
}

/// Credentials and callbacks shared by every Management connection thread.
struct ServerShared {
    token: Option<String>,
    agent_management_admin_token: Option<String>,
    context: ManagementRequestContext,
}

static SERVER: OnceLock<ServerShared> = OnceLock::new();

/// Single-address entry point (kept for callers that bind one address).
#[allow(clippy::too_many_arguments)]
pub fn run_management_server(
    bind_addr: IpAddr,
    port: u16,
    token: Option<String>,
    agent_management_admin_token: Option<String>,
    register_handoff: ManagementHandoffRegistrar,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    run_management_server_on(
        &[ListenSpec::Socket(std::net::SocketAddr::new(bind_addr, port))],
        token,
        agent_management_admin_token,
        register_handoff,
        context,
    )
}

/// Serve the Management API on every listen spec (loopback first; remote
/// listeners need a root credential, see `management::listeners`).
#[allow(clippy::too_many_arguments)]
pub fn run_management_server_on(
    specs: &[ListenSpec],
    token: Option<String>,
    agent_management_admin_token: Option<String>,
    register_handoff: ManagementHandoffRegistrar,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    if specs.is_empty() {
        anyhow::bail!("no management listen address configured");
    }
    let allow_remote = agent_management_admin_token
        .as_deref()
        .is_some_and(|root| !root.trim().is_empty());
    if SERVER
        .set(ServerShared {
            token,
            agent_management_admin_token,
            context,
        })
        .is_err()
    {
        anyhow::bail!("the management server is already running in this process");
    }
    // Create the persisted instance id before the first client asks for it.
    let _ = crate::management::instance::instance_id();
    // P1 users: run the one-time migration (every existing client / device
    // token → owner) before the first request, so its moment is well defined.
    if let Err(error) = crate::management::users::UserStore::open_default()
        .and_then(|users| users.snapshot())
    {
        eprintln!("users store unavailable (client tokens act as pending): {error:#}");
    }
    if let Err(error) = crate::management::v2::summary::start(context) {
        eprintln!("host summary unavailable: {error:#}");
    }
    // Loopback listeners first so a slow Tailscale never delays them.
    let mut ordered: Vec<ListenSpec> = specs.iter().filter(|s| s.is_loopback()).cloned().collect();
    ordered.extend(specs.iter().filter(|s| !s.is_loopback()).cloned());
    let handles = crate::management::listeners::start_listeners(
        &ordered,
        allow_remote,
        serve_management_connection,
    )?;
    if let Some(ListenSpec::Socket(handoff)) = ordered.first() {
        let handoff = *handoff;
        std::thread::spawn(move || register_handoff(handoff.ip(), handoff.port()));
    }
    for handle in handles {
        let _ = handle.join();
    }
    // Accept loops never return; this is reached only if every bind was skipped.
    anyhow::bail!("no management listener could be started")
}

/// One connection: requests until the client closes, a response that cannot
/// be kept alive, or 30 s idle.
fn serve_management_connection(mut stream: TcpStream, info: ConnectionInfo, slot: ConnectionSlot) {
    let Some(shared) = SERVER.get() else {
        return;
    };
    let peer = info.peer;
    crate::management::listeners::set_current_connection(Some(info.clone()));
    let mut slot = Some(slot);
    let mut buffer = Vec::new();
    let mut first = true;
    loop {
        if crate::services::proxy::intercept(&mut stream, &mut buffer, &info, &mut slot, first) { break; }
        let idle = if first { None } else { Some(KEEP_ALIVE_IDLE) };
        first = false;
        let request = match read_next_http_request(&mut stream, &mut buffer, MAX_REQUEST_BYTES, idle) {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(error) => {
                set_keep_alive(KeepAlive::Off);
                if let Some(too_large) = error.downcast_ref::<HttpRequestBodyTooLarge>() {
                    if too_large.path == "/v2" || too_large.path.starts_with("/v2/") {
                        let _ = crate::management::v2::server::write_payload_too_large(
                            &mut stream,
                            too_large.content_length,
                            too_large.limit,
                        );
                        break;
                    }
                }
                let _ = write_http_response(
                    &mut stream,
                    500,
                    "Internal Server Error",
                    "text/plain",
                    format!("{error:#}").as_bytes(),
                );
                break;
            }
        };
        let wants_close = request
            .headers
            .get("connection")
            .is_some_and(|value| value.to_ascii_lowercase().contains("close"));
        set_keep_alive(if wants_close { KeepAlive::Off } else { KeepAlive::Requested });
        set_extra_response_headers(crate::management::listeners::cors_headers(
            request.headers.get("origin").map(String::as_str),
        ));
        crate::http::server::set_accept_gzip(crate::http::server::accepts_gzip(
            request.headers.get("accept-encoding").map(String::as_str),
        ));
        crate::management::listeners::set_current_client_token(None);
        crate::management::users::set_current_principal(None);
        let result = handle_parsed_management_request(
            &mut stream,
            request,
            shared.token.as_deref(),
            shared.agent_management_admin_token.as_deref(),
            shared.context,
            Some(peer),
        );
        if let Err(error) = result {
            set_keep_alive(KeepAlive::Off);
            let _ = write_http_response(
                &mut stream,
                500,
                "Internal Server Error",
                "text/plain",
                format!("{error:#}").as_bytes(),
            );
            break;
        }
        if keep_alive() != KeepAlive::Kept {
            break;
        }
    }
    set_keep_alive(KeepAlive::Off);
    set_extra_response_headers(String::new());
    crate::http::server::set_accept_gzip(false);
    crate::management::listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    crate::management::listeners::set_current_connection(None);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

pub fn handle_management_request(
    stream: &mut TcpStream,
    token: Option<&str>,
    agent_management_admin_token: Option<&str>,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let request = match read_simple_http_request_with_body_limit(stream, MAX_REQUEST_BYTES) {
        Ok(request) => request,
        Err(error) => {
            if let Some(too_large) = error.downcast_ref::<HttpRequestBodyTooLarge>() {
                if too_large.path == "/v2" || too_large.path.starts_with("/v2/") {
                    return crate::management::v2::server::write_payload_too_large(
                        stream,
                        too_large.content_length,
                        too_large.limit,
                    );
                }
            }
            return Err(error);
        }
    };
    let peer = stream.peer_addr().ok();
    handle_parsed_management_request(
        stream,
        request,
        token,
        agent_management_admin_token,
        context,
        peer,
    )
}

/// `OPTIONS` preflight: 204, with the CORS preflight headers when the
/// origin is allowed (none otherwise, so the browser blocks the request).
pub(crate) fn write_cors_preflight(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
) -> anyhow::Result<()> {
    let origin = request.headers.get("origin").map(String::as_str);
    set_extra_response_headers(crate::management::listeners::cors_preflight_headers(origin));
    write_http_response(stream, 204, "No Content", "text/plain", b"")
}

fn bearer_present(request: &SimpleHttpRequest) -> bool {
    request
        .headers
        .get("authorization")
        .is_some_and(|value| !value.trim().is_empty())
}

/// Listener-level gate, before any route runs. `Some((status, reason, code,
/// message))` refuses the request. Remote listeners accept only client /
/// device tokens (plus the public pairing route and `GET /`).
pub(crate) fn remote_gate(
    remote: bool,
    request: &SimpleHttpRequest,
    path: &str,
) -> Option<(u16, &'static str, &'static str, &'static str)> {
    if path.starts_with("/peer/v1/") {
        return if is_public_route(&request.method, path) || crate::peers::token::bearer(request).is_some_and(crate::peers::token::valid) { None } else { Some((401, "Unauthorized", "peer_credential_required", "a peer credential is required")) };
    }
    if crate::peers::token::bearer(request).is_some_and(|v|v.starts_with(crate::peers::token::PREFIX)) {
        return Some((401, "Unauthorized", "remote_credential_refused", "peer credentials cannot access Management routes"));
    }
    if !remote || is_public_route(&request.method, path) {
        return None;
    }
    if crate::management::client_tokens::presents_client_token(request) {
        return None;
    }
    Some(if bearer_present(request) {
        (
            401,
            "Unauthorized",
            "remote_credential_refused",
            "only Management client or device tokens are accepted on this listener",
        )
    } else {
        (401, "Unauthorized", "unauthorized", "a Management client or device token is required")
    })
}

fn is_public_route(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        ("POST", crate::management::v2::pairing::PAIR_PATH) | ("POST", "/peer/v1/pair") | ("GET", "/")
    )
}

#[allow(clippy::too_many_arguments)]
pub fn handle_parsed_management_request(
    stream: &mut TcpStream,
    mut request: SimpleHttpRequest,
    token: Option<&str>,
    agent_management_admin_token: Option<&str>,
    context: ManagementRequestContext,
    peer: Option<std::net::SocketAddr>,
) -> anyhow::Result<()> {
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or(request.path.as_str())
        .to_string();
    // CORS preflight: never authenticated.
    if request.method == "OPTIONS" {
        return write_cors_preflight(stream, &request);
    }
    let remote = !crate::management::listeners::current_request_is_loopback();
    if let Some((status, reason, code, message)) = remote_gate(remote, &request, &path) {
        if path.starts_with("/peer/v1/") { if let Ok(store)=crate::peers::store::Store::open() { crate::peers::audit::record(&store,"inbound",None,&path,&Err(crate::peers::Error::new(code,status)),request.body.len(),None); } }
        return crate::management::v2::server::write_v2_error(
            stream, status, reason, code, message, false, serde_json::json!({}),
        );
    }
    // A verified Management client token (remote clients) stands for the
    if path.starts_with("/peer/v1/") { return crate::peers::routes::handle(stream, &request); }
    // root credential from here on; see `management::client_tokens`.
    let verified = if crate::management::client_tokens::presents_client_token(&request) {
        match crate::management::client_tokens::ClientTokenStore::open_default() {
            Ok(store) => {
                let peer_ip = peer.map(|peer| peer.ip().to_string());
                crate::management::client_tokens::authenticate_management_request_from(
                    &mut request,
                    agent_management_admin_token,
                    &store,
                    peer_ip.as_deref(),
                )
            }
            Err(_) => crate::management::client_tokens::authenticate_management_request(
                &mut request,
                agent_management_admin_token,
            ),
        }
    } else {
        None
    };
    if remote && !is_public_route(&request.method, &path) && verified.is_none() {
        return crate::management::v2::server::write_v2_error(
            stream, 401, "Unauthorized", "unauthorized",
            "valid cutex management authentication is required", false, serde_json::json!({}),
        );
    }
    let principal = verified.as_ref().map(|token| token.principal.clone());
    let device_scope = verified.as_ref().map(|token| token.scope);
    crate::management::listeners::set_current_client_token(verified);
    crate::management::users::set_current_principal(principal.clone());
    if path == "/v2" || path.starts_with("/v2/") {
        // owner-device-notice: owner devices the CLI added (or any binding
        // not announced yet) are announced by the daemon's next request.
        if let Ok(users) = crate::management::users::UserStore::open_default() {
            crate::management::users::announce_owner_devices(&users);
        }
        // device-scope: a narrowed token passes the scope gate first
        // (effective rights = the user's rights ∩ the device scope).
        if let Some(scope) = device_scope.filter(|scope| !scope.is_full()) {
            match crate::management::v2::access::device_scope_gate(stream, &request, scope) {
                crate::management::v2::access::Gate::Handled(result) => return result,
                crate::management::v2::access::Gate::Continue => {}
            }
        }
    }
    // P1 users: a non-owner (or pending) token passes the access gate first;
    // every route not explicitly scoped for it is refused (default deny).
    if let Some(principal) = principal.filter(|principal| !principal.is_owner()) {
        if path == "/v2" || path.starts_with("/v2/") {
            match crate::management::v2::access::gate(stream, &request, &principal, context) {
                crate::management::v2::access::Gate::Handled(result) => return result,
                crate::management::v2::access::Gate::Continue => {}
            }
        }
    }
    if path == crate::management::v2::pairing::PAIR_PATH {
        return crate::management::v2::pairing::handle_pair(stream, &request);
    }
    if path == crate::management::v2::pairing::PAIRING_CODES_PATH {
        if remote {
            return crate::management::v2::server::write_v2_error(
                stream, 403, "Forbidden", "loopback_only",
                "pairing codes can only be created on the loopback listener", false,
                serde_json::json!({}),
            );
        }
        if !crate::management::v2::server::authenticated_root_request(
            &request,
            agent_management_admin_token,
        ) {
            return crate::management::v2::server::write_v2_error(
                stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential or a client token is required", false,
                serde_json::json!({}),
            );
        }
        // owner-local-only: creating a pairing code hands out access; only
        // the host itself or SSH (a paired device's token on loopback: no).
        if !crate::management::users::request_is_owner_local() {
            let error = crate::management::users::owner_local_only("pairingCode");
            return crate::management::v2::server::write_v2_error(
                stream, 403, "Forbidden", error.code, &error.message, false, error.details,
            );
        }
        return crate::management::v2::pairing::handle_issue(stream, &request);
    }
    if path == "/v2" || path.starts_with("/v2/") {
        return crate::management::v2::server::handle_v2_request(
            stream,
            &request,
            token,
            agent_management_admin_token,
            context,
        );
    }
    match (request.method.as_str(), path.as_str()) {
        ("GET", "/") => write_http_response(stream, 200, "OK", "text/plain", b"ok"),
        _ => write_http_response(stream, 404, "Not Found", "text/plain", b"not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn request(method: &str, path: &str, bearer: Option<&str>) -> SimpleHttpRequest {
        let mut headers = HashMap::new();
        if let Some(bearer) = bearer {
            headers.insert("authorization".to_string(), format!("Bearer {bearer}"));
        }
        SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            headers,
            body: Vec::new(),
        }
    }

    /// Serve one request with `serve` on a real socket (keep-alive requested)
    /// and return the raw response plus the keep-alive state afterwards.
    fn exchange(raw: &str, serve: fn(&mut TcpStream, &SimpleHttpRequest)) -> (String, KeepAlive) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let raw = raw.to_string();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut response = Vec::new();
            let mut chunk = [0_u8; 4096];
            // The connection stays open (keep-alive): read until the
            // headers and the (empty or short) body have arrived.
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&response);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            String::from_utf8(response).unwrap()
        });
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = Vec::new();
        let request = read_next_http_request(&mut stream, &mut buffer, 1024, None)
            .unwrap()
            .unwrap();
        set_keep_alive(KeepAlive::Requested);
        set_extra_response_headers(crate::management::listeners::cors_headers(
            request.headers.get("origin").map(String::as_str),
        ));
        serve(&mut stream, &request);
        let state = keep_alive();
        set_keep_alive(KeepAlive::Off);
        set_extra_response_headers(String::new());
        (client.join().unwrap(), state)
    }

    #[test]
    fn cors_preflight_and_headers_with_keep_alive() {
        let (response, state) = exchange(
            "OPTIONS /v2/summary HTTP/1.1\r\nHost: x\r\nOrigin: http://localhost\r\nAccess-Control-Request-Method: GET\r\nAccess-Control-Request-Headers: authorization\r\n\r\n",
            |stream, request| write_cors_preflight(stream, request).unwrap(),
        );
        assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"), "{response}");
        assert!(response.contains("Access-Control-Allow-Origin: http://localhost\r\n"));
        assert!(response.contains("Access-Control-Allow-Headers: Authorization, Content-Type, If-Match, Last-Event-ID"));
        assert!(response.contains("Access-Control-Allow-Methods: GET, POST"));
        assert!(response.contains("Connection: keep-alive\r\n"));
        assert_eq!(state, KeepAlive::Kept);
        // Disallowed origin: 204 without CORS headers.
        let (response, _) = exchange(
            "OPTIONS /v2/summary HTTP/1.1\r\nHost: x\r\nOrigin: https://evil.example\r\n\r\n",
            |stream, request| write_cors_preflight(stream, request).unwrap(),
        );
        assert!(response.starts_with("HTTP/1.1 204 "));
        assert!(!response.contains("Access-Control-"));
        // Ordinary responses (errors too) carry the origin for allowed origins.
        let (response, state) = exchange(
            "GET /v2/x HTTP/1.1\r\nHost: x\r\nOrigin: capacitor://localhost\r\n\r\n",
            |stream, _| {
                crate::management::v2::server::write_v2_error(
                    stream, 401, "Unauthorized", "unauthorized", "no", false, serde_json::json!({}),
                )
                .unwrap()
            },
        );
        assert!(response.starts_with("HTTP/1.1 401 "));
        assert!(response.contains("Access-Control-Allow-Origin: capacitor://localhost\r\nVary: Origin\r\n"));
        assert_eq!(state, KeepAlive::Kept);
    }

    #[test]
    fn remote_listener_accepts_only_client_and_device_tokens() {
        // Loopback: no gate at all (route checks apply as before).
        assert!(remote_gate(false, &request("GET", "/v2/sessions", Some("root")), "/v2/sessions").is_none());
        assert!(remote_gate(false, &request("GET", "/v2/sessions", None), "/v2/sessions").is_none());
        // Remote: root / legacy bearers are refused with a distinct code.
        assert_eq!(
            remote_gate(true, &request("GET", "/v2/sessions", Some("management-root")), "/v2/sessions")
                .map(|gate| gate.2),
            Some("remote_credential_refused")
        );
        assert_eq!(
            remote_gate(true, &request("GET", "/v2/capabilities", None), "/v2/capabilities")
                .map(|gate| gate.2),
            Some("unauthorized")
        );
        // A client-token-shaped bearer passes the gate (it is verified next).
        assert!(remote_gate(true, &request("GET", "/v2/sessions", Some("cxm1.abc")), "/v2/sessions").is_none());
        // Public routes.
        assert!(remote_gate(true, &request("POST", "/v2/pair", None), "/v2/pair").is_none());
        assert!(remote_gate(true, &request("GET", "/", None), "/").is_none());
        assert!(remote_gate(true, &request("GET", "/v2/pair", None), "/v2/pair").is_some());
        assert!(remote_gate(true, &request("POST", "/v2/pairing-codes", Some("root")), "/v2/pairing-codes").is_some());
    }
}
