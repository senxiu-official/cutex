use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::json;
use serde_json::Value;

use crate::http::server::require_service_bridge_token;
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::management::server::ManagementNativeForwardError;
use crate::management::server::ManagementNativeRequestHandler;
use crate::management::server::ManagementRequestContext;
use crate::management::server::ManagementSessionMutationHandler;
use crate::management::server::ManagementUserInputHandler;
use crate::management::server::ManagementUserInputQueueFlusher;

use super::model::EventEnvelope;
use super::model::MAX_SAFE_SEQUENCE;
use super::native_requests::request_idempotency_repository;
use super::native_requests::validate_native_request;
use super::request_idempotency::CapacityExceeded;
use super::native_requests::BeginRequest;
use super::native_requests::NativeRequestValidationError;
use super::native_requests::RequestClaim;
use super::native_requests::RequestIdempotencyRepository;
use super::native_requests::NATIVE_REQUEST_POLICY_VERSION;
use super::repository::management_v2_repository;
use super::repository::EventRepository;
use super::repository::ReplayError;
use super::repository::ReplayQuery;
use super::server_requests;
use super::subagent_lineage;
use super::subagent_lineage::LineageReadError;
use super::server_requests::ServerResponseClaimDecision;
use super::session::clear_focus;
use super::session::cutex_method_is_registered;
use super::session::focus_resource;
use super::session::session_list_resource;
use super::session::session_resource;
use super::session::session_resource_including_archive;
use super::session::session_resource_including_hidden;
use super::session::set_focus;
use super::user_input::parse_user_input_submit_params;
use super::user_input::user_input_repository;
use super::user_input::validate_user_input;
use super::user_input::UserInputDisposition;
use super::user_input::UserInputSubmitCommand;

const SSE_EVENT_NAME: &str = "cutex_management_event_v2";
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
pub(super) const SSE_SUBSCRIBER_CAPACITY: usize = 1024;


pub fn handle_v2_request(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    token: Option<&str>,
    agent_management_admin_token: Option<&str>,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let _diagnostic_scope = super::error_log::scope(request);
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or(request.path.as_str());
    if path == "/v2/diagnostics/control" {
        return handle_control_diagnostics(stream, request, agent_management_admin_token);
    }
    if crate::browser_relay::http::is_path(path) {
        return crate::browser_relay::http::handle(stream,request,agent_management_admin_token,context);
    }
    if path == super::dsh_spike::PATH {
        if !authenticated_root_request(request, agent_management_admin_token)
            || crate::management::users::current_principal().is_some_and(|p| !p.is_owner()) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized", "owner credential required", false, json!({}));
        }
        return super::dsh_spike::handle(stream, request);
    }
    // Removed routes answer before authentication and before any state access.
    if let Some(message) = removed_v2_route(path) {
        return write_v2_error(stream, 410, "Gone", "tool_removed", message, false, json!({}));
    }
    if super::workspace::is_workspace_path(path) {
        return super::workspace::handle(stream, request, agent_management_admin_token, context);
    }
    if super::side_chats::is_path(path) || super::notes::is_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for Side and Notes", false, json!({}));
        }
        return if super::side_chats::is_path(path) {
            super::side_chats::handle(stream, request, context)
        } else {
            super::notes::handle(stream, request)
        };
    }
    // Carry the existing Human bearer proof into exact-ID Cutex requests.
    // Listing exposure does not restrict the owner; method and lifecycle checks
    // still run in handle_cutex_request. Ordinary bridge callers have no proof.
    if authenticated_owner_cutex_request(request, agent_management_admin_token) {
        let repository = management_v2_repository()?;
        materialize_active_stream_reset(repository, context)?;
        let session_id = cutex_request_session_id_from_path(path)
            .expect("authenticated owner request has an exact session path");
        return handle_cutex_request(
            stream,
            request,
            context,
            repository,
            &session_id,
            Some(&crate::management::control_plane::HumanManagementPrincipal::authenticated()),
        );
    }
    if super::services::is_services_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for services", false, json!({}));
        }
        return super::services::handle(stream, request);
    }
    if super::director::is_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream,401,"Unauthorized","unauthorized","The Management root credential is required for Director review",false,json!({}));
        }
        return super::director::handle(stream,request);
    }
    if let Some(route) = super::delegations::parse_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token)
            || crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
            || (request.method != "GET" && crate::management::listeners::current_client_token().is_some_and(|token|
                token.scope != crate::management::client_tokens::DeviceScope::Full)) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized", "human owner credential required", false, json!({}));
        }
        return super::delegations::handle(stream, request, route);
    }
    if super::fork_preview::is_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token)
            || crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
        {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management owner credential is required for fork preview", false, json!({}));
        }
        return super::fork_preview::handle(stream, request);
    }
    if super::identity_api::is_path(path) {
        // Person identity: any authenticated principal, pending included
        // (the access gate lets every principal through); a claim concerns
        // only the caller's own credential.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "valid cutex management authentication is required", false, json!({}));
        }
        return super::identity_api::handle(stream, request);
    }
    if super::clients_api::is_path(path) {
        // Desktop client presence and remote close (client-remote-close):
        // any authenticated principal, pending included (the access gate
        // lets every principal through); the handler decides who may list
        // and close what.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "valid cutex management authentication is required", false, json!({}));
        }
        return super::clients_api::handle(stream, request);
    }
    if super::users_api::is_self_service_path(path) {
        // Self-service profile, avatars and own devices: any assigned
        // principal (the access gate refused pending tokens except for the
        // own-devices routes, whose handler answers 403 user_pending; the
        // handler decides "me" and who may see whom).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "valid cutex management authentication is required", false, json!({}));
        }
        return super::users_api::handle_self_service(stream, request);
    }
    if super::users_api::is_path(path) {
        // Users and grants: the owner only (root, or an owner-bound token;
        // the access gate has refused every other principal already).
        if !authenticated_root_request(request, agent_management_admin_token)
            || crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
        {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for users", false, json!({}));
        }
        return super::users_api::handle(stream, request);
    }
    if path == "/v2/peers" || path.starts_with("/v2/peers/") {
        if !authenticated_root_request(request, agent_management_admin_token) { return write_v2_error(stream, 401, "Unauthorized", "unauthorized", "the Management root credential is required", false, json!({})); }
        return crate::peers::routes::handle_v2(stream, request);
    }
    // Saved local media is owner-only, including owner-bound client tokens.
    if super::media_refs::parse_path(path).is_some()
        && crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
    {
        return write_v2_error(stream, 403, "Forbidden", "forbidden", "only the owner may read history media", false, json!({}));
    }
    // Remote-client routes (root credential or a Management client token,
    // which `management::client_tokens` has already mapped to root).
    if is_remote_client_root_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential or a client token is required", false, json!({}));
        }
        if super::client_tokens_api::is_client_tokens_path(path) {
            return super::client_tokens_api::handle(stream, request);
        }
        if super::attachments::is_attachments_path(path) {
            return super::attachments::handle(stream, request, context);
        }
        if super::host_files::is_host_files_path(path) {
            return super::host_files::handle(stream, request);
        }
        if super::file_transfer::is_host_path(path) {
            return super::file_transfer::handle(stream, request, None);
        }
        return super::host_dirs::handle(stream, request);
    }
    if super::runtime_packages::is_runtime_packages_path(path) {
        // Re-pinning agents onto another runtime package is the Human
        // owner's action: root only.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for runtime packages", false, json!({}));
        }
        return super::runtime_packages::handle(stream, request, context);
    }
    if path == super::stop_idle::IDLE_PATH || path == super::stop_idle::STOP_PATH {
        // Stopping idle agents is the owner's action: root only (before the
        // /v2/runtimes/ prefix below).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required to stop idle agents", false, json!({}));
        }
        return match (request.method.as_str(), path) {
            ("GET", super::stop_idle::IDLE_PATH) => super::stop_idle::handle_idle(stream, request, context),
            ("POST", super::stop_idle::STOP_PATH) => super::stop_idle::handle_stop(stream, request, context),
            _ => write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
                "use GET /v2/runtimes/idle or POST /v2/runtimes/stop-idle", false, json!({})),
        };
    }
    if super::runtime_restore_api::is_path(path) {
        // Restore candidates after an unclean stop: root, incl. client /
        // device tokens (before the /v2/runtimes/ prefix below).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for runtime restore", false, json!({}));
        }
        return super::runtime_restore_api::handle(stream, request);
    }
    if super::runtimes_api::is_runtimes_path(path) {
        // Read-only runtime overview (desktop 运行时 page): root, incl.
        // client / device tokens, like runtime packages.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for runtimes", false, json!({}));
        }
        return super::runtimes_api::handle(stream, request);
    }
    if super::components_api::is_path(path) {
        // Optional components (install / remove backend services): root only.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for components", false, json!({}));
        }
        return super::components_api::handle(stream, request);
    }
    if super::upgrade_api::is_path(path) {
        // Backend upgrade (stop / swap / restart this daemon): root only.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for upgrades", false, json!({}));
        }
        return super::upgrade_api::handle(stream, request);
    }
    if super::owner_rotation_api::is_path(path) { return super::owner_rotation_api::handle(stream, request, agent_management_admin_token, context); }
    if super::claude_skills_import::is_path(path) {
        // Import skills into the host's shared Claude skills directory
        // (capability claude-skills-import): the owner only.
        if !authenticated_root_request(request, agent_management_admin_token)
            || crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
        {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required to import Claude skills", false, json!({}));
        }
        return super::claude_skills_import::handle(stream, request);
    }
    if super::kimi_accounts_api::is_path(path) {
        // r266 Kimi accounts (subscription logins of the owner): root only.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for Kimi accounts", false, json!({}));
        }
        return super::kimi_accounts_api::handle(stream, request);
    }
    if super::claude_accounts_api::is_path(path) {
        // Claude accounts (logins / API keys of the owner): root, incl.
        // client / device tokens.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for Claude accounts", false, json!({}));
        }
        return super::claude_accounts_api::handle(stream, request);
    }
    if super::approval_rules::is_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for approval rules", false, json!({}));
        }
        return super::approval_rules::handle(stream, request);
    }
    if super::approvals::is_approvals_path(path) {
        // Deciding owner approvals is the Human owner's action: root only.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for approvals", false, json!({}));
        }
        return super::approvals::handle(stream, request);
    }
    if super::model_overload_retry::is_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized", "the Management root credential is required", false, json!({}));
        }
        return super::model_overload_retry::handle(stream, request);
    }
    if super::quota_continue::is_path(path) {
        // Claude quota auto-continue switch / pending continuation: root,
        // incl. device / client tokens.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for quota auto-continue", false, json!({}));
        }
        return super::quota_continue::handle(stream, request);
    }
    if super::skills_archive::is_path(path) {
        // Skills archive (list, archive, restore): root, incl. device / client tokens.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for the skills archive", false, json!({}));
        }
        return super::skills_archive::handle(stream, request);
    }
    if super::alarms_api::is_path(path) {
        // Agent alarms (list, owner cancel): root, incl. device / client tokens.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for alarms", false, json!({}));
        }
        return super::alarms_api::handle(stream, request);
    }
    if super::workflows_api::is_path(path) {
        // Workflow runs (list, detail, owner start / stop / resume / delete): root.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for workflows", false, json!({}));
        }
        return super::workflows_api::handle(stream, request);
    }
    if super::asks::is_asks_path(path) {
        // Asks and their artifacts are the owner's view: root only.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for asks", false, json!({}));
        }
        return super::asks::handle(stream, request);
    }
    if super::documents::is_path(path) {
        // Overflow documents and the record archive are the owner's view: root
        // (incl. device / client tokens).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for documents", false, json!({}));
        }
        return super::documents::handle(stream, request);
    }
    if super::artifact_store::is_path(path) {
        // The artifact store (capability `artifacts`) is the owner's view:
        // root (incl. device / client tokens). Agents use the MCP tools.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for the artifact store", false, json!({}));
        }
        return super::artifact_store::handle(stream, request);
    }
    if super::agent_harness::is_path(path) {
        // The launch text preview of one session: root (incl. device /
        // client tokens).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for the harness preview", false, json!({}));
        }
        return super::agent_harness::handle(stream, request);
    }
    if super::claude_runtime_api::is_path(path) {
        // The Claude runtime pin of one Claude agent (claude-runtime-repin):
        // GET root (incl. device / client tokens); the repin itself is the
        // owner's (root, or an owner-bound token).
        if !authenticated_root_request(request, agent_management_admin_token)
            || (request.method != "GET"
                && crate::management::users::current_principal().is_some_and(|p| !p.is_owner()))
        {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for the Claude runtime pin", false, json!({}));
        }
        return super::claude_runtime_api::handle(stream, request);
    }
    if super::claude_code_sources::is_path(path) {
        // Claude Code sources / MCP of one Claude agent (claude-code-sources):
        // root (incl. device / client tokens), read-only, no secret values.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for Claude Code sources", false, json!({}));
        }
        return super::claude_code_sources::handle(stream, request);
    }
    if super::session_skills::is_path(path) {
        // The skill catalog of one Claude session (capability
        // claude-session-skills): root (incl. device / client tokens).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for session skills", false, json!({}));
        }
        return super::session_skills::handle(stream, request);
    }
    if super::sandbox_extras::is_path(path) {
        // Network / extra writable roots of one session (capability
        // session-sandbox-extras): the owner only (root, or an owner-bound
        // token; the access gate classifies /v2/sessions owner-only too).
        if !authenticated_root_request(request, agent_management_admin_token)
            || crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
        {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for session sandbox settings", false, json!({}));
        }
        return super::sandbox_extras::handle(stream, request, context);
    }
    if super::claude_permission_rules::is_path(path) {
        // Persistent "always allow" rules of a Claude agent's workspace
        // (capability claude-permission-rules): the owner only.
        if !authenticated_root_request(request, agent_management_admin_token)
            || crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
        {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for Claude permission rules", false, json!({}));
        }
        return super::claude_permission_rules::handle(stream, request);
    }
    if super::safety_checks::is_path(path) {
        // Safety blocks (owner decisions): root (incl. device / client tokens).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for safety checks", false, json!({}));
        }
        return super::safety_checks::handle(stream, request);
    }
    if super::owner_notify::is_path(path) {
        // Agent → owner messages are the owner's view: root (incl. device /
        // client tokens).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for owner notifications", false, json!({}));
        }
        return super::owner_notify::handle(stream, request);
    }
    if super::health::is_path(path) {
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for health", false, json!({}));
        }
        return super::health::handle(stream, request, context);
    }
    if super::agent_bus_stuck::is_path(path) {
        if path == super::agent_bus_stuck::CLOSE_PATH
            && (!authenticated_root_request(request, agent_management_admin_token)
                || crate::management::users::current_principal().is_some_and(|p| !p.is_owner())
                || crate::management::listeners::current_client_token().is_some_and(|token|
                    token.scope != crate::management::client_tokens::DeviceScope::Full))
        {
            return write_v2_error(stream, 403, "Forbidden", "forbidden",
                "the Management owner credential with full device scope is required to close a message", false, json!({}));
        }
        // Listing and discarding stuck Agent Bus messages is the owner's
        // action: root (incl. device / client tokens).
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(stream, 401, "Unauthorized", "unauthorized",
                "the Management root credential is required for stuck agent-bus messages", false, json!({}));
        }
        return super::agent_bus_stuck::handle(stream, request);
    }
    if super::memory::is_memory_path(path) {
        // Project memory is the Human owner's view: root credential only.
        if !authenticated_root_request(request, agent_management_admin_token) {
            return write_v2_error(
                stream,
                401,
                "Unauthorized",
                "unauthorized",
                "the Management root credential is required for project memory",
                false,
                json!({}),
            );
        }
        let repository = management_v2_repository()?;
        return super::memory::handle_memory_request(stream, request, repository);
    }
    let required_token = v2_required_token(path, token, agent_management_admin_token);
    let agent_management_admin_scoped = agent_management_admin_path(path);
    let agent_management_scoped =
        path.starts_with("/v2/agent-management") && !agent_management_admin_scoped;
    let required_credential_missing = (agent_management_scoped && token.is_none())
        || (agent_management_admin_scoped && agent_management_admin_token.is_none());
    if require_service_bridge_token(request, required_token, "Management v2").is_err()
        || required_credential_missing
    {
        return write_v2_error(
            stream,
            401,
            "Unauthorized",
            "unauthorized",
            "valid cutex management authentication is required",
            false,
            json!({}),
        );
    }
    let repository = management_v2_repository()?;
    materialize_active_stream_reset(repository, context)?;
    let root = authenticated_root_request(request, agent_management_admin_token);
    handle_v2_request_with_repository(stream, request, repository, context, root)
}

/// Remote-client routes that only the root credential (or a Management
/// client token mapped to it) may use: client tokens, attachments, host
/// directory browsing and the host file read.
pub(super) fn is_remote_client_root_path(path: &str) -> bool {
    super::client_tokens_api::is_client_tokens_path(path)
        || super::attachments::is_attachments_path(path)
        || super::host_dirs::is_host_dirs_path(path)
        || super::host_files::is_host_files_path(path)
        || super::file_transfer::is_host_path(path)
}

/// True only when a dedicated Management root credential is configured and
/// the request carries exactly it. Root callers are the Human owner's full
/// view: exact-ID session routes resolve hidden (local-only, not IM-exposed)
/// durable sessions and `GET /v2/sessions?includeHidden=true` lists them.
/// Persistent agents are never hidden since `im-exposure-retired`.
pub(crate) fn authenticated_root_request(request: &SimpleHttpRequest, root: Option<&str>) -> bool {
    root.is_some_and(|root| !root.is_empty())
        && require_service_bridge_token(request, root, "Management root").is_ok()
}

fn authenticated_owner_cutex_request(request: &SimpleHttpRequest, root: Option<&str>) -> bool {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    request.method == "POST"
        && cutex_request_session_id_from_path(path).is_some()
        && root.is_some()
        && require_service_bridge_token(request, root, "Human Cutex request").is_ok()
}

/// Routes removed in r219 that still answer for one release (HTTP 410,
/// code `tool_removed`, not retryable). Drop in r220.
fn removed_v2_route(path: &str) -> Option<&'static str> {
    if path == "/v2/release-rotation" || path.starts_with("/v2/release-rotation/") {
        return Some("release rotation was removed in r219");
    }
    if path == "/v2/task-service/seats" || path.starts_with("/v2/task-service/seats/") {
        return Some(
            "seat administration was removed in r219; the Director is the Agent Management project authority",
        );
    }
    // Whole prefix: management-query, human-recovery and every other Task
    // Service route.
    if path == "/v2/task-service" || path.starts_with("/v2/task-service/") {
        return Some("the Task Service was removed in r219");
    }
    // Exact segment shapes only: /v2/tasks/{taskId}, /v2/sessions/{id}/tasks
    // and the owner task read /v2/projects/{id}/tasks. Other session and
    // project routes are unaffected.
    let exact_segment = |value: &str| !value.is_empty() && !value.contains('/');
    if path.strip_prefix("/v2/tasks/").is_some_and(exact_segment) {
        return Some("the Task Service was removed in r219");
    }
    for prefix in ["/v2/sessions/", "/v2/projects/"] {
        if path
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix("/tasks"))
            .is_some_and(exact_segment)
        {
            return Some("the Task Service was removed in r219");
        }
    }
    None
}

fn v2_required_token<'a>(
    path: &str,
    management_token: Option<&'a str>,
    agent_management_admin_token: Option<&'a str>,
) -> Option<&'a str> {
    if agent_management_admin_path(path) {
        agent_management_admin_token
    } else {
        management_token
    }
}

fn agent_management_admin_path(path: &str) -> bool {
    if super::director::is_path(path) {return true}
    matches!(
        path,
        "/v2/agent-management/authority"
            | "/v2/agent-management/explicit-launch"
            | "/v2/agent-management/human-config"
            | "/v2/agent-management/native-recovery"
            | "/v2/agent-management/durable-candidates"
            | "/v2/agent-management/durable-import"
            | "/v2/agent-management/archive-review"
            | super::archive::api::REVIEW
            | super::archive::api::EXECUTE
            | "/v2/agent-management/adopt-saved-native"
            | "/v2/agent-management/archive-actions"
            | "/v2/agent-management/legacy-director-ownership-import"
            | "/v2/agent-management/reservation-reconciliation"
            | "/v2/agent-management/create-abandon"
            | "/v2/agent-management/projects"
            | "/v2/agent-management/project-presentation"
            | "/v2/agent-management/operator-actions"
            | "/v2/agent-management/project-mutations"
            | "/v2/job-service/management-query"
            | "/v2/job-service/capabilities"
            | "/v2/job-service/cancel"
            | "/v2/job-service/output"
            | "/v2/job-service/mark-external"
            | "/v2/nci-verification"
            | "/v2/nci-verification/config"
            | "/v2/nci-verification/key"
            | "/v2/nci-verification/helper"
            | "/v2/inbound-mail"
            | "/v2/inbound-mail/config"
            | "/v2/inbound-mail/test"
            | "/v2/host"
            | "/v2/host/sessions"
            | "/v2/human/agents"
            | "/v2/human/sessions"
    ) || management_project_id_from_path(path).is_some()
        || super::profile_quota::parse_path(path).is_some()
        || path == super::profile_auto_pick::PATH
        || path == super::profile_link::SECRET_PATH
        || path == super::guides_preferences::PATH
        || super::profile_reset_credits::parse_path(path).is_some()
        || super::profiles_api::is_profiles_path(path)
        || super::profile_login::parse_path(path).is_some()
        || super::profile_models::parse_path(path).is_some()
        || super::storage::is_path(path)
        || path == super::settings_api::PATH
        || super::notification_levels::is_path(path)
        || path == super::user_instructions::PATH
        || super::usage_api::parse_path(path).is_some()
        || super::codex_config::is_codex_requests_path(path)
        || super::scoped_mcp::parse_path(path).is_some()
        || history_session_id_from_path(path).is_some()
        || history_turn_path(path).is_some()
        || super::media_refs::parse_path(path).is_some()
        || super::event_blobs::parse_path(path).is_some()
}

pub(super) fn materialize_active_stream_reset(
    repository: &EventRepository,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    if repository.recovery_reset()?.is_none() {
        return Ok(());
    }
    let registry = (context.load_registry)()?;
    let sessions = session_list_resource(&registry, context.load_runtime_status, repository)?;
    let active_session_ids = sessions
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|session| {
            session.pointer("/runtime/status").and_then(Value::as_str) == Some("online")
        })
        .filter_map(|session| session.get("cutexSessionId").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    repository.materialize_recovery_reset(&active_session_ids, true)?;
    Ok(())
}

fn handle_v2_request_with_repository(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    repository: &EventRepository,
    context: ManagementRequestContext,
    root: bool,
) -> anyhow::Result<()> {
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or(request.path.as_str());
    match (request.method.as_str(), path) {
        ("POST", "/v2/agent-management/actions") => handle_agent_management_action(stream, request),
        ("POST", "/v2/agent-management/authority") => {
            handle_project_authority(stream, request, context)
        }
        ("POST", "/v2/agent-management/legacy-director-ownership-import") => {
            handle_legacy_director_ownership_import(stream, request, context)
        }
        ("POST", "/v2/agent-management/create-abandon") => {
            handle_agent_create_abandon(stream, request, context)
        }
        ("POST", "/v2/agent-management/reservation-reconciliation") => {
            handle_agent_reservation_reconciliation(stream, request, context)
        }
        ("GET", "/v2/agent-management/projects") => {
            handle_management_project_collection(stream, context)
        }
        ("POST", "/v2/agent-management/native-recovery") => {
            let payload: crate::app_server::external_recovery::RecoveryRequest =
                match serde_json::from_slice(&request.body) {
                    Ok(value) => value,
                    Err(_) => {
                        return write_v2_error(
                            stream,
                            400,
                            "Bad Request",
                            "invalid_request",
                            "strict native recovery request required",
                            false,
                            json!({}),
                        )
                    }
                };
            match crate::app_server::external_recovery::handle(
                &crate::management::control_plane::HumanManagementPrincipal::authenticated(),
                &payload,
            ) {
                Ok(value) => write_json_response(stream, 200, "OK", &value),
                Err(error) => write_v2_error(
                    stream,
                    409,
                    "Conflict",
                    "native_recovery_not_committed",
                    &error.to_string(),
                    false,
                    json!({"recovery":"inspect status and reconcile the exact action; never infer rollback or choose a new retry ID"}),
                ),
            }
        }
        ("POST", "/v2/agent-management/explicit-launch") => {
            let payload: crate::agent_management::ExplicitLaunchRequest =
                match serde_json::from_slice(&request.body) {
                    Ok(payload) => payload,
                    Err(_) => {
                        return write_v2_error(
                            stream,
                            400,
                            "Bad Request",
                            "invalid_request",
                            "strict explicit launch request required",
                            false,
                            json!({}),
                        )
                    }
                };
            match (context.explicit_launch_action)(
                &crate::management::control_plane::HumanManagementPrincipal::authenticated(),
                &payload,
            ) {
                Ok(response) => write_json_response(stream, 200, "OK", &response),
                Err(error) => write_agent_management_control_error(stream, error),
            }
        }
        ("GET", "/v2/agent-management/durable-candidates") => {
            let principal =
                crate::management::control_plane::HumanManagementPrincipal::authenticated();
            match (context.durable_agent_candidates)(&principal) {
                Ok(response) => {
                    write_json_response(stream, 200, "OK", &serde_json::to_value(response)?)
                }
                Err(error) => write_agent_management_control_error(stream, error),
            }
        }
        ("POST", "/v2/agent-management/adopt-saved-native") => {
            let payload = match serde_json::from_slice(&request.body) {
                Ok(payload) => payload,
                Err(_) => {
                    return write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        "strict Human adoption request required",
                        false,
                        json!({}),
                    )
                }
            };
            match (context.adopt_saved_native)(
                &crate::management::control_plane::HumanManagementPrincipal::authenticated(),
                &payload,
            ) {
                Ok(response) => {
                    write_json_response(stream, 200, "OK", &serde_json::to_value(response)?)
                }
                Err(error) => write_agent_management_control_error(stream, error),
            }
        }
        ("POST", super::archive::api::REVIEW | super::archive::api::EXECUTE) => {
            super::archive::api::handle(stream, &request, context.owner_archive_action)
        }
        ("POST", "/v2/agent-management/archive-review") => {
            let payload = match serde_json::from_slice(&request.body) {
                Ok(payload) => payload,
                Err(_) => {
                    return write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        "strict Archive review required",
                        false,
                        json!({}),
                    )
                }
            };
            let principal =
                crate::management::control_plane::HumanManagementPrincipal::authenticated();
            match (context.review_agent_archive)(&principal, &payload) {
                Ok(response) => {
                    write_json_response(stream, 200, "OK", &serde_json::to_value(response)?)
                }
                Err(error) => write_agent_management_control_error(stream, error),
            }
        }
        ("POST", "/v2/agent-management/archive-actions") => {
            let payload = match serde_json::from_slice(&request.body) {
                Ok(payload) => payload,
                Err(_) => {
                    return write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        "strict confirmed Archive action required",
                        false,
                        json!({}),
                    )
                }
            };
            let principal =
                crate::management::control_plane::HumanManagementPrincipal::authenticated();
            match (context.execute_agent_archive)(&principal, &payload) {
                Ok(response) => {
                    write_json_response(stream, 200, "OK", &serde_json::to_value(response)?)
                }
                Err(error) => write_agent_management_control_error(stream, error),
            }
        }
        ("POST", "/v2/agent-management/durable-import") => {
            let payload: crate::agent_management::DurableImportRequest =
                match serde_json::from_slice(&request.body) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return write_v2_error(
                            stream,
                            400,
                            "Bad Request",
                            "invalid_request",
                            &format!("strict durable import request: {error}"),
                            false,
                            json!({}),
                        )
                    }
                };
            let principal =
                crate::management::control_plane::HumanManagementPrincipal::authenticated();
            match (context.import_durable_agent)(&principal, &payload) {
                Ok(response) => {
                    write_json_response(stream, 200, "OK", &serde_json::to_value(response)?)
                }
                Err(error) => write_agent_management_control_error(stream, error),
            }
        }
        ("GET", path) if management_project_id_from_path(path).is_some() => {
            let project_id = management_project_id_from_path(path)
                .expect("matched strict Management Project path");
            handle_management_project_read(stream, context, &project_id)
        }
        ("POST", "/v2/agent-management/project-presentation") => {
            handle_management_project_presentation(stream, request, context)
        }
        ("POST", "/v2/agent-management/operator-actions") => {
            handle_management_operator_action(stream, request, context)
        }
        ("POST", "/v2/agent-management/project-mutations") => {
            handle_management_project_mutation(stream, request, context)
        }
        ("POST", "/v2/agent-management/human-config") => {
            super::human_config::handle(stream, request)
        }
        ("GET", "/v2/host") => write_json_response(stream, 200, "OK", &super::host_info::host_resource()),
        (_, super::profile_auto_pick::PATH) => super::profile_auto_pick::handle(stream, request),
        (_, super::profile_link::SECRET_PATH) => super::profile_link::handle(stream, request),
        (_, super::guides_preferences::PATH) => super::guides_preferences::handle(stream, request),
        (_, path) if super::profile_quota::parse_path(path).is_some() => {
            let (profile, route) =
                super::profile_quota::parse_path(path).expect("matched profile quota path");
            super::profile_quota::handle(stream, request, &profile, route)
        }
        (_, path) if super::profile_reset_credits::parse_path(path).is_some() => {
            let (profile, route) =
                super::profile_reset_credits::parse_path(path).expect("matched reset credits path");
            super::profile_reset_credits::handle(stream, request, &profile, route)
        }
        (_, path) if super::profile_models::parse_path(path).is_some() => {
            let (profile, route) =
                super::profile_models::parse_path(path).expect("matched profile models path");
            super::profile_models::handle(stream, request, &profile, route)
        }
        (_, path) if super::profile_login::parse_path(path).is_some() => {
            let (profile, route) =
                super::profile_login::parse_path(path).expect("matched profile login path");
            super::profile_login::handle(stream, request, &profile, route)
        }
        (_, path) if super::profiles_api::is_profiles_path(path) => {
            super::profiles_api::handle(stream, request, path)
        }
        (_, path) if super::storage::is_path(path) => super::storage::handle(stream, request),
        (_, super::settings_api::PATH) => super::settings_api::handle(stream, request),
        (_, path) if super::notification_levels::is_path(path) => super::notification_levels::handle(stream, request),
        (_, super::user_instructions::PATH) => super::user_instructions::handle(stream, request),
        (_, path) if super::usage_api::parse_path(path).is_some() => {
            let route = super::usage_api::parse_path(path).expect("matched usage path");
            super::usage_api::handle(stream, request, route)
        }
        (_, path) if super::scoped_mcp::parse_path(path).is_some() => {
            let route = super::scoped_mcp::parse_path(path).expect("matched scoped MCP path");
            super::scoped_mcp::handle(stream, request, route)
        }
        (_, path) if super::codex_config::is_codex_requests_path(path) => {
            super::codex_config::handle(stream, request)
        }
        ("GET", "/v2/capabilities") => write_json_response(
            stream,
            200,
            "OK",
            &super::access::capabilities_for(
                &crate::management::users::current_principal()
                    .unwrap_or(crate::management::users::Principal::Owner),
            ),
        ),
        ("POST", "/v2/human/agents") => {
            super::human_agents::handle_create(stream, request, context, repository)
        }
        ("POST", super::human_sessions::PATH) => {
            super::human_sessions::handle_create(stream, request, context, repository)
        }
        ("POST", "/v2/host/sessions") => super::host_sessions::handle(stream,request,context),
        ("POST", "/v2/job-service/management-query") => super::human_jobs::handle(stream, request),
        ("GET", "/v2/job-service/capabilities") => super::human_jobs::handle_capabilities(stream),
        ("POST", "/v2/job-service/cancel") => super::human_jobs::handle_cancel(stream, request),
        ("POST", "/v2/job-service/mark-external") => super::human_jobs::handle_mark_external(stream, request),
        ("GET", "/v2/nci-verification") | ("PUT", "/v2/nci-verification/config") | ("DELETE", "/v2/nci-verification/config") | ("POST", "/v2/nci-verification/key") | ("GET", "/v2/nci-verification/helper") => super::nci_verification::handle(stream, request),
        ("GET", "/v2/inbound-mail") | ("PUT", "/v2/inbound-mail/config") | ("DELETE", "/v2/inbound-mail/config") | ("POST", "/v2/inbound-mail/test") => super::inbound_mail::handle(stream, request),
        ("POST", "/v2/job-service/output") => super::human_jobs::handle_output(stream, request),
        ("GET", "/v2/sessions") => {
            super::archive::handle_session_collection_get(stream, request, context, repository, root)
        }
        ("POST", path) if native_request_session_id_from_path(path).is_some() => {
            let cutex_session_id =
                native_request_session_id_from_path(path).expect("matched native request path");
            handle_native_request(stream, request, context, repository, &cutex_session_id, root)
        }
        ("POST", path) if server_response_session_id_from_path(path).is_some() => {
            let cutex_session_id =
                server_response_session_id_from_path(path).expect("matched server response path");
            handle_native_server_response(
                stream,
                request,
                context,
                repository,
                &cutex_session_id,
                root,
            )
        }
        ("POST", path) if cutex_request_session_id_from_path(path).is_some() => {
            let cutex_session_id =
                cutex_request_session_id_from_path(path).expect("matched cutex request path");
            handle_cutex_request(
                stream,
                request,
                context,
                repository,
                &cutex_session_id,
                None,
            )
        }
        ("GET", path) if history_session_id_from_path(path).is_some() => {
            let cutex_session_id =
                history_session_id_from_path(path).expect("matched history path");
            handle_session_history(stream, request, context, repository, &cutex_session_id)
        }
        ("GET", path) if super::media_refs::parse_path(path).is_some() => {
            let (session_id, sha) = super::media_refs::parse_path(path).expect("matched media path");
            handle_session_media(stream, &session_id, &sha)
        }
        ("GET", path) if super::event_blobs::parse_path(path).is_some() => {
            let (session_id,sha)=super::event_blobs::parse_path(path).expect("matched event blob path");
            match super::event_blobs::EventBlobStore::open_default()?.get(&session_id,&sha) {
                Some((mime,bytes))=>crate::http::server::write_http_response(stream,200,"OK",&mime,&bytes),
                None=>write_v2_error(stream,404,"Not Found","blob_not_found","blob unavailable for this session",false,json!({})),
            }
        }
        ("GET", path) if history_turn_path(path).is_some() => {
            let (session_id, turn_id) = history_turn_path(path).expect("matched history turn path");
            handle_session_history_turn(stream, request, context, repository, &session_id, &turn_id)
        }
        ("GET", path) if bootstrap_session_id_from_path(path).is_some() => {
            let cutex_session_id =
                bootstrap_session_id_from_path(path).expect("matched bootstrap path");
            handle_bootstrap(stream, request, context, repository, &cutex_session_id, root)
        }
        ("GET", path) if session_id_from_path(path).is_some() => {
            let cutex_session_id = session_id_from_path(path).expect("matched session path");
            let lifecycle = match super::archive::session_lifecycle_query(&request.path) {
                Ok(lifecycle) => lifecycle,
                Err(error) => {
                    return write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        &error.to_string(),
                        false,
                        json!({}),
                    )
                }
            };
            match lifecycle {
                super::archive::SessionLifecycleQuery::Retired => {
                    match super::session::retired_session_resource(&cutex_session_id)? {
                        Some(resource) => write_json_response(stream, 200, "OK", &resource),
                        None => write_v2_error(
                            stream,
                            404,
                            "Not Found",
                            "session_not_found",
                            "the retired cutex session does not exist on this host",
                            false,
                            json!({ "cutexSessionId": cutex_session_id, "lifecycle": "retired" }),
                        ),
                    }
                }
                super::archive::SessionLifecycleQuery::Active => {
                    let registry = (context.load_registry)()?;
                    match owner_session_resource(
                        &cutex_session_id,
                        &registry,
                        context.load_runtime_status,
                        repository,
                        root,
                    )? {
                        Some(resource) => write_json_response(stream, 200, "OK", &resource),
                        None => write_v2_error(
                            stream,
                            404,
                            "Not Found",
                            "session_not_found",
                            "the owner-visible cutex session does not exist on this host",
                            false,
                            json!({ "cutexSessionId": cutex_session_id }),
                        ),
                    }
                }
            }
        }
        ("GET" | "POST", "/v2/notifications/states" | "/v2/notifications/ack" | "/v2/notifications/outbound" | "/v2/notifications/deliveries" | "/v2/notifications/test" | "/v2/notifications/retry") => crate::notify::outbound::handle_request(stream, request),
        ("GET" | "POST", "/v2/notifications/session") => crate::notify::session::handle_request(stream, request),
        ("GET", "/v2/events") => {
            let query = match replay_query(request, false) {
                Ok(query) => query,
                Err(error) => return write_replay_error(stream, error),
            };
            match repository.page(query) {
                Ok(page) => write_json_response(stream, 200, "OK", &serde_json::to_value(page)?),
                Err(error) => write_replay_error(stream, error),
            }
        }
        ("GET", super::summary::SUMMARY_PATH) => {
            super::summary::handle_summary(stream, context, repository)
        }
        ("GET", super::summary::SUMMARY_EVENTS_PATH) => super::summary::handle_events(stream, request),
        ("POST", super::summary::SUMMARY_READ_PATH) => super::summary::handle_read(stream, request),
        ("GET", path) if super::project_events::parse_route(path).is_some() => {
            match super::project_events::parse_route(path).expect("matched above") {
                super::project_events::Route::Background(project) => {
                    super::project_events::handle_background(stream, context, &project, &mut |_| true, true)
                }
                route => super::project_events::handle(stream, request, &route, &mut |_| true),
            }
        }
        ("GET", "/v2/events/stream") => {
            let query = match replay_query(request, true) {
                Ok(query) => query,
                Err(error) => return write_replay_error(stream, error),
            };
            let mut subscription =
                match repository.page_and_subscribe(query.clone(), SSE_SUBSCRIBER_CAPACITY) {
                    Ok(subscription) => subscription,
                    Err(error) => return write_replay_error(stream, error),
                };
            if enable_live_deltas(repository, request, &query, &mut subscription).is_err() {
                return write_live_capacity_error(stream);
            }
            write_sse_headers(stream)?;
            let watch = crate::management::listeners::RevocationWatch::for_stream(stream)?;
            stream_events(
                stream,
                subscription,
                &query,
                watch,
            )
        }
        _ => write_v2_error(
            stream,
            404,
            "Not Found",
            "route_not_found",
            "management v2 route not found",
            false,
            json!({ "path": path }),
        ),
    }
}

fn handle_agent_management_action(
    stream: &mut TcpStream,
    _request: &SimpleHttpRequest,
) -> anyhow::Result<()> {
    write_v2_error(
        stream,
        403,
        "Forbidden",
        "ambient_agent_bus_required",
        "Agent Management lifecycle actions require a current authenticated Agent Bus occurrence",
        false,
        json!({}),
    )
}

fn handle_project_authority(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let payload: crate::agent_management::ProjectAuthorityRequest =
        match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    &format!("strict project authority request parsing failed: {error}"),
                    false,
                    json!({}),
                )
            }
        };
    let response = (context.bind_project_authority)(payload);
    write_json_response(stream, 200, "OK", &response)
}

fn handle_legacy_director_ownership_import(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let payload: crate::agent_management::LegacyDirectorOwnershipImportRequest =
        match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    &format!(
                        "strict legacy Director ownership import request parsing failed: {error}"
                    ),
                    false,
                    json!({}),
                )
            }
        };
    let response = (context.import_legacy_director_ownership)(payload);
    write_json_response(stream, 200, "OK", &response)
}

fn handle_agent_create_abandon(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let payload: crate::agent_management::CreateAbandonRequest = match serde_json::from_slice(&request.body) {
        Ok(payload) => payload,
        Err(error) => {
            return write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &format!("strict create abandon request {{action_id, dry_run?}} required: {error}"),
                false,
                json!({}),
            )
        }
    };
    let response = (context.abandon_agent_create)(payload);
    write_json_response(stream, 200, "OK", &response)
}

fn handle_agent_reservation_reconciliation(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let payload: crate::agent_management::AgentReservationReconciliationRequest =
        match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    &format!(
                        "strict Agent reservation reconciliation request parsing failed: {error}"
                    ),
                    false,
                    json!({}),
                )
            }
        };
    let response = (context.reconcile_agent_reservation)(payload);
    write_json_response(stream, 200, "OK", &response)
}

pub(super) fn management_project_id_from_path(path: &str) -> Option<crate::agent_management::ProjectId> {
    let value = path.strip_prefix("/v2/agent-management/projects/")?;
    if value.is_empty() || value.contains('/') || value.contains('%') {
        return None;
    }
    crate::agent_management::ProjectId::new(value.to_string()).ok()
}

fn handle_management_project_collection(
    stream: &mut TcpStream,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    match (context.list_management_projects)(&principal) {
        Ok(response) => write_json_response(stream, 200, "OK", &serde_json::to_value(response)?),
        Err(error) => write_agent_management_control_error(stream, error),
    }
}

fn handle_management_project_read(
    stream: &mut TcpStream,
    context: ManagementRequestContext,
    project_id: &crate::agent_management::ProjectId,
) -> anyhow::Result<()> {
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    match (context.read_management_project)(&principal, project_id) {
        Ok(response) => write_json_response(stream, 200, "OK", &serde_json::to_value(response)?),
        Err(error) => write_agent_management_control_error(stream, error),
    }
}

fn handle_management_project_presentation(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let payload: crate::management::control_plane::HumanManagementPresentationUpdateRequest =
        match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    &format!("strict Management presentation request parsing failed: {error}"),
                    false,
                    json!({}),
                )
            }
        };
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    match (context.update_management_project_presentation)(&principal, &payload) {
        Ok(response) => write_json_response(stream, 200, "OK", &serde_json::to_value(response)?),
        Err(error) => write_agent_management_control_error(stream, error),
    }
}

fn handle_management_operator_action(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let payload: crate::management::control_plane::HumanManagementOperatorActionRequest =
        match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    &format!("strict Management Operator request parsing failed: {error}"),
                    false,
                    json!({}),
                )
            }
        };
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    match (context.execute_management_operator_action)(&principal, &payload) {
        Ok(response) => write_json_response(stream, 200, "OK", &serde_json::to_value(response)?),
        Err(error) => write_agent_management_control_error(stream, error),
    }
}

fn handle_management_project_mutation(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let payload: crate::management::control_plane::HumanManagementProjectMutationRequest =
        match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    &format!("strict Management Project mutation request parsing failed: {error}"),
                    false,
                    json!({}),
                )
            }
        };
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    match (context.execute_management_project_mutation)(&principal, &payload) {
        Ok(response) => write_json_response(stream, 200, "OK", &serde_json::to_value(response)?),
        Err(error) => write_agent_management_control_error(stream, error),
    }
}

pub(crate) fn write_agent_management_control_error(
    stream: &mut TcpStream,
    error: crate::agent_management::AgentManagementError,
) -> anyhow::Result<()> {
    use crate::agent_management::AgentManagementError;
    let (status, reason, retryable) = match &error {
        AgentManagementError::InvalidRequest(_)
        | AgentManagementError::InvalidRequestDetail(_) => (400, "Bad Request", false),
        AgentManagementError::Unauthorized
        | AgentManagementError::NotAuthorizedDirector
        | AgentManagementError::ProjectSelectionRequired
        | AgentManagementError::ProjectNotAuthorized => (403, "Forbidden", false),
        AgentManagementError::NotFound(_) => (404, "Not Found", false),
        AgentManagementError::Removed(_) => (410, "Gone", false),
        AgentManagementError::Conflict(_) | AgentManagementError::ConflictDetail(_) => {
            (409, "Conflict", false)
        }
        AgentManagementError::PersistenceUnavailable => (503, "Service Unavailable", true),
        AgentManagementError::OwnerActionRequired(_)
        | AgentManagementError::InvalidStore
        | AgentManagementError::External(_)
        | AgentManagementError::ReconciliationPresent(_)
        | AgentManagementError::ReconciliationAmbiguous(_)
        | AgentManagementError::ReconciliationUnavailable(_) => {
            (500, "Internal Server Error", false)
        }
    };
    write_v2_error(
        stream,
        status,
        reason,
        error.code(),
        &error.to_string(),
        retryable,
        json!({}),
    )
}

fn handle_bootstrap(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
    cutex_session_id: &str,
    root: bool,
) -> anyhow::Result<()> {
    let compact = match super::compact::parse_compact(request) {
        Ok(compact) => compact,
        Err(message) => {
            return write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &message,
                false,
                json!({}),
            )
        }
    };
    let media = match super::media_refs::parse_media(request) {
        Ok(media) => media,
        Err(message) => {
            return write_v2_error(stream, 400, "Bad Request", "invalid_request", &message, false, json!({}));
        }
    };
    let turn_limit = match super::history::parse_bootstrap_turn_limit(request) {
        Ok(turn_limit) => turn_limit,
        Err(message) => {
            return write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &message,
                false,
                json!({}),
            )
        }
    };
    // Capture the complete replay boundary BEFORE projection can arrange a
    // terminal stop event. No repository lock spans projection or native RPC.
    // An early cursor allows duplicates, never a permanently missing terminal.
    let checkpoint = repository.checkpoint()?;
    let registry = (context.load_registry)()?;
    let Some(session) = owner_session_resource(
        cutex_session_id,
        &registry,
        context.load_runtime_status,
        repository,
        root,
    )?
    else {
        return write_v2_error(
            stream,
            404,
            "Not Found",
            "session_not_found",
            "the owner-visible cutex session does not exist on this host",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    };
    let saved_files = media && super::media_refs::session_supports_saved_images(cutex_session_id);
    let store = media
        .then(super::media_refs::MediaStore::open_default)
        .and_then(|store| store.map_err(|error| eprintln!("history media: cache unavailable: {error:#}")).ok());
    let mut shaper = store
        .as_ref()
        .map(|store| super::history_turns::TurnShaper::new(cutex_session_id, store, compact).with_saved_files(saved_files));
    match build_bootstrap_resource_shaped(
        &session,
        checkpoint,
        turn_limit,
        shaper.as_mut(),
        |message| {
            (context.forward_native_request)(cutex_session_id, message)
                .map_err(|error| format!("{error:?}"))
        },
        |runtime_generation| {
            let mut state = (context.load_bootstrap_state)(cutex_session_id, runtime_generation)?;
            if let Some(state) = state.as_object_mut() {
                state.insert(
                    "latestPlan".to_string(),
                    (context.load_latest_plan)(cutex_session_id),
                );
                state.insert(
                    "latestTokenUsage".to_string(),
                    (context.load_latest_token_usage)(cutex_session_id),
                );
            }
            Ok(state)
        },
    ) {
        Ok(mut response) => {
            super::native_errors::annotate_history_response(&mut response);
            super::event_blobs::EventBlobStore::open_default()?
                .externalize_media(cutex_session_id,&mut response)?;
            if let Some(shaper) = &shaper {
                shaper.finish_body(&mut response);
                return write_json_response(stream, 200, "OK", &response);
            }
            if compact {
                super::compact::compact_items(&mut response);
            }
            if media {
                super::media_refs::apply_default_with_saved_files(&mut response, cutex_session_id, saved_files);
            }
            write_json_response(stream, 200, "OK", &response)
        }
        Err(BootstrapBuildError::Native { message, details }) => write_v2_error(
            stream,
            502,
            "Bad Gateway",
            "app_server_unavailable",
            &message,
            false,
            details,
        ),
        Err(BootstrapBuildError::State(error)) => write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "event_persistence_unavailable",
            "the bootstrap checkpoint or cutex state could not be read",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        ),
    }
}

/// Root-only `GET /v2/sessions/{id}/history` (see `history.rs`).
fn handle_session_history(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
    cutex_session_id: &str,
) -> anyhow::Result<()> {
    handle_session_history_read(stream, request, context, repository, cutex_session_id, None)
}

fn handle_session_history_turn(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
    cutex_session_id: &str,
    turn_id: &str,
) -> anyhow::Result<()> {
    handle_session_history_read(stream, request, context, repository, cutex_session_id, Some(turn_id))
}

fn handle_session_history_read(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
    cutex_session_id: &str,
    turn_id: Option<&str>,
) -> anyhow::Result<()> {
    let compact = super::compact::parse_compact(request);
    let media = match super::media_refs::parse_media(request) {
        Ok(media) => media,
        Err(message) => {
            return write_v2_error(stream, 400, "Bad Request", "invalid_request", &message, false, json!({}));
        }
    };
    let parsed = if turn_id.is_some() {
        super::history::parse_turn_query(request)
            .map(|()| super::history::HistoryQuery { limit: 1, cursor: None })
    } else {
        super::history::parse_query(request)
    };
    let query = match compact.clone().and(parsed) {
        Ok(query) => query,
        Err(message) => {
            return write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &message,
                false,
                json!({}),
            )
        }
    };
    let registry = (context.load_registry)()?;
    let session = match session_resource_including_hidden(
        cutex_session_id,
        &registry,
        context.load_runtime_status,
        repository,
    )? {
        Some(session) => Some(session),
        None => session_resource_including_archive(
            cutex_session_id,
            &registry,
            context.load_runtime_status,
            repository,
        )?,
    };
    let Some(session) = session else {
        return write_v2_error(
            stream,
            404,
            "Not Found",
            "session_not_found",
            "the cutex session does not exist on this host",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    };
    let saved_files = media && super::media_refs::session_supports_saved_images(cutex_session_id);
    // media=ref pages shape their turns while reading (history_turns).
    let store = (media && turn_id.is_none())
        .then(super::media_refs::MediaStore::open_default)
        .and_then(|store| store.map_err(|error| eprintln!("history media: cache unavailable: {error:#}")).ok());
    let mut shaper = store
        .as_ref()
        .map(|store| super::history_turns::TurnShaper::new(cutex_session_id, store, compact == Ok(true)).with_saved_files(saved_files));
    let mut response = if let Some(turn_id) = turn_id {
        super::history::respond_turn(
            cutex_session_id, &session, turn_id,
            context.forward_native_request, context.open_offline_history,
        )
    } else {
        super::history::respond_shaped(
            cutex_session_id, &session, &query, shaper.as_mut(),
            context.forward_native_request, context.open_offline_history,
        )
    };
    if response.is_success() {
        super::native_errors::annotate_history_response(&mut response.body);
        super::event_blobs::EventBlobStore::open_default()?
            .externalize_media(cutex_session_id,&mut response.body)?;
        if turn_id.is_none() {
            super::history::attach_cutex_state(
                &mut response.body,
                (context.load_latest_plan)(cutex_session_id),
                (context.load_latest_token_usage)(cutex_session_id),
            );
        }
        if let Some(shaper) = &shaper {
            shaper.finish_body(&mut response.body);
            return write_json_response(stream, 200, "OK", &response.body);
        }
        if compact == Ok(true) {
            let count = super::compact::compact_items(&mut response.body);
            if turn_id.is_some() && count > 0 {
                response.body["complete"] = json!(false);
            }
        }
        if media {
            super::media_refs::apply_default_with_saved_files(&mut response.body, cutex_session_id, saved_files);
        }
        return write_json_response(stream, 200, "OK", &response.body);
    }
    write_v2_error(
        stream,
        response.status,
        response.reason,
        response.body["code"].as_str().unwrap_or("internal_error"),
        response.body["message"].as_str().unwrap_or_default(),
        response.body["retryable"].as_bool().unwrap_or(false),
        response.body["details"].clone(),
    )
}

/// `GET /v2/sessions/{id}/media/{sha256}` (history-media-refs): a media
/// payload `?media=ref` replaced in this session's history.
fn handle_session_media(stream: &mut TcpStream, cutex_session_id: &str, sha: &str) -> anyhow::Result<()> {
    let saved_files = super::media_refs::session_supports_saved_images(cutex_session_id);
    let found = super::event_blobs::EventBlobStore::open_default().ok()
        .and_then(|store| store.get(cutex_session_id, sha))
        .or_else(|| super::media_refs::MediaStore::open_default().ok().and_then(|store| store.get_with_saved_files(cutex_session_id, sha, saved_files)));
    match found {
        Some((mime, bytes)) => crate::http::server::write_http_response(stream, 200, "OK", &mime, &bytes),
        None => write_v2_error(
            stream,
            404,
            "Not Found",
            "media_not_found",
            "media unavailable: missing, changed, linked, oversized, unauthorized, or unsupported by this session runtime",
            false,
            json!({ "cutexSessionId": cutex_session_id, "sha256": sha }),
        ),
    }
}

#[derive(Debug)]
enum BootstrapBuildError {
    Native { message: String, details: Value },
    State(anyhow::Error),
}

/// `turn_limit` (history-pages, `?turnLimit=`): read only the newest turns
/// with `thread/turns/list` and report the page as `historyPage`; `None` keeps
/// the full `thread/read includeTurns` snapshot (and no `historyPage` key).
#[cfg(test)]
fn build_bootstrap_resource(
    session: &Value,
    repository: &EventRepository,
    turn_limit: Option<u32>,
    forward_native: impl FnMut(Value) -> Result<Value, String>,
    load_state: impl FnOnce(u64) -> anyhow::Result<Value>,
) -> Result<Value, BootstrapBuildError> {
    let checkpoint = repository.checkpoint().map_err(BootstrapBuildError::State)?;
    build_bootstrap_resource_shaped(session, checkpoint, turn_limit, None, forward_native, load_state)
}

/// `shaper` (`media=ref`): the `turnLimit` page's turns come back shaped
/// (`history_turns`); finish with `TurnShaper::finish_body`.
fn build_bootstrap_resource_shaped(
    session: &Value,
    checkpoint: super::model::EventCheckpoint,
    turn_limit: Option<u32>,
    mut shaper: Option<&mut super::history_turns::TurnShaper>,
    mut forward_native: impl FnMut(Value) -> Result<Value, String>,
    load_state: impl FnOnce(u64) -> anyhow::Result<Value>,
) -> Result<Value, BootstrapBuildError> {
    let cutex_session_id = session
        .get("cutexSessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let thread_id = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .filter(|thread_id| !thread_id.is_empty());
    let app_server_connected = session
        .pointer("/runtime/appServerConnected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut history_page = Value::Null;
    let native = if app_server_connected {
        let Some(thread_id) = thread_id else {
            return Err(BootstrapBuildError::Native {
                message: "the connected app-server session has no bound native thread".to_string(),
                details: json!({ "cutexSessionId": cutex_session_id }),
            });
        };
        if let Some(limit) = turn_limit {
            let query = super::history::HistoryQuery { limit, cursor: None };
            let key = super::history::index_key(cutex_session_id, session, "live");
            if let Some(shaper) = shaper.as_deref_mut() {
                shaper.begin(&key, true);
            }
            let mut page = super::history::read_history_page_shaped(
                thread_id,
                &query,
                shaper,
                &mut super::history::rpc_call(&mut forward_native),
            )
            .map_err(|error| match error {
                super::history::HistoryCallError::Rpc(native) => BootstrapBuildError::Native {
                    message: "thread/read returned a native error".to_string(),
                    details: json!({ "native": { "message": native } }),
                },
                super::history::HistoryCallError::Unavailable(diagnostic) => {
                    BootstrapBuildError::Native {
                        message: "the native thread snapshot could not be read".to_string(),
                        details: json!({ "diagnostic": diagnostic }),
                    }
                }
            })?;
            super::history::annotate_bootstrap(&key, &mut page);
            history_page = super::history::page_json(limit, None, &page);
            json!({
                "method": "thread/read",
                "result": { "thread": page.thread },
            })
        } else {
            let native_response = forward_native(json!({
                "id": format!("cutex-bootstrap-{}", uuid::Uuid::new_v4()),
                "method": "thread/read",
                "params": { "threadId": thread_id, "includeTurns": true },
            }))
            .map_err(|diagnostic| BootstrapBuildError::Native {
                message: "the native thread snapshot could not be read".to_string(),
                details: json!({ "diagnostic": diagnostic }),
            })?;
            let mut native_response = native_response;
            let Some(native_result) = native_response.get_mut("result").map(Value::take) else {
                return Err(BootstrapBuildError::Native {
                    message: "thread/read returned a native error".to_string(),
                    details: json!({ "native": { "message": native_response } }),
                });
            };
            json!({
                "method": "thread/read",
                "result": native_result,
            })
        }
    } else {
        Value::Null
    };
    let runtime_generation = session
        .pointer("/runtime/runtimeGeneration")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let collections = load_state(runtime_generation).map_err(BootstrapBuildError::State)?;
    let user_input_queue = required_bootstrap_array(&collections, "userInputQueue")
        .map_err(BootstrapBuildError::State)?;
    let pending_server_requests = required_bootstrap_array(&collections, "pendingServerRequests")
        .map_err(BootstrapBuildError::State)?;
    let agent_bus_messages = required_bootstrap_array(&collections, "agentBusMessages")
        .map_err(BootstrapBuildError::State)?;
    let mut resource = json!({
        "contractVersion": 2,
        "cutexSessionId": cutex_session_id,
        "hostId": session.get("hostId").cloned().unwrap_or(Value::Null),
        "checkpoint": checkpoint,
        "schema": session.pointer("/native/schema").cloned().unwrap_or(Value::Null),
        "management": session.get("management").cloned().unwrap_or(Value::Null),
        "native": native,
        "cutexState": {
            "sessionRevision": session.get("revision").cloned().unwrap_or(Value::Null),
            "runtime": {
                "status": session.pointer("/runtime/status").cloned().unwrap_or(Value::Null),
                "runtimeGeneration": runtime_generation,
            },
            "focus": session.get("focus").cloned().unwrap_or(Value::Null),
            "userInputQueue": user_input_queue,
            "userInputQueuePause": collections.get("userInputQueuePause").cloned().unwrap_or(Value::Null),
            // Delegation (委托) pause (r208, `asks-pause-v2`): {since, by, origin} | null.
            "asksPaused": collections.get("asksPaused").cloned().unwrap_or(Value::Null),
            "pendingServerRequests": pending_server_requests,
            "agentBusMessages": agent_bus_messages,
            // Agent messaging v2 (older state loaders: []).
            "agentBusSentMessages": collections.get("agentBusSentMessages").cloned().unwrap_or_else(|| json!([])),
            // Agent → owner messages of this session (owner-notify), oldest first.
            "ownerNotifications": super::owner_notify::bootstrap_records(cutex_session_id),
            // Latest accepted update_plan checklist from the current rollout
            // (`launch::native_plan`), or null.
            "latestPlan": collections.get("latestPlan").cloned().unwrap_or(Value::Null),
            // Newest native token_count from the current rollout
            // (`launch::native_token_usage`; Claude: the kept newest report,
            // `claude_token_usage`) in the thread/tokenUsage/updated params
            // shape plus updatedAt, or null.
            "latestTokenUsage": collections.get("latestTokenUsage").cloned().unwrap_or(Value::Null),
        }
    });
    if let Some(admission) = collections.get("admission") {
        resource["cutexState"]["admission"] = admission.clone();
    }
    if turn_limit.is_some() {
        // history-pages: the page of `native` (null while offline, where the
        // client reads GET /v2/sessions/{id}/history instead).
        resource["historyPage"] = history_page;
    }
    Ok(resource)
}

fn required_bootstrap_array(value: &Value, key: &str) -> anyhow::Result<Value> {
    value
        .get(key)
        .filter(|value| value.is_array())
        .cloned()
        .with_context(|| format!("bootstrap state omitted array {key}"))
}

fn handle_native_server_response(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    event_repository: &EventRepository,
    cutex_session_id: &str,
    root: bool,
) -> anyhow::Result<()> {
    let response_id_hint = response_id_hint(&request.body);
    let registry = (context.load_registry)()?;
    let Some(session) = owner_session_resource(
        cutex_session_id,
        &registry,
        context.load_runtime_status,
        event_repository,
        root,
    )?
    else {
        return write_v2_server_response_error(
            stream,
            404,
            "Not Found",
            response_id_hint.as_deref(),
            "session_not_found",
            "the owner-visible cutex session does not exist on this host",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    };
    let response = match validate_server_response_body(&request.body) {
        Ok(response) => response,
        Err(message) => {
            return write_v2_server_response_error(
                stream,
                400,
                "Bad Request",
                response_id_hint.as_deref(),
                "invalid_request",
                &message,
                false,
                json!({}),
            );
        }
    };
    let expected_runtime_generation = match request.headers.get("if-match") {
        Some(value) => match parse_runtime_generation_precondition(value) {
            Ok(generation) => generation,
            Err(message) => {
                return write_v2_server_response_error(
                    stream,
                    400,
                    "Bad Request",
                    Some(&response.response_id),
                    "invalid_precondition",
                    &message,
                    false,
                    json!({ "header": "If-Match" }),
                );
            }
        },
        None => {
            return write_v2_server_response_error(
                stream,
                428,
                "Precondition Required",
                Some(&response.response_id),
                "precondition_required",
                "If-Match must identify the expected Cutex runtime generation",
                false,
                json!({ "header": "If-Match" }),
            );
        }
    };
    let current_runtime_generation = session
        .pointer("/runtime/runtimeGeneration")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if current_runtime_generation != expected_runtime_generation {
        return write_stale_runtime_generation(
            stream,
            &response.response_id,
            expected_runtime_generation,
            Some(current_runtime_generation).filter(|generation| *generation > 0),
        );
    }
    if !session
        .pointer("/runtime/appServerConnected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return write_v2_server_response_error(
            stream,
            409,
            "Conflict",
            Some(&response.response_id),
            "session_offline",
            "the cutex session has no connected app-server",
            true,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    }
    let canonical_body: Value = serde_json::from_slice(&request.body)
        .expect("validated server response body must remain valid JSON");
    let canonical_claim = json!({
        "expectedRuntimeGeneration": expected_runtime_generation,
        "body": canonical_body,
    });
    let native_request_id = response.native_message["id"].clone();
    match server_requests::claim_response(
        cutex_session_id,
        expected_runtime_generation,
        &response.response_id,
        &canonical_claim,
        &response.native_message,
    )? {
        ServerResponseClaimDecision::Submit => {
            // "Always allow in this project" answers keep their rules once delivered.
            let pending_params = server_requests::request_native(
                cutex_session_id,
                expected_runtime_generation,
                &native_request_id,
            )
            .ok()
            .flatten()
            .and_then(|native| native.get("params").cloned());
            match (context.respond_native_server_request)(
                cutex_session_id,
                expected_runtime_generation,
                response.native_message.clone(),
            ) {
                Ok(()) => {
                    server_requests::mark_response_submitted(
                        cutex_session_id,
                        expected_runtime_generation,
                        &response.response_id,
                    )?;
                    if let (Some(params), Some(result)) = (&pending_params, response.native_message.get("result")) {
                        super::claude_permission_rules::remember_from_answer(cutex_session_id, params, result);
                    }
                    write_server_response_result(
                        stream,
                        cutex_session_id,
                        &response.response_id,
                        "submitted",
                        response.native_message,
                    )
                }
                Err(ManagementNativeForwardError::BeforeForward(diagnostic))
                | Err(ManagementNativeForwardError::NativeRequestIdInUse(diagnostic)) => {
                    server_requests::release_definitely_failed_response(
                        cutex_session_id,
                        expected_runtime_generation,
                        &response.response_id,
                    )?;
                    write_v2_server_response_error(
                        stream,
                        502,
                        "Bad Gateway",
                        Some(&response.response_id),
                        "server_request_delivery_failed",
                        "the response was not written to the current app-server connection",
                        true,
                        json!({
                            "nativeRequestId": native_request_id,
                            "runtimeGeneration": expected_runtime_generation,
                            "claimReleased": true,
                            "diagnostic": diagnostic,
                        }),
                    )
                }
                Err(ManagementNativeForwardError::StaleRuntimeGeneration { expected, actual }) => {
                    server_requests::release_definitely_failed_response(
                        cutex_session_id,
                        expected_runtime_generation,
                        &response.response_id,
                    )?;
                    write_stale_runtime_generation(stream, &response.response_id, expected, actual)
                }
                Err(ManagementNativeForwardError::OutcomeUnknown(diagnostic)) => {
                    server_requests::mark_response_indeterminate(
                        cutex_session_id,
                        expected_runtime_generation,
                        &response.response_id,
                    )?;
                    write_v2_server_response_error(
                        stream,
                        409,
                        "Conflict",
                        Some(&response.response_id),
                        "server_request_resolved",
                        "the native server request response write has an indeterminate outcome",
                        false,
                        json!({
                            "nativeRequestId": native_request_id,
                            "runtimeGeneration": expected_runtime_generation,
                            "deliveryOutcome": "unknown",
                            "diagnostic": diagnostic,
                        }),
                    )
                }
            }
        }
        ServerResponseClaimDecision::InProgress => write_v2_server_response_error(
            stream,
            202,
            "Accepted",
            Some(&response.response_id),
            "request_in_progress",
            "an identical request is still in progress",
            true,
            json!({ "retryAfterMs": 250 }),
        ),
        ServerResponseClaimDecision::Deduplicated => write_server_response_result(
            stream,
            cutex_session_id,
            &response.response_id,
            "deduplicated",
            response.native_message,
        ),
        ServerResponseClaimDecision::IdempotencyConflict => write_v2_server_response_error(
            stream,
            409,
            "Conflict",
            Some(&response.response_id),
            "idempotency_conflict",
            "responseId was already used with a different response body",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        ),
        ServerResponseClaimDecision::Resolved => write_server_request_resolved(
            stream,
            &response.response_id,
            native_request_id,
            expected_runtime_generation,
        ),
        ServerResponseClaimDecision::NotFound => write_v2_server_response_error(
            stream,
            404,
            "Not Found",
            Some(&response.response_id),
            "server_request_not_found",
            "the exact native server request is not pending for this runtime generation",
            false,
            json!({
                "nativeRequestId": native_request_id,
                "runtimeGeneration": expected_runtime_generation,
            }),
        ),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ValidatedServerResponse {
    response_id: String,
    native_message: Value,
}

fn parse_runtime_generation_precondition(value: &str) -> Result<u64, String> {
    let generation = value
        .strip_prefix("\"cutex-runtime-generation:")
        .and_then(|value| value.strip_suffix('"'))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "If-Match must be a single quoted cutex-runtime-generation token".to_string()
        })?
        .parse::<u64>()
        .map_err(|_| "If-Match runtime generation must be an integer".to_string())?;
    if generation == 0 || generation > MAX_SAFE_SEQUENCE {
        return Err("If-Match runtime generation must be a positive JSON-safe integer".to_string());
    }
    Ok(generation)
}

fn validate_server_response_body(body: &[u8]) -> Result<ValidatedServerResponse, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("invalid JSON request body: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    if object.len() != 2 || !object.contains_key("responseId") || !object.contains_key("native") {
        return Err("request body must contain exactly responseId and native".to_string());
    }
    let response_id = object
        .get("responseId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.chars().count() <= 256)
        .ok_or_else(|| {
            "responseId must be a non-empty string of at most 256 characters".to_string()
        })?
        .to_string();
    let native = object
        .get("native")
        .and_then(Value::as_object)
        .ok_or_else(|| "native must be an object".to_string())?;
    if native.len() != 1 || !native.contains_key("message") {
        return Err("native must contain exactly message".to_string());
    }
    let native_message = native
        .get("message")
        .cloned()
        .ok_or_else(|| "native.message is required".to_string())?;
    let message = native_message
        .as_object()
        .ok_or_else(|| "native.message must be an object".to_string())?;
    let id = message
        .get("id")
        .filter(|id| !id.is_null())
        .ok_or_else(|| "native.message.id is required".to_string())?;
    if !id.is_string() && id.as_i64().is_none() {
        return Err("native.message.id must be a string or signed 64-bit integer".to_string());
    }
    let has_result = message.contains_key("result");
    let has_error = message.contains_key("error");
    if has_result == has_error {
        return Err("native.message must contain exactly one of result or error".to_string());
    }
    if let Some(error) = message.get("error") {
        let error = error
            .as_object()
            .ok_or_else(|| "native.message.error must be an object".to_string())?;
        if error.get("code").and_then(Value::as_i64).is_none()
            || error.get("message").and_then(Value::as_str).is_none()
        {
            return Err(
                "native.message.error requires integer code and string message".to_string(),
            );
        }
    }
    Ok(ValidatedServerResponse {
        response_id,
        native_message,
    })
}

fn write_server_response_result(
    stream: &mut TcpStream,
    cutex_session_id: &str,
    response_id: &str,
    disposition: &str,
    native_message: Value,
) -> anyhow::Result<()> {
    write_json_response(
        stream,
        200,
        "OK",
        &json!({
            "contractVersion": 2,
            "responseId": response_id,
            "cutexSessionId": cutex_session_id,
            "disposition": disposition,
            "native": { "message": native_message },
        }),
    )
}

fn write_stale_runtime_generation(
    stream: &mut TcpStream,
    response_id: &str,
    expected_runtime_generation: u64,
    actual_runtime_generation: Option<u64>,
) -> anyhow::Result<()> {
    write_v2_server_response_error(
        stream,
        412,
        "Precondition Failed",
        Some(response_id),
        "stale_runtime_generation",
        "the app-server runtime generation no longer matches this request occurrence",
        false,
        json!({
            "expectedRuntimeGeneration": expected_runtime_generation,
            "actualRuntimeGeneration": actual_runtime_generation,
            "claimCreated": false,
            "nativeResponseForwarded": false,
        }),
    )
}

fn write_server_request_resolved(
    stream: &mut TcpStream,
    response_id: &str,
    native_request_id: Value,
    runtime_generation: u64,
) -> anyhow::Result<()> {
    write_v2_server_response_error(
        stream,
        409,
        "Conflict",
        Some(response_id),
        "server_request_resolved",
        "the native server request was already resolved",
        false,
        json!({
            "nativeRequestId": native_request_id,
            "runtimeGeneration": runtime_generation,
        }),
    )
}

fn handle_native_request(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    event_repository: &EventRepository,
    cutex_session_id: &str,
    root: bool,
) -> anyhow::Result<()> {
    let request_id_hint = request_id_hint(&request.body);
    let registry = (context.load_registry)()?;
    let Some(session) = owner_session_resource(
        cutex_session_id,
        &registry,
        context.load_runtime_status,
        event_repository,
        root,
    )?
    else {
        return write_v2_request_error(
            stream,
            404,
            "Not Found",
            request_id_hint.as_deref(),
            "session_not_found",
            "the owner-visible cutex session does not exist on this host",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    };
    let idempotency = request_idempotency_repository()?;
    handle_native_request_for_session(
        stream,
        request,
        context.forward_native_request,
        cutex_session_id,
        &session,
        idempotency,
    )
}

fn handle_native_request_for_session(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    forward_native_request: ManagementNativeRequestHandler,
    cutex_session_id: &str,
    session: &Value,
    idempotency: &RequestIdempotencyRepository,
) -> anyhow::Result<()> {
    let request_id_hint = request_id_hint(&request.body);
    let Some(bound_thread_id) = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .filter(|thread_id| !thread_id.is_empty())
    else {
        return write_v2_request_error(
            stream,
            409,
            "Conflict",
            request_id_hint.as_deref(),
            "session_offline",
            "the cutex session has no bound app-server thread",
            true,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    };
    let native_request = match validate_native_request(&request.body, bound_thread_id) {
        Ok(request) => request,
        Err(NativeRequestValidationError::Invalid(message)) => {
            return write_v2_request_error(
                stream,
                400,
                "Bad Request",
                request_id_hint.as_deref(),
                "invalid_request",
                &message,
                false,
                json!({}),
            );
        }
        Err(NativeRequestValidationError::MethodDenied(method)) => {
            return write_v2_request_error(
                stream,
                403,
                "Forbidden",
                request_id_hint.as_deref(),
                "native_method_denied",
                "the native request method is not authorized by policy version 2",
                false,
                json!({
                    "method": method,
                    "policyVersion": NATIVE_REQUEST_POLICY_VERSION,
                }),
            );
        }
        Err(NativeRequestValidationError::MissingThreadId) => {
            return write_v2_request_error(
                stream,
                400,
                "Bad Request",
                request_id_hint.as_deref(),
                "missing_thread_id",
                "the native request requires params.threadId",
                false,
                json!({}),
            );
        }
        Err(NativeRequestValidationError::ForeignThread { requested, bound }) => {
            return write_v2_request_error(
                stream,
                403,
                "Forbidden",
                request_id_hint.as_deref(),
                "foreign_thread",
                "the native request thread does not match the bound cutex session thread",
                false,
                json!({ "requestedThreadId": requested, "boundThreadId": bound }),
            );
        }
    };
    if matches!(native_request.method.as_str(), "turn/start" | "turn/steer") {
        let paused = match crate::asks::store::AskStore::open_default().and_then(|store| {
            store.inspect(|state| crate::asks::agent_pause::is_paused(state, cutex_session_id))
        }) {
            Ok(paused) => paused,
            Err(_) => {
                return write_v2_request_error(
                    stream,
                    503,
                    "Service Unavailable",
                    Some(&native_request.request_id),
                    "agent_pause_persistence_unavailable",
                    "the agent pause state could not be read",
                    true,
                    json!({ "cutexSessionId": cutex_session_id }),
                );
            }
        };
        if paused {
            return write_v2_request_error(stream, 409, "Conflict", Some(&native_request.request_id),
                "agent_paused", "the agent is paused; resume it before sending native input", true,
                json!({ "cutexSessionId": cutex_session_id }));
        }
    }
    if !session
        .pointer("/runtime/appServerConnected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return write_v2_request_error(
            stream,
            409,
            "Conflict",
            Some(&native_request.request_id),
            "session_offline",
            "the cutex session has no connected app-server",
            true,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    }
    if let Some(requested_thread_id) = native_request.descendant_thread_id.as_deref() {
        match verify_subagent_thread(
            forward_native_request,
            cutex_session_id,
            bound_thread_id,
            requested_thread_id,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return write_v2_request_error(
                    stream,
                    403,
                    "Forbidden",
                    Some(&native_request.request_id),
                    "foreign_thread",
                    "the native request thread is neither the bound cutex session thread nor one of its sub-agent threads",
                    false,
                    json!({
                        "requestedThreadId": requested_thread_id,
                        "boundThreadId": bound_thread_id,
                    }),
                );
            }
            Err(LineageReadError::Unavailable(diagnostic)) => {
                return write_v2_request_error(
                    stream,
                    502,
                    "Bad Gateway",
                    Some(&native_request.request_id),
                    "app_server_unavailable",
                    "the app-server could not be asked to verify the sub-agent thread lineage",
                    true,
                    json!({ "diagnostic": diagnostic }),
                );
            }
        }
    }

    let canonical_body: Value = serde_json::from_slice(&request.body)
        .expect("validated native request body must remain valid JSON");
    let claim = match idempotency.begin_for_method(
        &native_request.method,
        cutex_session_id,
        &native_request.request_id,
        &canonical_body,
    ) {
        Ok(BeginRequest::Forward(claim)) => claim,
        Ok(BeginRequest::InProgress) => {
            return write_v2_request_error(
                stream,
                202,
                "Accepted",
                Some(&native_request.request_id),
                "request_in_progress",
                "an identical request is still in progress",
                true,
                json!({ "retryAfterMs": 250 }),
            );
        }
        Ok(BeginRequest::Completed(mut response)) => {
            super::event_blobs::EventBlobStore::open_default()?
                .externalize_media(cutex_session_id,&mut response.body)?;
            return write_json_response(stream, response.status, &response.reason, &response.body);
        }
        Ok(BeginRequest::Conflict) => {
            return write_v2_request_error(
                stream,
                409,
                "Conflict",
                Some(&native_request.request_id),
                "idempotency_conflict",
                "requestId was already used with a different request body",
                false,
                json!({ "cutexSessionId": cutex_session_id }),
            );
        }
        Ok(BeginRequest::OutcomeUnknown) => {
            return write_request_outcome_unknown(
                stream,
                &native_request.request_id,
                json!({ "resyncRequired": true }),
            );
        }
        Ok(BeginRequest::CapacityExceeded(full)) => {
            return write_idempotency_capacity_exceeded(stream, &native_request.request_id, &full);
        }
        Err(error) => {
            return write_v2_request_error(
                stream,
                503,
                "Service Unavailable",
                Some(&native_request.request_id),
                "event_persistence_unavailable",
                "durable management request state is unavailable",
                true,
                json!({ "diagnostic": format!("{error:#}") }),
            );
        }
    };

    match forward_native_request(cutex_session_id, native_request.native_message) {
        Ok(mut native_response) => {
            if let Err(error) = super::event_blobs::EventBlobStore::open_default()
                .and_then(|store| store.externalize_media(cutex_session_id,&mut native_response)) {
                // Forwarding already happened. Do not label this request safe
                // to replay merely because media publication failed afterward.
                idempotency.mark_outcome_unknown(&claim)?;
                return write_request_outcome_unknown(stream,&native_request.request_id,
                    json!({"resyncRequired":true,"diagnostic":format!("native response media could not be stored: {error:#}")}));
            }
            let response = json!({
                "contractVersion": 2,
                "requestId": native_request.request_id,
                "cutexSessionId": cutex_session_id,
                "native": { "message": native_response },
            });
            if let Err(error) = idempotency.complete(&claim, 200, "OK", response.clone()) {
                return write_v2_request_error(
                    stream,
                    503,
                    "Service Unavailable",
                    response.get("requestId").and_then(Value::as_str),
                    "event_persistence_unavailable",
                    "the native response could not be durably recorded",
                    false,
                    json!({
                        "diagnostic": format!("{error:#}"),
                        "resyncRequired": true,
                    }),
                );
            }
            write_json_response(stream, 200, "OK", &response)
        }
        Err(ManagementNativeForwardError::BeforeForward(diagnostic)) => {
            idempotency.release_before_forward(&claim)?;
            write_v2_request_error(
                stream,
                502,
                "Bad Gateway",
                Some(&native_request.request_id),
                "app_server_unavailable",
                "the current app-server connection failed before forwarding the request",
                true,
                json!({ "diagnostic": diagnostic }),
            )
        }
        Err(ManagementNativeForwardError::NativeRequestIdInUse(diagnostic)) => {
            idempotency.release_before_forward(&claim)?;
            write_v2_request_error(
                stream,
                409,
                "Conflict",
                Some(&native_request.request_id),
                "native_request_id_in_use",
                "the exact native request id is already pending on this app-server connection",
                true,
                json!({ "diagnostic": diagnostic }),
            )
        }
        Err(ManagementNativeForwardError::StaleRuntimeGeneration { expected, actual }) => {
            idempotency.release_before_forward(&claim)?;
            write_v2_request_error(
                stream,
                409,
                "Conflict",
                Some(&native_request.request_id),
                "session_runtime_changed",
                "the app-server runtime changed before forwarding the native request",
                true,
                json!({
                    "expectedRuntimeGeneration": expected,
                    "actualRuntimeGeneration": actual,
                    "resyncRequired": true,
                }),
            )
        }
        Err(ManagementNativeForwardError::OutcomeUnknown(diagnostic)) => {
            idempotency.mark_outcome_unknown(&claim)?;
            write_request_outcome_unknown(
                stream,
                &native_request.request_id,
                json!({ "resyncRequired": true, "diagnostic": diagnostic }),
            )
        }
    }
}

/// Verifies (and caches) that `thread_id` is a transitive sub-agent of the
/// bound thread by walking native `thread/read` `parentThreadId` links.
fn verify_subagent_thread(
    forward_native_request: ManagementNativeRequestHandler,
    cutex_session_id: &str,
    bound_thread_id: &str,
    thread_id: &str,
) -> Result<bool, LineageReadError> {
    subagent_lineage::verify_descendant(cutex_session_id, bound_thread_id, thread_id, |current| {
        let message = json!({
            "id": format!("cutex-lineage-{}", uuid::Uuid::new_v4()),
            "method": "thread/read",
            "params": { "threadId": current, "includeTurns": false },
        });
        let response = forward_native_request(cutex_session_id, message).map_err(|error| {
            LineageReadError::Unavailable(match error {
                ManagementNativeForwardError::BeforeForward(message)
                | ManagementNativeForwardError::NativeRequestIdInUse(message)
                | ManagementNativeForwardError::OutcomeUnknown(message) => message,
                ManagementNativeForwardError::StaleRuntimeGeneration { expected, actual } => {
                    format!("runtime generation changed (expected {expected}, actual {actual:?})")
                }
            })
        })?;
        Ok(response.pointer("/result/thread").cloned())
    })
}

#[derive(Clone, Copy)]
enum SubmitStage {
    SessionMutex,
    SessionResource,
    IdempotencyBegin,
    Dispatch,
    IdempotencyComplete,
    ResponseWrite,
}

/// Only slow, validated submit requests emit a bounded metadata-only line.
/// These are monotonic call durations, not the SQLite receipt's wall-clock
/// timestamps: complete() includes its actual UPDATE/commit/checkpoint wait.
struct SlowSubmitTrace<'lock> {
    started: Instant,
    stage_started: Instant,
    stage: SubmitStage,
    elapsed: [Duration; 6],
    ids: Option<(String, String)>,
    status: u16,
    finished: Option<Instant>,
    request_guard: Option<super::session_coordination::OperationReservation<'lock>>,
    control: Option<super::control_timing::Trace>,
}

fn handle_control_diagnostics(stream: &mut TcpStream, request: &SimpleHttpRequest, root: Option<&str>) -> anyhow::Result<()> {
    if !authenticated_root_request(request, root)
        || crate::management::users::current_principal().is_some_and(|principal| !principal.is_owner()) {
        return write_v2_error(stream, 401, "Unauthorized", "unauthorized", "the owner credential is required", false, json!({}));
    }
    if request.method != "GET" {
        return write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed", "use GET", false, json!({}));
    }
    write_json_response(stream, 200, "OK", &json!({"contractVersion":2,
        "scope":"management_process", "timing":super::control_timing::snapshot(),
        "asksStoreLocks":crate::asks::store::diagnostics::snapshot(),
        "capacity":super::control_capacity::snapshot(),
        "queues":{"repository":super::repository::queue_diagnostics(),
            "nativeEvents":crate::app_server::client::event_queue_diagnostics()}}))
}

impl<'lock> SlowSubmitTrace<'lock> {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            stage_started: now,
            stage: SubmitStage::SessionMutex,
            elapsed: [Duration::ZERO; 6],
            ids: None,
            status: 500,
            finished: None,
            request_guard: None,
            control: None,
        }
    }

    fn enable(&mut self, request_id: &str, session_id: &str) {
        fn identifier(value: &str) -> String {
            value
                .chars()
                .take(96)
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "-_.:".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        }
        self.ids = Some((identifier(request_id), identifier(session_id)));
    }

    fn advance(&mut self, stage: SubmitStage) {
        self.advance_at(stage, Instant::now());
    }

    fn advance_at(&mut self, stage: SubmitStage, now: Instant) {
        if matches!(stage, SubmitStage::ResponseWrite) {
            // The operation and its durable receipt are finished. A slow
            // reader must not hold this session's business serialization.
            self.request_guard.take();
        }
        if let Some(control) = self.control.as_mut() {
            use super::control_timing::Stage;
            control.stage(match stage {
                SubmitStage::SessionMutex => Stage::LockWait,
                SubmitStage::SessionResource => Stage::StoreRead,
                SubmitStage::IdempotencyBegin | SubmitStage::IdempotencyComplete => Stage::StoreWrite,
                SubmitStage::Dispatch => Stage::Dispatch,
                SubmitStage::ResponseWrite => Stage::ResponseWrite,
            });
        }
        self.elapsed[self.stage as usize] += now.duration_since(self.stage_started);
        self.stage_started = now;
        self.stage = stage;
    }

    fn finish_response(&mut self, response: anyhow::Result<()>) -> anyhow::Result<()> {
        self.finished = Some(Instant::now());
        if response.is_err() {
            self.status = 0;
        } // no complete HTTP response
        if let Some(control) = self.control.as_mut() { control.status(self.status); }
        response
    }

    fn line_at(&self, now: Instant) -> Option<String> {
        let now = self.finished.unwrap_or(now);
        let (request, session) = self.ids.as_ref()?;
        let total = now.duration_since(self.started);
        if total < Duration::from_secs(1) {
            return None;
        }
        let mut elapsed = self.elapsed;
        elapsed[self.stage as usize] += now.duration_since(self.stage_started);
        Some(format!(
            "cutex-submit-slow request={request} session={session} status={} total_ms={} session_mutex_ms={} session_resource_ms={} idempotency_begin_ms={} dispatch_ms={} idempotency_complete_ms={} response_write_ms={}",
            self.status, total.as_millis(), elapsed[0].as_millis(), elapsed[1].as_millis(),
            elapsed[2].as_millis(), elapsed[3].as_millis(), elapsed[4].as_millis(), elapsed[5].as_millis()))
    }
}

impl Drop for SlowSubmitTrace<'_> {
    fn drop(&mut self) {
        self.request_guard.take();
        if let Some(control) = self.control.as_mut() { control.status(self.status); }
        if let Some(line) = self.line_at(Instant::now()) {
            // Best effort; no new file, fsync, or diagnostic repository lock.
            let _ = writeln!(std::io::stderr().lock(), "{line}");
        }
    }
}

fn handle_cutex_request(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    event_repository: &EventRepository,
    cutex_session_id: &str,
    owner: Option<&crate::management::control_plane::HumanManagementPrincipal>,
) -> anyhow::Result<()> {
    let request_lock = cutex_request_lock(cutex_session_id)?;
    let request_id_hint = request_id_hint(&request.body);
    let cutex_request = match validate_cutex_request_body(&request.body) {
        Ok(request) => request,
        Err(error) => {
            return write_cutex_request_validation_error(stream, request_id_hint.as_deref(), error);
        }
    };
    // Safety grants are the owner's: refused before any state is read.
    if super::safety_checks::is_command(&cutex_request.method) && owner.is_none() {
        return write_v2_request_error(
            stream,
            403,
            "Forbidden",
            Some(&cutex_request.request_id),
            "cutex_method_denied",
            "safety check decisions are the owner's (Management root credential)",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    }
    // Start the lock clock after pure validation, so the legacy lock-wait
    // field does not accidentally include JSON parsing or permission checks.
    let mut trace = SlowSubmitTrace::new();
    if cutex_request.method == "cutex/userInput/submit" {
        trace.enable(&cutex_request.request_id, cutex_session_id);
    }
    use super::control_timing::{Path as ControlPath, Stage as ControlStage, Trace as ControlTrace};
    let mut control = ControlTrace::new(if cutex_request.method == "cutex/userInput/submit" { ControlPath::UserInput } else { ControlPath::ControlOther });
    control.stage(ControlStage::LockWait);
    trace.control = Some(control);
    // Pure reads observe repository snapshots without waiting for a native
    // mutation. Mutations retain an execution capability through durable
    // settlement, but no coordinator mutex during reads, RPC or fanout.
    if super::request_idempotency::method_needs_idempotency(&cutex_request.method) {
        trace.request_guard = Some(request_lock.reserve()?);
    }
    trace.advance(SubmitStage::SessionResource);
    let registry = (context.load_registry)()?;
    // Online input only needs its bound native thread and connected runtime.
    // Keep UI projections and their shared activity/pending/cache reads out
    // of this session's serialized submit path. A changing/offline binding
    // falls back to the existing full resource used by offline start plans.
    let online_submit = if matches!(cutex_request.method.as_str(),"cutex/userInput/queue/list"|"cutex/focus/get") {
        super::session::read_route_resource(cutex_session_id,&registry,
            !request_may_resolve_hidden_session(&cutex_request.method,owner.is_some()))?
    } else if cutex_request.method == "cutex/userInput/submit" {
        super::session::online_submit_resource(
            cutex_session_id, &registry, context.load_runtime_status,
            !request_may_resolve_hidden_session(&cutex_request.method, owner.is_some()),
        )?
    } else { None };
    let session = if let Some(session) = online_submit {
        Some(session)
    } else if matches!(
        cutex_request.method.as_str(),
        "cutex/session/retire" | "cutex/session/restore" | super::session_delete::METHOD
    ) {
        session_resource_including_archive(
            cutex_session_id,
            &registry,
            context.load_runtime_status,
            event_repository,
        )?
    } else if request_may_resolve_hidden_session(&cutex_request.method, owner.is_some()) {
        session_resource_including_hidden(
            cutex_session_id,
            &registry,
            context.load_runtime_status,
            event_repository,
        )?
    } else {
        session_resource(
            cutex_session_id,
            &registry,
            context.load_runtime_status,
            event_repository,
        )?
    };
    let Some(session) = session else {
        trace.status = 404;
        trace.advance(SubmitStage::ResponseWrite);
        return trace.finish_response(write_v2_request_error(
            stream,
            404,
            "Not Found",
            request_id_hint.as_deref(),
            "session_not_found",
            "the owner-visible cutex session does not exist on this host",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        ));
    };
    trace.advance(SubmitStage::IdempotencyBegin);
    let canonical_body: Value = serde_json::from_slice(&request.body)
        .expect("validated cutex request body must remain valid JSON");
    let idempotency = request_idempotency_repository()?;
    let begin = idempotency.begin_for_method(
        &cutex_request.method,
        cutex_session_id,
        &cutex_request.request_id,
        &canonical_body,
    );
    let claim = match begin {
        Ok(BeginRequest::Forward(claim)) => claim,
        Ok(BeginRequest::InProgress) => {
            trace.status = 202;
            trace.advance(SubmitStage::ResponseWrite);
            return trace.finish_response(write_v2_request_error(
                stream,
                202,
                "Accepted",
                Some(&cutex_request.request_id),
                "request_in_progress",
                "an identical request is still in progress",
                true,
                json!({ "retryAfterMs": 250 }),
            ));
        }
        Ok(BeginRequest::Completed(response)) => {
            trace.status = response.status;
            trace.advance(SubmitStage::ResponseWrite);
            return trace.finish_response(write_json_response(
                stream,
                response.status,
                &response.reason,
                &response.body,
            ));
        }
        Ok(BeginRequest::Conflict) => {
            trace.status = 409;
            trace.advance(SubmitStage::ResponseWrite);
            return trace.finish_response(write_v2_request_error(
                stream,
                409,
                "Conflict",
                Some(&cutex_request.request_id),
                "idempotency_conflict",
                "requestId was already used with a different request body",
                false,
                json!({ "cutexSessionId": cutex_session_id }),
            ));
        }
        Ok(BeginRequest::OutcomeUnknown) => {
            trace.status = 409;
            trace.advance(SubmitStage::ResponseWrite);
            return trace.finish_response(write_request_outcome_unknown(
                stream,
                &cutex_request.request_id,
                json!({ "resyncRequired": true }),
            ));
        }
        Ok(BeginRequest::CapacityExceeded(full)) => {
            trace.status = 503;
            trace.advance(SubmitStage::ResponseWrite);
            return trace.finish_response(write_idempotency_capacity_exceeded(
                stream,
                &cutex_request.request_id,
                &full,
            ));
        }
        Err(error) => {
            trace.status = 503;
            trace.advance(SubmitStage::ResponseWrite);
            return trace.finish_response(write_v2_request_error(
                stream,
                503,
                "Service Unavailable",
                Some(&cutex_request.request_id),
                "event_persistence_unavailable",
                "durable management request state is unavailable",
                true,
                json!({ "diagnostic": format!("{error:#}") }),
            ));
        }
    };

    trace.advance(SubmitStage::Dispatch);
    // Safety cancellation preflight uses try-locks and latches retry disabled on
    // failure. The original dispatcher below keeps its native concurrency guard;
    // errors still converge through normal operation/idempotency settlement.
    let operation = super::model_overload_retry::dispatch_after_cancel_with_reservation(cutex_session_id, &cutex_request.method, trace.request_guard.as_ref(), || { if cutex_request.method == "cutex/runtime/online" {
        dispatch_owner_runtime_online(
            event_repository,
            |id, _method, params| (context.online_session)(owner, id, params),
            crate::asks::wake::note_owner_start,
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if cutex_request.method == super::profile_switch::METHOD {
        super::profile_switch::dispatch(
            event_repository,
            context,
            owner,
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if cutex_request.method == super::history_revert::METHOD {
        super::history_revert::dispatch(
            event_repository,
            context.revert_session_history,
            owner,
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if cutex_request.method == super::files_revert::METHOD {
        super::files_revert::dispatch(event_repository, context, owner, cutex_session_id, &session, &cutex_request)
    } else if cutex_request.method == super::session_delete::METHOD {
        super::session_delete::dispatch(
            event_repository,
            context.delete_session,
            owner,
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if cutex_request.method == "cutex/session/retire" {
        super::session_delete::dispatch_retire(
            event_repository,
            context.delete_session,
            context.mutate_session,
            owner,
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if super::asks::is_command(&cutex_request.method) {
        super::asks::dispatch_command(owner, cutex_session_id, &cutex_request.method, &cutex_request.params)
    } else if cutex_request.method == super::session_fork::METHOD {
        super::session_fork::dispatch(
            event_repository,
            context,
            owner,
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if super::queue_order::is_method(&cutex_request.method) {
        user_input_repository().map_err(persistence_user_input_error).and_then(|queue| {
            super::queue_order::dispatch(event_repository, queue, context.steer_user_input_queue,
                cutex_session_id, &session, &cutex_request)
        }).inspect(|result| {
            // A steered / started queue item is the owner's next message.
            if let Some(disposition) = result.get("disposition").and_then(Value::as_str).filter(|_| owner_message_landed(result)) {
                crate::asks::control::owner_message_default(cutex_session_id, disposition);
            }
        })
    } else if super::queue_pause::is_method(&cutex_request.method) {
        user_input_repository()
            .map_err(persistence_user_input_error)
            .and_then(|queue| {
                super::queue_pause::dispatch(
                    event_repository,
                    queue,
                    context.interrupt_active_turn,
                    context.flush_user_input_queue,
                    cutex_session_id,
                    &session,
                    &cutex_request,
                )
            })
    } else if super::safety_checks::is_command(&cutex_request.method) {
        super::safety_checks::dispatch(
            event_repository,
            context,
            owner.copied(),
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if super::owner_notify::is_command(&cutex_request.method) {
        super::owner_notify::dispatch(
            event_repository,
            context,
            owner.copied(),
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if cutex_request.method == super::agent_bus_wake::METHOD {
        super::agent_bus_wake::dispatch(
            event_repository,
            context,
            owner.copied(),
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } else if cutex_request.method == "cutex/userInput/submit" {
        let result = super::offline_submit::dispatch(
            event_repository,
            context,
            owner.copied(),
            cutex_session_id,
            &session,
            &cutex_request,
        );
        // Owner decision 2026-10-08: the owner's next message ends a
        // delegation pause of this session.
        if let Some(disposition) = result.as_ref().ok().filter(|result| owner_message_landed(result))
            .and_then(|result| result.get("disposition").and_then(Value::as_str))
        {
            crate::asks::control::owner_message_default(cutex_session_id, disposition);
        }
        result
    } else {
        dispatch_cutex_request(
            event_repository,
            context.handle_user_input,
            context.flush_user_input_queue,
            context.mutate_session,
            cutex_session_id,
            &session,
            &cutex_request,
        )
    } });
    trace.advance(SubmitStage::IdempotencyComplete);
    if operation.is_ok() {
        // The owner acted on the agent: a pending quota continuation ends.
        if let Some(reason) = super::quota_continue::cancels_pending(&cutex_request.method) {
            super::quota_continue::cancel_default(cutex_session_id, reason);
        }
    }
    match operation {
        Ok(result) => {
            let response = json!({
                "contractVersion": 2,
                "requestId": cutex_request.request_id,
                "cutexSessionId": cutex_session_id,
                "cutex": {
                    "method": cutex_request.method,
                    "result": result,
                }
            });
            if let Err(error) = idempotency.complete(&claim, 200, "OK", response.clone()) {
                return write_post_cutex_operation_outcome_unknown_traced(
                    stream,
                    idempotency,
                    &claim,
                    &cutex_request.request_id,
                    error,
                    Some(&mut trace),
                );
            }
            trace.status = 200;
            trace.advance(SubmitStage::ResponseWrite);
            trace.finish_response(write_json_response(stream, 200, "OK", &response))
        }
        Err(error) => write_cutex_operation_error_traced(
            stream,
            idempotency,
            &claim,
            &cutex_request.request_id,
            error,
            Some(&mut trace),
            Some(&cutex_request.params),
        ),
    }
}

/// A `cutex/userInput/submit` result that put an owner message into the
/// session: started, steered or queued (not a deduplicated replay).
pub(super) fn owner_message_landed(result: &Value) -> bool {
    matches!(
        result.get("disposition").and_then(Value::as_str),
        Some("started" | "steered" | "queued")
    )
}

fn write_cutex_request_validation_error(
    stream: &mut TcpStream,
    request_id: Option<&str>,
    error: CutexRequestValidationError,
) -> anyhow::Result<()> {
    match error {
        CutexRequestValidationError::Invalid(message) => write_v2_request_error(
            stream,
            400,
            "Bad Request",
            request_id,
            "invalid_request",
            &message,
            false,
            json!({}),
        ),
        CutexRequestValidationError::UnsupportedMethod(method) => write_v2_request_error(
            stream,
            403,
            "Forbidden",
            request_id,
            "cutex_method_denied",
            "the cutex request method is not present in the advertised registry",
            false,
            json!({
                "method": method,
                "registryVersion": super::session::CUTEX_METHOD_REGISTRY_VERSION,
            }),
        ),
    }
}

pub(super) fn write_cutex_operation_error_for(
    stream: &mut TcpStream,
    idempotency: &RequestIdempotencyRepository,
    claim: &RequestClaim,
    request_id: &str,
    error: super::user_input::UserInputExecutionError,
) -> anyhow::Result<()> {
    write_cutex_operation_error(stream, idempotency, claim, request_id, error)
}

pub(super) fn human_agent_error_status(code: &str) -> u16 {
    cutex_operation_error_status(code, false)
}

pub(super) fn http_reason_for(status: u16) -> &'static str {
    http_reason(status)
}

fn write_cutex_operation_error(
    stream: &mut TcpStream,
    idempotency: &RequestIdempotencyRepository,
    claim: &RequestClaim,
    request_id: &str,
    error: super::user_input::UserInputExecutionError,
) -> anyhow::Result<()> {
    write_cutex_operation_error_traced(stream, idempotency, claim, request_id, error, None, None)
}

fn write_cutex_operation_error_traced(
    stream: &mut TcpStream,
    idempotency: &RequestIdempotencyRepository,
    claim: &RequestClaim,
    request_id: &str,
    error: super::user_input::UserInputExecutionError,
    mut trace: Option<&mut SlowSubmitTrace>,
    correlation: Option<&Value>,
) -> anyhow::Result<()> {
    let diagnostic = super::request_idempotency::UnknownDiagnostic::new(&error.code, &error.stage)
        .with_correlation(&error.details)
        .with_correlation(correlation.unwrap_or(&Value::Null));
    let status = cutex_operation_error_status(&error.code, error.outcome_unknown);
    let reason = http_reason(status);
    let outcome_unknown = error.outcome_unknown;
    let generic_unknown = error.outcome_unknown && error.code != "revert_partial";
    let response_code = if generic_unknown {
        "request_outcome_unknown".to_string()
    } else {
        error.code.clone()
    };
    let response_message = if generic_unknown {
        "the cutex operation completed far enough that its durable outcome is unknown".to_string()
    } else {
        error.message.clone()
    };
    let mut body = json!({
        "contractVersion": 2,
        "requestId": request_id,
        "error": {
            "source": "cutex",
            "code": response_code,
            "message": response_message,
            "retryable": if outcome_unknown { false } else { error.retryable },
            "details": if generic_unknown {
                json!({
                    "resyncRequired": true,
                    "originalCode": error.code,
                    "originalDetails": error.details,
                })
            } else {
                error.details
            },
        }
    });
    if outcome_unknown {
        if let Err(mark_error) = idempotency.mark_outcome_unknown_diagnostic(claim, &diagnostic) {
            body.pointer_mut("/error/details")
                .and_then(Value::as_object_mut)
                .expect("outcome-unknown details are an object")
                .insert(
                    "idempotencyDiagnostic".to_string(),
                    Value::String(format!("{mark_error:#}")),
                );
        }
    } else if error.code == "user_input_in_progress" {
        // Nothing was sent: a retry with the same requestId must run again
        // (not replay this transient answer).
        idempotency.release_before_forward(claim)?;
    } else {
        idempotency.complete(claim, status, reason, body.clone())?;
    }
    if let Some(trace) = trace.as_mut() {
        trace.status = status;
        trace.advance(SubmitStage::ResponseWrite);
    }
    let response = write_json_response(stream, status, reason, &body);
    if let Some(trace) = trace {
        trace.finish_response(response)
    } else {
        response
    }
}

fn write_post_cutex_operation_outcome_unknown_traced(
    stream: &mut TcpStream,
    idempotency: &RequestIdempotencyRepository,
    claim: &RequestClaim,
    request_id: &str,
    persistence_error: anyhow::Error,
    mut trace: Option<&mut SlowSubmitTrace>,
) -> anyhow::Result<()> {
    let mut details = json!({
        "resyncRequired": true,
        "originalCode": "response_persistence_failed",
        "diagnostic": format!("{persistence_error:#}"),
    });
    let diagnostic = super::request_idempotency::UnknownDiagnostic::new(
        "response_persistence_failed", "response_persistence",
    );
    if let Err(mark_error) = idempotency.mark_outcome_unknown_diagnostic(claim, &diagnostic) {
        details
            .as_object_mut()
            .expect("outcome-unknown details are an object")
            .insert(
                "idempotencyDiagnostic".to_string(),
                Value::String(format!("{mark_error:#}")),
            );
    }
    if let Some(trace) = trace.as_mut() {
        trace.status = 409;
        trace.advance(SubmitStage::ResponseWrite);
    }
    let response = write_v2_request_error(
        stream,
        409,
        "Conflict",
        Some(request_id),
        "request_outcome_unknown",
        "the cutex operation completed but its response could not be durably recorded",
        false,
        details,
    );
    if let Some(trace) = trace {
        trace.finish_response(response)
    } else {
        response
    }
}

/// Exact-ID session resolution for read/native routes. The Management root
/// credential is the Human owner's full view, so it also resolves hidden
/// (local-only, not IM-exposed) active durable sessions; persistent agents
/// resolve for both since `im-exposure-retired`. Ordinary bridge callers keep
/// the listing-visibility rule.
fn owner_session_resource(
    cutex_session_id: &str,
    registry: &crate::im::registry::ImRegistry,
    load_runtime_status: super::session::RuntimeStatusLoader,
    repository: &EventRepository,
    root: bool,
) -> anyhow::Result<Option<Value>> {
    if root {
        session_resource_including_hidden(cutex_session_id, registry, load_runtime_status, repository)
    } else {
        session_resource(cutex_session_id, registry, load_runtime_status, repository)
    }
}

fn request_may_resolve_hidden_session(method: &str, authenticated_owner: bool) -> bool {
    // Listing visibility is not permission: authenticated Human owners can address
    // active durable Agents by exact ID. Method and archive guards still apply.
    authenticated_owner || method == "cutex/session/visibility/show"
}

fn cutex_request_lock(cutex_session_id: &str) -> anyhow::Result<Arc<super::session_coordination::OperationLane>> {
    // One lane for HTTP mutations, native input/queue, asks, ingress and
    // revert: an independent outer gate could let a lifecycle change cross
    // an already authorized native send. Nested calls retain the same token.
    super::user_input::session_input_lock(cutex_session_id)
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ValidatedCutexRequest {
    pub(super) request_id: String,
    pub(super) method: String,
    pub(super) params: Value,
}

#[derive(Debug, Clone, PartialEq)]
enum CutexRequestValidationError {
    Invalid(String),
    UnsupportedMethod(String),
}

fn validate_cutex_request_body(
    body: &[u8],
) -> Result<ValidatedCutexRequest, CutexRequestValidationError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        CutexRequestValidationError::Invalid(format!("invalid JSON request body: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        CutexRequestValidationError::Invalid("request body must be a JSON object".to_string())
    })?;
    if object.len() != 3
        || !object.contains_key("requestId")
        || !object.contains_key("method")
        || !object.contains_key("params")
    {
        return Err(CutexRequestValidationError::Invalid(
            "request body must contain exactly requestId, method, and params".to_string(),
        ));
    }
    let request_id = object
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.chars().count() <= 256)
        .ok_or_else(|| {
            CutexRequestValidationError::Invalid(
                "requestId must be a non-empty string of at most 256 characters".to_string(),
            )
        })?
        .to_string();
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| method.starts_with("cutex/") && method.matches('/').count() >= 2)
        .ok_or_else(|| {
            CutexRequestValidationError::Invalid(
                "method must be a namespaced cutex method".to_string(),
            )
        })?
        .to_string();
    if !cutex_method_is_registered(&method) {
        return Err(CutexRequestValidationError::UnsupportedMethod(method));
    }
    let params = object
        .get("params")
        .filter(|params| params.is_object())
        .cloned()
        .ok_or_else(|| {
            CutexRequestValidationError::Invalid("params must be an object".to_string())
        })?;
    super::contract_validation::validate_cutex_request(&value)
        .map_err(CutexRequestValidationError::Invalid)?;
    Ok(ValidatedCutexRequest {
        request_id,
        method,
        params,
    })
}

fn dispatch_cutex_request(
    event_repository: &EventRepository,
    handle_user_input: ManagementUserInputHandler,
    flush_user_input_queue: ManagementUserInputQueueFlusher,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    match request.method.as_str() {
        "cutex/session/get" => {
            require_empty_params(&request.params)?;
            Ok(session.clone())
        }
        "cutex/session/retire" => super::archive::dispatch_session_retire(
            event_repository,
            mutate_session,
            cutex_session_id,
            session,
            request,
        ),
        "cutex/session/restore" => super::archive::dispatch_session_restore(
            event_repository,
            mutate_session,
            cutex_session_id,
            session,
            request,
        ),
        "cutex/session/groups/get" => {
            require_empty_params(&request.params)?;
            Ok(json!({
                "revision": session.get("revision").cloned().unwrap_or(json!(1)),
                "groups": session.pointer("/runtimeDefaults/groups").cloned().unwrap_or(json!([])),
            }))
        }
        "cutex/session/defaults/update" => dispatch_session_defaults_update(
            event_repository,
            mutate_session,
            cutex_session_id,
            session,
            request,
        ),
        "cutex/session/profile/set" | "cutex/session/profile/clear" => {
            dispatch_session_profile_mutation(
                event_repository,
                mutate_session,
                cutex_session_id,
                session,
                request,
            )
        }
        "cutex/session/metadata/update" => dispatch_session_metadata_update(
            event_repository,
            mutate_session,
            cutex_session_id,
            session,
            request,
        ),
        "cutex/session/groups/set" | "cutex/session/groups/add" | "cutex/session/groups/remove" => {
            dispatch_session_groups_mutation(
                event_repository,
                mutate_session,
                cutex_session_id,
                session,
                request,
            )
        }
        "cutex/session/visibility/show" | "cutex/session/visibility/hide" => {
            dispatch_session_visibility_mutation(
                event_repository,
                mutate_session,
                cutex_session_id,
                session,
                request,
            )
        }
        "cutex/runtime/online" | "cutex/runtime/offline" | "cutex/runtime/close" => {
            // Asks (委托): an agent the owner took offline is never auto-woken.
            crate::asks::wake::note_owner_runtime_action_default(
                cutex_session_id,
                request.method != "cutex/runtime/online",
            );
            dispatch_runtime_mutation(
                event_repository,
                mutate_session,
                cutex_session_id,
                session,
                request,
            )
        }
        "cutex/focus/get" => {
            require_empty_params(&request.params)?;
            focus_resource(cutex_session_id).map_err(persistence_user_input_error)
        }
        "cutex/focus/set" => dispatch_focus_set(event_repository, cutex_session_id, request),
        "cutex/focus/clear" => dispatch_focus_clear(event_repository, cutex_session_id, request),
        "cutex/userInput/submit" => dispatch_user_input_submit(
            event_repository,
            handle_user_input,
            cutex_session_id,
            session,
            request,
        ),
        "cutex/userInput/queue/list" => {
            require_empty_params(&request.params)?;
            let (revision, items, pause, admission) = user_input_repository()
                .and_then(|repository| repository.list_admission(cutex_session_id))
                .map_err(persistence_user_input_error)?;
            let (paused, pause) = super::queue_pause::pause_fields(pause.as_ref());
            Ok(json!({ "revision": revision, "items": items, "admission": admission, "paused": paused, "pause": pause, "asksPaused": crate::asks::control::pause_state_default(cutex_session_id) }))
        }
        "cutex/userInput/queue/update" => {
            dispatch_queue_update(event_repository, cutex_session_id, request)
        }
        "cutex/userInput/queue/remove" => {
            dispatch_queue_remove(event_repository, cutex_session_id, request)
        }
        "cutex/userInput/queue/flush" => {
            let result = dispatch_queue_flush(cutex_session_id, request, flush_user_input_queue)?;
            // An explicitly sent queue item is the owner's next message: it
            // ends a delegation pause or stop hold like a submit that started.
            if result["affectedQueueIds"].as_array().is_some_and(|ids| !ids.is_empty()) {
                crate::asks::control::owner_message_default(cutex_session_id, "started");
            }
            Ok(result)
        }
        _ => Err(super::user_input::UserInputExecutionError {
            stage: "route".to_string(),
            code: "cutex_method_denied".to_string(),
            message: "the cutex request method is not present in the advertised registry"
                .to_string(),
            retryable: false,
            details: json!({
                "method": request.method,
                "registryVersion": super::session::CUTEX_METHOD_REGISTRY_VERSION,
            }),
            outcome_unknown: false,
        }),
    }
}

/// The owner's `cutex/runtime/online` (HTTP). It bypasses
/// [`dispatch_cutex_request`], so it clears the asks owner-stop mark here:
/// an agent the owner brought online again may be auto-woken / relaunched
/// for delegations after a later unclean stop (`asks::wake::decide`
/// `ownerStopped`). Cleared once the start succeeded: clearing first let an
/// asks relaunch race the owner's own start.
fn dispatch_owner_runtime_online(
    event_repository: &EventRepository,
    online_session: impl Fn(
        &str,
        &str,
        Value,
    ) -> Result<Value, super::user_input::UserInputExecutionError>,
    clear_owner_stop: impl FnOnce(&str),
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let result = dispatch_runtime_mutation(event_repository, online_session, cutex_session_id, session, request)?;
    if result["status"] == "online" {
        clear_owner_stop(cutex_session_id);
    }
    Ok(result)
}

pub(super) fn dispatch_runtime_mutation(
    event_repository: &EventRepository,
    mutate_session: impl Fn(
        &str,
        &str,
        Value,
    ) -> Result<Value, super::user_input::UserInputExecutionError>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let object = request
        .params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    let online = request.method == "cutex/runtime/online";
    let max_fields = 4;
    if object.len() > max_fields
        || object.keys().any(|key| {
            !matches!(key.as_str(), "expectedRuntimeGeneration" | "reason")
                && !if online {
                    matches!(key.as_str(), "openVisibleTerminal" | "launchProfile")
                } else {
                    matches!(key.as_str(), "force" | "requireDefaultLaunch")
                }
        })
    {
        return Err(invalid_user_input_error(
            "runtime params contain a field outside the method's v2 schema",
        ));
    }
    if object
        .get("requireDefaultLaunch")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid_user_input_error(
            "requireDefaultLaunch must be boolean",
        ));
    }
    // Optional generation fence (If-Match equivalent for runtime mutations):
    // omitted applies to the current runtime; supplied values are checked.
    let current_generation = session
        .pointer("/runtime/runtimeGeneration")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let supplied_generation = optional_safe_integer_param(object, "expectedRuntimeGeneration")?;
    if let Some(expected_generation) =
        supplied_generation.filter(|expected| *expected != current_generation)
    {
        return Err(super::user_input::UserInputExecutionError {
            stage: "route".to_string(),
            code: "revision_conflict".to_string(),
            message: format!(
                "runtime generation conflict: expected {expected_generation}, current {current_generation}"
            ),
            retryable: true,
            details: json!({
                "expectedRuntimeGeneration": expected_generation,
                "currentRuntimeGeneration": current_generation,
                "resyncRequired": true,
            }),
            outcome_unknown: false,
        });
    }
    let reason = match object.get("reason") {
        Some(Value::String(reason)) => Some(reason.clone()),
        Some(_) => return Err(invalid_user_input_error("reason must be a string")),
        None => None,
    };
    let optional_boolean_key = if online {
        "openVisibleTerminal"
    } else {
        "force"
    };
    if object
        .get(optional_boolean_key)
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid_user_input_error(&format!(
            "{optional_boolean_key} must be a boolean"
        )));
    }
    let launch_profile = match object.get("launchProfile") {
        Some(Value::String(profile)) if !profile.trim().is_empty() => {
            Some(profile.trim().to_string())
        }
        Some(_) => {
            return Err(invalid_user_input_error(
                "launchProfile must be a non-empty string",
            ))
        }
        None => None,
    };
    let target_generation = current_generation;
    let backend = session
        .pointer("/runtime/backend")
        .and_then(Value::as_str)
        .unwrap_or("host");
    let runtime_agent_id = session.pointer("/runtime/runtimeAgentId").cloned();
    let (requested_method, requested_status, terminal_method) = match request.method.as_str() {
        "cutex/runtime/online" => (
            "cutex/runtime/onlineRequested",
            "starting",
            "cutex/runtime/online",
        ),
        "cutex/runtime/offline" => (
            "cutex/runtime/offlineRequested",
            "closing",
            "cutex/runtime/offline",
        ),
        "cutex/runtime/close" => ("cutex/runtime/closing", "closing", "cutex/runtime/closed"),
        _ => unreachable!(),
    };
    let correlation = super::model::EventCorrelation {
        thread_id: session
            .pointer("/native/threadId")
            .and_then(Value::as_str)
            .map(str::to_string),
        management_request_id: Some(request.request_id.clone()),
        ..Default::default()
    };
    append_cutex_event(
        event_repository,
        cutex_session_id,
        correlation.clone(),
        requested_method,
        runtime_event_params(
            target_generation,
            backend,
            requested_status,
            runtime_agent_id.clone(),
            reason.clone(),
        ),
    )
    .map_err(persistence_user_input_error)?;

    let mut mutation_params = request.params.clone();
    if let Some(profile) = launch_profile.as_deref() {
        mutation_params
            .as_object_mut()
            .expect("validated runtime params object")
            .insert("launchProfile".to_string(), json!(profile));
    }
    if online {
        // The runtime is about to own the native home: stop any offline
        // history reader still warm on it.
        super::history::evict_offline_reader(cutex_session_id);
    }
    let result = match mutate_session(cutex_session_id, &request.method, mutation_params) {
        Ok(result) => result,
        Err(error) => {
            let mut failed_params = runtime_event_params(
                target_generation,
                backend,
                "error",
                runtime_agent_id.clone(),
                reason.clone(),
            );
            failed_params
                .as_object_mut()
                .expect("runtime failed params object")
                .insert(
                    "error".to_string(),
                    json!({
                        "source": "cutex",
                        "code": error.code,
                        "message": error.message,
                        "retryable": error.retryable,
                        "details": error.details,
                    }),
                );
            append_cutex_event(
                event_repository,
                cutex_session_id,
                correlation,
                "cutex/runtime/failed",
                failed_params,
            )
            .map_err(post_operation_persistence_error)?;
            return Err(error);
        }
    };
    let result_generation = result
        .get("runtimeGeneration")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_user_input_error("runtime mutation omitted runtimeGeneration"))?;
    let result_status = result
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| {
            matches!(
                *status,
                "starting" | "online" | "offline" | "closing" | "closed" | "error"
            )
        })
        .ok_or_else(|| invalid_user_input_error("runtime mutation returned an invalid status"))?;
    let result_runtime_agent_id = result.get("runtimeAgentId").cloned().or(runtime_agent_id);
    let launch_profile_receipt =
        validate_launch_profile_receipt(launch_profile.as_deref(), result.get("launchProfile"))?;
    let foreground_required_reason = match result.get("foregroundRequiredReason") {
        Some(Value::String(reason)) if !reason.is_empty() => Some(reason.clone()),
        Some(_) => {
            return Err(invalid_user_input_error(
                "runtime mutation returned an invalid foregroundRequiredReason",
            ))
        }
        None => None,
    };
    let terminal_method = match result_status {
        "starting" => "cutex/runtime/onlineRequested",
        "closing" => "cutex/runtime/closing",
        _ => terminal_method,
    };
    append_cutex_event(
        event_repository,
        cutex_session_id,
        correlation.clone(),
        terminal_method,
        runtime_event_params(
            result_generation,
            backend,
            result_status,
            result_runtime_agent_id,
            reason,
        ),
    )
    .map_err(post_operation_persistence_error)?;
    if let Some(reason) = foreground_required_reason {
        append_cutex_event(
            event_repository,
            cutex_session_id,
            correlation,
            "cutex/host/foregroundRequired",
            json!({
                "runtimeGeneration": result_generation,
                "reason": reason,
                "requiredAt": chrono::Utc::now().to_rfc3339(),
            }),
        )
        .map_err(post_operation_persistence_error)?;
    }
    let mut response = json!({
        "runtimeGeneration": result_generation,
        "status": result_status,
    });
    if let Some(receipt) = launch_profile_receipt {
        response
            .as_object_mut()
            .expect("runtime mutation response object")
            .insert("launchProfile".to_string(), receipt);
    }
    for key in ["actionId", "attachCommand", "runtimeAgentId"] {
        if let Some(value) = result.get(key) {
            response
                .as_object_mut()
                .unwrap()
                .insert(key.into(), value.clone());
        }
    }
    Ok(response)
}

fn validate_launch_profile_receipt(
    requested: Option<&str>,
    receipt: Option<&Value>,
) -> Result<Option<Value>, super::user_input::UserInputExecutionError> {
    let Some(receipt) = receipt else {
        return requested.map_or(Ok(None), |_| {
            Err(invalid_user_input_error(
                "runtime mutation omitted launchProfile for an explicit override request",
            ))
        });
    };
    let object = receipt.as_object().ok_or_else(|| {
        invalid_user_input_error("runtime mutation returned an invalid launchProfile receipt")
    })?;
    let source = object.get("source").and_then(Value::as_str);
    if object.len() != 6
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "requested"
                    | "selected"
                    | "effective"
                    | "source"
                    | "applicationScope"
                    | "persisted"
            )
        })
        || requested.is_some_and(|requested| {
            object.get("requested").and_then(Value::as_str) != Some(requested)
        })
        || requested.is_none()
            && !matches!(
                source,
                Some("session_configured" | "global_default" | "unknown")
            )
        || object
            .get("effective")
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        || object.get("selected").and_then(Value::as_str)
            != object.get("effective").and_then(Value::as_str)
        || !matches!(
            source,
            Some("one_launch_override" | "session_configured" | "global_default" | "unknown")
        )
        || requested.is_some() && source != Some("one_launch_override")
        || !object
            .get("applicationScope")
            .and_then(Value::as_str)
            .is_some_and(|scope| matches!(scope, "runtime" | "tui" | "runtime_and_tui"))
        || object.get("persisted").and_then(Value::as_bool) != Some(false)
    {
        return Err(invalid_user_input_error(
            "runtime mutation returned an invalid launchProfile receipt",
        ));
    }
    Ok(Some(receipt.clone()))
}

fn runtime_event_params(
    runtime_generation: u64,
    backend: &str,
    status: &str,
    runtime_agent_id: Option<Value>,
    reason: Option<String>,
) -> Value {
    let mut params = json!({
        "runtimeGeneration": runtime_generation,
        "backend": backend,
        "status": status,
        "runtimeAgentId": runtime_agent_id.unwrap_or(Value::Null),
        "occurredAt": chrono::Utc::now().to_rfc3339(),
    });
    if let Some(reason) = reason {
        params
            .as_object_mut()
            .expect("runtime event params object")
            .insert("reason".to_string(), Value::String(reason));
    }
    params
}

fn dispatch_session_defaults_update(
    event_repository: &EventRepository,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let object = allowed_params(&request.params, &["patch"], &["expectedRevision"])?;
    require_session_revision(session, object)?;
    let patch = object
        .get("patch")
        .and_then(Value::as_object)
        .filter(|patch| !patch.is_empty())
        .ok_or_else(|| invalid_user_input_error("patch must be a non-empty object"))?;
    let allowed = [
        "backend",
        "managedCwd",
        "permissions",
        "approvalPolicy",
        "sandboxMode",
        "model",
        "reasoningEffort",
        "serviceTier",
        "cliArgs",
        "groups",
        "claudeAllowWrite",
        "claudeAllowExec",
    ];
    if patch.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid_user_input_error(
            "defaults patch contains a field outside registry version 2",
        ));
    }
    validate_service_tier_patch(patch, session)?;
    mutate_session(cutex_session_id, &request.method, request.params.clone())?;
    let revision = next_session_revision(session)?;
    let mut runtime_defaults = session
        .get("runtimeDefaults")
        .cloned()
        .unwrap_or_else(|| json!({}));
    for (key, value) in patch {
        runtime_defaults
            .as_object_mut()
            .expect("runtimeDefaults is an object")
            .insert(key.clone(), value.clone());
    }
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        "cutex/session/defaultsUpdated",
        json!({
            "sessionRevision": revision,
            "runtimeDefaults": runtime_defaults,
            "updatedAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(json!({ "revision": revision }))
}

/// `session-fast-mode`: `serviceTier` is `priority` (Fast), `default`
/// (standard), `flex`, the legacy alias `fast` (stored as `priority`), or null
/// (follow the profile config). Codex sessions only: Claude has no tier.
fn validate_service_tier_patch(
    patch: &serde_json::Map<String, Value>,
    session: &Value,
) -> Result<(), super::user_input::UserInputExecutionError> {
    let Some(tier) = patch.get("serviceTier") else {
        return Ok(());
    };
    match tier {
        Value::Null => {}
        Value::String(tier) if SESSION_SERVICE_TIERS.contains(&tier.as_str()) => {}
        _ => {
            return Err(invalid_user_input_error(
                "serviceTier must be \"priority\", \"default\", \"flex\", \"fast\" or null",
            ))
        }
    }
    let backend = patch
        .get("backend")
        .and_then(Value::as_str)
        .or_else(|| session.pointer("/runtime/backend").and_then(Value::as_str));
    if backend.is_some_and(|backend| backend.starts_with("claude")) {
        return Err(invalid_user_input_error(
            "serviceTier applies to Codex sessions only",
        ));
    }
    Ok(())
}

pub const SESSION_SERVICE_TIERS: &[&str] = &["priority", "default", "flex", "fast"];

pub(super) fn dispatch_session_profile_mutation(
    event_repository: &EventRepository,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let expected = if request.method == "cutex/session/profile/set" {
        allowed_params(&request.params, &["profile"], &["expectedRevision"])?
    } else {
        allowed_params(&request.params, &[], &["expectedRevision"])?
    };
    require_session_revision(session, expected)?;
    if request.method == "cutex/session/profile/set"
        && expected
            .get("profile")
            .and_then(Value::as_str)
            .is_none_or(|profile| profile.trim().is_empty())
    {
        return Err(invalid_user_input_error(
            "profile must be a non-empty string",
        ));
    }
    let result = mutate_session(cutex_session_id, &request.method, request.params.clone())?;
    let revision = result
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_user_input_error("profile mutation omitted revision"))?;
    let configured_profile = result
        .get("configuredProfile")
        .cloned()
        .unwrap_or(Value::Null);
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        "cutex/session/profileUpdated",
        json!({
            "sessionRevision": revision,
            "configuredProfile": configured_profile,
            "updatedAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(json!({
        "cutexSessionId": cutex_session_id,
        "revision": revision,
        "configuredProfile": configured_profile,
    }))
}

pub const SESSION_DISPLAY_NAME_MAX_CHARS: usize = 64;
pub const SESSION_NOTE_MAX_CHARS: usize = 280;
pub const SESSION_AVATAR_VALUE_MAX_CHARS: usize = 64;

/// Validate `cutex/session/metadata/update` params beyond the JSON schema.
/// Each of displayName / note / avatar / avatarMode is tri-state: omitted = unchanged,
/// null = clear, value = set. At least one must be present.
pub(super) fn validate_session_metadata_params(
    params: &Value,
) -> Result<(), super::user_input::UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    let allowed = ["expectedRevision", "displayName", "note", "avatar", "avatarMode", "badge"];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid_user_input_error(
            "metadata params accept only expectedRevision, displayName, note, avatar, avatarMode, badge",
        ));
    }
    optional_safe_integer_param(object, "expectedRevision")?;
    if object.contains_key("avatarMode") {
        if object.get("expectedRevision").and_then(Value::as_u64).is_none() {
            return Err(invalid_user_input_error("avatarMode changes require expectedRevision"));
        }
        if !matches!(object.get("avatarMode"), Some(Value::Null))
            && !object.get("avatarMode").and_then(Value::as_str).is_some_and(|mode| matches!(mode, "fixed" | "inherit"))
        {
            return Err(invalid_user_input_error("avatarMode must be fixed, inherit or null"));
        }
        if object.get("avatarMode").and_then(Value::as_str) == Some("inherit")
            && object.get("avatar").is_some_and(|value| !value.is_null())
        {
            return Err(invalid_user_input_error("avatarMode inherit cannot include a non-null avatar"));
        }
    }
    if object.keys().filter(|key| key.as_str() != "expectedRevision").count() < 1 {
        return Err(invalid_user_input_error(
            "metadata update requires at least one of displayName, note, avatar, avatarMode, badge",
        ));
    }
    match object.get("displayName") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) => {
            let trimmed = name.trim();
            if trimmed.is_empty()
                || trimmed != name
                || name.chars().count() > SESSION_DISPLAY_NAME_MAX_CHARS
                || name.chars().any(char::is_control)
            {
                return Err(invalid_user_input_error(&format!(
                    "displayName must be a trimmed, non-empty string of at most {SESSION_DISPLAY_NAME_MAX_CHARS} characters without control characters (null clears)"
                )));
            }
        }
        Some(_) => return Err(invalid_user_input_error("displayName must be a string or null")),
    }
    match object.get("note") {
        None | Some(Value::Null) => {}
        Some(Value::String(note)) => {
            if note.chars().count() > SESSION_NOTE_MAX_CHARS
                || note
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err(invalid_user_input_error(&format!(
                    "note must be at most {SESSION_NOTE_MAX_CHARS} characters without control characters other than newline/tab (null clears)"
                )));
            }
        }
        Some(_) => return Err(invalid_user_input_error("note must be a string or null")),
    }
    if let Some(value) = object.get("badge").filter(|v| !v.is_null()) {
        let badge = serde_json::from_value(value.clone())
            .map_err(|_| invalid_user_input_error("badge must be {text, tint?} or null"))?;
        crate::session::model::validate_session_badge(&badge).map_err(invalid_user_input_error)?;
    }
    match object.get("avatar") {
        None | Some(Value::Null) => {}
        Some(avatar) => {
            let parsed: crate::session::model::CutexSessionAvatar =
                serde_json::from_value(avatar.clone()).map_err(|_| {
                    invalid_user_input_error(
                        "avatar must be {kind: emoji|color|icon, value: string, color?: #rrggbb} or null",
                    )
                })?;
            if avatar.as_object().is_some_and(|object| {
                object
                    .keys()
                    .any(|key| !["kind", "value", "color"].contains(&key.as_str()))
            }) {
                return Err(invalid_user_input_error(
                    "avatar accepts only kind, value, color",
                ));
            }
            if parsed.value.trim().is_empty()
                || parsed.value.chars().count() > SESSION_AVATAR_VALUE_MAX_CHARS
                || parsed.value.chars().any(char::is_control)
            {
                return Err(invalid_user_input_error(&format!(
                    "avatar.value must be a non-empty string of at most {SESSION_AVATAR_VALUE_MAX_CHARS} characters"
                )));
            }
            crate::session::model::validate_session_avatar(&parsed)
                .map_err(invalid_user_input_error)?;
        }
    }
    Ok(())
}

fn dispatch_session_metadata_update(
    event_repository: &EventRepository,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    _session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    validate_session_metadata_params(&request.params)?;
    // `expectedRevision` is checked by the store mutation, per field: a
    // revision moved only by writes to other fields still applies (r252).
    let result = mutate_session(cutex_session_id, &request.method, request.params.clone())?;
    let revision = result
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_user_input_error("metadata mutation omitted revision"))?;
    let field = |key: &str| result.get(key).cloned().unwrap_or(Value::Null);
    let metadata = json!({
        "cutexSessionId": cutex_session_id,
        "revision": revision,
        "displayName": field("displayName"),
        "displayNameOverride": field("displayNameOverride"),
        "note": field("note"),
        "avatar": field("avatar"),
        "avatarMode": field("avatarMode"),
        "badge": field("badge"),
    });
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        "cutex/session/metadataUpdated",
        json!({
            "sessionRevision": revision,
            "displayName": metadata["displayName"],
            "displayNameOverride": metadata["displayNameOverride"],
            "note": metadata["note"],
            "avatar": metadata["avatar"],
            "avatarMode": metadata["avatarMode"],
            "badge": metadata["badge"],
            "updatedAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(metadata)
}

fn dispatch_session_groups_mutation(
    event_repository: &EventRepository,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let object = allowed_params(&request.params, &["groups"], &["expectedRevision"])?;
    require_session_revision(session, object)?;
    let groups = object
        .get("groups")
        .and_then(Value::as_array)
        .filter(|groups| !groups.is_empty())
        .ok_or_else(|| invalid_user_input_error("groups must be a non-empty array"))?;
    let mut normalized = Vec::new();
    for group in groups {
        let group = group
            .as_str()
            .filter(|group| !group.is_empty())
            .ok_or_else(|| invalid_user_input_error("groups must contain non-empty strings"))?;
        if normalized.iter().any(|existing| existing == group) {
            return Err(invalid_user_input_error("groups must be unique"));
        }
        normalized.push(group.to_string());
    }
    let mutation_result =
        mutate_session(cutex_session_id, &request.method, request.params.clone())?;
    let resulting_groups = mutation_result
        .get("groups")
        .cloned()
        .unwrap_or_else(|| json!(normalized));
    let revision = next_session_revision(session)?;
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        "cutex/session/groupsUpdated",
        json!({
            "sessionRevision": revision,
            "groups": resulting_groups,
            "updatedAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(json!({ "revision": revision, "groups": resulting_groups }))
}

fn dispatch_session_visibility_mutation(
    event_repository: &EventRepository,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let object = allowed_params(&request.params, &[], &["expectedRevision"])?;
    require_session_revision(session, object)?;
    mutate_session(cutex_session_id, &request.method, request.params.clone())?;
    let revision = next_session_revision(session)?;
    let visible = request.method.ends_with("/show");
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            thread_id: session
                .pointer("/native/threadId")
                .and_then(Value::as_str)
                .map(str::to_string),
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        if visible {
            "cutex/im/registered"
        } else {
            "cutex/im/unregistered"
        },
        json!({
            "registrationId": cutex_session_id,
            "provider": "cutex",
            "externalConversationId": session.pointer("/native/threadId")
                .and_then(Value::as_str)
                .unwrap_or(cutex_session_id),
            "changedAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(json!({ "revision": revision }))
}

pub(super) fn require_session_revision(
    session: &Value,
    params: &serde_json::Map<String, Value>,
) -> Result<(), super::user_input::UserInputExecutionError> {
    // Optional fence: omitted means last-write-wins (requests are already
    // serialized per session); supplied values are still checked.
    let Some(expected_revision) = optional_safe_integer_param(params, "expectedRevision")? else {
        return Ok(());
    };
    let current_revision = session.get("revision").and_then(Value::as_u64).unwrap_or(0);
    if expected_revision != current_revision {
        return Err(super::user_input::UserInputExecutionError {
            stage: "route".to_string(),
            code: "revision_conflict".to_string(),
            message: format!(
                "session revision conflict: expected {expected_revision}, current {current_revision}"
            ),
            retryable: true,
            details: json!({
                "expectedRevision": expected_revision,
                "currentRevision": current_revision,
                "resyncRequired": true,
            }),
            outcome_unknown: false,
        });
    }
    Ok(())
}

fn next_session_revision(
    session: &Value,
) -> Result<u64, super::user_input::UserInputExecutionError> {
    let current = session.get("revision").and_then(Value::as_u64).unwrap_or(0);
    if current >= super::model::MAX_SAFE_SEQUENCE {
        return Err(invalid_user_input_error(
            "session revision exhausted the JSON-safe integer range",
        ));
    }
    Ok(current + 1)
}

fn dispatch_focus_set(
    event_repository: &EventRepository,
    cutex_session_id: &str,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let object = allowed_params(
        &request.params,
        &["owner", "mobileMuted", "source"],
        &["expectedRevision"],
    )?;
    let expected_revision = optional_safe_integer_param(object, "expectedRevision")?;
    let owner = required_string_param(object, "owner")?;
    let mobile_muted = object
        .get("mobileMuted")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid_user_input_error("mobileMuted must be boolean"))?;
    let source = required_string_param(object, "source")?.to_string();
    let focus = set_focus(
        cutex_session_id,
        expected_revision,
        owner,
        mobile_muted,
        Some(source),
    )
    .map_err(focus_mutation_error)?;
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        "cutex/focus/changed",
        focus.clone(),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(focus)
}

fn dispatch_focus_clear(
    event_repository: &EventRepository,
    cutex_session_id: &str,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let object = allowed_params(&request.params, &[], &["expectedRevision"])?;
    let expected_revision = optional_safe_integer_param(object, "expectedRevision")?;
    let focus = clear_focus(cutex_session_id, expected_revision).map_err(focus_mutation_error)?;
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        "cutex/focus/changed",
        focus.clone(),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(focus)
}

pub(super) fn dispatch_user_input_submit(
    event_repository: &EventRepository,
    handle_user_input: ManagementUserInputHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let params = parse_user_input_submit_params(&request.params).map_err(|message| {
        super::user_input::UserInputExecutionError {
            stage: "route".to_string(),
            code: "invalid_request".to_string(),
            message,
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        }
    })?;
    let thread_id = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .filter(|thread_id| !thread_id.is_empty())
        .ok_or_else(|| super::user_input::UserInputExecutionError {
            stage: "route".to_string(),
            code: "session_offline".to_string(),
            message: "the cutex session has no bound app-server thread".to_string(),
            retryable: true,
            details: json!({ "cutexSessionId": cutex_session_id }),
            outcome_unknown: false,
        })?
        .to_string();
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            thread_id: Some(thread_id.clone()),
            client_user_message_id: Some(params.client_user_message_id.clone()),
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        "cutex/userInput/routeAccepted",
        {
            let mut accepted = json!({
                "managementRequestId": request.request_id,
                "clientUserMessageId": params.client_user_message_id,
                "origin": params.origin,
                "input": params.input,
                "strategy": params.strategy,
                "state": "routed_to_cutex",
                "acceptedAt": chrono::Utc::now().to_rfc3339(),
            });
            // P1 users §2.3: a non-owner's message reaches the agent as the
            // owner's; only the management event records who sent it.
            if let Some(user) = crate::management::users::current_acting_user() {
                accepted["actingUser"] = user;
            }
            accepted
        },
    )
    .map_err(persistence_user_input_error)?;
    let execution = handle_user_input(UserInputSubmitCommand {
        management_request_id: request.request_id.clone(),
        cutex_session_id: cutex_session_id.to_string(),
        thread_id: thread_id.clone(),
        params: params.clone(),
    });
    match execution {
        Ok(execution) => {
            let image_delivery = super::user_input::image_steering::accepted_delivery(
                &params.input,
                session.pointer("/runtime/backend").and_then(Value::as_str) == Some("claude_subscription"),
                &execution,
            );
            if execution.disposition == UserInputDisposition::Queued {
                let queue = execution
                    .queue
                    .clone()
                    .expect("queued execution requires queue item");
                let mut queued_event = serde_json::to_value(&queue).expect("queue item serialization");
                if let Ok((revision, items)) = user_input_repository().and_then(|repository| repository.list(cutex_session_id)) {
                    if let Some(current) = items.iter().find(|item| item.queue_id == queue.queue_id) {
                        queued_event = serde_json::to_value(current).expect("queue item serialization");
                        queued_event["queueRevision"] = json!(revision);
                    }
                }
                append_cutex_event(
                    event_repository,
                    cutex_session_id,
                    super::model::EventCorrelation {
                        thread_id: Some(thread_id),
                        client_user_message_id: Some(params.client_user_message_id.clone()),
                        management_request_id: Some(request.request_id.clone()),
                        queue_id: Some(queue.queue_id.clone()),
                        ..Default::default()
                    },
                    "cutex/userInput/queued",
                    queued_event,
                )
                .map_err(post_operation_persistence_error)?;
            } else if matches!(
                execution.disposition,
                UserInputDisposition::Started | UserInputDisposition::Steered
            ) {
                append_cutex_event(
                    event_repository,
                    cutex_session_id,
                    super::model::EventCorrelation {
                        thread_id: Some(thread_id),
                        turn_id: execution.turn_id.clone(),
                        client_user_message_id: Some(params.client_user_message_id.clone()),
                        management_request_id: Some(request.request_id.clone()),
                        native_request_id: execution.native_request_id.clone(),
                        ..Default::default()
                    },
                    "cutex/userInput/submitted",
                    {
                        let mut submitted = json!({
                        "managementRequestId": request.request_id,
                        "clientUserMessageId": params.client_user_message_id,
                        "origin": params.origin,
                        "input": params.input,
                        "disposition": execution.disposition,
                        "nativeRequestId": execution.native_request_id,
                        "nativeMethod": execution.native_method,
                        "turnId": execution.turn_id,
                        "appServerAccepted": true,
                        "submittedAt": chrono::Utc::now().to_rfc3339(),
                        });
                        if let Some(mode) = image_delivery { submitted["imageDelivery"] = json!(mode); }
                        submitted
                    },
                )
                .map_err(post_operation_persistence_error)?;
            }
            let mut result = json!({
                "clientUserMessageId": params.client_user_message_id,
                "disposition": execution.disposition,
                "appServerAccepted": execution.app_server_accepted,
                "nativeRequestId": execution.native_request_id,
                "turnId": execution.turn_id,
                "queue": execution.queue,
            });
            if let Some(mode) = image_delivery { result["imageDelivery"] = json!(mode); }
            // Capability `queue-idle-start`: whether a queued item is held by
            // a queue pause (only with `queue`).
            if execution.queue.is_some() {
                result["paused"] = json!(execution.queue_paused);
                if let Some(reason) = &execution.join_refusal { result["joinRefusal"] = json!(reason); }
            }
            Ok(result)
        }
        Err(error) => {
            let event_error = json!({
                "source": "cutex",
                "code": error.code,
                "message": error.message,
                "retryable": error.retryable,
                "details": error.details,
            });
            append_cutex_event(
                event_repository,
                cutex_session_id,
                super::model::EventCorrelation {
                    thread_id: Some(thread_id),
                    client_user_message_id: Some(params.client_user_message_id.clone()),
                    management_request_id: Some(request.request_id.clone()),
                    ..Default::default()
                },
                "cutex/userInput/failed",
                json!({
                    "managementRequestId": request.request_id,
                    "clientUserMessageId": params.client_user_message_id,
                    "origin": params.origin,
                    "input": params.input,
                    "stage": error.stage,
                    "error": event_error,
                    "failedAt": chrono::Utc::now().to_rfc3339(),
                }),
            )
            .map_err(post_operation_persistence_error)?;
            Err(error)
        }
    }
}

fn dispatch_queue_update(
    event_repository: &EventRepository,
    cutex_session_id: &str,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let lock = super::user_input::session_input_lock(cutex_session_id).map_err(persistence_user_input_error)?;
    let _guard = lock.lock().map_err(|_| persistence_user_input_error(anyhow::anyhow!("session input lock poisoned")))?;
    let object = exact_params(&request.params, &["queueId", "expectedRevision", "input"])?;
    let queue_id = required_string_param(object, "queueId")?;
    let expected_revision = required_safe_integer_param(object, "expectedRevision")?;
    let input = object
        .get("input")
        .and_then(Value::as_array)
        .filter(|input| !input.is_empty())
        .cloned()
        .ok_or_else(|| invalid_user_input_error("input must be a non-empty array"))?;
    for item in &input {
        validate_user_input(item).map_err(|message| invalid_user_input_error(&message))?;
    }
    let item = user_input_repository()
        .and_then(|repository| {
            repository.update(cutex_session_id, queue_id, expected_revision, input)
        })
        .map_err(queue_repository_error)?
        .ok_or_else(|| queue_not_found_error(queue_id))?;
    let (queue_revision, _) = user_input_repository().and_then(|repository| repository.list(cutex_session_id))
        .map_err(post_operation_persistence_error)?;
    let mut event = serde_json::to_value(&item).expect("queue item serialization");
    event["queueRevision"] = json!(queue_revision);
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            client_user_message_id: Some(item.client_user_message_id.clone()),
            management_request_id: Some(request.request_id.clone()),
            queue_id: Some(item.queue_id.clone()),
            ..Default::default()
        },
        "cutex/userInput/queueUpdated",
        event,
    )
    .map_err(persistence_user_input_error)?;
    Ok(serde_json::to_value(item).expect("queue item serialization"))
}

fn dispatch_queue_remove(
    event_repository: &EventRepository,
    cutex_session_id: &str,
    request: &ValidatedCutexRequest,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let lock = super::user_input::session_input_lock(cutex_session_id).map_err(persistence_user_input_error)?;
    let _guard = lock.lock().map_err(|_| persistence_user_input_error(anyhow::anyhow!("session input lock poisoned")))?;
    let object = exact_params(&request.params, &["queueId", "expectedRevision"])?;
    let queue_id = required_string_param(object, "queueId")?;
    let expected_revision = required_safe_integer_param(object, "expectedRevision")?;
    let removed = user_input_repository()
        .and_then(|repository| repository.remove(cutex_session_id, queue_id, expected_revision))
        .map_err(queue_repository_error)?
        .ok_or_else(|| queue_not_found_error(queue_id))?;
    let (queue_revision, _) = user_input_repository()
        .and_then(|repository| repository.list(cutex_session_id))
        .map_err(persistence_user_input_error)?;
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            client_user_message_id: Some(removed.client_user_message_id.clone()),
            management_request_id: Some(request.request_id.clone()),
            queue_id: Some(removed.queue_id.clone()),
            ..Default::default()
        },
        "cutex/userInput/queueRemoved",
        json!({
            "queueId": removed.queue_id,
            "clientUserMessageId": removed.client_user_message_id,
            "revision": removed.revision,
            "queueRevision": queue_revision,
            "reason": "cancelled",
            "removedAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(persistence_user_input_error)?;
    Ok(json!({
        "revision": queue_revision,
        "affectedQueueIds": [queue_id],
    }))
}

fn dispatch_queue_flush(
    cutex_session_id: &str,
    request: &ValidatedCutexRequest,
    flush_user_input_queue: ManagementUserInputQueueFlusher,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    let object = request
        .params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    if !object.contains_key("expectedQueueRevision")
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "expectedQueueRevision" | "maxItems"))
    {
        return Err(invalid_user_input_error(
            "queue flush requires expectedQueueRevision and optional maxItems",
        ));
    }
    let expected_revision = required_safe_integer_param(object, "expectedQueueRevision")?;
    let max_items = match object.get("maxItems") {
        None => 100,
        Some(value) => value
            .as_u64()
            .filter(|value| (1..=100).contains(value))
            .map(|value| value as usize)
            .ok_or_else(|| invalid_user_input_error("maxItems must be between 1 and 100"))?,
    };
    let (before_revision, before_items) = user_input_repository()
        .and_then(|repository| repository.list(cutex_session_id))
        .map_err(persistence_user_input_error)?;
    if before_revision != expected_revision {
        return Err(super::user_input::UserInputExecutionError {
            stage: "queue".to_string(),
            code: "revision_conflict".to_string(),
            message: format!(
                "queue revision conflict: expected {expected_revision}, current {before_revision}"
            ),
            retryable: true,
            details: json!({
                "expectedRevision": expected_revision,
                "currentRevision": before_revision,
                "resyncRequired": true,
            }),
            outcome_unknown: false,
        });
    }
    // Explicit: sends even while the queue is paused (the pause stays).
    flush_user_input_queue(cutex_session_id, max_items, true).map_err(|error| {
        super::user_input::UserInputExecutionError {
            stage: "native_request".to_string(),
            code: "app_server_unavailable".to_string(),
            message: format!("{error:#}"),
            retryable: true,
            details: json!({}),
            outcome_unknown: false,
        }
    })?;
    let (revision, after_items) = user_input_repository()
        .and_then(|repository| repository.list(cutex_session_id))
        .map_err(persistence_user_input_error)?;
    let remaining = after_items
        .iter()
        .map(|item| item.queue_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let affected_queue_ids = before_items
        .into_iter()
        .filter_map(|item| (!remaining.contains(item.queue_id.as_str())).then_some(item.queue_id))
        .collect::<Vec<_>>();
    Ok(json!({
        "revision": revision,
        "affectedQueueIds": affected_queue_ids,
    }))
}

pub(super) fn append_cutex_event(
    repository: &EventRepository,
    cutex_session_id: &str,
    correlation: super::model::EventCorrelation,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    repository.append(super::model::PendingEvent {
        cutex_session_id: cutex_session_id.to_string(),
        host_id: crate::platform::host::current_host_name(),
        source: super::model::EventSource::Cutex,
        schema: None,
        correlation,
        native: None,
        cutex: Some(super::model::CutexMessage {
            method: method.to_string(),
            params,
        }),
    })?;
    Ok(())
}

fn require_empty_params(params: &Value) -> Result<(), super::user_input::UserInputExecutionError> {
    if params.as_object().is_some_and(|params| params.is_empty()) {
        Ok(())
    } else {
        Err(invalid_user_input_error("params must be an empty object"))
    }
}

pub(super) fn exact_params<'a>(
    params: &'a Value,
    keys: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, super::user_input::UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    if object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key)) {
        return Err(invalid_user_input_error(&format!(
            "params must contain exactly: {}",
            keys.join(", ")
        )));
    }
    Ok(object)
}

pub(super) fn required_string_param<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, super::user_input::UserInputExecutionError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_user_input_error(&format!("{key} must be a non-empty string")))
}

/// Like `exact_params`, but `optional` keys may be omitted.
pub(super) fn allowed_params<'a>(
    params: &'a Value,
    required: &[&str],
    optional: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, super::user_input::UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    if required.iter().any(|key| !object.contains_key(*key))
        || object
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(invalid_user_input_error(&format!(
            "params must contain [{}] and may contain [{}]",
            required.join(", "),
            optional.join(", ")
        )));
    }
    Ok(object)
}

/// `None` when the key is absent or null; otherwise a JSON-safe integer.
pub(super) fn optional_safe_integer_param(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, super::user_input::UserInputExecutionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_safe_integer_param(object, key).map(Some),
    }
}

pub(super) fn required_safe_integer_param(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<u64, super::user_input::UserInputExecutionError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value <= super::model::MAX_SAFE_SEQUENCE)
        .ok_or_else(|| invalid_user_input_error(&format!("{key} must be a JSON-safe integer")))
}

pub(super) fn invalid_user_input_error(
    message: &str,
) -> super::user_input::UserInputExecutionError {
    super::user_input::UserInputExecutionError {
        stage: "route".to_string(),
        code: "invalid_request".to_string(),
        message: message.to_string(),
        retryable: false,
        details: json!({}),
        outcome_unknown: false,
    }
}

pub(super) fn persistence_user_input_error(
    error: anyhow::Error,
) -> super::user_input::UserInputExecutionError {
    super::user_input::UserInputExecutionError {
        stage: "route".to_string(),
        code: "event_persistence_unavailable".to_string(),
        message: format!("{error:#}"),
        retryable: true,
        details: json!({}),
        outcome_unknown: false,
    }
}

pub(super) fn post_operation_persistence_error(
    error: anyhow::Error,
) -> super::user_input::UserInputExecutionError {
    super::user_input::UserInputExecutionError {
        stage: "native_request".to_string(),
        code: "event_persistence_unavailable".to_string(),
        message: format!("{error:#}"),
        retryable: false,
        details: json!({ "resyncRequired": true }),
        outcome_unknown: true,
    }
}

pub(super) fn queue_repository_error(error: anyhow::Error) -> super::user_input::UserInputExecutionError {
    let message = format!("{error:#}");
    if error
        .downcast_ref::<super::user_input::QueueInputAlreadySubmitted>()
        .is_some()
    {
        super::user_input::UserInputExecutionError {
            stage: "queue".to_string(),
            code: "queue_input_already_submitted".to_string(),
            message,
            retryable: false,
            details: json!({ "resyncRequired": true }),
            outcome_unknown: false,
        }
    } else if let Some(conflict) = error.downcast_ref::<super::user_input::QueueRevisionConflict>() {
        super::user_input::UserInputExecutionError {
            stage: "queue".to_string(),
            code: "revision_conflict".to_string(),
            message,
            retryable: true,
            details: json!({ "resyncRequired": true, "expectedRevision": conflict.expected, "currentRevision": conflict.current }),
            outcome_unknown: false,
        }
    } else {
        persistence_user_input_error(error)
    }
}

fn focus_mutation_error(error: anyhow::Error) -> super::user_input::UserInputExecutionError {
    let message = format!("{error:#}");
    if message.contains("focus revision conflict") {
        super::user_input::UserInputExecutionError {
            stage: "route".to_string(),
            code: "revision_conflict".to_string(),
            message,
            retryable: true,
            details: json!({ "resyncRequired": true }),
            outcome_unknown: false,
        }
    } else {
        persistence_user_input_error(error)
    }
}

pub(super) fn queue_not_found_error(queue_id: &str) -> super::user_input::UserInputExecutionError {
    super::user_input::UserInputExecutionError {
        stage: "queue".to_string(),
        code: "queue_item_not_found".to_string(),
        message: "the queue item does not exist".to_string(),
        retryable: false,
        details: json!({ "queueId": queue_id }),
        outcome_unknown: false,
    }
}

pub(super) fn cutex_operation_error_status(code: &str, outcome_unknown: bool) -> u16 {
    if code == "revert_partial" { return 500; }
    if outcome_unknown {
        return 409;
    }
    match code {
        "invalid_request" => 400,
        "plan_changed" | "revert_conflict" | "nothing_to_revert" | "thread_unbound" => 409,
        "turn_history_too_large" => 422,
        "codex_app_server_unavailable" => 503,
        "native_history_unavailable" => 502,
        "cutex_method_denied" => 403,
        "session_not_found" => 404,
        "project_not_found" => 404,
        "workspace_root_required"
        | "cwd_required"
        | "unsupported_profile"
        | "unsupported_launch_option"
        | "provider_required"
        | "unknown_provider" => 400,
        "no_auto_pick_candidate" | "auto_pick_unavailable" => 409,
        // profile-relay-runtimes: the profile has no usable address for the runtime.
        "variant_base_url_missing" | "relay_runtime_unavailable" => 409,
        "queue_item_not_found" | "admission_item_not_found" => 404,
        // An earlier submit of the clientUserMessageId is unresolved (C-010).
        "user_input_in_progress" => 202,
        "idempotency_conflict"
        | "queue_input_already_submitted"
        | "passing_not_allowed"
        | "steer_unavailable"
        | "revision_conflict"
        | "already_retired"
        | "already_active"
        | "agent_name_taken"
        | "turn_conflict"
        | "session_offline"
        | "turn_active"
        | "profile_unchanged"
        | "profile_switch_disallowed"
        | "profile_switch_cli_mismatch"
        | "provider_family_mismatch"
        | "profile_fork_disallowed"
        | "fork_history_signature_rejected"
        | "claude_account_mismatch"
        | "history_changed"
        | "history_revert_unsupported"
        | "session_referenced"
        | "session_busy"
        | "session_delete_unsupported"
        | "session_fork_unsupported"
        | "session_fork_blocked_external_input"
        | "turn_in_progress"
        // Asks (委托).
        | "ask_obligations_pending"
        | "ask_closed"
        | "ask_not_retryable"
        | "ask_not_awaiting_acceptance"
        | "ask_not_waiting"
        | "ask_not_reopenable"
        | "ask_not_running"
        | "too_many_revisions"
        | "already_resolved"
        | "stale"
        | "target_overloaded"
        | "busy"
        // Owner notify.
        | "notification_closed"
        | "notification_not_replyable" => 409,
        "ask_not_found" | "question_not_found" | "recipient_not_found" | "revision_not_found" => 404,
        "notification_not_found" | "safety_block_not_found" => 404,
        // Safety grants.
        "safety_block_closed" | "safety_grants_unsupported" | "runtime_refused" => 409,
        "owner_notify_unavailable" => 503,
        "recipient_outside_boundary" | "recipient_archived" => 403,
        "invalid_arguments" | "unsupported_context" => 400,
        "asks_unavailable" | "persistence_unavailable" | "approvals_unavailable" => 503,
        "profile_not_found" | "turn_not_found" => 404,
        "history_revert_failed" | "session_delete_failed" | "session_fork_failed" => 502,
        "app_server_unavailable" | "native_request_failed" | "interrupt_timeout" => 502,
        "runtime_stop_failed" => 502,
        "event_persistence_unavailable" | "persistence_uncertain" => 503,
        _ => 500,
    }
}

fn http_reason(status: u16) -> &'static str {
    match status {
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

pub(super) fn native_request_session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/app-server/requests")?;
    decode_session_id(encoded)
}

pub(super) fn server_response_session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/app-server/server-request-responses")?;
    decode_session_id(encoded)
}

pub(super) fn cutex_request_session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/cutex/requests")?;
    decode_session_id(encoded)
}

pub(super) fn bootstrap_session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/bootstrap")?;
    decode_session_id(encoded)
}

pub(super) fn history_session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/history")?;
    decode_session_id(encoded)
}

pub(super) fn history_turn_path(path: &str) -> Option<(String, String)> {
    let (session, turn) = path.strip_prefix("/v2/sessions/")?.split_once("/history/turns/")?;
    Some((decode_session_id(session)?, decode_session_id(turn)?))
}

pub(super) fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path.strip_prefix("/v2/sessions/")?;
    decode_session_id(encoded)
}

/// One percent-decoded path segment (no `/`).
pub(super) fn decode_path_segment(encoded: &str) -> Option<String> {
    decode_session_id(encoded)
}

fn decode_session_id(encoded: &str) -> Option<String> {
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_path_digit(bytes[index + 1])?;
            let low = hex_path_digit(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.contains('\\')).then_some(decoded)
}

fn hex_path_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn request_id_hint(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("requestId")?
        .as_str()
        .filter(|request_id| !request_id.is_empty())
        .map(str::to_string)
}

fn response_id_hint(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("responseId")?
        .as_str()
        .filter(|response_id| !response_id.is_empty())
        .map(str::to_string)
}

pub(super) fn replay_query(
    request: &SimpleHttpRequest,
    allow_last_event_id: bool,
) -> Result<ReplayQuery, ReplayError> {
    let stream_id = query_parameter(&request.path, "streamId")?;
    let query_after = query_parameter(&request.path, "after")?;
    let header_after = allow_last_event_id
        .then(|| request.headers.get("last-event-id").cloned())
        .flatten()
        .filter(|value| !value.is_empty());
    if query_after.is_some() && header_after.is_some() && query_after != header_after {
        return Err(ReplayError::ConflictingCursor);
    }
    let after = query_after.or(header_after);
    let limit = query_parameter(&request.path, "limit")?
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| ReplayError::InvalidQuery("limit must be an integer".to_string()))
        })
        .transpose()?
        .unwrap_or(100);
    Ok(ReplayQuery {
        stream_id,
        after,
        limit,
        cutex_session_id: query_parameter(&request.path, "cutexSessionId")?
            .filter(|value| !value.is_empty()),
    })
}

fn query_parameter(path: &str, key: &str) -> Result<Option<String>, ReplayError> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(None);
    };
    let values = url::form_urlencoded::parse(query.as_bytes())
        .filter_map(|(name, value)| (name == key).then(|| value.into_owned()))
        .collect::<Vec<_>>();
    match values.as_slice() {
        [] => Ok(None),
        [value] => Ok(Some(value.clone())),
        _ => Err(ReplayError::InvalidQuery(format!(
            "query parameter {key} must not be repeated"
        ))),
    }
}

pub(super) fn write_replay_error(stream: &mut TcpStream, error: ReplayError) -> anyhow::Result<()> {
    match error {
        ReplayError::InvalidQuery(message) => write_v2_error(
            stream,
            400,
            "Bad Request",
            "invalid_request",
            &message,
            false,
            json!({}),
        ),
        ReplayError::ConflictingCursor => write_v2_error(
            stream,
            400,
            "Bad Request",
            "conflicting_cursor",
            "after and Last-Event-ID identify different cursors",
            false,
            json!({}),
        ),
        ReplayError::StreamChanged {
            requested_stream_id,
            current_stream_id,
        } => write_v2_error(
            stream,
            409,
            "Conflict",
            "stream_changed",
            "the host event stream identity changed",
            false,
            json!({
                "hostId": crate::platform::host::current_host_name(),
                "requestedStreamId": requested_stream_id,
                "currentStreamId": current_stream_id,
                "resyncRequired": true,
            }),
        ),
        ReplayError::CursorExpired {
            stream_id,
            cursor: _,
            earliest,
            latest,
        } => write_v2_error(
            stream,
            409,
            "Conflict",
            "cursor_expired",
            "the requested cursor is no longer retained",
            false,
            json!({
                "hostId": crate::platform::host::current_host_name(),
                "streamId": stream_id,
                "earliestCursor": earliest.as_ref().map(|boundary| boundary.cursor.clone()),
                "earliestSequence": earliest.as_ref().map(|boundary| boundary.sequence),
                "latestCursor": latest.as_ref().map(|boundary| boundary.cursor.clone()),
                "latestSequence": latest.as_ref().map(|boundary| boundary.sequence),
                "resyncRequired": true,
            }),
        ),
        ReplayError::Repository(error) => write_v2_error(
            stream,
            500,
            "Internal Server Error",
            "repository_error",
            "the durable management event repository failed",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        ),
    }
}

pub(crate) fn write_v2_error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    code: &str,
    message: &str,
    retryable: bool,
    details: serde_json::Value,
) -> anyhow::Result<()> {
    write_v2_request_error(
        stream, status, reason, None, code, message, retryable, details,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_v2_request_error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    request_id: Option<&str>,
    code: &str,
    message: &str,
    retryable: bool,
    details: serde_json::Value,
) -> anyhow::Result<()> {
    let diagnostic_id = super::error_log::record(status, request_id, code, message);
    let mut response = json!({
        "contractVersion": 2,
        "error": {
            "source": "cutex",
            "code": code,
            "message": message,
            "retryable": retryable,
            "details": details,
        }
    });
    if let Some(request_id) = request_id {
        response
            .as_object_mut()
            .expect("v2 error response is an object")
            .insert(
                "requestId".to_string(),
                Value::String(request_id.to_string()),
            );
    }
    if let Some(id) = diagnostic_id { response["error"]["diagnosticId"] = json!(id); }
    write_json_response(stream, status, reason, &response)
}

/// `503 idempotency_capacity_exceeded`: the request was neither recorded nor
/// forwarded, so retrying it later is safe.
pub(crate) fn write_idempotency_capacity_exceeded(
    stream: &mut TcpStream,
    request_id: &str,
    full: &CapacityExceeded,
) -> anyhow::Result<()> {
    write_v2_request_error(
        stream,
        503,
        "Service Unavailable",
        Some(request_id),
        CapacityExceeded::CODE,
        CapacityExceeded::MESSAGE,
        true,
        full.details(),
    )
}

fn write_request_outcome_unknown(
    stream: &mut TcpStream,
    request_id: &str,
    details: Value,
) -> anyhow::Result<()> {
    write_v2_request_error(
        stream,
        409,
        "Conflict",
        Some(request_id),
        "request_outcome_unknown",
        "cutex lost the native response after forwarding the request; it will not be replayed",
        false,
        details,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_v2_server_response_error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    response_id: Option<&str>,
    code: &str,
    message: &str,
    retryable: bool,
    details: Value,
) -> anyhow::Result<()> {
    let mut response = json!({
        "contractVersion": 2,
        "error": {
            "source": "cutex",
            "code": code,
            "message": message,
            "retryable": retryable,
            "details": details,
        }
    });
    if let Some(response_id) = response_id {
        response
            .as_object_mut()
            .expect("v2 error response is an object")
            .insert(
                "responseId".to_string(),
                Value::String(response_id.to_string()),
            );
    }
    write_json_response(stream, status, reason, &response)
}

pub(crate) fn write_payload_too_large(
    stream: &mut TcpStream,
    _content_length: usize,
    limit: usize,
) -> anyhow::Result<()> {
    write_v2_error(
        stream,
        413,
        "Payload Too Large",
        "payload_too_large",
        "the management request body exceeds the published limit",
        false,
        json!({
            "maxRequestBytes": limit,
        }),
    )
}

pub(super) fn write_sse_headers(stream: &mut impl Write) -> anyhow::Result<()> {
    stream.write_all(
        crate::http::server::sse_response_head().as_bytes(),
    )?;
    stream.flush()?;
    Ok(())
}

pub(super) fn enable_live_deltas(
    repository: &EventRepository,
    request: &SimpleHttpRequest,
    query: &ReplayQuery,
    subscription: &mut super::repository::EventSubscription,
) -> Result<(),super::live_deltas::LiveCapacityError> {
    if query_parameter(&request.path, "liveDeltas").ok().flatten().as_deref() == Some("true") {
        repository.enable_live(subscription,query.cutex_session_id.clone())?;
    }
    Ok(())
}

pub(super) fn write_live_capacity_error(stream:&mut TcpStream)->anyhow::Result<()> {
    write_v2_error(stream,429,"Too Many Requests","live_delta_capacity_exceeded",
        "live delta subscriber capacity is full; retry the subscription",true,json!({"retryAfterMs":1000}))
}

fn write_sse_event(stream: &mut impl Write, event: &EventEnvelope) -> anyhow::Result<()> {
    let data = serde_json::to_string(event)?;
    write!(
        stream,
        "id: {}\nevent: {SSE_EVENT_NAME}\ndata: {data}\n\n",
        event.cursor
    )?;
    stream.flush()?;
    Ok(())
}

fn write_live_delta(stream: &mut impl Write, json: &str) -> anyhow::Result<()> {
    write!(stream, "event: live_delta\ndata: {json}\n\n")?;
    stream.flush()?;
    Ok(())
}

/// Body of `/v2/events/stream` after the headers: the replay page, then live
/// events with keepalives; ends on a stream change, a revoked token
/// (`cutex/device/revoked`) or a dropped subscription (`lagged` frame when the
/// subscriber fell behind).
pub(super) fn stream_events(
    stream: &mut impl Write,
    subscription: super::repository::EventSubscription,
    query: &ReplayQuery,
    watch: crate::management::listeners::RevocationWatch,
) -> anyhow::Result<()> {
    stream_events_filtered(stream, subscription, query, watch, &mut |_| true, &mut |_| true)
}

/// `stream_events` that writes only the events `visible` accepts (a
/// non-owner user's scope, P1 users); skipped events still advance the
/// cursor, so a reconnect never replays them.
pub(super) fn stream_events_filtered(
    stream: &mut impl Write,
    subscription: super::repository::EventSubscription,
    query: &ReplayQuery,
    mut watch: crate::management::listeners::RevocationWatch,
    visible: &mut dyn FnMut(&EventEnvelope) -> bool,
    live_visible: &mut dyn FnMut(&str) -> bool,
) -> anyhow::Result<()> {
    let subscribed_stream_id = subscription.page.stream_id.clone();
    let mut last_cursor = query.after.clone();
    let mut delivered_sequence = 0;
    for event in subscription.page.events {
        if visible(&event) {
            write_sse_event(stream, &event)?;
        }
        last_cursor = Some(event.cursor.clone());
        delivered_sequence = delivered_sequence.max(event.sequence);
    }
    // Replay may be an empty page after a supplied cursor. Its checkpoint is
    // only safe when replay has been exhausted, never when has_more is true.
    if !subscription.page.has_more {
        delivered_sequence = delivered_sequence.max(subscription.page.checkpoint.sequence);
    }
    let mut last_write = std::time::Instant::now();
    loop {
        let wake_version=subscription.wake.version();
        let mut progressed=false;
        if watch.disconnected()? { return Ok(()); }
        if let Some(token_id) = watch.revoked() {
            crate::management::listeners::write_revoked_frame(stream, &token_id)?;
            return Ok(());
        }
        match subscription.receiver.try_recv() {
            Ok(event) => {
                progressed=true;
                if event.stream_id != subscribed_stream_id {
                    write_sse_stream_changed(
                        stream,
                        &event.host_id,
                        &subscribed_stream_id,
                        &event.stream_id,
                    )?;
                    return Ok(());
                }
                if query
                    .cutex_session_id
                    .as_deref()
                    .is_none_or(|session_id| event.cutex_session_id == session_id)
                    && visible(&event)
                {
                    write_sse_event(stream, &event)?;
                    last_write = std::time::Instant::now();
                }
                last_cursor = Some(event.cursor.clone());
                delivered_sequence = delivered_sequence.max(event.sequence);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if last_write.elapsed() >= SSE_KEEPALIVE_INTERVAL {
                    stream.write_all(b": keepalive\n\n")?;
                    stream.flush()?;
                    last_write = std::time::Instant::now();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if subscription
                    .lagged
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    write_sse_lagged(stream, last_cursor.as_deref())?;
                }
                return Ok(());
            }
        }
        // Durable/control always gets the first scheduling opportunity. Keep
        // this batch bounded so revocation and durable arrivals are not starved.
        if let Some(live) = subscription.live_deltas.as_ref() {
            let mut live_bytes = 0;
            for _ in 0..16 {
                let Some(delivery) = live.pop_ready(&subscribed_stream_id,delivered_sequence) else { break; };
                progressed=true;
                let frame = delivery.frame();
                let anchored = if frame.kind=="gap" {
                    frame.anchor.as_ref().is_none_or(|a|a.stream_id==subscribed_stream_id)
                } else {frame.anchor.as_ref().is_some_and(|a|
                    a.stream_id == subscribed_stream_id && a.sequence <= delivered_sequence)};
                if anchored && live_visible(&frame.cutex_session_id) {
                    write_live_delta(stream, delivery.json())?;
                    last_write = std::time::Instant::now();
                    live_bytes += delivery.json().len();
                    if live_bytes >= 64 * 1024 { break; }
                }
            }
        }
        if !progressed { subscription.wake.wait(wake_version,crate::management::listeners::STREAM_TICK); }
    }
}

/// The subscriber fell behind and is being disconnected; it can resume from
/// `lastDeliveredCursor` (the events are still in the log).
pub(crate) fn write_sse_lagged(
    stream: &mut impl Write,
    last_delivered_cursor: Option<&str>,
) -> anyhow::Result<()> {
    let data = serde_json::to_string(&json!({
        "contractVersion": 2,
        "error": {
            "source": "cutex",
            "code": "lagged",
            "message": "the subscriber fell too far behind; reconnect from the last delivered cursor",
            "retryable": true,
            "details": {
                "lastDeliveredCursor": last_delivered_cursor,
                "resyncRequired": false,
            }
        }
    }))?;
    write!(stream, "event: cutex_management_stream_error\ndata: {data}\n\n")?;
    stream.flush()?;
    Ok(())
}

fn write_sse_stream_changed(
    stream: &mut impl Write,
    host_id: &str,
    requested_stream_id: &str,
    current_stream_id: &str,
) -> anyhow::Result<()> {
    let data = serde_json::to_string(&json!({
        "contractVersion": 2,
        "error": {
            "source": "cutex",
            "code": "stream_changed",
            "message": "the host event stream identity changed",
            "retryable": false,
            "details": {
                "hostId": host_id,
                "requestedStreamId": requested_stream_id,
                "currentStreamId": current_stream_id,
                "resyncRequired": true,
            }
        }
    }))?;
    write!(
        stream,
        "event: cutex_management_stream_error\ndata: {data}\n\n"
    )?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;

    fn safety_http_context() -> ManagementRequestContext {
    // Denied and gate-answered requests never reach native runtime callbacks.
    ManagementRequestContext {
        explicit_launch_action: |_, _| panic!("unexpected runtime action"),
        adopt_saved_native: |_, _| panic!("unexpected native adoption"),
        review_agent_archive: |_, _| panic!("unexpected archive review"),
        owner_archive_action: |_, _| panic!("unexpected permanent archive"),
        execute_agent_archive: |_, _| panic!("unexpected archive"),
        durable_agent_candidates: |_| panic!("unexpected agent candidates"),
        import_durable_agent: |_, _| panic!("unexpected agent import"),
        load_registry: || Ok(crate::im::registry::ImRegistry::default()),
        load_runtime_status: |_| Ok(None),
        forward_native_request: |_, _| panic!("unexpected native request"),
        respond_native_server_request: |_, _, _| panic!("unexpected native response"),
        handle_user_input: |_| panic!("unexpected user input"),
        flush_user_input_queue: |_, _, _| panic!("unexpected queue flush"),
        steer_user_input_queue: |_, _, _| panic!("unexpected queue steer"),
        interrupt_active_turn: |_, _| panic!("unexpected interrupt"),
        load_bootstrap_state: |_, _| panic!("unexpected bootstrap"),
        load_latest_plan: |_| panic!("unexpected plan"),
        load_latest_token_usage: |_| panic!("unexpected usage"),
        revert_session_history: |_, _, _| panic!("unexpected revert"),
        delete_session: |_, _| panic!("unexpected session delete"),
        fork_session: |_, _, _| panic!("unexpected fork"),
        side_chat: |_, _, _| panic!("unexpected side chat"),
        open_offline_history: |_| panic!("unexpected history"),
        mutate_session: |id, method, params| {
            assert_eq!(method, "cutex/runtime/offline");
            assert_eq!(params["expectedRuntimeGeneration"], 7);
            let lane = super::super::session_coordination::session_lane(id).unwrap();
            let nested = lane.reserve().unwrap();
            assert!(nested.is_current_for(&lane).unwrap(), "HTTP owner thread retains its capability");
            let other = lane.clone();
            assert!(std::thread::spawn(move || other.try_lock().is_err()).join().unwrap(),
                "another execution thread cannot enter during the native callback");
            let retry = super::super::model_overload_retry::Store::open_default().unwrap();
            let state = retry.snapshot().unwrap();
            let entry = &state.sessions[id];
            assert!(entry.plan.is_none());
            assert_eq!(entry.blocked_generation, Some(7));
            let cancelled = entry.settlement.as_ref().unwrap();
            assert_eq!(cancelled.plan.thread_id, "retry-thread");
            assert_eq!(cancelled.plan.runtime_generation, 7);
            assert_eq!(cancelled.plan.binding_turn_id.as_deref(), Some("accepted-retry-turn"));
            assert!(!retry.is_disabled());
            Ok(json!({"runtimeGeneration":7,"runtimeAgentId":null,"status":"offline"}))
        },
        online_session: |_, _, _| panic!("unexpected online request"),
        bind_project_authority: |_| panic!("unexpected project bind"),
        import_legacy_director_ownership: |_| panic!("unexpected ownership import"),
        reconcile_agent_reservation: |_| panic!("unexpected reservation"),
        abandon_agent_create: |_| panic!("unexpected reservation"),
        list_management_projects: |_| panic!("unexpected project list"),
        read_management_project: |_, _| panic!("unexpected project read"),
        update_management_project_presentation: |_, _| panic!("unexpected project update"),
        execute_management_operator_action: |_, _| panic!("unexpected operator action"),
        execute_management_project_mutation: |_, _| panic!("unexpected project mutation"),
        create_human_agent: |_, _| panic!("unexpected agent create"),
        create_human_session: |_, _| panic!("unexpected session create"),
    }
}


    #[test]
    fn settlement_http_owner_auth_and_method_gate() {
        use crate::management::client_tokens::{DeviceScope, VerifiedClientToken};
        use crate::management::users::{self, Principal};
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                users::set_current_principal(None);
                crate::management::listeners::set_current_client_token(None);
            }
        }
        let _reset = Reset;
        let send = |bearer: Option<&str>, method: &str, body: &[u8]| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            let mut headers = HashMap::new();
            if let Some(bearer) = bearer {
                headers.insert("authorization".into(), format!("Bearer {bearer}"));
            }
            let request = SimpleHttpRequest { method: method.into(),
                path: format!("{}?test=1", super::super::agent_bus_stuck::CLOSE_PATH),
                headers, body: body.to_vec() };
            handle_v2_request(&mut server, &request, Some("agent-secret"), Some("owner-secret"), safety_http_context()).unwrap();
            drop(server);
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response
        };
        for bearer in [None, Some("wrong"), Some("agent-secret")] {
            assert!(send(bearer, "POST", b"{}").starts_with("HTTP/1.1 403"));
        }
        for principal in [Principal::Pending, Principal::User {
            id: "other-user".into(), name: "Other".into(), grant: Default::default(),
        }] {
            users::set_current_principal(Some(principal));
            assert!(send(Some("owner-secret"), "POST", b"{}").starts_with("HTTP/1.1 403"));
        }
        users::set_current_principal(Some(Principal::Owner));
        for scope in [DeviceScope::Chat, DeviceScope::Read] {
            let mut token = VerifiedClientToken::test_token("scoped-owner");
            token.scope = scope;
            crate::management::listeners::set_current_client_token(Some(token));
            assert!(send(Some("owner-secret"), "POST", b"{}").starts_with("HTTP/1.1 403"));
        }
        for token in [None, Some(VerifiedClientToken::test_token("full-owner"))] {
            crate::management::listeners::set_current_client_token(token);
            assert!(send(Some("owner-secret"), "GET", b"").starts_with("HTTP/1.1 405"));
            assert!(send(Some("owner-secret"), "POST", b"{}").starts_with("HTTP/1.1 400"));
            assert!(send(Some("owner-secret"), "POST", br#"{"messageId":"m","expectedEvidenceToken":"t","requestId":"r","reason":"reviewed","actor":"forged-owner"}"#)
                .starts_with("HTTP/1.1 400"));
        }
    }

    #[test]
    fn delegation_controls_require_owner_and_full_mutation_scope() {
        use crate::management::client_tokens::{DeviceScope,VerifiedClientToken};
        use crate::management::users::{self,Principal};
        struct Reset;
        impl Drop for Reset {fn drop(&mut self){users::set_current_principal(None);crate::management::listeners::set_current_client_token(None);}}
        let _reset=Reset;
        let send=|path:&str,bearer:&str|{
            let listener=TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client=TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let (mut server,_)=listener.accept().unwrap();
            let request=SimpleHttpRequest{method:"POST".into(),path:path.into(),headers:HashMap::from([("authorization".into(),format!("Bearer {bearer}"))]),body:br#"{"actor":"owner"}"#.to_vec()};
            handle_v2_request(&mut server,&request,Some("agent-secret"),Some("owner-secret"),safety_http_context()).unwrap();
            drop(server);let mut response=String::new();client.read_to_string(&mut response).unwrap();response
        };
        for path in ["/v2/projects/p/pause","/v2/projects/p/resume","/v2/delegations/d/stop","/v2/delegations/d/resume-continuation","/v2/projects/p/delegation-grants"] {
            assert!(send(path,"agent-secret").starts_with("HTTP/1.1 401"));
            users::set_current_principal(Some(Principal::Pending));
            assert!(send(path,"owner-secret").starts_with("HTTP/1.1 401"));
            users::set_current_principal(Some(Principal::Owner));
            for scope in [DeviceScope::Chat,DeviceScope::Read] {
                let mut token=VerifiedClientToken::test_token("scoped-owner");token.scope=scope;
                crate::management::listeners::set_current_client_token(Some(token));
                assert!(send(path,"owner-secret").starts_with("HTTP/1.1 401"));
            }
            crate::management::listeners::set_current_client_token(None);
            users::set_current_principal(None);
        }
    }

    #[test]
    fn http_safety_stop_reuses_outer_reservation_without_disabling_retry() {
        const CHILD: &str = "CUTEX_HTTP_SAFETY_LANE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let parent = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap());
            let home = parent.join(format!("http-safety-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join(".cutex-test-private-home"), b"private HTTP regression").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "management::v2::server::tests::http_safety_stop_reuses_outer_reservation_without_disabling_retry", "--nocapture"])
                .env("HOME", &home).env("CUTEX_TEST_PRIVATE_HOME", &home).env(CHILD, "1")
                .output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("OVERLOAD_RETRY_DISABLE_NOT_DURABLE"));
            std::fs::remove_dir_all(home).unwrap();
            return;
        }
        use super::super::model_overload_retry::{Store, Phase};
        let id = "cutex.http-safety";
        let home = std::env::var("CUTEX_TEST_PRIVATE_HOME").unwrap();
        let mut record = crate::session::model::CutexSessionRecord::new(id.into(), Some("retry-thread".into()),
            crate::platform::host::current_host_name(), home, None).unwrap();
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        record.runtime_generation = 7;
        let mut sessions = crate::session::model::CutexSessionStore::default();
        sessions.sessions.insert(id.into(), record);
        crate::session::store::save_cutex_session_store(&sessions).unwrap();
        let retry = Store::open_default().unwrap();
        retry.update(|state| {
            state.terminal(id, "retry-thread", 7, &json!({"id":"failed-turn","status":"failed",
                "error":{"codexErrorInfo":"serverOverloaded"}}), None, chrono::Utc::now());
            let plan = state.sessions.get_mut(id).unwrap().plan.as_mut().unwrap();
            plan.phase = Phase::Binding;
            plan.binding_turn_id = Some("accepted-retry-turn".into());
        }).unwrap();
        let path = format!("/v2/sessions/{id}/cutex/requests");
        let send = |request_id: &str, generation: u64, token: &str| {
            let request = SimpleHttpRequest { method:"POST".into(), path:path.clone(),
                headers: [("authorization".into(), format!("Bearer {token}"))].into_iter().collect(),
                body:serde_json::to_vec(&json!({"requestId":request_id,"method":"cutex/runtime/offline",
                    "params":{"expectedRuntimeGeneration":generation,"force":false}})).unwrap() };
            capture_http_response(|stream| handle_v2_request(stream, &request, Some("bridge-test"), Some("root-test"), safety_http_context()))
        };
        let denied = send("denied",7,"wrong");
        assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
        assert!(retry.snapshot().unwrap().sessions[id].plan.is_some());
        let response = send("owner-stop",7,"root-test");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("offline"));
        assert!(!retry.is_disabled());
        assert!(!crate::config::paths::runtime_dir().unwrap().join("model-overload-retry/state-v1.disabled").exists());
        assert_eq!(retry.snapshot().unwrap().sessions[id].blocked_generation, Some(7));
        assert!(super::super::session_coordination::session_lane(id).unwrap().try_lock().is_ok());
        let stale = send("stale-stop",6,"root-test");
        assert!(stale.contains("revision_conflict"), "{stale}");
        assert!(!retry.is_disabled());
    }

    #[test]
    fn request_lock_protects_durable_completion_but_not_response_io() {
        let lock = super::super::session_coordination::OperationLane::new();
        let mut trace = SlowSubmitTrace::new();
        trace.request_guard = Some(lock.lock().unwrap());
        for stage in [SubmitStage::SessionResource, SubmitStage::IdempotencyBegin,
            SubmitStage::Dispatch, SubmitStage::IdempotencyComplete] {
            trace.advance(stage);
            assert!(matches!(lock.try_lock(), Err(super::super::session_coordination::ReservationError::WouldBlock)));
        }
        trace.advance(SubmitStage::ResponseWrite);
        assert!(lock.try_lock().is_ok(), "socket writes must not serialize the next operation");
    }

    #[test]
    fn request_lock_is_released_on_early_failure() {
        let lock = super::super::session_coordination::OperationLane::new();
        {
            let mut trace = SlowSubmitTrace::new();
            trace.request_guard = Some(lock.lock().unwrap());
            trace.advance(SubmitStage::Dispatch);
        }
        assert!(lock.try_lock().is_ok());
    }

    #[test]
    fn control_diagnostics_requires_root_and_reports_process_scope() {
        let mut request = request("/v2/diagnostics/control");
        let rejected = capture_http_response(|stream| handle_control_diagnostics(stream, &request, Some("test-root")));
        assert!(rejected.starts_with("HTTP/1.1 401"));
        request.headers.insert("authorization".into(), "Bearer test-root".into());
        let accepted = capture_http_response(|stream| handle_control_diagnostics(stream, &request, Some("test-root")));
        let value = response_json(&accepted);
        assert_eq!(value["scope"], "management_process");
        assert_eq!(value["timing"]["scope"], "process");
        assert_eq!(value["capacity"]["controlConnectionReserve"], 32);
        assert!(value["queues"]["repository"]["available"].is_boolean());
        assert!(value["queues"]["nativeEvents"]["outstandingEvents"].is_number());
        request.method = "POST".into();
        let rejected = capture_http_response(|stream| handle_control_diagnostics(stream, &request, Some("test-root")));
        assert!(rejected.starts_with("HTTP/1.1 405"));
    }

    #[test]
    fn service_tier_patch_accepts_known_tiers_on_codex_sessions_only() {
        let codex = json!({ "runtime": { "backend": "host" } });
        let claude = json!({ "runtime": { "backend": "claude_subscription" } });
        let patch = |value: Value| json!({ "serviceTier": value }).as_object().unwrap().clone();
        for tier in ["priority", "default", "flex", "fast"] {
            assert!(validate_service_tier_patch(&patch(json!(tier)), &codex).is_ok(), "{tier}");
        }
        assert!(validate_service_tier_patch(&patch(Value::Null), &codex).is_ok());
        assert!(validate_service_tier_patch(&patch(json!("turbo")), &codex).is_err());
        assert!(validate_service_tier_patch(&patch(json!(true)), &codex).is_err());
        assert!(validate_service_tier_patch(&patch(json!("priority")), &claude).is_err());
        // Other patches on Claude sessions are not affected.
        let model = json!({ "model": "m" }).as_object().unwrap().clone();
        assert!(validate_service_tier_patch(&model, &claude).is_ok());
    }

    #[test]
    fn slow_submit_trace_is_bounded_and_has_no_payload_fields() {
        let mut trace = SlowSubmitTrace::new();
        let at = trace.started + Duration::from_secs(1);
        assert!(
            trace.line_at(at).is_none(),
            "non-submit requests stay silent"
        );
        trace.enable(&"r\n\t".repeat(100), &"s\r\n".repeat(100));
        assert!(trace
            .line_at(trace.started + Duration::from_millis(999))
            .is_none());
        let line = trace.line_at(at).unwrap();
        assert!(line.len() < 512);
        assert!(!line.contains(['\n', '\r', '\t']));
        assert!(line.starts_with("cutex-submit-slow request="));
        assert_eq!(line.split_whitespace().count(), 11);
        trace.ids = None;
    }

    #[test]
    fn slow_submit_trace_measures_actual_completion_and_response_separately() {
        let mut trace = SlowSubmitTrace::new();
        trace.enable("request-1", "cutex.fixture");
        let start = trace.started;
        trace.advance_at(
            SubmitStage::SessionResource,
            start + Duration::from_millis(1000),
        );
        trace.advance_at(
            SubmitStage::IdempotencyBegin,
            start + Duration::from_millis(3300),
        );
        trace.advance_at(SubmitStage::Dispatch, start + Duration::from_millis(3321));
        trace.advance_at(
            SubmitStage::IdempotencyComplete,
            start + Duration::from_millis(3456),
        );
        trace.advance_at(
            SubmitStage::ResponseWrite,
            start + Duration::from_millis(5394),
        );
        trace.finished = Some(start + Duration::from_millis(5404));
        trace.status = 200;
        let line = trace.line_at(start + Duration::from_secs(10)).unwrap();
        for span in [
            "total_ms=5404",
            "session_mutex_ms=1000",
            "session_resource_ms=2300",
            "idempotency_begin_ms=21",
            "dispatch_ms=135",
            "idempotency_complete_ms=1938",
            "response_write_ms=10",
            "status=200",
        ] {
            assert!(line.contains(span), "{line}");
        }
        trace.ids = None;
    }

    static NATIVE_FORWARD_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn queue_update_reports_moved_position_and_queue_revision_in_events() {
        const CHILD: &str = "CUTEX_QUEUE_UPDATE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let parent = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test home"));
            let home = parent.join(format!("queue-update-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join(".cutex-test-private-home"), b"queue update test\n").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "management::v2::server::tests::queue_update_reports_moved_position_and_queue_revision_in_events", "--nocapture"])
                .env("HOME", &home).env("CUTEX_TEST_PRIVATE_HOME", &home).env(CHILD, "1")
                .status().unwrap();
            std::fs::remove_dir_all(home).unwrap();
            assert!(status.success(), "isolated queue update test failed");
            return;
        }

        let session = format!("queue-position-{}", uuid::Uuid::new_v4());
        let root = std::env::temp_dir().join(&session);
        let events = EventRepository::open(&root, crate::platform::host::current_host_name()).unwrap();
        let repository = user_input_repository().unwrap();
        for client in ["a", "b"] {
            let params = parse_user_input_submit_params(&json!({"clientUserMessageId":client,
                "origin":{"kind":"other","clientId":"desktop"},"strategy":"queue",
                "input":[{"type":"text","text":client}]}))
            .unwrap();
            repository
                .enqueue(&session, "thread", "enqueue", &params)
                .unwrap();
        }
        let (_, items) = repository.list(&session).unwrap();
        repository
            .move_admission_item(
                &session,
                &items[1].queue_id,
                repository.admission(&session).unwrap().order_revision,
                Some(&crate::management::v2::admission::AdmissionRef { kind: "owner".into(), id: items[0].queue_id.clone() }),
                true,
            )
            .unwrap();
        let request = ValidatedCutexRequest {
            request_id: "update".into(),
            method: "cutex/userInput/queue/update".into(),
            params: json!({"queueId":items[1].queue_id,"expectedRevision":1,"input":[{"type":"text","text":"edited"}]}),
        };
        let result = dispatch_queue_update(&events, &session, &request).unwrap();
        assert_eq!(result["position"], 0);
        let conflict = dispatch_queue_update(&events, &session, &request).unwrap_err();
        assert_eq!(conflict.details["currentRevision"], 2);
        dispatch_queue_remove(
            &events,
            &session,
            &ValidatedCutexRequest {
                request_id: "remove".into(),
                method: "cutex/userInput/queue/remove".into(),
                params: json!({"queueId":items[1].queue_id,"expectedRevision":2}),
            },
        )
        .unwrap();
        let published = events
            .page(super::super::repository::ReplayQuery {
                cutex_session_id: Some(session.clone()),
                ..Default::default()
            })
            .unwrap()
            .events;
        let updated = published[0].cutex.as_ref().unwrap();
        assert_eq!(updated.params["position"], 0);
        assert_eq!(updated.params["queueRevision"], 4);
        assert_eq!(
            published[1].cutex.as_ref().unwrap().params["queueRevision"],
            5
        );
        for event in published {
            let event = event.cutex.unwrap();
            super::super::contract_validation::validate_cutex_event_message(
                &json!({"method":event.method,"params":event.params}),
            )
            .unwrap();
        }
        repository.clear_session(&session).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// C-010 error codes: an accepted queue item's edit is a non-retryable
    /// 409 (not a storage error); an unresolved earlier submit is a retryable
    /// 202 `user_input_in_progress`.
    #[test]
    fn user_input_delivery_errors_have_client_statuses() {
        let error = queue_repository_error(super::super::user_input::QueueInputAlreadySubmitted.into());
        assert_eq!(error.code, "queue_input_already_submitted");
        assert!(!error.retryable);
        assert!(!error.outcome_unknown);
        assert_eq!(cutex_operation_error_status(&error.code, false), 409);
        let storage = queue_repository_error(anyhow::anyhow!("disk failed"));
        assert_eq!(storage.code, "event_persistence_unavailable");
        assert_eq!(cutex_operation_error_status(&storage.code, false), 503);
        assert_eq!(cutex_operation_error_status("user_input_in_progress", false), 202);
        assert_eq!(http_reason(202), "Accepted");
        assert_eq!(cutex_operation_error_status("admission_item_not_found", false), 404);
    }

    #[test]
    fn owner_delegation_state_errors_are_client_errors_not_500() {
        // cutex/ask/resume, cutex/ask/close changesRequested (reopen) on an ask in the wrong state.
        for code in ["ask_not_waiting", "ask_not_reopenable", "ask_not_running", "too_many_revisions"] {
            assert_eq!(cutex_operation_error_status(code, false), 409, "{code}");
        }
        assert_eq!(cutex_operation_error_status("revision_not_found", false), 404);
    }

    #[test]
    fn relay_address_refusals_are_conflicts_not_500() {
        for code in ["variant_base_url_missing", "relay_runtime_unavailable"] {
            assert_eq!(human_agent_error_status(code), 409, "{code}");
            assert_eq!(cutex_operation_error_status(code, false), 409, "{code}");
        }
    }

    #[test]
    fn removed_release_rotation_routes_cover_the_whole_prefix_only() {
        for path in [
            "/v2/release-rotation",
            "/v2/release-rotation/template",
            "/v2/release-rotation/retry",
            "/v2/release-rotation/director-request",
        ] {
            assert!(removed_v2_route(path).is_some(), "{path}");
        }
        for path in ["/v2/release-rotations", "/v2/release"] {
            assert!(removed_v2_route(path).is_none(), "{path}");
        }
    }

    /// r219: the Task Service and seat administration answer 410 on the
    /// whole `/v2/task-service` prefix and on the exact task segment shapes;
    /// neighbouring session / project routes keep their handlers.
    #[test]
    fn removed_task_service_routes_cover_exact_shapes_only() {
        for path in [
            "/v2/task-service",
            "/v2/task-service/seats",
            "/v2/task-service/seats/bind",
            "/v2/task-service/management-query",
            "/v2/task-service/human-recovery",
            "/v2/task-service/anything/else",
            "/v2/tasks/task-1",
            "/v2/tasks/task%2F1",
            "/v2/sessions/cutex.worker/tasks",
            "/v2/projects/project-alpha/tasks",
        ] {
            assert!(removed_v2_route(path).is_some(), "{path}");
        }
        assert!(removed_v2_route("/v2/task-service/seats/bind")
            .unwrap()
            .contains("seat administration"));
        for path in [
            "/v2/task-services",
            "/v2/tasks",
            "/v2/tasks/",
            "/v2/tasks/a/b",
            "/v2/sessions",
            "/v2/sessions/tasks",
            "/v2/sessions/cutex.worker",
            "/v2/sessions//tasks",
            "/v2/sessions/a/b/tasks",
            "/v2/sessions/cutex.worker/tasks/extra",
            "/v2/sessions/cutex.worker/history",
            "/v2/sessions/cutex.worker/mcp",
            "/v2/projects/tasks",
            "/v2/projects//tasks",
            "/v2/projects/project-alpha",
            "/v2/projects/project-alpha/memory",
            "/v2/projects/project-alpha/memory/tasks",
            "/v2/projects/project-alpha/mcp",
            "/v2/projects/project-alpha/fs/dir",
            "/v2/projects/project-alpha/tasks/task-1",
            "/v2/agent-management/projects/project-alpha",
            "/v2/job-service/management-query",
        ] {
            assert!(removed_v2_route(path).is_none(), "{path}");
        }
    }

    #[test]
    fn legacy_director_import_is_root_scoped_and_not_an_ambient_agent_route() {
        let path = "/v2/agent-management/legacy-director-ownership-import";
        let raw_agent_bus_bearer = "ordinary-agent-bus-bearer";
        assert!(agent_management_admin_path(path));
        assert!(!agent_management_admin_path("/v2/agent-management/actions"));
        assert_eq!(
            v2_required_token(path, Some(raw_agent_bus_bearer), Some("agent-management-root")),
            Some("agent-management-root")
        );
        assert_ne!(
            v2_required_token(path, Some(raw_agent_bus_bearer), Some("agent-management-root")),
            Some(raw_agent_bus_bearer),
            "ordinary Agent Bus authentication must not authorize ownership import"
        );
        assert_eq!(v2_required_token(path, Some(raw_agent_bus_bearer), None), None);
        // Ordinary Agent Management actions keep the ordinary bearer; project
        // authority stays root.
        assert_eq!(
            v2_required_token(
                "/v2/agent-management/actions",
                Some(raw_agent_bus_bearer),
                Some("agent-management-root"),
            ),
            Some(raw_agent_bus_bearer)
        );
        assert_eq!(
            v2_required_token(
                "/v2/agent-management/authority",
                Some(raw_agent_bus_bearer),
                Some("agent-management-root"),
            ),
            Some("agent-management-root")
        );
    }

    #[test]
    fn history_turn_paths_decode_both_ids_and_require_root() {
        let path = "/v2/sessions/cutex%2Esession/history/turns/turn%20one";
        assert_eq!(history_turn_path(path), Some(("cutex.session".into(), "turn one".into())));
        assert!(agent_management_admin_path(path));
        assert!(history_session_id_from_path(path).is_none());
        for path in ["/v2/sessions/s/history/turns/", "/v2/sessions//history/turns/t",
            "/v2/sessions/s%2Fbad/history/turns/t", "/v2/sessions/s/history/turns/t%2fbad",
            "/v2/sessions/s/history/turns/t/more", "/v2/sessions/s/history/turns/%zz"] {
            assert!(history_turn_path(path).is_none(), "{path}");
        }
        // Design 025: history media read back like history (root only).
        let media = format!("/v2/sessions/cutex%2Esession/media/{}", "0".repeat(64));
        assert!(agent_management_admin_path(&media));
        assert!(session_id_from_path(&media).is_none() && history_turn_path(&media).is_none());
    }

    #[test]
    fn reservation_reconciliation_is_root_scoped_and_not_an_ambient_agent_route() {
        let path = "/v2/agent-management/reservation-reconciliation";
        let raw_agent_bus_bearer = "ordinary-agent-bus-bearer";
        assert!(agent_management_admin_path(path));
        assert!(!agent_management_admin_path("/v2/agent-management/actions"));
        assert_eq!(
            v2_required_token(path, Some(raw_agent_bus_bearer), Some("agent-management-root")),
            Some("agent-management-root")
        );
        assert_ne!(
            v2_required_token(path, Some(raw_agent_bus_bearer), Some("agent-management-root")),
            Some(raw_agent_bus_bearer),
            "ordinary Agent Bus authentication must not authorize reservation reconciliation"
        );
        assert_eq!(v2_required_token(path, Some(raw_agent_bus_bearer), None), None);
    }

    #[test]
    fn human_management_projects_jobs_and_writes_require_the_dedicated_root_credential() {
        for path in [
            "/v2/projects/project/director",
            "/v2/projects/project/director/handover",
            "/v2/projects/project/director/scopes",
            "/v2/projects/project/director/scopes/candidates",
            "/v2/projects/project/director/start-reviews",
            "/v2/projects/project/director/start-reviews/review/accept",
            "/v2/projects/project/director/start-reviews/review/content",
            "/v2/agent-management/native-recovery",
            "/v2/agent-management/human-config",
            "/v2/agent-management/durable-candidates",
            "/v2/agent-management/durable-import",
            "/v2/agent-management/legacy-director-ownership-import",
            "/v2/agent-management/reservation-reconciliation",
            "/v2/agent-management/create-abandon",
            "/v2/agent-management/projects",
            "/v2/agent-management/projects/cutex-stack-main",
            "/v2/agent-management/project-presentation",
            "/v2/agent-management/operator-actions",
            "/v2/agent-management/project-mutations",
            "/v2/job-service/management-query",
            "/v2/job-service/capabilities",
            "/v2/job-service/cancel",
            "/v2/job-service/output",
            "/v2/job-service/mark-external",
            "/v2/nci-verification",
            "/v2/nci-verification/config",
            "/v2/nci-verification/key",
            "/v2/nci-verification/helper",
            "/v2/inbound-mail",
            "/v2/inbound-mail/config",
            "/v2/inbound-mail/test",
            "/v2/host",
            "/v2/host/sessions",
            "/v2/human/agents",
            "/v2/human/sessions",
            "/v2/profiles",
            "/v2/profiles/GLM",
            "/v2/profiles/GLM/models",
            "/v2/profiles/GLM/models/glm-5.3/reset",
            "/v2/profile-imports/cc-switch",
            "/v2/settings",
            "/v2/notification-levels",
            "/v2/notification-levels/evaluate",
            "/v2/user-instructions",
        ] {
            assert!(agent_management_admin_path(path), "root scope: {path}");
            assert_eq!(
                v2_required_token(path, Some("ordinary-management"), Some("agent-management-root")),
                Some("agent-management-root"),
                "dedicated credential: {path}"
            );
            assert_eq!(
                v2_required_token(path, Some("ordinary-management"), None),
                None,
                "the ordinary credential must not authorize: {path}"
            );
        }
        // The removed task routes are no longer root routes: they answer 410
        // before any credential check.
        for path in [
            "/v2/task-service/management-query",
            "/v2/task-service/human-recovery",
            "/v2/sessions/cutex.worker/tasks",
            "/v2/tasks/task-1",
        ] {
            assert!(!agent_management_admin_path(path), "{path}");
            assert!(removed_v2_route(path).is_some(), "{path}");
        }
    }

    #[test]
    fn direct_forged_agent_management_invocation_is_rejected() {
        let request = SimpleHttpRequest {
            method: "POST".to_string(),
            path: "/v2/agent-management/actions".to_string(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "invocation": {
                    "caller_cutex_session": "cutex.director-forged",
                    "caller_runtime_agent_id": "runtime-forged"
                },
                "request": {
                    "schema": "cutex/agent-management/v1",
                    "action_id": "forged-management-action",
                    "project_id": "cutex-project",
                    "operation": { "kind": "query_managed" }
                }
            }))
            .unwrap(),
        };
        let response =
            capture_http_response(|stream| handle_agent_management_action(stream, &request));
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        let body = response_json(&response);
        assert_eq!(body["error"]["code"], "ambient_agent_bus_required");
    }

    fn forward_test_native_request(
        cutex_session_id: &str,
        message: Value,
    ) -> Result<Value, ManagementNativeForwardError> {
        NATIVE_FORWARD_CALLS.fetch_add(1, Ordering::SeqCst);
        assert_eq!(cutex_session_id, "cutex-session-1");
        assert_eq!(message["id"], "native-request-1");
        assert_eq!(message["futureRequestField"], json!({ "kept": true }));
        Ok(json!({
            "id": message["id"].clone(),
            "result": { "thread": { "id": "thread-1" } },
            "futureResponseField": null
        }))
    }

    fn execute_test_user_input(
        command: UserInputSubmitCommand,
    ) -> Result<
        super::super::user_input::UserInputSubmitExecution,
        super::super::user_input::UserInputExecutionError,
    > {
        assert_eq!(command.management_request_id, "management-input-1");
        assert_eq!(command.params.origin.client_id, "device-1");
        Ok(super::super::user_input::UserInputSubmitExecution {
            join_refusal: None,
            disposition: UserInputDisposition::Started,
            app_server_accepted: true,
            native_request_id: Some(json!("native-input-1")),
            native_method: Some("turn/start".to_string()),
            turn_id: Some("turn-1".to_string()),
            queue: None,
            queue_paused: false,
        })
    }

    fn execute_test_runtime_mutation(
        cutex_session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, super::super::user_input::UserInputExecutionError> {
        assert_eq!(cutex_session_id, "cutex-session-1");
        assert_eq!(method, "cutex/runtime/offline");
        assert_eq!(params["expectedRuntimeGeneration"], 7);
        assert_eq!(params["reason"], "owner_requested");
        assert_eq!(params["force"], true);
        Ok(json!({
            "runtimeGeneration": 7,
            "runtimeAgentId": null,
            "status": "offline"
        }))
    }

    fn execute_test_runtime_online_requiring_foreground(
        cutex_session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, super::super::user_input::UserInputExecutionError> {
        assert_eq!(cutex_session_id, "cutex-session-1");
        assert_eq!(method, "cutex/runtime/online");
        assert_eq!(params["expectedRuntimeGeneration"], 7);
        assert_eq!(params["openVisibleTerminal"], true);
        Ok(json!({
            "runtimeGeneration": 8,
            "runtimeAgentId": "runtime-2",
            "status": "online",
            "foregroundRequiredReason": "desktop_launcher_unavailable"
        }))
    }

    fn execute_test_runtime_online_with_profile(
        cutex_session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, super::super::user_input::UserInputExecutionError> {
        assert_eq!(cutex_session_id, "cutex-session-1");
        assert_eq!(method, "cutex/runtime/online");
        assert_eq!(params["expectedRuntimeGeneration"], 7);
        assert_eq!(params["launchProfile"], "beta");
        Ok(json!({
            "runtimeGeneration": 8,
            "runtimeAgentId": "runtime-2",
            "status": "online",
            "launchProfile": {
                "requested": "beta",
                "selected": "beta-canonical",
                "effective": "beta-canonical",
                "source": "one_launch_override",
                "applicationScope": "runtime_and_tui",
                "persisted": false
            }
        }))
    }

    fn execute_test_metadata_mutation(
        cutex_session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, super::super::user_input::UserInputExecutionError> {
        assert_eq!(method, "cutex/session/metadata/update");
        assert_eq!(params["expectedRevision"], 7);
        Ok(json!({
            "cutexSessionId": cutex_session_id,
            "revision": 8,
            "displayName": params["displayName"].as_str().unwrap_or("Formal"),
            "displayNameOverride": params["displayName"],
            "note": params.get("note").cloned().unwrap_or(Value::Null),
            "avatar": params.get("avatar").cloned().unwrap_or(Value::Null),
            "avatarMode": params.get("avatarMode").cloned().unwrap_or(Value::Null),
            "badge": params.get("badge").cloned().unwrap_or(Value::Null),
        }))
    }

    #[test]
    fn avatar_mode_metadata_requires_cas_and_round_trips_response_and_event() {
        for mode in [json!("fixed"), json!("inherit"), Value::Null] {
            let params=json!({"expectedRevision":7,"avatarMode":mode});
            validate_session_metadata_params(&params).unwrap();
            let body=json!({"requestId":"avatar-mode","method":"cutex/session/metadata/update","params":params});
            let request=validate_cutex_request_body(&serde_json::to_vec(&body).unwrap()).unwrap();
            let root=std::env::temp_dir().join(format!("cutex-avatar-mode-dispatch-{}",uuid::Uuid::new_v4()));
            let repository=EventRepository::open(&root,crate::platform::host::current_host_name()).unwrap();
            let result=dispatch_session_metadata_update(&repository,execute_test_metadata_mutation,"cutex-session-1",&json!({"revision":7}),&request).unwrap();
            assert_eq!(result["avatarMode"],mode);
            super::super::contract_validation::validate_cutex_response(&json!({
                "contractVersion":2,"requestId":"avatar-mode","cutexSessionId":"cutex-session-1",
                "cutex":{"method":"cutex/session/metadata/update","result":result},
            })).unwrap();
            let page=repository.page(ReplayQuery::default()).unwrap();
            let event=page.events.iter().filter_map(|event|event.cutex.as_ref()).find(|event|event.method=="cutex/session/metadataUpdated").unwrap();
            assert_eq!(event.params["avatarMode"],mode);
            super::super::contract_validation::validate_cutex_event_message(&json!({"method":event.method,"params":event.params})).unwrap();
            std::fs::remove_dir_all(root).unwrap();
            let unfenced=json!({"requestId":"avatar-mode-bad","method":"cutex/session/metadata/update","params":{"avatarMode":mode}});
            assert!(validate_session_metadata_params(&unfenced["params"]).is_err());
            assert!(validate_cutex_request_body(&serde_json::to_vec(&unfenced).unwrap()).is_err());
        }
        for bad in [json!({"expectedRevision":7,"avatarMode":"dynamic"}),json!({"expectedRevision":7,"avatarMode":true}),
            json!({"expectedRevision":null,"avatarMode":"fixed"}),json!({"expectedRevision":-1,"avatarMode":"fixed"}),
            json!({"expectedRevision":7,"avatarMode":"inherit","avatar":{"kind":"emoji","value":"🦊"}})] {
            assert!(validate_session_metadata_params(&bad).is_err());
        }
        validate_session_metadata_params(&json!({"expectedRevision":7,"avatarMode":"inherit","avatar":null})).unwrap();
        validate_session_metadata_params(&json!({"avatar":null})).unwrap();
    }

    #[test]
    fn optional_fences_may_be_omitted_but_are_checked_when_supplied() {
        for (method, params) in [
            ("cutex/session/metadata/update", json!({ "displayName": "Fox" })),
            ("cutex/session/profile/set", json!({ "profile": "alpha" })),
            ("cutex/session/profile/clear", json!({})),
            ("cutex/session/profile/switch", json!({ "profile": "alpha", "when": "now" })),
            ("cutex/session/groups/set", json!({ "groups": ["a"] })),
            ("cutex/session/visibility/hide", json!({})),
            ("cutex/session/defaults/update", json!({ "patch": { "model": "m" } })),
            ("cutex/focus/set", json!({ "owner": "pc", "mobileMuted": false, "source": "desktop" })),
            ("cutex/focus/clear", json!({})),
            ("cutex/runtime/online", json!({})),
            ("cutex/runtime/offline", json!({ "force": true })),
            ("cutex/runtime/close", json!({})),
        ] {
            let body = json!({ "requestId": "fence-free", "method": method, "params": params });
            validate_cutex_request_body(&serde_json::to_vec(&body).unwrap())
                .unwrap_or_else(|_| panic!("{method} must accept omitted fences"));
        }
        validate_session_metadata_params(&json!({ "note": "n" })).unwrap();
        assert!(validate_session_metadata_params(&json!({})).is_err());
        let session = json!({ "revision": 6 });
        require_session_revision(&session, json!({}).as_object().unwrap()).unwrap();
        require_session_revision(&session, json!({ "expectedRevision": 6 }).as_object().unwrap())
            .unwrap();
        let conflict =
            require_session_revision(&session, json!({ "expectedRevision": 5 }).as_object().unwrap())
                .unwrap_err();
        assert_eq!(conflict.code, "revision_conflict");
    }

    #[test]
    fn session_metadata_update_validates_dispatches_and_emits_contract_event() {
        let request_body = json!({
            "requestId": "metadata-1",
            "method": "cutex/session/metadata/update",
            "params": {
                "expectedRevision": 7,
                "displayName": "Fox",
                "avatar": { "kind": "emoji", "value": "🦊" },
                "note": null
            }
        });
        let validated =
            validate_cutex_request_body(&serde_json::to_vec(&request_body).unwrap())
                .expect("metadata request matches the registry");
        for params in [
            json!({ "expectedRevision": 7 }),
            json!({ "expectedRevision": 7, "displayName": "" }),
            json!({ "expectedRevision": 7, "displayName": "x".repeat(65) }),
            json!({ "expectedRevision": 7, "note": "n".repeat(281) }),
            json!({ "expectedRevision": 7, "avatar": { "kind": "image", "value": "x" } }),
            json!({ "expectedRevision": 7, "color": "red" }),
            json!({ "expectedRevision": 7, "avatar": { "kind": "emoji", "value": "🦊", "color": "red" } }),
            json!({ "expectedRevision": 7, "avatar": { "kind": "color", "value": "#e8590c", "color": "#e8590c" } }),
            json!({ "expectedRevision": 7, "avatar": { "kind": "emoji", "value": "🦊", "tint": "#e8590c" } }),
        ] {
            let body = json!({
                "requestId": "metadata-bad",
                "method": "cutex/session/metadata/update",
                "params": params,
            });
            assert!(
                validate_cutex_request_body(&serde_json::to_vec(&body).unwrap()).is_err()
                    || validate_session_metadata_params(&body["params"]).is_err(),
                "must reject {body}"
            );
        }
        assert!(validate_session_metadata_params(&json!({
            "expectedRevision": 7, "displayName": " padded "
        }))
        .is_err());
        let combined = json!({
            "requestId": "metadata-color",
            "method": "cutex/session/metadata/update",
            "params": { "expectedRevision": 7, "avatar": { "kind": "emoji", "value": "🦊", "color": "#e8590c" } }
        });
        validate_cutex_request_body(&serde_json::to_vec(&combined).unwrap())
            .expect("emoji + colour matches the registry");
        validate_session_metadata_params(&combined["params"]).expect("emoji + colour");
        validate_session_metadata_params(&json!({ "avatar": { "kind": "icon", "value": "pic-1", "color": "#40C057" } }))
            .expect("icon + colour");
        validate_session_metadata_params(&json!({ "avatar": { "kind": "color", "value": "#40c057" } }))
            .expect("old colour shape");

        let root = std::env::temp_dir().join(format!(
            "cutex-v2-metadata-dispatch-{}",
            uuid::Uuid::new_v4()
        ));
        let repository =
            EventRepository::open(&root, crate::platform::host::current_host_name())
                .expect("open event repository");
        // The fence belongs to the store mutation (per field, r252): a
        // snapshot at another revision is not refused here, and the
        // mutation's conflict comes back without an event.
        fn conflicting_metadata_mutation(
            _: &str,
            _: &str,
            params: Value,
        ) -> Result<Value, super::super::user_input::UserInputExecutionError> {
            assert_eq!(params["expectedRevision"], 7);
            Err(super::super::user_input::UserInputExecutionError {
                stage: "route".to_string(),
                code: "revision_conflict".to_string(),
                message: "session revision conflict: expected 7, current 9".to_string(),
                retryable: true,
                details: json!({ "expectedRevision": 7, "currentRevision": 9, "resyncRequired": true }),
                outcome_unknown: false,
            })
        }
        let stale = json!({ "revision": 6, "native": { "threadId": "thread-1" } });
        let conflict = dispatch_cutex_request(
            &repository,
            execute_test_user_input,
            flush_test_user_input_queue,
            conflicting_metadata_mutation,
            "cutex-session-1",
            &stale,
            &validated,
        )
        .expect_err("stale revision");
        assert_eq!(conflict.code, "revision_conflict");
        assert!(repository
            .page(ReplayQuery::default())
            .expect("events")
            .events
            .iter()
            .all(|event| event.cutex.as_ref().is_none_or(|cutex| cutex.method != "cutex/session/metadataUpdated")));

        let session = json!({ "revision": 7, "native": { "threadId": "thread-1" } });
        let result = dispatch_cutex_request(
            &repository,
            execute_test_user_input,
            flush_test_user_input_queue,
            execute_test_metadata_mutation,
            "cutex-session-1",
            &session,
            &validated,
        )
        .expect("dispatch metadata");
        assert_eq!(result["revision"], 8);
        assert_eq!(result["displayName"], "Fox");
        assert_eq!(result["avatar"]["kind"], "emoji");
        super::super::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "metadata-1",
            "cutexSessionId": "cutex-session-1",
            "cutex": { "method": "cutex/session/metadata/update", "result": result },
        }))
        .expect("metadata response matches the registry");
        let page = repository.page(ReplayQuery::default()).expect("events");
        let event = page
            .events
            .iter()
            .filter_map(|event| event.cutex.as_ref())
            .find(|cutex| cutex.method == "cutex/session/metadataUpdated")
            .expect("metadataUpdated event");
        assert_eq!(event.params["sessionRevision"], 8);
        assert_eq!(event.params["note"], Value::Null);
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": event.method,
            "params": event.params,
        }))
        .expect("metadataUpdated matches the registry");
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    fn execute_test_profile_mutation(
        cutex_session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, super::super::user_input::UserInputExecutionError> {
        assert_eq!(cutex_session_id, "cutex-session-1");
        assert_eq!(params["expectedRevision"], 7);
        match method {
            "cutex/session/profile/set" => {
                assert_eq!(params["profile"], "alpha");
                Ok(json!({
                    "cutexSessionId": cutex_session_id,
                    "revision": 8,
                    "configuredProfile": "alpha",
                }))
            }
            "cutex/session/profile/clear" => Ok(json!({
                "cutexSessionId": cutex_session_id,
                "revision": 8,
                "configuredProfile": null,
            })),
            other => panic!("unexpected profile mutation {other}"),
        }
    }

    fn flush_test_user_input_queue(
        _cutex_session_id: &str,
        _max_items: usize,
        _ignore_pause: bool,
    ) -> anyhow::Result<usize> {
        Ok(0)
    }

    fn request(path: &str) -> SimpleHttpRequest {
        SimpleHttpRequest {
            method: "GET".to_string(),
            path: path.to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn last_event_id_and_query_cursor_must_not_conflict() {
        let mut request = request("/v2/events/stream?after=c2:query&streamId=stream-1");
        request
            .headers
            .insert("last-event-id".to_string(), "c2:header".to_string());
        assert!(matches!(
            replay_query(&request, true),
            Err(ReplayError::ConflictingCursor)
        ));
    }

    #[test]
    fn sse_stream_change_frame_matches_frozen_error_contract_and_has_no_cursor() {
        let mut bytes = Vec::new();
        write_sse_stream_changed(&mut bytes, "devbox", "old-stream", "new-stream")
            .expect("write stream change frame");
        let frame = String::from_utf8(bytes).expect("utf8 SSE frame");
        assert!(frame.starts_with("event: cutex_management_stream_error\n"));
        assert!(!frame.contains("\nid: "));
        let data = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("SSE data line");
        let body: Value = serde_json::from_str(data).expect("parse SSE error body");
        assert_eq!(body["error"]["code"], "stream_changed");
        assert_eq!(body["error"]["details"]["hostId"], "devbox");
        assert_eq!(body["error"]["details"]["requestedStreamId"], "old-stream");
        assert_eq!(body["error"]["details"]["currentStreamId"], "new-stream");
        assert_eq!(body["error"]["details"]["resyncRequired"], true);
    }

    #[test]
    fn page_query_keeps_opaque_cursor_and_session_scope() {
        let query = replay_query(
            &request(
                "/v2/events?streamId=stream-1&after=c2%3Aopaque&limit=17&cutexSessionId=cutex.session-1",
            ),
            false,
        )
        .expect("parse replay query");
        assert_eq!(query.stream_id.as_deref(), Some("stream-1"));
        assert_eq!(query.after.as_deref(), Some("c2:opaque"));
        assert_eq!(query.limit, 17);
        assert_eq!(query.cutex_session_id.as_deref(), Some("cutex.session-1"));
    }

    #[test]
    fn session_path_decoding_is_strict_and_does_not_apply_form_semantics() {
        assert_eq!(
            session_id_from_path("/v2/sessions/cutex%2Esession%2B1").as_deref(),
            Some("cutex.session+1")
        );
        assert_eq!(
            session_id_from_path("/v2/sessions/cutex+session").as_deref(),
            Some("cutex+session")
        );
        assert!(session_id_from_path("/v2/sessions/cutex%2Fsession").is_none());
        assert!(session_id_from_path("/v2/sessions/cutex%ZZsession").is_none());
    }

    #[test]
    fn exact_id_cutex_requests_preserve_root_bearer_proof() {
        let mut request = SimpleHttpRequest {
            method: "POST".into(),
            path: "/v2/sessions/cutex.example/cutex/requests".into(),
            headers: std::collections::HashMap::from([(
                "authorization".into(),
                "Bearer root-test-token".into(),
            )]),
            body: Vec::new(),
        };
        // Authentication is independent of method parsing; the target handler
        // must still return its normal validation error for malformed bodies.
        assert!(authenticated_owner_cutex_request(
            &request,
            Some("root-test-token")
        ));
        assert!(!authenticated_owner_cutex_request(
            &request,
            Some("different-token")
        ));
        assert!(!authenticated_owner_cutex_request(&request, None));
        request
            .headers
            .insert("authorization".into(), "Bearer bus-test-token".into());
        assert!(!authenticated_owner_cutex_request(
            &request,
            Some("root-test-token")
        ));
        request
            .headers
            .insert("authorization".into(), "Bearer root-test-token".into());
        request.path = "/v2/sessions/cutex.example/native/requests".into();
        assert!(!authenticated_owner_cutex_request(
            &request,
            Some("root-test-token")
        ));
    }

    #[test]
    fn human_supported_requests_resolve_offline_unexposed_agents() {
        for method in [
            "cutex/runtime/online",
            "cutex/runtime/offline",
            "cutex/runtime/close",
            "cutex/session/get",
            "cutex/session/profile/set",
            "cutex/userInput/submit",
        ] {
            assert!(request_may_resolve_hidden_session(method, true));
            assert!(!request_may_resolve_hidden_session(method, false));
        }
    }

    #[test]
    fn only_visibility_show_can_resolve_a_hidden_session() {
        assert!(request_may_resolve_hidden_session(
            "cutex/session/visibility/show",
            false
        ));
        assert!(!request_may_resolve_hidden_session(
            "cutex/session/visibility/hide",
            false
        ));
        assert!(!request_may_resolve_hidden_session(
            "cutex/runtime/online",
            false
        ));
    }

    #[test]
    fn unsupported_cutex_method_precheck_returns_forbidden() {
        let body = serde_json::to_vec(&json!({
            "requestId": "unsupported-method-1",
            "method": "cutex/session/notRegistered",
            "params": {}
        }))
        .expect("serialize request");
        let error = validate_cutex_request_body(&body).expect_err("method must be denied");
        assert_eq!(
            error,
            CutexRequestValidationError::UnsupportedMethod(
                "cutex/session/notRegistered".to_string()
            )
        );

        let response = capture_http_response(|stream| {
            write_cutex_request_validation_error(stream, Some("unsupported-method-1"), error)
        });

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        let body = response_json(&response);
        assert_eq!(body["requestId"], "unsupported-method-1");
        assert_eq!(body["error"]["code"], "cutex_method_denied");
        assert_eq!(
            body["error"]["details"]["method"],
            "cutex/session/notRegistered"
        );
        assert_eq!(
            body["error"]["details"]["registryVersion"],
            super::super::session::CUTEX_METHOD_REGISTRY_VERSION
        );
    }

    #[test]
    fn post_mutation_persistence_uncertainty_is_not_replayed() {
        let root =
            std::env::temp_dir().join(format!("cutex-v2-outcome-unknown-{}", uuid::Uuid::new_v4()));
        let idempotency =
            RequestIdempotencyRepository::open(&root, "outcome-test-process".to_string())
                .expect("open idempotency repository");
        let canonical_body = json!({
            "requestId": "retire-uncertain-1",
            "method": "cutex/session/retire",
            "params": {
                "expectedRevision": 4,
                "expectedRuntimeGeneration": 7
            }
        });
        let claim = match idempotency
            .begin("cutex-session-1", "retire-uncertain-1", &canonical_body)
            .expect("begin request")
        {
            BeginRequest::Forward(claim) => claim,
            other => panic!("expected fresh claim, got {other:?}"),
        };
        let response = capture_http_response(|stream| {
            write_cutex_operation_error(
                stream,
                &idempotency,
                &claim,
                "retire-uncertain-1",
                super::super::user_input::UserInputExecutionError {
                    stage: "persistence".to_string(),
                    code: "persistence_uncertain".to_string(),
                    message: "archive mutation may have persisted".to_string(),
                    retryable: true,
                    details: json!({ "writeStage": "store_replace" }),
                    outcome_unknown: true,
                },
            )
        });

        assert!(response.starts_with("HTTP/1.1 409 Conflict"));
        let body = response_json(&response);
        assert_eq!(body["requestId"], "retire-uncertain-1");
        assert_eq!(body["error"]["code"], "request_outcome_unknown");
        assert_eq!(body["error"]["retryable"], false);
        assert_eq!(body["error"]["details"]["resyncRequired"], true);
        assert_eq!(
            body["error"]["details"]["originalCode"],
            "persistence_uncertain"
        );
        assert_eq!(
            body["error"]["details"]["originalDetails"]["writeStage"],
            "store_replace"
        );
        assert!(matches!(
            idempotency
                .begin("cutex-session-1", "retire-uncertain-1", &canonical_body)
                .expect("repeat request"),
            BeginRequest::OutcomeUnknown
        ));

        // Unknown metadata is diagnostic only: it must not contain the error
        // message/details, nor change the repeat outcome asserted above.
        let connection = rusqlite::Connection::open(
            root.join(super::super::request_idempotency::IDEMPOTENCY_DB_FILE),
        ).expect("open diagnostic evidence");
        let stored: String = connection.query_row(
            "SELECT response_body FROM requests WHERE request_id = ?1",
            ["retire-uncertain-1"], |row| row.get(0),
        ).expect("persisted unknown diagnostic");
        let diagnostic: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(diagnostic["originalCode"], "persistence_uncertain");
        assert_eq!(diagnostic["stage"], "persistence");
        assert_eq!(diagnostic["schema"], "cutex.request-unknown-diagnostic.v1");
        assert!(!stored.contains("archive mutation"));
        assert!(!stored.contains("store_replace"));
        drop(connection);
        drop(idempotency);
        std::fs::remove_dir_all(root).expect("remove outcome-unknown test root");
    }

    #[test]
    fn full_idempotency_store_refuses_with_a_retryable_503() {
        let root =
            std::env::temp_dir().join(format!("cutex-v2-idem-full-{}", uuid::Uuid::new_v4()));
        let idempotency = RequestIdempotencyRepository::open_with_policy(
            &root,
            "full-test-process".to_string(),
            super::super::request_idempotency::RetentionPolicy {
                max_records: 1,
                ..Default::default()
            },
        )
        .expect("open idempotency repository");
        let body = |id: &str| json!({ "requestId": id, "method": "turn/start" });
        assert!(matches!(
            idempotency.begin("cutex-session-1", "in-flight", &body("in-flight")).unwrap(),
            BeginRequest::Forward(_)
        ));
        let BeginRequest::CapacityExceeded(full) =
            idempotency.begin("cutex-session-1", "next", &body("next")).unwrap()
        else {
            panic!("the only slot is in flight");
        };
        let response =
            capture_http_response(|stream| write_idempotency_capacity_exceeded(stream, "next", &full));
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"), "{response}");
        let body = response_json(&response);
        assert_eq!(body["requestId"], "next");
        assert_eq!(body["error"]["code"], "idempotency_capacity_exceeded");
        assert_eq!(body["error"]["retryable"], true);
        assert_eq!(body["error"]["details"]["maxRecords"], 1);
        assert_eq!(body["error"]["details"]["inFlightRecords"], 1);
        assert_eq!(body["error"]["details"]["retryAfterMs"], 1_000);
        drop(idempotency);
        std::fs::remove_dir_all(root).expect("remove full-store test root");
    }

    #[test]
    fn native_response_images_are_externalized_before_receipt_and_replay() {
        use base64::Engine;
        static IMAGE_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn forward(_: &str, message: Value) -> Result<Value, ManagementNativeForwardError> {
            IMAGE_CALLS.fetch_add(1,Ordering::SeqCst);
            Ok(json!({"id":message["id"],"result":{"image":{"type":"imageGeneration","result":base64::engine::general_purpose::STANDARD.encode(vec![8;1024*1024])}}}))
        }
        IMAGE_CALLS.store(0,Ordering::SeqCst);
        let root=std::env::temp_dir().join(format!("image-receipt-{}",uuid::Uuid::new_v4()));
        let store=RequestIdempotencyRepository::open(&root,"test".into()).unwrap();
        let session=json!({"native":{"threadId":"thread-1"},"runtime":{"appServerConnected":true}});
        let session_id=format!("cutex.image-{}",uuid::Uuid::new_v4());
        let request=SimpleHttpRequest { method:"POST".into(),path:format!("/v2/sessions/{session_id}/app-server/requests"),headers:HashMap::new(),body:serde_json::to_vec(&json!({"requestId":"image-receipt","native":{"message":{"id":"image","method":"thread/name/set","params":{"threadId":"thread-1","name":"image"}}}})).unwrap() };
        let first=capture_http_response(|stream|handle_native_request_for_session(stream,&request,forward,&session_id,&session,&store));
        assert!(first.starts_with("HTTP/1.1 200 OK"),"{first}");
        let body=response_json(&first);
        assert!(serde_json::to_vec(&body).unwrap().len()<4096);
        let blob=&body["native"]["message"]["result"]["image"]["result"];
        assert_eq!(blob["type"],"cutexBlob");
        let second=capture_http_response(|stream|handle_native_request_for_session(stream,&request,forward,&session_id,&session,&store));
        assert_eq!(response_json(&second),body);
        assert_eq!(IMAGE_CALLS.load(Ordering::SeqCst),1);
        let blobs=super::super::event_blobs::EventBlobStore::open_default().unwrap();
        assert!(blobs.get(&session_id,blob["blobId"].as_str().unwrap()).is_some());
        assert!(blobs.get("cutex.other",blob["blobId"].as_str().unwrap()).is_none());
        drop(store); std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_route_forwards_exact_message_and_deduplicates_completed_response() {
        NATIVE_FORWARD_CALLS.store(0, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("cutex-v2-native-route-{}", uuid::Uuid::new_v4()));
        let idempotency =
            RequestIdempotencyRepository::open(&root, "route-test-process".to_string())
                .expect("open idempotency repository");
        let session = json!({
            "native": { "threadId": "thread-1" },
            "runtime": { "appServerConnected": true }
        });
        let body = serde_json::to_vec(&json!({
            "requestId": "management-request-1",
            "native": {
                "message": {
                    "id": "native-request-1",
                    "method": "thread/name/set",
                    "params": { "threadId": "thread-1", "name": "first" },
                    "futureRequestField": { "kept": true }
                }
            }
        }))
        .expect("serialize request");
        let request = SimpleHttpRequest {
            method: "POST".to_string(),
            path: "/v2/sessions/cutex-session-1/app-server/requests".to_string(),
            headers: HashMap::new(),
            body: body.clone(),
        };

        let first = execute_native_route(&request, &session, &idempotency);
        assert!(first.starts_with("HTTP/1.1 200 OK"));
        let first_body = response_json(&first);
        assert_eq!(first_body["requestId"], "management-request-1");
        assert_eq!(first_body["native"]["message"]["id"], "native-request-1");
        assert!(first_body["native"]["message"]
            .get("futureResponseField")
            .is_some());

        let second = execute_native_route(&request, &session, &idempotency);
        assert_eq!(response_json(&second), first_body);
        assert_eq!(NATIVE_FORWARD_CALLS.load(Ordering::SeqCst), 1);

        let conflicting = SimpleHttpRequest {
            body: serde_json::to_vec(&json!({
                "requestId": "management-request-1",
                "native": {
                    "message": {
                        "id": "native-request-1",
                        "method": "thread/name/set",
                        "params": { "threadId": "thread-1", "name": "second" }
                    }
                }
            }))
            .expect("serialize conflicting request"),
            ..request
        };
        let conflict = execute_native_route(&conflicting, &session, &idempotency);
        assert!(conflict.starts_with("HTTP/1.1 409 Conflict"));
        assert_eq!(
            response_json(&conflict)["error"]["code"],
            "idempotency_conflict"
        );
        assert_eq!(NATIVE_FORWARD_CALLS.load(Ordering::SeqCst), 1);

        // Read-only methods are not recorded: a repeated requestId reads again.
        let read = SimpleHttpRequest {
            method: "POST".to_string(),
            path: "/v2/sessions/cutex-session-1/app-server/requests".to_string(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "requestId": "management-read-1",
                "native": {
                    "message": {
                        "id": "native-request-1",
                        "method": "thread/read",
                        "params": { "threadId": "thread-1", "includeTurns": true },
                        "futureRequestField": { "kept": true }
                    }
                }
            }))
            .expect("serialize read request"),
        };
        for expected_calls in [2, 3] {
            let response = execute_native_route(&read, &session, &idempotency);
            assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
            assert_eq!(NATIVE_FORWARD_CALLS.load(Ordering::SeqCst), expected_calls);
        }

        drop(idempotency);
        std::fs::remove_dir_all(root).expect("remove route test root");
    }

    fn forward_subagent_test_native_request(
        _cutex_session_id: &str,
        message: Value,
    ) -> Result<Value, ManagementNativeForwardError> {
        let thread_id = message["params"]["threadId"].as_str().unwrap_or_default();
        let is_lineage_read = message["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("cutex-lineage-"));
        if is_lineage_read {
            assert_eq!(message["method"], "thread/read");
            assert_eq!(message["params"]["includeTurns"], false);
            let thread = match thread_id {
                "child-1" => json!({ "id": "child-1", "parentThreadId": "thread-1" }),
                "grandchild-1" => json!({
                    "id": "grandchild-1",
                    "parentThreadId": null,
                    "source": { "subAgent": { "thread_spawn": {
                        "parent_thread_id": "child-1", "depth": 2
                    } } }
                }),
                "stranger" => json!({ "id": "stranger", "parentThreadId": null, "source": "vscode" }),
                "stranger-child" => json!({ "id": "stranger-child", "parentThreadId": "stranger" }),
                _ => {
                    return Ok(json!({
                        "id": message["id"].clone(),
                        "error": { "code": -32600, "message": "thread not found" }
                    }))
                }
            };
            return Ok(json!({ "id": message["id"].clone(), "result": { "thread": thread } }));
        }
        Ok(json!({
            "id": message["id"].clone(),
            "result": { "thread": { "id": thread_id }, "forwardedMethod": message["method"].clone() }
        }))
    }

    #[test]
    fn native_route_allows_reads_of_verified_subagent_threads_only() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-native-subagent-{}",
            uuid::Uuid::new_v4()
        ));
        let idempotency =
            RequestIdempotencyRepository::open(&root, "subagent-test-process".to_string())
                .expect("open idempotency repository");
        let cutex_session_id = format!("cutex-session-subagent-{}", uuid::Uuid::new_v4());
        let session = json!({
            "native": { "threadId": "thread-1" },
            "runtime": { "appServerConnected": true }
        });
        let route = |request_id: &str, method: &str, params: Value| {
            let request = SimpleHttpRequest {
                method: "POST".to_string(),
                path: format!("/v2/sessions/{cutex_session_id}/native/requests"),
                headers: HashMap::new(),
                body: serde_json::to_vec(&json!({
                    "requestId": request_id,
                    "native": { "message": { "id": request_id, "method": method, "params": params } }
                }))
                .expect("serialize request"),
            };
            capture_http_response(|stream| {
                handle_native_request_for_session(
                    stream,
                    &request,
                    forward_subagent_test_native_request,
                    &cutex_session_id,
                    &session,
                    &idempotency,
                )
            })
        };

        for (request_id, thread_id) in [("read-child", "child-1"), ("read-grandchild", "grandchild-1")] {
            let response = route(
                request_id,
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": true }),
            );
            assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
            let body = response_json(&response);
            assert_eq!(body["native"]["message"]["result"]["thread"]["id"], thread_id);
        }
        assert!(subagent_lineage::is_descendant(&cutex_session_id, "thread-1", "grandchild-1"));
        let items = route(
            "items-child",
            "thread/items/list",
            json!({ "threadId": "child-1" }),
        );
        assert!(items.starts_with("HTTP/1.1 200 OK"), "{items}");

        for (request_id, thread_id) in [
            ("read-stranger", "stranger"),
            ("read-stranger-child", "stranger-child"),
            ("read-missing", "missing"),
        ] {
            let response = route(
                request_id,
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": false }),
            );
            assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
            let body = response_json(&response);
            assert_eq!(body["error"]["code"], "foreign_thread");
            assert_eq!(body["error"]["details"]["requestedThreadId"], thread_id);
        }

        // Writes stay pinned to the bound thread even for a verified child.
        for (request_id, method, params) in [
            ("start-child", "turn/start", json!({ "threadId": "child-1", "input": [] })),
            ("interrupt-child", "turn/interrupt", json!({ "threadId": "child-1", "turnId": "t" })),
        ] {
            let response = route(request_id, method, params);
            assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
            assert_eq!(response_json(&response)["error"]["code"], "foreign_thread");
        }

        drop(idempotency);
        std::fs::remove_dir_all(root).expect("remove subagent route test root");
    }

    #[test]
    fn server_response_parser_preserves_exact_signed_id_and_unknown_fields() {
        let body = serde_json::to_vec(&json!({
            "responseId": "response-1",
            "native": {
                "message": {
                    "id": i64::MIN,
                    "error": {
                        "code": -32001,
                        "message": "denied",
                        "data": null
                    },
                    "futureField": { "kept": true }
                }
            }
        }))
        .expect("serialize response");
        let response = validate_server_response_body(&body).expect("valid response");
        assert_eq!(response.response_id, "response-1");
        assert_eq!(response.native_message["id"], i64::MIN);
        assert_eq!(
            response.native_message["futureField"],
            json!({ "kept": true })
        );
    }

    #[test]
    fn runtime_generation_precondition_is_strict_and_json_safe() {
        assert_eq!(
            parse_runtime_generation_precondition("\"cutex-runtime-generation:304\"")
                .expect("valid generation"),
            304
        );
        assert_eq!(
            parse_runtime_generation_precondition("\"cutex-runtime-generation:9007199254740991\"")
                .expect("maximum JSON-safe generation"),
            MAX_SAFE_SEQUENCE
        );
        for invalid in [
            "cutex-runtime-generation:304",
            "W/\"cutex-runtime-generation:304\"",
            "\"cutex-runtime-generation:0\"",
            "\"cutex-runtime-generation:9007199254740992\"",
            "\"cutex-runtime-generation:-1\"",
            "\"cutex-runtime-generation:304\", \"other\"",
            "\"runtime-generation:304\"",
        ] {
            assert!(
                parse_runtime_generation_precondition(invalid).is_err(),
                "accepted invalid precondition {invalid}"
            );
        }
    }

    #[test]
    fn user_input_dispatch_appends_route_and_native_acceptance_events() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-user-input-events-{}",
            uuid::Uuid::new_v4()
        ));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id).expect("open event repository");
        let request = ValidatedCutexRequest {
            request_id: "management-input-1".to_string(),
            method: "cutex/userInput/submit".to_string(),
            params: json!({
                "clientUserMessageId": "client-input-1",
                "origin": { "kind": "android", "clientId": "device-1" },
                "strategy": "auto",
                "input": [{ "type": "text", "text": "hello", "text_elements": [] }]
            }),
        };
        let result = dispatch_user_input_submit(
            &repository,
            execute_test_user_input,
            "cutex-session-1",
            &json!({ "native": { "threadId": "thread-1" } }),
            &request,
        )
        .expect("dispatch input");
        assert_eq!(result["nativeRequestId"], "native-input-1");
        assert_eq!(result["disposition"], "started");

        let page = repository
            .page(ReplayQuery {
                cutex_session_id: Some("cutex-session-1".to_string()),
                ..Default::default()
            })
            .expect("read events");
        assert_eq!(page.events.len(), 2);
        assert_eq!(
            page.events[0].cutex.as_ref().expect("route event").method,
            "cutex/userInput/routeAccepted"
        );
        let submitted = page.events[1].cutex.as_ref().expect("submitted event");
        assert_eq!(submitted.method, "cutex/userInput/submitted");
        assert_eq!(submitted.params["input"], request.params["input"]);
        assert_eq!(submitted.params["origin"], request.params["origin"]);
        assert_eq!(
            page.events[1].correlation.native_request_id,
            Some(json!("native-input-1"))
        );
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    /// The owner's HTTP `cutex/runtime/online` clears the asks owner-stop
    /// mark that `cutex/runtime/offline` set (found by the Director
    /// reviewed-relaunch e2e, 2026-10-14: the mark stayed forever, so the
    /// agent was never woken or relaunched for delegations again).
    #[test]
    fn owner_online_clears_the_asks_owner_stop_mark() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-owner-online-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = EventRepository::open(&root, crate::platform::host::current_host_name())
            .expect("open event repository");
        let id = format!("cutex.owner-online-{}", uuid::Uuid::new_v4());
        let asks = crate::asks::store::AskStore::at(root.join("asks"));
        asks.with_state(|state| crate::asks::wake::note_owner_runtime_action(state, &id, true)).unwrap();
        let marked = |id: &str| asks.snapshot().unwrap().owner_offline.contains(id);
        let online_seen_mark = std::cell::Cell::new(None);
        assert!(marked(&id), "offline marks the agent owner-stopped");
        let session = json!({
            "native": { "threadId": "thread-1" },
            "runtime": { "backend": "host", "status": "offline", "runtimeGeneration": 7, "runtimeAgentId": null }
        });
        let request = ValidatedCutexRequest {
            request_id: "owner-online-1".to_string(),
            method: "cutex/runtime/online".to_string(),
            params: json!({ "expectedRuntimeGeneration": 7 }),
        };
        let result = dispatch_owner_runtime_online(
            &repository,
            |id, method, _| {
                assert_eq!(method, "cutex/runtime/online");
                online_seen_mark.set(Some(marked(id)));
                Ok(json!({ "runtimeGeneration": 8, "runtimeAgentId": "runtime-2", "status": "online" }))
            },
            |id| asks.with_state(|state| crate::asks::wake::note_owner_runtime_action(state, id, false)).unwrap(),
            &id,
            &session,
            &request,
        )
        .expect("dispatch owner online");
        assert_eq!(result["status"], "online");
        assert!(!marked(&id), "the owner's online clears the owner-stop mark");
        assert_eq!(online_seen_mark.get(), Some(true), "cleared only after the start succeeded");
        // A failed start keeps it.
        asks.with_state(|state| crate::asks::wake::note_owner_runtime_action(state, &id, true)).unwrap();
        let failed = dispatch_owner_runtime_online(
            &repository,
            |_, _, _| {
                Err(super::super::user_input::UserInputExecutionError {
                    stage: "route".into(),
                    code: "runtime_start_failed".into(),
                    message: "no".into(),
                    retryable: false,
                    details: json!({}),
                    outcome_unknown: false,
                })
            },
            |id| asks.with_state(|state| crate::asks::wake::note_owner_runtime_action(state, id, false)).unwrap(),
            &id,
            &json!({
                "native": { "threadId": "thread-1" },
                "runtime": { "backend": "host", "status": "offline", "runtimeGeneration": 8, "runtimeAgentId": null }
            }),
            &ValidatedCutexRequest { request_id: "owner-online-2".to_string(), method: "cutex/runtime/online".to_string(), params: json!({ "expectedRuntimeGeneration": 8 }) },
        );
        assert!(failed.is_err());
        assert!(marked(&id), "a failed start leaves the agent owner-stopped");
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn runtime_mutation_enforces_generation_and_emits_frozen_lifecycle_events() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-runtime-mutation-events-{}",
            uuid::Uuid::new_v4()
        ));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id).expect("open event repository");
        let session = json!({
            "native": { "threadId": "thread-1" },
            "runtime": {
                "backend": "host",
                "status": "online",
                "runtimeGeneration": 7,
                "runtimeAgentId": "runtime-1"
            }
        });
        let request = ValidatedCutexRequest {
            request_id: "runtime-request-1".to_string(),
            method: "cutex/runtime/offline".to_string(),
            params: json!({
                "expectedRuntimeGeneration": 7,
                "reason": "owner_requested",
                "force": true
            }),
        };
        let result = dispatch_runtime_mutation(
            &repository,
            execute_test_runtime_mutation,
            "cutex-session-1",
            &session,
            &request,
        )
        .expect("dispatch runtime mutation");
        assert_eq!(
            result,
            json!({ "runtimeGeneration": 7, "status": "offline", "runtimeAgentId": null })
        );
        let page = repository
            .page(ReplayQuery::default())
            .expect("read events");
        assert_eq!(page.events.len(), 2);
        assert_eq!(
            page.events[0].cutex.as_ref().unwrap().method,
            "cutex/runtime/offlineRequested"
        );
        assert_eq!(
            page.events[1].cutex.as_ref().unwrap().method,
            "cutex/runtime/offline"
        );
        assert_eq!(
            page.events[0].cutex.as_ref().unwrap().params["status"],
            "closing"
        );
        assert_eq!(
            page.events[1].cutex.as_ref().unwrap().params["status"],
            "offline"
        );
        assert!(page.events[1].cutex.as_ref().unwrap().params["runtimeAgentId"].is_null());
        assert_eq!(
            page.events[1].correlation.management_request_id.as_deref(),
            Some("runtime-request-1")
        );

        let conflict = dispatch_runtime_mutation(
            &repository,
            execute_test_runtime_mutation,
            "cutex-session-1",
            &session,
            &ValidatedCutexRequest {
                request_id: "runtime-request-2".to_string(),
                method: "cutex/runtime/offline".to_string(),
                params: json!({ "expectedRuntimeGeneration": 6 }),
            },
        )
        .expect_err("stale generation must conflict");
        assert_eq!(conflict.code, "revision_conflict");
        assert_eq!(
            repository
                .page(ReplayQuery::default())
                .unwrap()
                .events
                .len(),
            2
        );
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn runtime_online_callback_preserves_native_action_and_attach_route() {
        let root =
            std::env::temp_dir().join(format!("cutex-native-online-{}", uuid::Uuid::new_v4()));
        let repository =
            EventRepository::open(&root, crate::platform::host::current_host_name()).unwrap();
        let session = json!({"runtime": {"runtimeGeneration": 7, "backend": "host"}});
        let request = ValidatedCutexRequest {
            request_id: "native-online".into(),
            method: "cutex/runtime/online".into(),
            params: json!({"expectedRuntimeGeneration": 7, "openVisibleTerminal": true}),
        };
        let original_action = String::from("original-action");
        let callback = |id: &str, method: &str, params: Value| {
            assert_eq!(id, "cutex.example");
            assert_eq!(method, "cutex/runtime/online");
            assert_eq!(params["expectedRuntimeGeneration"], 7);
            Ok(
                json!({"runtimeGeneration": 8, "runtimeAgentId":"stock.exact", "status": "online", "actionId": original_action, "attachCommand": "cutex session takeover cutex.example"}),
            )
        };
        let result =
            dispatch_runtime_mutation(&repository, callback, "cutex.example", &session, &request)
                .unwrap();
        assert_eq!(result["actionId"], "original-action");
        assert_eq!(result["runtimeAgentId"], "stock.exact");
        assert_eq!(
            result["attachCommand"],
            "cutex session takeover cutex.example"
        );
        let stale_session = json!({"runtime": {"runtimeGeneration": 9}});
        let error = dispatch_runtime_mutation(
            &repository,
            |_, _, _| panic!("stale generation must not start an owner"),
            "cutex.example",
            &stale_session,
            &request,
        )
        .unwrap_err();
        assert_eq!(error.code, "revision_conflict");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_online_emits_typed_foreground_required_event() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-runtime-foreground-required-{}",
            uuid::Uuid::new_v4()
        ));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id).expect("open event repository");
        let session = json!({
            "native": { "threadId": "thread-1" },
            "runtime": {
                "backend": "host_foreground",
                "status": "offline",
                "runtimeGeneration": 7,
                "runtimeAgentId": null
            }
        });
        let request = ValidatedCutexRequest {
            request_id: "runtime-online-1".to_string(),
            method: "cutex/runtime/online".to_string(),
            params: json!({
                "expectedRuntimeGeneration": 7,
                "openVisibleTerminal": true
            }),
        };

        let result = dispatch_runtime_mutation(
            &repository,
            execute_test_runtime_online_requiring_foreground,
            "cutex-session-1",
            &session,
            &request,
        )
        .expect("dispatch runtime mutation");
        assert_eq!(
            result,
            json!({ "runtimeGeneration": 8, "status": "online", "runtimeAgentId": "runtime-2" })
        );

        let page = repository
            .page(ReplayQuery::default())
            .expect("read runtime events");
        let methods = page
            .events
            .iter()
            .map(|event| event.cutex.as_ref().unwrap().method.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec![
                "cutex/runtime/onlineRequested",
                "cutex/runtime/online",
                "cutex/host/foregroundRequired"
            ]
        );
        let foreground = page.events[2].cutex.as_ref().unwrap();
        assert_eq!(foreground.params["runtimeGeneration"], 8);
        assert_eq!(foreground.params["reason"], "desktop_launcher_unavailable");
        assert!(foreground.params["requiredAt"].as_str().is_some());
        assert_eq!(
            page.events[2].correlation.management_request_id.as_deref(),
            Some("runtime-online-1")
        );
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn runtime_online_returns_validated_one_launch_profile_receipt() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-runtime-launch-profile-{}",
            uuid::Uuid::new_v4()
        ));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id).expect("open event repository");
        let session = json!({
            "native": { "threadId": "thread-1" },
            "runtime": {
                "backend": "host",
                "status": "offline",
                "runtimeGeneration": 7,
                "runtimeAgentId": null
            }
        });
        let request = ValidatedCutexRequest {
            request_id: "runtime-online-profile-1".to_string(),
            method: "cutex/runtime/online".to_string(),
            params: json!({
                "expectedRuntimeGeneration": 7,
                "launchProfile": " beta "
            }),
        };

        let result = dispatch_runtime_mutation(
            &repository,
            execute_test_runtime_online_with_profile,
            "cutex-session-1",
            &session,
            &request,
        )
        .expect("dispatch runtime mutation");
        assert_eq!(
            result,
            json!({
                "runtimeGeneration": 8,
                "status": "online",
                "runtimeAgentId": "runtime-2",
                "launchProfile": {
                    "requested": "beta",
                    "selected": "beta-canonical",
                    "effective": "beta-canonical",
                    "source": "one_launch_override",
                    "applicationScope": "runtime_and_tui",
                    "persisted": false
                }
            })
        );
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn management_dispatch_routes_profile_mutations_and_normal_launch_receipts() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-profile-dispatch-{}",
            uuid::Uuid::new_v4()
        ));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id).expect("open event repository");
        let session = json!({
            "revision": 7,
            "native": { "threadId": "thread-1" },
            "runtime": {
                "backend": "host",
                "status": "offline",
                "runtimeGeneration": 7,
                "runtimeAgentId": null,
            },
        });
        let set = dispatch_cutex_request(
            &repository,
            execute_test_user_input,
            flush_test_user_input_queue,
            execute_test_profile_mutation,
            "cutex-session-1",
            &session,
            &ValidatedCutexRequest {
                request_id: "profile-set-1".to_string(),
                method: "cutex/session/profile/set".to_string(),
                params: json!({ "expectedRevision": 7, "profile": "alpha" }),
            },
        )
        .expect("dispatch profile set");
        assert_eq!(
            set,
            json!({
                "cutexSessionId": "cutex-session-1",
                "revision": 8,
                "configuredProfile": "alpha",
            })
        );
        let clear = dispatch_cutex_request(
            &repository,
            execute_test_user_input,
            flush_test_user_input_queue,
            execute_test_profile_mutation,
            "cutex-session-1",
            &session,
            &ValidatedCutexRequest {
                request_id: "profile-clear-1".to_string(),
                method: "cutex/session/profile/clear".to_string(),
                params: json!({ "expectedRevision": 7 }),
            },
        )
        .expect("dispatch profile clear");
        assert_eq!(clear["configuredProfile"], Value::Null);

        let online = dispatch_cutex_request(
            &repository,
            execute_test_user_input,
            flush_test_user_input_queue,
            execute_test_runtime_online_with_profile,
            "cutex-session-1",
            &session,
            &ValidatedCutexRequest {
                request_id: "runtime-online-profile-2".to_string(),
                method: "cutex/runtime/online".to_string(),
                params: json!({ "expectedRuntimeGeneration": 7, "launchProfile": "beta" }),
            },
        )
        .expect("dispatch runtime online");
        assert_eq!(online["launchProfile"]["source"], "one_launch_override");
        assert_eq!(online["launchProfile"]["persisted"], false);

        let event_page = repository
            .page(ReplayQuery::default())
            .expect("read events");
        let methods = event_page
            .events
            .iter()
            .filter_map(|event| event.cutex.as_ref().map(|cutex| cutex.method.as_str()))
            .collect::<Vec<_>>();
        assert!(methods.contains(&"cutex/session/profileUpdated"));
        assert!(methods.contains(&"cutex/runtime/online"));
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn legacy_tui_only_normal_receipt_accepts_unknown_provenance() {
        let receipt = json!({
            "requested": "legacy-profile",
            "selected": "legacy-profile",
            "effective": "legacy-profile",
            "source": "unknown",
            "applicationScope": "tui",
            "persisted": false,
        });
        assert_eq!(
            validate_launch_profile_receipt(None, Some(&receipt)).expect("legacy receipt"),
            Some(receipt)
        );
    }

    #[test]
    fn bootstrap_early_checkpoint_replays_terminal_before_and_after_snapshot_barrier() {
        let _home = crate::config::paths::test_home_env_lock();
        use crate::session::runtime_stop::{RequestedStop, StopState};
        use std::sync::{Arc, Barrier};
        let repository = super::super::repository::management_v2_repository().unwrap();
        // 0: terminal before projection read; 1: read closing then terminal
        // before builder; 2: terminal after builder. No scheduling sleeps decide
        // the interleaving: barriers gate the stop owner and event publication.
        for ordering in 0..3 {
            let id = format!("cutex.bootstrap-stop-{ordering}-{}", uuid::Uuid::new_v4());
            let stop = RequestedStop::begin(&id, 7).unwrap();
            let checkpoint = repository.checkpoint().unwrap();
            super::super::native_events::observe_stop_from_snapshot(&id, 7, "host", None).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let peer = barrier.clone();
            let worker_id = id.clone();
            let worker = std::thread::spawn(move || {
                peer.wait();
                stop.finish(true).unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                while super::super::native_events::stop_test_events(&worker_id).is_empty() {
                    assert!(std::time::Instant::now() < deadline, "terminal event missing");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                peer.wait();
            });
            if ordering == 0 { barrier.wait(); barrier.wait(); }
            // This value is precisely the read that can go stale during projection.
            let captured = crate::session::runtime_stop::state(&id, 7).unwrap();
            assert_eq!(captured, if ordering == 0 { StopState::Offline } else { StopState::Closing });
            if ordering == 1 { barrier.wait(); barrier.wait(); }
            let session = json!({ "cutexSessionId": id, "hostId": crate::platform::host::current_host_name(),
                "revision": 1, "runtime": {"status": if captured == StopState::Closing {"closing"} else {"offline"},
                    "runtimeGeneration": 7, "appServerConnected": false}, "management": {} });
            let bootstrap = build_bootstrap_resource_shaped(&session, checkpoint.clone(), None, None,
                |_| panic!("offline snapshot cannot call native"),
                |_| Ok(json!({"userInputQueue":[], "pendingServerRequests":[], "agentBusMessages":[]}))).unwrap();
            if ordering == 2 { barrier.wait(); barrier.wait(); }
            worker.join().unwrap();
            assert_eq!(bootstrap["checkpoint"], serde_json::to_value(&checkpoint).unwrap(),
                "streamId/sequence/cursor are one unchanged early checkpoint");
            let query = ReplayQuery { stream_id: Some(checkpoint.stream_id.clone()), after: checkpoint.cursor.clone(),
                cutex_session_id: Some(id.clone()), ..Default::default() };
            let replay = repository.page(query.clone()).unwrap();
            assert_eq!(replay.events.len(), 1);
            let terminal = &replay.events[0];
            assert!(terminal.sequence > checkpoint.sequence);
            assert_eq!(terminal.cutex.as_ref().unwrap().method, "cutex/runtime/offline");
            assert_eq!(terminal.cutex.as_ref().unwrap().params["runtimeGeneration"], 7);
            // Early cursors intentionally duplicate a terminal already reflected
            // in the snapshot. Replay is read-only, with stable event identity.
            let again = repository.page(query).unwrap();
            assert_eq!(again.events, replay.events);
            assert_eq!(crate::session::runtime_stop::state(&id, 7), Some(StopState::Offline));
            let newer = RequestedStop::begin(&id, 8).unwrap();
            let _ = repository.page(ReplayQuery { stream_id: Some(checkpoint.stream_id), after: checkpoint.cursor,
                cutex_session_id: Some(id.clone()), ..Default::default() }).unwrap();
            assert_eq!(crate::session::runtime_stop::state(&id, 8), Some(StopState::Closing));
            newer.finish(true).unwrap();
        }
    }

    #[test]
    fn bootstrap_keeps_complete_early_checkpoint_on_stream_reset() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("bootstrap-stop-reset-{}", uuid::Uuid::new_v4()));
        let host = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host.clone()).unwrap();
        repository.append(super::super::model::PendingEvent {
            cutex_session_id: "cutex.reset-test".into(), host_id: host.clone(),
            source: super::super::model::EventSource::Cutex, schema: None, native: None,
            correlation: Default::default(), cutex: Some(super::super::model::CutexMessage {
                method: "cutex/test/beforeReset".into(), params: json!({}) }) }).unwrap();
        let checkpoint = repository.checkpoint().unwrap();
        let mut journal = std::fs::OpenOptions::new().append(true).open(root.join("events.jsonl")).unwrap();
        journal.write_all(b"{broken").unwrap(); journal.sync_all().unwrap(); drop(journal);
        let reopened = EventRepository::open(&root, host.clone()).unwrap();
        assert_ne!(reopened.checkpoint().unwrap().stream_id, checkpoint.stream_id);
        let session = json!({"cutexSessionId":"cutex.reset-test", "hostId":host,
            "runtime":{"status":"offline", "runtimeGeneration":7,"appServerConnected":false}});
        let result = build_bootstrap_resource_shaped(&session, checkpoint.clone(), None, None,
            |_| panic!("no native RPC"),
            |_| Ok(json!({"userInputQueue":[],"pendingServerRequests":[],"agentBusMessages":[]}))).unwrap();
        assert_eq!(result["checkpoint"], serde_json::to_value(&checkpoint).unwrap());
        assert!(reopened.page(ReplayQuery { stream_id: Some(checkpoint.stream_id), after: checkpoint.cursor,
            ..Default::default() }).is_err(), "old stream follows existing replay resync error, never hybrid cursor");
    }

    #[test]
    fn bootstrap_checkpoint_precedes_events_racing_with_thread_read() {
        let root =
            std::env::temp_dir().join(format!("cutex-v2-bootstrap-race-{}", uuid::Uuid::new_v4()));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id.clone()).expect("open repository");
        let session = json!({
            "contractVersion": 2,
            "cutexSessionId": "cutex-session-1",
            "hostId": host_id,
            "revision": 1,
            "native": {
                "threadId": "thread-1",
                "schema": {
                    "protocol": "codex-app-server",
                    "majorVersion": 2,
                    "version": "0.144.1",
                    "sha256": "schema",
                    "channel": "experimental",
                    "capabilities": {},
                    "extensions": []
                }
            },
            "runtime": {
                "status": "online",
                "runtimeGeneration": 7,
                "appServerConnected": true
            },
            "focus": {
                "revision": 1,
                "owner": "none",
                "mobileMuted": false,
                "source": null,
                "updatedAt": "2026-07-13T00:00:00Z"
            },
            "management": {}
        });
        let bootstrap = build_bootstrap_resource(
            &session,
            &repository,
            None,
            |request| {
                assert_eq!(request["method"], "thread/read");
                repository
                    .append(super::super::model::PendingEvent {
                        cutex_session_id: "cutex-session-1".to_string(),
                        host_id: crate::platform::host::current_host_name(),
                        source: super::super::model::EventSource::Cutex,
                        schema: None,
                        correlation: Default::default(),
                        native: None,
                        cutex: Some(super::super::model::CutexMessage {
                            method: "cutex/test/raced".to_string(),
                            params: json!({}),
                        }),
                    })
                    .expect("append racing event");
                Ok(json!({
                    "id": request["id"].clone(),
                    "result": { "thread": { "id": "thread-1", "turns": [] } }
                }))
            },
            |_| {
                Ok(json!({
                    "userInputQueue": [],
                    "pendingServerRequests": [],
                    "agentBusMessages": []
                }))
            },
        )
        .expect("build bootstrap");
        assert_eq!(bootstrap["checkpoint"]["sequence"], 0);
        assert!(bootstrap["checkpoint"]["cursor"].is_null());
        let page = repository
            .page(ReplayQuery::default())
            .expect("replay raced event");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].sequence, 1);
        assert_eq!(
            page.events[0].cutex.as_ref().expect("cutex event").method,
            "cutex/test/raced"
        );
        std::fs::remove_dir_all(root).expect("remove bootstrap repository");
    }

    #[test]
    fn paged_bootstrap_reads_the_newest_turns_and_reports_the_page() {
        let root =
            std::env::temp_dir().join(format!("cutex-v2-bootstrap-paged-{}", uuid::Uuid::new_v4()));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id.clone()).expect("open repository");
        let mut session = json!({
            "contractVersion": 2,
            "cutexSessionId": "cutex-session-paged",
            "hostId": host_id,
            "revision": 1,
            "native": { "threadId": "thread-1", "schema": null },
            "runtime": { "status": "online", "runtimeGeneration": 2, "appServerConnected": true },
            "focus": null,
            "management": {}
        });
        let state = |_: u64| -> anyhow::Result<Value> {
            Ok(json!({ "userInputQueue": [], "pendingServerRequests": [], "agentBusMessages": [] }))
        };
        let turn = |n: usize| json!({ "id": format!("turn-{n}"), "status": "completed", "items": [] });
        let mut methods = Vec::new();
        let paged = build_bootstrap_resource(
            &session,
            &repository,
            Some(2),
            |request| {
                let method = request["method"].as_str().unwrap().to_string();
                methods.push(method.clone());
                let result = match method.as_str() {
                    "thread/read" => {
                        assert_eq!(request["params"]["includeTurns"], false);
                        json!({ "thread": { "id": "thread-1", "preview": "p", "turns": [] } })
                    }
                    "thread/turns/list" => {
                        assert_eq!(request["params"]["limit"], 2);
                        assert_eq!(request["params"]["sortDirection"], "desc");
                        json!({ "data": [turn(4), turn(3)], "nextCursor": "c3" })
                    }
                    other => panic!("unexpected {other}"),
                };
                Ok(json!({ "id": request["id"].clone(), "result": result }))
            },
            state,
        )
        .expect("build paged bootstrap");
        assert_eq!(methods, ["thread/read", "thread/turns/list"]);
        let turns = paged["native"]["result"]["thread"]["turns"].as_array().unwrap();
        assert_eq!(turns.iter().map(|t| t["id"].as_str().unwrap()).collect::<Vec<_>>(), ["turn-3", "turn-4"]);
        assert_eq!(paged["native"]["method"], "thread/read");
        assert_eq!(paged["native"]["result"]["thread"]["preview"], "p");
        assert_eq!(
            paged["historyPage"],
            json!({ "limit": 2, "cursor": null, "nextCursor": "c3", "hasMore": true, "firstOrdinal": null, "turnCount": null })
        );

        // A native without thread/turns/list: the full read, paged locally.
        let older = build_bootstrap_resource(
            &session,
            &repository,
            Some(2),
            |request| {
                let result = match request["method"].as_str().unwrap() {
                    "thread/read" if request["params"]["includeTurns"] == true => {
                        json!({ "thread": { "id": "thread-1", "turns": [turn(0), turn(1)] } })
                    }
                    "thread/read" => json!({ "thread": { "id": "thread-1", "turns": [] } }),
                    _ => {
                        return Ok(json!({ "id": request["id"].clone(),
                                          "error": { "code": -32601, "message": "unknown" } }))
                    }
                };
                Ok(json!({ "id": request["id"].clone(), "result": result }))
            },
            state,
        )
        .expect("build bootstrap on an older native");
        assert_eq!(older["native"]["result"]["thread"]["turns"].as_array().unwrap().len(), 2);
        assert_eq!(older["historyPage"]["hasMore"], false);

        // Without turnLimit: unchanged shape, no historyPage key.
        let full = build_bootstrap_resource(
            &session,
            &repository,
            None,
            |request| {
                assert_eq!(request["params"]["includeTurns"], true);
                Ok(json!({ "id": request["id"].clone(),
                           "result": { "thread": { "id": "thread-1", "turns": [turn(0)] } } }))
            },
            state,
        )
        .expect("build full bootstrap");
        assert!(full.get("historyPage").is_none());

        // Offline with turnLimit: no native snapshot, historyPage null.
        session["runtime"]["appServerConnected"] = json!(false);
        let offline = build_bootstrap_resource(
            &session,
            &repository,
            Some(2),
            |_| -> Result<Value, String> { panic!("offline bootstrap must not call the app-server") },
            state,
        )
        .expect("build offline paged bootstrap");
        assert!(offline["native"].is_null());
        assert!(offline["historyPage"].is_null() && offline.get("historyPage").is_some());
        std::fs::remove_dir_all(root).expect("remove bootstrap repository");
    }

    /// The desktop's steer check (`?turnLimit=10&media=ref`): shaped turns
    /// give the same bytes as the whole-body pass, and a second check reads
    /// only the live newest turn in full.
    #[test]
    fn media_ref_bootstrap_pages_reuse_shaped_turns() {
        use base64::Engine;
        let root = std::env::temp_dir().join(format!("cutex-v2-bootstrap-shaped-{}", uuid::Uuid::new_v4()));
        let media_dir = root.join("media");
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id.clone()).expect("open repository");
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let session = json!({
            "contractVersion": 2, "cutexSessionId": id, "hostId": host_id, "revision": 1,
            "native": { "threadId": "thread-1", "schema": null },
            "runtime": { "status": "online", "runtimeGeneration": 2, "appServerConnected": true },
            "focus": null, "management": {}
        });
        let state = |_: u64| -> anyhow::Result<Value> {
            Ok(json!({ "userInputQueue": [], "pendingServerRequests": [], "agentBusMessages": [],
                       "latestPlan": null, "latestTokenUsage": null }))
        };
        let turns: Vec<Value> = (0..4u8)
            .map(|n| {
                let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
                png.extend(std::iter::repeat_n(n, super::super::media_refs::INLINE_MAX));
                let image = base64::engine::general_purpose::STANDARD.encode(png);
                json!({ "id": format!("turn-{n}"), "status": "completed", "error": null, "itemsView": "full",
                        "items": [{ "type": "imageGeneration", "id": format!("ig{n}"), "status": "completed",
                                    "result": image, "savedPath": format!("C:/img/{n}.png") },
                                  { "type": "agentMessage", "id": format!("a{n}"), "text": "done" }] })
            })
            .collect();
        let calls = std::cell::RefCell::new(Vec::new());
        let native = |request: Value| -> Result<Value, String> {
            let params = &request["params"];
            let result = match request["method"].as_str().unwrap() {
                "thread/read" => json!({ "thread": { "id": "thread-1", "preview": "p", "turns": [] } }),
                "thread/turns/list" => {
                    let view = params["itemsView"].as_str().unwrap();
                    calls.borrow_mut().push((view.to_string(), params["limit"].as_u64().unwrap()));
                    let limit = params["limit"].as_u64().unwrap() as usize;
                    let data: Vec<Value> = turns.iter().rev().take(limit).map(|turn| {
                        let mut turn = turn.clone();
                        if view == "notLoaded" {
                            turn["items"] = json!([]);
                            turn["itemsView"] = json!("notLoaded");
                        }
                        turn
                    }).collect();
                    json!({ "data": data, "nextCursor": "c1" })
                }
                other => panic!("unexpected {other}"),
            };
            Ok(json!({ "id": request["id"].clone(), "result": result }))
        };
        let store = super::super::media_refs::MediaStore::at(&media_dir, super::super::media_refs::CACHE_MAX_BYTES);
        let mut expected = build_bootstrap_resource(&session, &repository, Some(3), native, state).unwrap();
        super::super::media_refs::apply(&mut expected, &id, &store);
        assert_eq!(expected["mediaRefs"], 3);
        let shaped = |calls: &std::cell::RefCell<Vec<(String, u64)>>| {
            calls.borrow_mut().clear();
            let mut shaper = super::super::history_turns::TurnShaper::new(&id, &store, false);
            let mut body =
                build_bootstrap_resource_shaped(&session, repository.checkpoint().unwrap(), Some(3), Some(&mut shaper), native, state)
                    .unwrap();
            shaper.finish_body(&mut body);
            (serde_json::to_vec(&body).unwrap(), calls.borrow().clone())
        };
        let full = |limit| ("full".to_string(), limit);
        let listing = |limit| ("notLoaded".to_string(), limit);
        let expected = serde_json::to_vec(&expected).unwrap();
        assert_eq!(shaped(&calls), (expected.clone(), vec![listing(3), full(3)]));
        assert_eq!(shaped(&calls), (expected, vec![listing(3), full(1)]));
        super::super::history_turns::invalidate(&id);
        std::fs::remove_dir_all(root).expect("remove bootstrap repository");
    }

    #[test]
    fn offline_bootstrap_keeps_checkpoint_and_cutex_state_without_native_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-bootstrap-offline-{}",
            uuid::Uuid::new_v4()
        ));
        let host_id = crate::platform::host::current_host_name();
        let repository = EventRepository::open(&root, host_id.clone()).expect("open repository");
        let session = json!({
            "contractVersion": 2,
            "cutexSessionId": "cutex-session-offline",
            "hostId": host_id,
            "revision": 3,
            "native": { "threadId": "thread-offline", "schema": null },
            "runtime": {
                "status": "offline",
                "runtimeGeneration": 0,
                "appServerConnected": false
            },
            "focus": {
                "revision": 1,
                "owner": "none",
                "mobileMuted": false,
                "source": null,
                "updatedAt": "2026-07-13T00:00:00Z"
            },
            "management": {}
        });
        let bootstrap = build_bootstrap_resource(
            &session,
            &repository,
            None,
            |_| -> Result<Value, String> {
                panic!("offline bootstrap must not call the app-server")
            },
            |_| {
                Ok(json!({
                    "userInputQueue": [{ "queueId": "queue-offline-1" }],
                    "admission": { "revision": 7, "items": [], "nextStep": [], "currentTurn": null },
                    "pendingServerRequests": [],
                    "agentBusMessages": []
                }))
            },
        )
        .expect("build offline bootstrap");

        assert!(bootstrap["schema"].is_null());
        assert!(bootstrap["native"].is_null());
        assert_eq!(bootstrap["checkpoint"]["sequence"], 0);
        assert_eq!(bootstrap["cutexState"]["runtime"]["status"], "offline");
        assert_eq!(
            bootstrap["cutexState"]["userInputQueue"][0]["queueId"],
            "queue-offline-1"
        );
        assert_eq!(bootstrap["cutexState"]["admission"]["revision"], 7);
        assert!(bootstrap["cutexState"]["latestPlan"].is_null());
        let plan = json!({
            "threadId": "thread-1", "turnId": "turn-1", "explanation": null,
            "plan": [{ "step": "a", "status": "inProgress" }],
            "updatedAt": "2026-10-07T00:00:00.000Z"
        });
        let with_plan = build_bootstrap_resource(
            &session,
            &repository,
            None,
            |_| -> Result<Value, String> {
                panic!("offline bootstrap must not call the app-server")
            },
            |_| {
                Ok(json!({
                    "userInputQueue": [],
                    "pendingServerRequests": [],
                    "agentBusMessages": [],
                    "latestPlan": plan.clone()
                }))
            },
        )
        .expect("build offline bootstrap with a plan");
        assert_eq!(with_plan["cutexState"]["latestPlan"], plan);
        assert!(with_plan["cutexState"]["latestTokenUsage"].is_null());
        let usage = json!({
            "threadId": "thread-1", "turnId": "turn-1",
            "tokenUsage": {
                "total": { "totalTokens": 30, "inputTokens": 20, "cachedInputTokens": 5,
                           "cacheWriteInputTokens": 0, "outputTokens": 10, "reasoningOutputTokens": 2 },
                "last": { "totalTokens": 30, "inputTokens": 20, "cachedInputTokens": 5,
                          "cacheWriteInputTokens": 0, "outputTokens": 10, "reasoningOutputTokens": 2 },
                "modelContextWindow": 258400
            },
            "updatedAt": "2026-10-07T00:00:00.000Z"
        });
        let with_usage = build_bootstrap_resource(
            &session,
            &repository,
            None,
            |_| -> Result<Value, String> {
                panic!("offline bootstrap must not call the app-server")
            },
            |_| {
                Ok(json!({
                    "userInputQueue": [],
                    "pendingServerRequests": [],
                    "agentBusMessages": [],
                    "latestTokenUsage": usage.clone()
                }))
            },
        )
        .expect("build offline bootstrap with token usage");
        assert_eq!(with_usage["cutexState"]["latestTokenUsage"], usage);
        assert!(with_usage["cutexState"]["latestPlan"].is_null());
        std::fs::remove_dir_all(root).expect("remove bootstrap repository");
    }

    fn execute_native_route(
        request: &SimpleHttpRequest,
        session: &Value,
        idempotency: &RequestIdempotencyRepository,
    ) -> String {
        capture_http_response(|stream| {
            handle_native_request_for_session(
                stream,
                request,
                forward_test_native_request,
                "cutex-session-1",
                session,
                idempotency,
            )
        })
    }

    fn capture_http_response(
        write_response: impl FnOnce(&mut TcpStream) -> anyhow::Result<()>,
    ) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind response listener");
        let address = listener.local_addr().expect("response listener address");
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect response client");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("read route response");
            response
        });
        let (mut stream, _) = listener.accept().expect("accept response client");
        write_response(&mut stream).expect("write response");
        client.join().expect("response client")
    }

    fn response_json(response: &str) -> Value {
        serde_json::from_str(
            response
                .split_once("\r\n\r\n")
                .expect("HTTP response body")
                .1,
        )
        .expect("parse response JSON")
    }
}

#[cfg(test)]
#[path = "model_overload_retry_server_tests.rs"]
mod model_overload_retry_server_tests;
