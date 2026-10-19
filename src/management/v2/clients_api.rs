//! Remote close of a desktop client (capability `client-remote-close`;
//! cutex-desktop docs/research/remote-close-desktop-20261019.md). Presence
//! lives only in memory in the Management daemon: an instance that sent no
//! heartbeat for 3 × `heartbeatSeconds` disappears; a restart forgets all.
//!
//! - `POST /v2/clients/presence` `{installId, instanceId, appName,
//!   appVersion, deviceName, platform, allowRemoteClose}` → `{instanceId,
//!   heartbeatSeconds: 60, state, closeRequest}`. Repeating it is the
//!   heartbeat. Any authenticated principal, pending devices and narrowed
//!   scopes included. The host records the calling credential (root
//!   credential or token id) and its user; an instance id registered by
//!   another credential: 409 `client_instance_conflict`. `closeRequest`
//!   (`{requestedBy, graceSeconds, at}` while `state` is `closeRequested`,
//!   else null) lets a client that cannot read the event stream (a pending
//!   device) or missed the event see the request.
//! - `GET /v2/clients` → `{clients: [{instanceId, installId, appName,
//!   appVersion, deviceName, platform, userId, tokenId, local,
//!   connectedSince, lastSeen, allowRemoteClose, state, current}]}`. An
//!   owner principal with device scope `full` sees every instance; any other
//!   principal only its own user's (a pending device: only its own token's).
//!   `current` = registered by the credential of this request.
//! - `POST /v2/clients/{instanceId}/close` `{graceSeconds?}` (default 15,
//!   0..=300) → 202 `{instanceId, state: "closeRequested"}`. Allowed for an
//!   owner principal or the instance's own user, with device scope `full`;
//!   else 403 `client_close_forbidden` `{reason: pending|scope|otherUser}`;
//!   404 `client_not_found`; 409 `remote_close_disabled` (the instance
//!   reported `allowRemoteClose: false`). Audited (`clientCloseRequested`).
//!   Host event `cutex/client/closeRequested {instanceId, requestedBy
//!   {deviceName, userId, local}, graceSeconds, at}`.
//! - `POST /v2/clients/{instanceId}/closing` `{outcome: "closing" |
//!   "cancelled"}` → `{instanceId, state}`; only the credential that
//!   registered the instance (else 403 `client_close_forbidden` `{reason:
//!   notRegistrant}`). Host event `cutex/client/stateChanged {instanceId,
//!   state}`.
//!
//! Host events `cutex/client/*` reach every owner subscriber; a non-owner
//! user's stream carries those of its own user's instances
//! (`access::event_visible`).

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::client_tokens::DeviceScope;
use crate::management::users::{self, Principal, UserStore, OWNER};

pub const CLIENTS_PATH: &str = "/v2/clients";
pub const PRESENCE_PATH: &str = "/v2/clients/presence";
pub const CLOSE_REQUESTED: &str = "cutex/client/closeRequested";
pub const STATE_CHANGED: &str = "cutex/client/stateChanged";
pub const HEARTBEAT_SECONDS: i64 = 60;
/// An instance without a heartbeat for this long is gone.
pub const EXPIRE_SECONDS: i64 = 3 * HEARTBEAT_SECONDS;
pub const DEFAULT_GRACE_SECONDS: u64 = 15;
pub const MAX_GRACE_SECONDS: u64 = 300;
/// Instances one credential may hold (a restarting desktop leaves its old
/// instance behind until it expires); the oldest is dropped beyond it.
const MAX_PER_CREDENTIAL: usize = 16;
/// Instances on the host; a new instance beyond it: 503 `clients_full`.
const MAX_INSTANCES: usize = 512;
const MAX_ID_CHARS: usize = 128;
const MAX_TEXT_CHARS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientState {
    Online,
    CloseRequested,
    Closing,
    Cancelled,
}

impl ClientState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::CloseRequested => "closeRequested",
            Self::Closing => "closing",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Who made a request: the credential (token id, `None` = the root
/// credential), its principal, whether it arrived on a loopback listener
/// and the device scope of the token (root: full).
#[derive(Clone, Debug)]
pub struct Caller {
    pub token_id: Option<String>,
    pub principal: Principal,
    pub local: bool,
    pub scope: DeviceScope,
}

impl Caller {
    /// The caller of the current Management request.
    pub fn current() -> Self {
        let token = crate::management::listeners::current_client_token();
        Self {
            principal: users::current_principal().unwrap_or(Principal::Owner),
            scope: token.as_ref().map_or(DeviceScope::Full, |token| token.scope),
            token_id: token.map(|token| token.token_id),
            local: crate::management::listeners::current_request_is_loopback(),
        }
    }

    fn user_id(&self) -> Option<String> {
        user_of(&self.principal)
    }
}

fn user_of(principal: &Principal) -> Option<String> {
    match principal {
        Principal::Owner => Some(OWNER.to_string()),
        Principal::User { id, .. } => Some(id.clone()),
        Principal::Pending => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Presence {
    pub install_id: String,
    pub instance_id: String,
    pub app_name: String,
    pub app_version: String,
    pub device_name: String,
    pub platform: String,
    pub allow_remote_close: bool,
}

#[derive(Clone, Debug)]
pub struct CloseRequest {
    pub requested_by: Value,
    pub grace_seconds: u64,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct Instance {
    pub presence: Presence,
    /// The registering credential (`None` = root).
    pub token_id: Option<String>,
    /// The user of the registering credential at its last heartbeat.
    pub user_id: Option<String>,
    pub local: bool,
    pub connected_since: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub state: ClientState,
    pub close_request: Option<CloseRequest>,
}

impl Instance {
    fn view(&self, caller: &Caller) -> Value {
        let p = &self.presence;
        json!({
            "instanceId": p.instance_id,
            "installId": p.install_id,
            "appName": p.app_name,
            "appVersion": p.app_version,
            "deviceName": p.device_name,
            "platform": p.platform,
            "userId": self.user_id,
            "tokenId": self.token_id,
            "local": self.local,
            "connectedSince": crate::management::instance::format_utc(self.connected_since),
            "lastSeen": crate::management::instance::format_utc(self.last_seen),
            "allowRemoteClose": p.allow_remote_close,
            "state": self.state.as_str(),
            "current": self.token_id == caller.token_id,
        })
    }

    fn close_request_view(&self) -> Value {
        match (&self.close_request, self.state) {
            (Some(request), ClientState::CloseRequested) => json!({
                "requestedBy": request.requested_by,
                "graceSeconds": request.grace_seconds,
                "at": crate::management::instance::format_utc(request.at),
            }),
            _ => Value::Null,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClientError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl ClientError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), details: json!({}) }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }
    fn forbidden(reason: &str, message: &str) -> Self {
        Self { details: json!({ "reason": reason }), ..Self::new(403, "client_close_forbidden", message) }
    }
    fn not_found(instance_id: &str) -> Self {
        Self {
            details: json!({ "instanceId": instance_id }),
            ..Self::new(404, "client_not_found", "no desktop client with this instance id is online on this host")
        }
    }
}

/// What a successful close request produced (the caller publishes the
/// event and writes the audit line).
#[derive(Debug)]
pub struct CloseOutcome {
    pub state: ClientState,
    /// `Some(event params)` when a new request was recorded.
    pub event: Option<Value>,
}

/// The in-memory presence registry. Every method takes `now` (tests inject
/// the clock) and first drops expired instances.
#[derive(Debug, Default)]
pub struct Registry {
    instances: HashMap<String, Instance>,
}

impl Registry {
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let limit = Duration::seconds(EXPIRE_SECONDS);
        self.instances.retain(|_, instance| now - instance.last_seen <= limit);
    }

    pub fn get(&self, instance_id: &str) -> Option<&Instance> {
        self.instances.get(instance_id)
    }

    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Register or refresh (heartbeat) an instance for `caller`.
    pub fn heartbeat(&mut self, presence: Presence, caller: &Caller, now: DateTime<Utc>) -> Result<&Instance, ClientError> {
        self.prune(now);
        let id = presence.instance_id.clone();
        if let Some(existing) = self.instances.get(&id) {
            if existing.token_id != caller.token_id {
                return Err(ClientError {
                    details: json!({ "instanceId": id }),
                    ..ClientError::new(409, "client_instance_conflict", "this instance id is registered by another credential")
                });
            }
        } else {
            let mine: Vec<(String, DateTime<Utc>)> = self
                .instances
                .values()
                .filter(|instance| instance.token_id == caller.token_id)
                .map(|instance| (instance.presence.instance_id.clone(), instance.last_seen))
                .collect();
            if mine.len() >= MAX_PER_CREDENTIAL {
                if let Some((oldest, _)) = mine.iter().min_by_key(|(_, seen)| *seen) {
                    self.instances.remove(oldest);
                }
            }
            if self.instances.len() >= MAX_INSTANCES {
                return Err(ClientError::new(503, "clients_full", "too many desktop clients are online on this host"));
            }
        }
        let instance = self.instances.entry(id).or_insert_with(|| Instance {
            presence: presence.clone(),
            token_id: caller.token_id.clone(),
            user_id: None,
            local: caller.local,
            connected_since: now,
            last_seen: now,
            state: ClientState::Online,
            close_request: None,
        });
        instance.presence = presence;
        instance.user_id = caller.user_id();
        instance.local = caller.local;
        instance.last_seen = now;
        Ok(instance)
    }

    /// The instances `caller` may list, oldest connection first. `user_of`
    /// answers an instance's current user (bindings change).
    pub fn visible(&mut self, caller: &Caller, now: DateTime<Utc>) -> Vec<Value> {
        self.prune(now);
        let all = caller.principal.is_owner() && caller.scope.is_full();
        let mine = caller.user_id();
        let mut items: Vec<&Instance> = self
            .instances
            .values()
            .filter(|instance| {
                all || match &mine {
                    Some(user) => instance.user_id.as_deref() == Some(user.as_str()),
                    None => instance.token_id == caller.token_id,
                }
            })
            .collect();
        items.sort_by(|a, b| {
            (a.connected_since, &a.presence.instance_id).cmp(&(b.connected_since, &b.presence.instance_id))
        });
        items.into_iter().map(|instance| instance.view(caller)).collect()
    }

    /// A close request by `caller` (permissions, then the instance's own
    /// switch). `requested_by_device` names the caller's device.
    pub fn request_close(
        &mut self,
        instance_id: &str,
        caller: &Caller,
        requested_by_device: &str,
        grace_seconds: u64,
        now: DateTime<Utc>,
    ) -> Result<CloseOutcome, ClientError> {
        self.prune(now);
        if matches!(caller.principal, Principal::Pending) {
            return Err(ClientError::forbidden("pending", "a device waiting for the host owner cannot close desktop clients"));
        }
        if !caller.scope.is_full() {
            return Err(ClientError {
                details: json!({ "reason": "scope", "scope": caller.scope }),
                ..ClientError::forbidden("scope", "this device's scope does not allow closing desktop clients")
            });
        }
        let instance = self.instances.get_mut(instance_id).ok_or_else(|| ClientError::not_found(instance_id))?;
        let caller_user = caller.user_id();
        if !caller.principal.is_owner() && instance.user_id != caller_user {
            return Err(ClientError::forbidden("otherUser", "only the host owner or the same user may close this desktop client"));
        }
        if !instance.presence.allow_remote_close {
            return Err(ClientError {
                details: json!({ "instanceId": instance_id }),
                ..ClientError::new(409, "remote_close_disabled", "this desktop client does not allow remote close")
            });
        }
        if instance.state == ClientState::Closing {
            return Ok(CloseOutcome { state: ClientState::Closing, event: None });
        }
        let requested_by = json!({ "deviceName": requested_by_device, "userId": caller_user, "local": caller.local });
        instance.state = ClientState::CloseRequested;
        instance.close_request = Some(CloseRequest { requested_by: requested_by.clone(), grace_seconds, at: now });
        Ok(CloseOutcome {
            state: ClientState::CloseRequested,
            event: Some(json!({
                "instanceId": instance_id,
                "requestedBy": requested_by,
                "graceSeconds": grace_seconds,
                "at": crate::management::instance::format_utc(now),
            })),
        })
    }

    /// The instance's own report: it is closing, or its user cancelled.
    pub fn report_closing(
        &mut self,
        instance_id: &str,
        caller: &Caller,
        state: ClientState,
        now: DateTime<Utc>,
    ) -> Result<(), ClientError> {
        self.prune(now);
        let instance = self.instances.get_mut(instance_id).ok_or_else(|| ClientError::not_found(instance_id))?;
        if instance.token_id != caller.token_id {
            return Err(ClientError::forbidden("notRegistrant", "only the desktop client itself may report its state"));
        }
        instance.state = state;
        instance.last_seen = now;
        if state != ClientState::CloseRequested {
            instance.close_request = None;
        }
        Ok(())
    }

    /// The device name of `caller`'s most recently seen instance.
    pub fn device_name_of(&self, caller: &Caller) -> Option<String> {
        self.instances
            .values()
            .filter(|instance| instance.token_id == caller.token_id)
            .max_by_key(|instance| instance.last_seen)
            .map(|instance| instance.presence.device_name.clone())
            .filter(|name| !name.is_empty())
    }

    /// The user an instance belongs to (`None`: unknown / pending).
    pub fn user_of_instance(&self, instance_id: &str) -> Option<String> {
        self.instances.get(instance_id).and_then(|instance| instance.user_id.clone())
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        self.instances.clear();
    }
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

pub fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(Registry::default))
}

#[cfg(test)]
thread_local! {
    /// Seconds added to the clock of this thread's requests (tests).
    pub static CLOCK_OFFSET: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

fn now() -> DateTime<Utc> {
    #[cfg(test)]
    {
        Utc::now() + Duration::seconds(CLOCK_OFFSET.with(std::cell::Cell::get))
    }
    #[cfg(not(test))]
    {
        Utc::now()
    }
}

/// Whether a `cutex/client/*` host event concerns an instance of `user`
/// (a non-owner user's event stream).
pub fn event_for_user(params: &Value, user: &str) -> bool {
    params["instanceId"]
        .as_str()
        .is_some_and(|id| with_registry(|registry| registry.user_of_instance(id)).as_deref() == Some(user))
}

// ------------------------------------------------------------------- routes

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    List,
    Presence,
    Close(String),
    Closing(String),
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_CHARS
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == CLIENTS_PATH {
        return Some(Route::List);
    }
    if path == PRESENCE_PATH {
        return Some(Route::Presence);
    }
    let rest = path.strip_prefix("/v2/clients/")?;
    let (id, action) = rest.split_once('/')?;
    if !valid_id(id) || id == "presence" {
        return None;
    }
    match action {
        "close" => Some(Route::Close(id.to_string())),
        "closing" => Some(Route::Closing(id.to_string())),
        _ => None,
    }
}

pub fn is_path(path: &str) -> bool {
    path == CLIENTS_PATH || path.starts_with("/v2/clients/")
}

/// `POST /v2/clients/{id}/close` (access rule shape).
pub fn close_shape(path: &str) -> bool {
    matches!(parse_path(path), Some(Route::Close(_)))
}

/// `POST /v2/clients/{id}/closing` (access rule shape).
pub fn closing_shape(path: &str) -> bool {
    matches!(parse_path(path), Some(Route::Closing(_)))
}

fn body(request: &SimpleHttpRequest) -> Result<serde_json::Map<String, Value>, ClientError> {
    if request.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Default::default());
    }
    match serde_json::from_slice::<Value>(&request.body) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(ClientError::invalid("a JSON object body is required")),
    }
}

fn text(object: &serde_json::Map<String, Value>, key: &str, required: bool) -> Result<String, ClientError> {
    match object.get(key) {
        None | Some(Value::Null) if !required => Ok(String::new()),
        Some(Value::String(value)) if value.chars().count() <= MAX_TEXT_CHARS && !value.chars().any(char::is_control) => {
            if required && value.trim().is_empty() {
                Err(ClientError::invalid(format!("{key} must not be empty")))
            } else {
                Ok(value.trim().to_string())
            }
        }
        _ => Err(ClientError::invalid(format!("{key}: a string of at most {MAX_TEXT_CHARS} characters is required"))),
    }
}

fn id_field(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, ClientError> {
    match object.get(key).and_then(Value::as_str) {
        Some(id) if valid_id(id) && id != "presence" => Ok(id.to_string()),
        _ => Err(ClientError::invalid(format!("{key}: 1..={MAX_ID_CHARS} characters of A-Z a-z 0-9 - _ . : required"))),
    }
}

pub fn parse_presence(object: &serde_json::Map<String, Value>) -> Result<Presence, ClientError> {
    let allow_remote_close = match object.get("allowRemoteClose") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(ClientError::invalid("allowRemoteClose must be a boolean")),
    };
    Ok(Presence {
        install_id: id_field(object, "installId")?,
        instance_id: id_field(object, "instanceId")?,
        app_name: text(object, "appName", true)?,
        app_version: text(object, "appVersion", false)?,
        device_name: text(object, "deviceName", false)?,
        platform: text(object, "platform", false)?,
        allow_remote_close,
    })
}

/// The caller's device name for `requestedBy`: its own most recent
/// presence, else its token's names, else the host name (root).
fn requester_device_name(registry: &Registry, caller: &Caller) -> String {
    if let Some(name) = registry.device_name_of(caller) {
        return name;
    }
    if let Some(token_id) = &caller.token_id {
        let token = crate::management::client_tokens::ClientTokenStore::open_default()
            .and_then(|store| store.list())
            .ok()
            .and_then(|tokens| tokens.into_iter().find(|token| &token.token_id == token_id));
        if let Some(token) = token {
            return token.owner_name.or(token.device_name).unwrap_or(token.label);
        }
        return token_id.clone();
    }
    crate::platform::host::current_host_name()
}

fn audit(action: &str, details: Value) {
    if let Ok(store) = UserStore::open_default() {
        users::audit(store.dir(), action, details);
    }
}

fn route(request: &SimpleHttpRequest, caller: &Caller) -> Result<(u16, Value), ClientError> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let route = parse_path(path).ok_or_else(|| ClientError::new(404, "route_not_found", "no such clients route"))?;
    let method_ok = match route {
        Route::List => request.method == "GET",
        _ => request.method == "POST",
    };
    if !method_ok {
        return Err(ClientError::new(405, "method_not_allowed", "unsupported method for this route"));
    }
    let now = now();
    match route {
        Route::List => {
            let clients = with_registry(|registry| registry.visible(caller, now));
            Ok((200, json!({ "clients": clients })))
        }
        Route::Presence => {
            let presence = parse_presence(&body(request)?)?;
            with_registry(|registry| {
                let instance = registry.heartbeat(presence, caller, now)?;
                Ok((
                    200,
                    json!({
                        "instanceId": instance.presence.instance_id,
                        "heartbeatSeconds": HEARTBEAT_SECONDS,
                        "state": instance.state.as_str(),
                        "closeRequest": instance.close_request_view(),
                    }),
                ))
            })
        }
        Route::Close(id) => {
            let object = body(request)?;
            let grace = match object.get("graceSeconds") {
                None | Some(Value::Null) => DEFAULT_GRACE_SECONDS,
                Some(value) => value
                    .as_u64()
                    .filter(|grace| *grace <= MAX_GRACE_SECONDS)
                    .ok_or_else(|| ClientError::invalid(format!("graceSeconds: an integer 0..={MAX_GRACE_SECONDS}")))?,
            };
            let outcome = with_registry(|registry| {
                let device = requester_device_name(registry, caller);
                registry.request_close(&id, caller, &device, grace, now)
            });
            let actor = caller.token_id.clone().unwrap_or_else(|| "root".into());
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    if error.status == 403 || error.status == 409 {
                        audit(
                            "clientCloseRefused",
                            json!({ "instanceId": id, "actor": actor, "code": error.code, "details": error.details }),
                        );
                    }
                    return Err(error);
                }
            };
            if let Some(event) = &outcome.event {
                audit(
                    "clientCloseRequested",
                    json!({ "instanceId": id, "actor": actor, "graceSeconds": grace, "requestedBy": event["requestedBy"] }),
                );
                super::documents::publish_host_event(CLOSE_REQUESTED, event.clone());
            }
            Ok((202, json!({ "instanceId": id, "state": outcome.state.as_str() })))
        }
        Route::Closing(id) => {
            let object = body(request)?;
            let state = match object.get("outcome").and_then(Value::as_str) {
                Some("closing") => ClientState::Closing,
                Some("cancelled") => ClientState::Cancelled,
                _ => return Err(ClientError::invalid("outcome must be \"closing\" or \"cancelled\"")),
            };
            with_registry(|registry| registry.report_closing(&id, caller, state, now))?;
            audit(
                "clientCloseOutcome",
                json!({ "instanceId": id, "outcome": state.as_str(), "actor": caller.token_id.clone().unwrap_or_else(|| "root".into()) }),
            );
            super::documents::publish_host_event(STATE_CHANGED, json!({ "instanceId": id, "state": state.as_str() }));
            Ok((200, json!({ "instanceId": id, "state": state.as_str() })))
        }
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match route(request, &Caller::current()) {
        Ok((status, value)) => {
            let reason = if status == 202 { "Accepted" } else { "OK" };
            write_json_response(stream, status, reason, &value)
        }
        Err(failure) => {
            let reason = match failure.status {
                400 => "Bad Request",
                403 => "Forbidden",
                404 => "Not Found",
                405 => "Method Not Allowed",
                409 => "Conflict",
                _ => "Service Unavailable",
            };
            super::server::write_v2_error(
                stream,
                failure.status,
                reason,
                failure.code,
                &failure.message,
                failure.status == 503,
                failure.details,
            )
        }
    }
}

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "clients",
    title: "Desktop client presence and remote close",
    routes: &[
        "POST /v2/clients/presence",
        "GET /v2/clients",
        "POST /v2/clients/{instanceId}/close",
        "POST /v2/clients/{instanceId}/closing",
        "host event cutex/client/closeRequested",
        "host event cutex/client/stateChanged",
    ],
    text: include_str!("schema/export/clients.json"),
};

#[cfg(test)]
#[path = "clients_api_tests.rs"]
mod tests;
