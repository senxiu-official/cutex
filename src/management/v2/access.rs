//! Route classification and enforcement for non-owner principals (P1 users;
//! cutex-desktop docs/research/user-isolation-p1-20261014.md §3–4; P2 access
//! r236, docs/research/user-isolation-p2-access-20261015.md: read-only
//! workspace files of visible agents, own attachments as `localImage`,
//! delegation commands on visible asks; capability `users-p2-access`).
//!
//! Every Management route has a class: `Public`, `HostRead`,
//! `Scoped(...)` (a non-owner may use it, filtered or checked against the
//! user's grant) or `OwnerOnly`. A route without a rule is `OwnerOnly`
//! (default deny); the tests enumerate every `/v2` literal of the source and
//! require an explicit rule, so a new route cannot slip through unclassified.
//!
//! The owner (root credential or an owner-bound token) never passes through
//! this gate. For a non-owner the gate either answers itself (filtered lists,
//! denials) or lets the ordinary handler run after checking the object (the
//! request then carries the root bearer, as every client-token request does).

use std::collections::HashSet;
use std::net::TcpStream;

use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;
use crate::management::users::{Grant, Principal, Scope};

use super::server::write_v2_error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// No principal needed (pairing).
    Public,
    /// Any authenticated principal, pending included (minimal host facts).
    HostRead,
    /// Any authenticated principal, pending included, answered by the
    /// route's own handler for the caller's own credential only (person
    /// identity challenge / claim; HostRead level; own devices, where the
    /// handler refuses a pending device with 403 `user_pending`).
    AnyPrincipal,
    /// A non-owner may use it within its grant.
    Scoped(Scoped),
    OwnerOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scoped {
    SessionList,
    SessionRead,
    SessionBootstrap,
    SessionHistory,
    SessionHistoryTurn,
    NativeRequest,
    ServerResponse,
    CutexRequest,
    EventPage,
    EventStream,
    Summary,
    SummaryEvents,
    ProjectList,
    ProjectRead,
    AskList,
    AskRead,
    AskAnswer,
    /// `GET|PATCH /v2/users/me`: the caller's own profile.
    SelfProfile,
    /// `GET /v2/users/{id}/avatar`: checked against who the caller may see.
    UserAvatar,
    /// `GET {scope}/fs/dir|file|search|watch` and `{scope}/git/status|diff`
    /// with scope `/v2/projects/{id}` (a granted project) or
    /// `/v2/sessions/{id}` (a visible agent's cwd): read-only, read grant
    /// (P2 access, capability `users-p2-access`).
    WorkspaceRead,
    /// `POST /v2/sessions/{id}/attachments`: a visible agent, `send`; the
    /// upload is recorded for the calling token (P2 access).
    AttachmentUpload,
}

enum Matcher {
    Exact(&'static str),
    /// Textual prefix (only for `OwnerOnly` rules).
    Prefix(&'static str),
    Shape(fn(&str) -> bool),
}

struct Rule {
    /// Empty = any method.
    methods: &'static [&'static str],
    matcher: Matcher,
    class: Class,
}

const GET: &[&str] = &["GET"];
const POST: &[&str] = &["POST"];
const GET_PATCH: &[&str] = &["GET", "PATCH"];
const DELETE: &[&str] = &["DELETE"];
const ANY: &[&str] = &[];

fn ask_read_shape(path: &str) -> bool {
    matches!(super::asks::parse_path(path), Some(super::asks::Route::Ask(_)))
}
fn ask_answer_shape(path: &str) -> bool {
    matches!(super::asks::parse_path(path), Some(super::asks::Route::Answer(_)))
}
fn session_read_shape(path: &str) -> bool {
    super::server::session_id_from_path(path).is_some()
}
fn bootstrap_shape(path: &str) -> bool {
    super::server::bootstrap_session_id_from_path(path).is_some()
}
fn history_shape(path: &str) -> bool {
    super::server::history_session_id_from_path(path).is_some()
}
fn history_turn_shape(path: &str) -> bool {
    super::server::history_turn_path(path).is_some()
}
fn native_shape(path: &str) -> bool {
    super::server::native_request_session_id_from_path(path).is_some()
}
fn server_response_shape(path: &str) -> bool {
    super::server::server_response_session_id_from_path(path).is_some()
}
fn cutex_request_shape(path: &str) -> bool {
    super::server::cutex_request_session_id_from_path(path).is_some()
}
fn user_avatar_shape(path: &str) -> bool {
    super::users_api::avatar_user(path).is_some()
}
fn own_device_shape(path: &str) -> bool {
    super::users_api::me_device_id(path).is_some()
}
fn workspace_read_shape(path: &str) -> bool {
    matches!(
        super::workspace::parse_route(path),
        Some(super::workspace::WorkspaceRoute::Scoped(_, route)) if route != super::workspace::ScopedRoute::Terminals
    )
}
fn attachment_shape(path: &str) -> bool {
    super::attachments::session_id_from_path(path).is_some()
}
fn project_read_shape(path: &str) -> bool {
    super::server::management_project_id_from_path(path).is_some()
}

macro_rules! owner_prefixes {
    ($($prefix:literal),* $(,)?) => {
        &[$(Rule { methods: ANY, matcher: Matcher::Prefix($prefix), class: Class::OwnerOnly }),*]
    };
}

/// Ordered: the first matching rule wins. Scoped rules use the exact path
/// parsers of the handlers they guard; `OwnerOnly` prefix rules follow.
const SCOPED_RULES: &[Rule] = &[
    Rule { methods: POST, matcher: Matcher::Exact(super::pairing::PAIR_PATH), class: Class::Public },
    Rule { methods: GET, matcher: Matcher::Exact("/v2/capabilities"), class: Class::HostRead },
    Rule { methods: GET, matcher: Matcher::Exact("/v2/host"), class: Class::HostRead },
    Rule { methods: GET, matcher: Matcher::Exact(super::identity_api::CHALLENGE_PATH), class: Class::AnyPrincipal },
    Rule { methods: POST, matcher: Matcher::Exact(super::identity_api::CLAIM_PATH), class: Class::AnyPrincipal },
    Rule { methods: GET, matcher: Matcher::Exact("/v2/sessions"), class: Class::Scoped(Scoped::SessionList) },
    Rule { methods: GET, matcher: Matcher::Shape(workspace_read_shape), class: Class::Scoped(Scoped::WorkspaceRead) },
    Rule { methods: POST, matcher: Matcher::Shape(attachment_shape), class: Class::Scoped(Scoped::AttachmentUpload) },
    Rule { methods: GET, matcher: Matcher::Shape(bootstrap_shape), class: Class::Scoped(Scoped::SessionBootstrap) },
    Rule { methods: GET, matcher: Matcher::Shape(history_shape), class: Class::Scoped(Scoped::SessionHistory) },
    Rule { methods: GET, matcher: Matcher::Shape(history_turn_shape), class: Class::Scoped(Scoped::SessionHistoryTurn) },
    Rule { methods: GET, matcher: Matcher::Shape(session_read_shape), class: Class::Scoped(Scoped::SessionRead) },
    Rule { methods: POST, matcher: Matcher::Shape(native_shape), class: Class::Scoped(Scoped::NativeRequest) },
    Rule { methods: POST, matcher: Matcher::Shape(server_response_shape), class: Class::Scoped(Scoped::ServerResponse) },
    Rule { methods: POST, matcher: Matcher::Shape(cutex_request_shape), class: Class::Scoped(Scoped::CutexRequest) },
    Rule { methods: GET, matcher: Matcher::Exact("/v2/events"), class: Class::Scoped(Scoped::EventPage) },
    Rule { methods: GET, matcher: Matcher::Exact("/v2/events/stream"), class: Class::Scoped(Scoped::EventStream) },
    Rule { methods: GET, matcher: Matcher::Exact(super::summary::SUMMARY_PATH), class: Class::Scoped(Scoped::Summary) },
    Rule { methods: GET, matcher: Matcher::Exact(super::summary::SUMMARY_EVENTS_PATH), class: Class::Scoped(Scoped::SummaryEvents) },
    Rule { methods: GET, matcher: Matcher::Exact("/v2/agent-management/projects"), class: Class::Scoped(Scoped::ProjectList) },
    Rule { methods: GET, matcher: Matcher::Shape(project_read_shape), class: Class::Scoped(Scoped::ProjectRead) },
    Rule { methods: GET, matcher: Matcher::Exact(super::asks::ASKS_PATH), class: Class::Scoped(Scoped::AskList) },
    Rule { methods: GET, matcher: Matcher::Shape(ask_read_shape), class: Class::Scoped(Scoped::AskRead) },
    Rule { methods: GET, matcher: Matcher::Shape(ask_answer_shape), class: Class::Scoped(Scoped::AskAnswer) },
    Rule { methods: GET_PATCH, matcher: Matcher::Exact(super::users_api::ME), class: Class::Scoped(Scoped::SelfProfile) },
    Rule { methods: GET, matcher: Matcher::Shape(user_avatar_shape), class: Class::Scoped(Scoped::UserAvatar) },
    // user-own-devices: the handler lists / revokes only tokens bound to the
    // caller's user and refuses a pending device (403 user_pending).
    Rule { methods: GET, matcher: Matcher::Exact(super::users_api::ME_DEVICES), class: Class::AnyPrincipal },
    Rule { methods: DELETE, matcher: Matcher::Shape(own_device_shape), class: Class::AnyPrincipal },
];

/// Every other route family: owner only (explicitly listed so the
/// enumeration test proves each one was considered).
const OWNER_RULES: &[Rule] = owner_prefixes![
    "/v2/agent-bus", "/v2/agent-management", "/v2/alarms", "/v2/approvals",
    "/v2/artifact-store", "/v2/artifacts", "/v2/asks", "/v2/browser", "/v2/capabilities",
    "/v2/claude", "/v2/client-tokens", "/v2/codex", "/v2/director-rotations",
    "/v2/documents", "/v2/events", "/v2/fs", "/v2/health", "/v2/host", "/v2/human", "/v2/identity",
    "/v2/inbound-mail", "/v2/items", "/v2/job-service", "/v2/nci-verification", "/v2/notes",
    "/v2/notification-levels", "/v2/notifications", "/v2/owner-notifications", "/v2/pair",
    "/v2/peers", "/v2/profile-presets", "/v2/profiles", "/v2/projects",
    "/v2/quota-auto-continue", "/v2/record-archive", "/v2/release", "/v2/runtime-packages",
    "/v2/runtimes", "/v2/services", "/v2/sessions", "/v2/settings", "/v2/side-chats",
    "/v2/summary", "/v2/task-service", "/v2/tasks", "/v2/terminals", "/v2/upgrade", "/v2/usage",
    "/v2/user-instructions", "/v2/users", "/v2/workflows",
];

fn rule_matches(rule: &Rule, method: &str, path: &str) -> bool {
    (rule.methods.is_empty() || rule.methods.contains(&method))
        && match rule.matcher {
            Matcher::Exact(exact) => path == exact,
            Matcher::Prefix(prefix) => path.starts_with(prefix),
            Matcher::Shape(shape) => shape(path),
        }
}

/// The explicit class of a route, `None` when no rule covers it (treated as
/// `OwnerOnly`).
pub fn classify(method: &str, path: &str) -> Option<Class> {
    if path == "/v2" {
        return Some(Class::OwnerOnly);
    }
    SCOPED_RULES
        .iter()
        .chain(OWNER_RULES)
        .find(|rule| rule_matches(rule, method, path))
        .map(|rule| rule.class)
}

// ------------------------------------------------------------ method tables

/// Native methods a non-owner may forward: reads need the grant, the rest
/// also `send`. Everything else (settings, compaction, review, thread
/// renames, `turn/start` / `turn/steer` with their cwd / sandbox / model
/// overrides) is the owner's.
pub const NATIVE_READ: &[&str] = &[
    "thread/read",
    "thread/turns/list",
    "thread/items/list",
    "thread/goal/get",
    "claude/model/status",
];
pub const NATIVE_SEND: &[&str] = &["turn/interrupt"];

pub const CUTEX_READ: &[&str] = &[
    "cutex/session/get",
    "cutex/session/groups/get",
    "cutex/focus/get",
    "cutex/userInput/queue/list",
];
pub const CUTEX_SEND: &[&str] = &[
    "cutex/userInput/submit",
    "cutex/userInput/queue/move",
    "cutex/userInput/queue/steer",
    "cutex/userInput/queue/yield",
    "cutex/userInput/queue/update",
    "cutex/userInput/queue/remove",
    "cutex/userInput/queue/flush",
    "cutex/turn/stop",
];

/// Delegation commands a sending user may run on asks / questions that
/// involve its visible agents (P2 access): the ask (or the question's ask)
/// must be visible to the user ([`ask_visible`]) and one of its ends must be
/// the session of the request path. `cutex/ask/create` (an ask in the
/// owner's name) and the session-wide `cutex/asks/pause|resume` stay the
/// owner's.
pub const CUTEX_ASK: &[&str] = &[
    "cutex/ask/stop",
    "cutex/ask/close",
    "cutex/ask/retry",
    "cutex/ask/respond",
    "cutex/ask/resume",
    "cutex/ask/reopen",
];

#[derive(Debug, PartialEq, Eq)]
enum Need {
    Read,
    Send,
    /// `send` plus the ask relevance check.
    Ask,
    Owner,
}

fn native_need(method: &str) -> Need {
    if NATIVE_READ.contains(&method) {
        Need::Read
    } else if NATIVE_SEND.contains(&method) {
        Need::Send
    } else {
        Need::Owner
    }
}

fn cutex_need(method: &str) -> Need {
    if CUTEX_READ.contains(&method) {
        Need::Read
    } else if CUTEX_SEND.contains(&method) {
        Need::Send
    } else if CUTEX_ASK.contains(&method) {
        Need::Ask
    } else {
        Need::Owner
    }
}

/// Why a non-owner's input items are refused.
#[derive(Debug, PartialEq, Eq)]
enum InputRefusal {
    /// Anything but `text` and `localImage` (remote `image` URLs, skills,
    /// mentions with host paths, malformed input).
    Type,
    /// A `localImage` that is not an attachment this token uploaded to this
    /// session through `POST /v2/sessions/{id}/attachments`.
    NotOwnAttachment,
}

/// Non-owner input: `text`, and `localImage` only for an attachment the
/// same token uploaded to the same session (never an arbitrary host path:
/// the agent would read it). `uploaded` answers that for a path.
fn check_input(params: &Value, uploaded: impl Fn(&str) -> bool) -> Result<(), InputRefusal> {
    let items = match params.get("input") {
        None => return Ok(()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(InputRefusal::Type),
    };
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {}
            Some("localImage") => {
                let path = item.get("path").and_then(Value::as_str).unwrap_or("");
                if !uploaded(path) {
                    return Err(InputRefusal::NotOwnAttachment);
                }
            }
            _ => return Err(InputRefusal::Type),
        }
    }
    Ok(())
}

/// `check_input` against the attachment store for the current request's
/// client token.
fn check_input_for(params: &Value, cutex_session_id: &str) -> Result<(), InputRefusal> {
    let token = crate::management::listeners::current_client_token().map(|token| token.token_id);
    check_input(params, |path| {
        token
            .as_deref()
            .is_some_and(|token| super::attachments::uploaded_by(cutex_session_id, token, path))
    })
}

// ------------------------------------------------------------------- gate

pub enum Gate {
    /// Let the ordinary handler answer.
    Continue,
    /// The gate answered.
    Handled(anyhow::Result<()>),
}

fn deny(stream: &mut TcpStream, code: &str, message: &str, details: Value) -> Gate {
    Gate::Handled(write_v2_error(stream, 403, "Forbidden", code, message, false, details))
}

fn session_not_found(stream: &mut TcpStream, id: &str) -> Gate {
    Gate::Handled(write_v2_error(
        stream,
        404,
        "Not Found",
        "session_not_found",
        "the owner-visible cutex session does not exist on this host",
        false,
        json!({ "cutexSessionId": id }),
    ))
}

fn not_found(stream: &mut TcpStream, code: &str) -> Gate {
    Gate::Handled(write_v2_error(stream, 404, "Not Found", code, "not found", false, json!({})))
}

fn owner_required(stream: &mut TcpStream) -> Gate {
    deny(stream, "owner_required", "this route is the host owner's", json!({}))
}

/// The gate for every Management request of a non-owner principal.
pub fn gate(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    principal: &Principal,
    context: ManagementRequestContext,
) -> Gate {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let class = classify(&request.method, path).unwrap_or(Class::OwnerOnly);
    match class {
        Class::Public | Class::AnyPrincipal => return Gate::Continue,
        Class::HostRead => return Gate::Handled(write_host_read(stream, path, principal)),
        _ => {}
    }
    let grant = match principal {
        Principal::Owner => return Gate::Continue,
        Principal::Pending => {
            return deny(
                stream,
                "access_pending",
                "this device is waiting for the host owner to assign it to a user",
                json!({}),
            )
        }
        Principal::User { grant, .. } => grant,
    };
    let Class::Scoped(scoped) = class else {
        return owner_required(stream);
    };
    let scope = match Scope::load(grant) {
        Ok(scope) => scope,
        Err(error) => {
            eprintln!("user scope unavailable: {error:#}");
            return Gate::Handled(write_v2_error(
                stream, 503, "Service Unavailable", "users_unavailable",
                "the user's scope could not be loaded", true, json!({}),
            ));
        }
    };
    match scoped_gate(stream, request, path, scoped, &scope, context) {
        Ok(gate) => gate,
        Err(error) => Gate::Handled(Err(error)),
    }
}

fn scoped_gate(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    path: &str,
    scoped: Scoped,
    scope: &Scope,
    context: ManagementRequestContext,
) -> anyhow::Result<Gate> {
    use super::server as srv;
    let query = |key: &str| crate::http::query::query_value(&request.path, key);
    let session_gate = |stream: &mut TcpStream, id: &str| -> Option<Gate> {
        (!scope.sees_session(id)).then(|| session_not_found(stream, id))
    };
    Ok(match scoped {
        Scoped::SessionList => {
            if query("includeHidden").is_some_and(|v| v != "false")
                || query("lifecycle").is_some_and(|v| v != "active")
            {
                return Ok(owner_required(stream));
            }
            let repository = super::repository::management_v2_repository()?;
            srv::materialize_active_stream_reset(repository, context)?;
            let registry = (context.load_registry)()?;
            let mut resource = super::session::session_list_resource_with_hidden(
                &registry,
                context.load_runtime_status,
                repository,
                false,
            )?;
            retain_array(&mut resource, "sessions", |item| {
                item["cutexSessionId"].as_str().is_some_and(|id| scope.sees_session(id))
            });
            Gate::Handled(write_json_response(stream, 200, "OK", &resource))
        }
        Scoped::SessionRead => {
            let id = srv::session_id_from_path(path).unwrap_or_default();
            if query("lifecycle").is_some_and(|v| v != "active") {
                return Ok(owner_required(stream));
            }
            session_gate(stream, &id).unwrap_or(Gate::Continue)
        }
        Scoped::SessionBootstrap => {
            let id = srv::bootstrap_session_id_from_path(path).unwrap_or_default();
            session_gate(stream, &id).unwrap_or(Gate::Continue)
        }
        Scoped::SessionHistory => {
            let id = srv::history_session_id_from_path(path).unwrap_or_default();
            session_gate(stream, &id).unwrap_or(Gate::Continue)
        }
        Scoped::SessionHistoryTurn => {
            let (id, _) = srv::history_turn_path(path).unwrap_or_default();
            session_gate(stream, &id).unwrap_or(Gate::Continue)
        }
        Scoped::NativeRequest => {
            let id = srv::native_request_session_id_from_path(path).unwrap_or_default();
            if let Some(gate) = session_gate(stream, &id) {
                return Ok(gate);
            }
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let method = body.pointer("/native/message/method").and_then(Value::as_str).unwrap_or("");
            match native_need(method) {
                Need::Read => Gate::Continue,
                Need::Send if scope.grant.send => Gate::Continue,
                Need::Send => send_not_granted(stream),
                Need::Owner | Need::Ask => deny(stream, "owner_required", "this native method is the host owner's", json!({ "method": method })),
            }
        }
        Scoped::ServerResponse => {
            let id = srv::server_response_session_id_from_path(path).unwrap_or_default();
            if let Some(gate) = session_gate(stream, &id) {
                return Ok(gate);
            }
            if scope.grant.send { Gate::Continue } else { send_not_granted(stream) }
        }
        Scoped::CutexRequest => {
            let id = srv::cutex_request_session_id_from_path(path).unwrap_or_default();
            if let Some(gate) = session_gate(stream, &id) {
                return Ok(gate);
            }
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let method = body["method"].as_str().unwrap_or("");
            match cutex_need(method) {
                Need::Read => Gate::Continue,
                Need::Send | Need::Ask if !scope.grant.send => send_not_granted(stream),
                Need::Send => match check_input_for(&body["params"], &id) {
                    Ok(()) => Gate::Continue,
                    Err(InputRefusal::Type) => deny(
                        stream,
                        "input_type_not_allowed",
                        "only text and own attachment (localImage) input is allowed for this user",
                        json!({}),
                    ),
                    Err(InputRefusal::NotOwnAttachment) => deny(
                        stream,
                        "attachment_not_owned",
                        "localImage must be an attachment this device uploaded to this session",
                        json!({}),
                    ),
                },
                Need::Ask => {
                    let state = crate::asks::store::AskStore::open_default()?.snapshot()?;
                    match ask_command_target(&state, &body["params"], &id, scope) {
                        AskTarget::Allowed => Gate::Continue,
                        AskTarget::Invalid => Gate::Continue, // the handler answers invalid_arguments
                        AskTarget::Hidden(code) => not_found(stream, code),
                    }
                }
                Need::Owner => deny(stream, "owner_required", "this cutex method is the host owner's", json!({ "method": method })),
            }
        }
        Scoped::WorkspaceRead => {
            use super::workspace::{parse_route, ScopeKind, WorkspaceRoute};
            let Some(WorkspaceRoute::Scoped(workspace, _)) = parse_route(path) else {
                return Ok(owner_required(stream));
            };
            match workspace.kind {
                ScopeKind::Session => session_gate(stream, &workspace.id).unwrap_or(Gate::Continue),
                ScopeKind::Project if scope.sees_project(&workspace.id) => Gate::Continue,
                ScopeKind::Project => not_found(stream, "project_not_found"),
            }
        }
        Scoped::AttachmentUpload => {
            let id = super::attachments::session_id_from_path(path).unwrap_or_default();
            if let Some(gate) = session_gate(stream, &id) {
                return Ok(gate);
            }
            if scope.grant.send { Gate::Continue } else { send_not_granted(stream) }
        }
        Scoped::EventPage => {
            let query = match srv::replay_query(request, false) {
                Ok(query) => query,
                Err(error) => return Ok(Gate::Handled(srv::write_replay_error(stream, error))),
            };
            if let Some(id) = query.cutex_session_id.as_deref() {
                if let Some(gate) = session_gate(stream, id) {
                    return Ok(gate);
                }
            }
            let repository = super::repository::management_v2_repository()?;
            srv::materialize_active_stream_reset(repository, context)?;
            match repository.page(query) {
                Ok(page) => {
                    let mut value = serde_json::to_value(page)?;
                    let asks = AskVisibility::default();
                    retain_array(&mut value, "events", |event| event_visible_json(event, scope, &asks));
                    Gate::Handled(write_json_response(stream, 200, "OK", &value))
                }
                Err(error) => Gate::Handled(srv::write_replay_error(stream, error)),
            }
        }
        Scoped::EventStream => {
            let query = match srv::replay_query(request, true) {
                Ok(query) => query,
                Err(error) => return Ok(Gate::Handled(srv::write_replay_error(stream, error))),
            };
            if let Some(id) = query.cutex_session_id.as_deref() {
                if let Some(gate) = session_gate(stream, id) {
                    return Ok(gate);
                }
            }
            let repository = super::repository::management_v2_repository()?;
            srv::materialize_active_stream_reset(repository, context)?;
            let subscription = match repository.page_and_subscribe(query.clone(), srv::SSE_SUBSCRIBER_CAPACITY) {
                Ok(subscription) => subscription,
                Err(error) => return Ok(Gate::Handled(srv::write_replay_error(stream, error))),
            };
            srv::write_sse_headers(stream)?;
            let grant = scope.grant.clone();
            let asks = AskVisibility::default();
            let mut visible = move |event: &super::model::EventEnvelope| {
                // Re-evaluated every few seconds (cached): agents join and
                // leave projects while the stream is open.
                let scope = Scope::cached(&grant);
                event_visible(event, &scope, &asks)
            };
            Gate::Handled(srv::stream_events_filtered(
                stream,
                subscription,
                &query,
                crate::management::listeners::RevocationWatch::for_current_request(),
                &mut visible,
            ))
        }
        Scoped::Summary => {
            let repository = super::repository::management_v2_repository()?;
            srv::materialize_active_stream_reset(repository, context)?;
            let mut value = super::summary::summary_resource(context, repository)?;
            filter_summary(&mut value, scope);
            Gate::Handled(write_json_response(stream, 200, "OK", &value))
        }
        Scoped::SummaryEvents => {
            let grant = scope.grant.clone();
            let mut visible = move |event: &super::summary::JournalEvent| {
                summary_event_visible(&event.name, &event.data, &Scope::cached(&grant))
            };
            Gate::Handled(super::summary::handle_events_filtered(stream, request, &mut visible))
        }
        Scoped::ProjectList => {
            let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
            match (context.list_management_projects)(&principal) {
                Ok(response) => {
                    let mut value = serde_json::to_value(response)?;
                    filter_projects(&mut value, scope);
                    Gate::Handled(write_json_response(stream, 200, "OK", &value))
                }
                Err(_) => Gate::Continue,
            }
        }
        Scoped::ProjectRead => {
            let id = srv::management_project_id_from_path(path)
                .map(|id| id.as_str().to_string())
                .unwrap_or_default();
            if scope.sees_project(&id) { Gate::Continue } else { not_found(stream, "project_not_found") }
        }
        Scoped::AskList => {
            let store = crate::asks::store::AskStore::open_default()?;
            // Filter the newest asks the owner could list, then apply the limit.
            let limit = query("limit").and_then(|v| v.parse::<usize>().ok()).filter(|l| (1..=super::asks::MAX_LIMIT).contains(l)).unwrap_or(super::asks::DEFAULT_LIMIT);
            let full = rewrite_query(&request.path, "limit", &super::asks::MAX_LIMIT.to_string());
            match super::asks::route_json(&store, &full, &super::asks::Route::Collection) {
                Ok(mut value) => {
                    let state = store.snapshot()?;
                    retain_array(&mut value, "asks", |ask| {
                        ask["askId"].as_str().is_some_and(|id| ask_visible(&state, id, scope))
                    });
                    let owner_more = value["hasMore"].as_bool().unwrap_or(false);
                    if let Some(items) = value["asks"].as_array_mut() {
                        let more = items.len() > limit;
                        items.truncate(limit);
                        value["hasMore"] = json!(more || owner_more);
                    }
                    Gate::Handled(write_json_response(stream, 200, "OK", &value))
                }
                Err(_) => Gate::Continue,
            }
        }
        // The handler answers for "me" only and checks who may see whom.
        Scoped::SelfProfile | Scoped::UserAvatar => Gate::Continue,
        Scoped::AskRead | Scoped::AskAnswer => {
            let id = match super::asks::parse_path(path) {
                Some(super::asks::Route::Ask(id) | super::asks::Route::Answer(id)) => id,
                _ => String::new(),
            };
            let state = crate::asks::store::AskStore::open_default()?.snapshot()?;
            if ask_visible(&state, &id, scope) { Gate::Continue } else { not_found(stream, "ask_not_found") }
        }
    })
}

fn send_not_granted(stream: &mut TcpStream) -> Gate {
    deny(stream, "send_not_granted", "this user may read these agents but not send to them", json!({}))
}

fn retain_array(value: &mut Value, key: &str, mut keep: impl FnMut(&Value) -> bool) {
    if let Some(items) = value.get_mut(key).and_then(Value::as_array_mut) {
        items.retain(|item| keep(item));
    }
}

fn rewrite_query(full_path: &str, key: &str, value: &str) -> String {
    let (path, query) = full_path.split_once('?').unwrap_or((full_path, ""));
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(name, _)| name != key)
        .map(|(n, v)| (n.into_owned(), v.into_owned()))
        .collect();
    pairs.push((key.to_string(), value.to_string()));
    let encoded = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish();
    format!("{path}?{encoded}")
}

/// An ask is visible when both ends are local agents the user sees (or the
/// owner asked) and no peer is involved (§2.4: a peer's agents never appear).
pub fn ask_visible(state: &crate::asks::store::AskState, ask_id: &str, scope: &Scope) -> bool {
    let Some(ask) = state.asks.get(ask_id) else {
        return false;
    };
    if ask.remote.is_some() || ask.peer_inbound.is_some() {
        return false;
    }
    let requester_ok = match &ask.requester {
        crate::asks::model::Requester::Owner => true,
        crate::asks::model::Requester::Agent { cutex_session_id, .. } => scope.sees_session(cutex_session_id),
        crate::asks::model::Requester::Peer { .. } => false,
    };
    requester_ok && scope.sees_session(&ask.target_cutex_session_id)
}

enum AskTarget {
    Allowed,
    /// No askId / questionId: let the handler refuse it.
    Invalid,
    /// Unknown, invisible or unrelated to the path session (404 code).
    Hidden(&'static str),
}

/// The relevance check of a delegation command: `askId` (or the ask of
/// `questionId`) must be visible to the user and the request path's
/// session must be its requester or target.
fn ask_command_target(
    state: &crate::asks::store::AskState,
    params: &Value,
    path_session: &str,
    scope: &Scope,
) -> AskTarget {
    let (ask_id, missing) = if let Some(question) = params.get("questionId") {
        let Some(question) = question.as_str().filter(|id| !id.is_empty()) else {
            return AskTarget::Invalid;
        };
        match state.questions.get(question) {
            Some(record) => (record.ask_id.as_str(), "question_not_found"),
            None => return AskTarget::Hidden("question_not_found"),
        }
    } else {
        match params.get("askId").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            Some(id) => (id, "ask_not_found"),
            None => return AskTarget::Invalid,
        }
    };
    let Some(ask) = state.asks.get(ask_id) else {
        return AskTarget::Hidden(missing);
    };
    let requester_session = match &ask.requester {
        crate::asks::model::Requester::Agent { cutex_session_id, .. } => Some(cutex_session_id.as_str()),
        _ => None,
    };
    let related = ask.target_cutex_session_id == path_session || requester_session == Some(path_session);
    if related && ask_visible(state, ask_id, scope) {
        AskTarget::Allowed
    } else {
        AskTarget::Hidden(missing)
    }
}

/// Lazily loaded ask store snapshot for event filtering.
#[derive(Default)]
struct AskVisibility {
    state: std::cell::RefCell<Option<(std::time::Instant, crate::asks::store::AskState)>>,
}

impl AskVisibility {
    fn visible(&self, ask_id: &str, scope: &Scope) -> bool {
        let mut cell = self.state.borrow_mut();
        let stale = cell.as_ref().is_none_or(|(at, state)| {
            at.elapsed() > std::time::Duration::from_secs(2) || !state.asks.contains_key(ask_id)
        });
        if stale {
            match crate::asks::store::AskStore::open_default().and_then(|store| store.snapshot()) {
                Ok(state) => *cell = Some((std::time::Instant::now(), state)),
                Err(_) => return false,
            }
        }
        cell.as_ref().is_some_and(|(_, state)| ask_visible(state, ask_id, scope))
    }
}

fn event_visible(event: &super::model::EventEnvelope, scope: &Scope, asks: &AskVisibility) -> bool {
    if event.cutex_session_id.is_empty() || !scope.sees_session(&event.cutex_session_id) {
        return false;
    }
    match &event.cutex {
        Some(message) if message.method.starts_with("cutex/ask") || message.method.starts_with("cutex/peers") => {
            message.params["askId"].as_str().is_some_and(|id| asks.visible(id, scope))
        }
        Some(message) if owner_only_event(&message.method) => false,
        _ => true,
    }
}

/// Events of owner-only features that may carry a visible agent's session id: workflows (routes,
/// settings and approvals are owner-only, so are their run / step / log events).
fn owner_only_event(method: &str) -> bool {
    method.starts_with("cutex/workflow")
}

fn event_visible_json(event: &Value, scope: &Scope, asks: &AskVisibility) -> bool {
    match serde_json::from_value::<super::model::EventEnvelope>(event.clone()) {
        Ok(event) => event_visible(&event, scope, asks),
        Err(_) => false,
    }
}

/// Summary journal events a non-owner may see: its agents' rows and turn
/// ends, and resyncs. Host-wide counts and owner messages are the owner's.
pub fn summary_event_visible(name: &str, data: &Value, scope: &Scope) -> bool {
    let agent = |value: &Value| value.as_str().is_some_and(|id| scope.sees_session(id));
    match name {
        "cutex/summary/agentChanged" => agent(&data["agent"]["id"]),
        "cutex/summary/agentRemoved" | "cutex/summary/turnEnded" => agent(&data["id"]),
        "cutex/summary/resync" => true,
        _ => false,
    }
}

pub fn filter_summary(value: &mut Value, scope: &Scope) {
    retain_array(value, "agents", |row| row["id"].as_str().is_some_and(|id| scope.sees_session(id)));
    let projects: HashSet<String> = value["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["projectId"].as_str().map(str::to_string))
        .collect();
    retain_array(value, "projects", |project| {
        project["id"].as_str().is_some_and(|id| projects.contains(id) && scope.sees_project(id))
    });
    let needs = value["agents"]
        .as_array()
        .map(|rows| rows.iter().filter(|row| row["needs"].as_array().is_some_and(|n| !n.is_empty())).count())
        .unwrap_or(0);
    value["needsYouCount"] = json!(needs);
}

pub fn filter_projects(value: &mut Value, scope: &Scope) {
    retain_array(value, "projects", |project| {
        project["project_id"].as_str().or(project["projectId"].as_str()).is_some_and(|id| scope.sees_project(id))
    });
    if let Some(object) = value.as_object_mut() {
        object.remove("archived_projects");
        object.remove("archivedProjects");
        object.remove("available_agents");
        object.remove("availableAgents");
    }
}

fn write_host_read(stream: &mut TcpStream, path: &str, principal: &Principal) -> anyhow::Result<()> {
    if path == "/v2/host" {
        let mut host = super::host_info::host_resource();
        if !principal.is_owner() {
            if let Some(object) = host.as_object_mut() {
                object.remove("executable");
            }
        }
        return write_json_response(stream, 200, "OK", &host);
    }
    write_json_response(stream, 200, "OK", &capabilities_for(principal))
}

/// `GET /v2/capabilities` with the caller's `user`.
pub fn capabilities_for(principal: &Principal) -> Value {
    let mut value = super::capabilities::capabilities_resource();
    let owner_name = crate::management::users::UserStore::open_default()
        .and_then(|users| users.snapshot())
        .ok()
        .and_then(|state| state.user(crate::management::users::OWNER).map(|u| u.name.clone()));
    value["user"] = principal.capability_view(owner_name.as_deref());
    if let Some(me) = crate::management::user_profiles::me(Some(principal)) {
        crate::management::user_profiles::decorate_default(&mut value["user"], &me);
    }
    // user-own-token: the client / device token of this request (null = the
    // root credential, or a token revoked since it was verified).
    value["user"]["token"] = crate::management::listeners::current_client_token()
        .and_then(|token| super::users_api::own_token_view(&token.token_id))
        .unwrap_or(Value::Null);
    value
}

/// Exposed for tests: the grant of a principal (owner = none).
pub fn grant_of(principal: &Principal) -> Option<&Grant> {
    match principal {
        Principal::User { grant, .. } => Some(grant),
        _ => None,
    }
}

#[cfg(test)]
#[path = "access_tests.rs"]
mod tests;
