//! Root-only per-profile account login (`profile-login`):
//!
//! - `GET  /v2/profiles/{profile}/account`
//! - `POST /v2/profiles/{profile}/login` `{type:"chatgpt"}` | `{type:"apiKey", apiKey}`
//! - `GET  /v2/profiles/{profile}/login/{loginId}`
//! - `POST /v2/profiles/{profile}/login/{loginId}/cancel`
//! - `POST /v2/profiles/{profile}/logout`
//!
//! Codex profiles are `--auth-file <profile auth.json>` + `-c` overrides on the
//! one shared CODEX_HOME. A login therefore runs a short-lived Cutex-owned
//! app-server for that shared home with `--auth-file` pointing at the
//! profile's materialized `auth.json` (the exact file owner launches pass)
//! and `cli_auth_credentials_store="file"`, and drives the native
//! `account/login/start` / `account/login/cancel` / `account/logout` methods.
//! The native credential store writes only that file; the default CODEX_HOME
//! auth is never touched.
//!
//! A ChatGPT browser login keeps its app-server (and the native localhost
//! callback listener) alive until `account/login/completed`, cancel, or the
//! login TTL (10 min), then reaps it. As a fallback for a lost or misreported
//! notification, a login whose profile auth file gained new ChatGPT tokens is
//! marked succeeded once the native side had a short grace period to report
//! (or when its app-server exits / the TTL hits). API-key login and logout use a process
//! for the duration of the one request. Responses never contain token or key
//! values; the account view is derived from the profile auth file's
//! non-secret metadata (mode, id-token email / plan claims).

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;

use crate::catalog::CatalogEndpoint;
use crate::catalog::CatalogError;
use crate::catalog::OwnedStdioEndpoint;
use crate::catalog::StdioAppServerOptions;
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::profiles::model::CliKind;
use crate::profiles::model::StoredAccount;

pub const LOGIN_TTL: Duration = Duration::from_secs(600);
const FINISHED_RETENTION: Duration = Duration::from_secs(1800);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_API_KEY_BYTES: usize = 4096;
const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;
/// How often a pending ChatGPT login re-reads the profile auth file.
const AUTH_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long a login whose auth file gained ChatGPT tokens waits for the native
/// `account/login/completed` (sent after the browser's success page) before it
/// is marked succeeded from the file alone.
const COMPLETION_GRACE: Duration = Duration::from_secs(10);
/// Notifications kept by a login app-server transport.
const LOGIN_NOTIFICATION_BUFFER: usize = 256;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginRoute {
    Account,
    Start,
    Status(String),
    Cancel(String),
    Logout,
}

fn valid_login_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub fn parse_path(path: &str) -> Option<(String, LoginRoute)> {
    let rest = path.strip_prefix("/v2/profiles/")?;
    let (encoded, tail) = rest.split_once('/')?;
    let segments: Vec<&str> = tail.split('/').collect();
    let route = match segments.as_slice() {
        ["account"] => LoginRoute::Account,
        ["login"] => LoginRoute::Start,
        ["logout"] => LoginRoute::Logout,
        ["login", id] if valid_login_id(id) => LoginRoute::Status((*id).to_string()),
        ["login", id, "cancel"] if valid_login_id(id) => LoginRoute::Cancel((*id).to_string()),
        _ => return None,
    };
    if encoded.is_empty() {
        return None;
    }
    let decoded = super::profile_quota::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control))
        .then_some((decoded, route))
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// Write-only API key: never Debug-printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginRequest {
    Chatgpt,
    ApiKey(ApiKey),
}

pub fn parse_login_request(body: &[u8]) -> Result<LoginRequest, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("invalid JSON request body: {error}"))?;
    let object = value.as_object().ok_or("request body must be a JSON object")?;
    match object.get("type").and_then(Value::as_str) {
        Some("chatgpt") => {
            if let Some(key) = object.keys().find(|key| key.as_str() != "type") {
                return Err(format!("unknown request field: {key} (chatgpt login takes only type)"));
            }
            Ok(LoginRequest::Chatgpt)
        }
        Some("apiKey") => {
            if let Some(key) = object
                .keys()
                .find(|key| !matches!(key.as_str(), "type" | "apiKey"))
            {
                return Err(format!("unknown request field: {key} (allowed: type, apiKey)"));
            }
            let key = object
                .get("apiKey")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key| {
                    !key.is_empty() && key.len() <= MAX_API_KEY_BYTES && !key.chars().any(char::is_control)
                })
                .ok_or_else(|| {
                    format!("apiKey must be a non-empty string of at most {MAX_API_KEY_BYTES} bytes")
                })?;
            Ok(LoginRequest::ApiKey(ApiKey(key.to_string())))
        }
        _ => Err("type must be \"chatgpt\" or \"apiKey\"".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Account view (secret-free)
// ---------------------------------------------------------------------------

fn read_auth_file(auth_path: &Path) -> Option<String> {
    std::fs::metadata(auth_path)
        .ok()
        .filter(|meta| meta.is_file() && meta.len() <= MAX_AUTH_FILE_BYTES)
        .and_then(|_| std::fs::read_to_string(auth_path).ok())
}

/// The auth file of a pending ChatGPT login, as it was when the login started.
/// The fingerprint is change detection only (in memory, never exposed).
struct AuthWatch {
    path: PathBuf,
    baseline: Option<u64>,
}

impl AuthWatch {
    fn fingerprint(raw: &str) -> u64 {
        use std::hash::Hash;
        use std::hash::Hasher;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut hasher);
        hasher.finish()
    }

    fn capture(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            baseline: read_auth_file(path).as_deref().map(Self::fingerprint),
        }
    }

    /// The login wrote ChatGPT tokens: the file changed since the login
    /// started and now holds a non-empty id and access token.
    fn written(&self) -> bool {
        let Some(raw) = read_auth_file(&self.path) else {
            return false;
        };
        if Some(Self::fingerprint(&raw)) == self.baseline {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        let token = |name: &str| {
            value
                .get("tokens")
                .and_then(|tokens| tokens.get(name))
                .and_then(Value::as_str)
                .is_some_and(|token| !token.trim().is_empty())
        };
        token("id_token") && token("access_token")
    }
}

/// Non-secret login state of one auth file (`planType` = the id-token claim).
pub fn account_view(auth_path: &Path) -> Value {
    account_view_for(auth_path, None)
}

/// [`account_view`] of a profile's auth file: `planType` is the profile's
/// live account plan (a native rate-limit observation newer than the token
/// wins over the token claim; `profiles::plan_label`), the same value its
/// `planLabel` uses when no manual label is set.
pub fn profile_account_view(auth_path: &Path, profile_id: &str) -> Value {
    account_view_for(auth_path, Some(profile_id))
}

fn account_view_for(auth_path: &Path, profile_id: Option<&str>) -> Value {
    let raw = read_auth_file(auth_path);
    let present = raw.is_some();
    let parsed = raw.as_deref().and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let has_tokens = parsed
        .as_ref()
        .and_then(|value| value.get("tokens"))
        .is_some_and(Value::is_object);
    let has_api_key = parsed
        .as_ref()
        .and_then(|value| {
            // Older cutex / hand-written auth files use the lowercase spelling
            // (launch accepts both; see launch::env).
            value
                .get("OPENAI_API_KEY")
                .filter(|key| key.is_string())
                .or_else(|| value.get("openai_api_key"))
        })
        .and_then(Value::as_str)
        .is_some_and(|key| !key.trim().is_empty());
    let other_mode = parsed
        .as_ref()
        .and_then(|value| value.get("auth_mode"))
        .and_then(Value::as_str)
        .is_some_and(|mode| !matches!(mode, "apikey" | "chatgpt"));
    let auth_mode = if has_tokens {
        "chatgpt"
    } else if has_api_key {
        "apiKey"
    } else if other_mode {
        "other"
    } else {
        "none"
    };
    let (email, plan_type) = match (auth_mode, raw.as_deref()) {
        ("chatgpt", Some(raw)) => {
            let (email, token_plan) = crate::profiles::import::parse_auth_metadata(raw);
            let plan = match profile_id {
                Some(id) => crate::profiles::plan_label::live_plan_for_profile(id, raw),
                None => token_plan,
            };
            (email, plan)
        }
        _ => (None, None),
    };
    let last_refresh = parsed
        .as_ref()
        .and_then(|value| value.get("last_refresh"))
        .and_then(Value::as_str)
        .map(str::to_string);
    json!({
        "loggedIn": auth_mode != "none",
        "authMode": auth_mode,
        "email": email,
        "planType": plan_type,
        "lastRefresh": last_refresh,
        "authFilePresent": present,
    })
}

// ---------------------------------------------------------------------------
// Login sessions
// ---------------------------------------------------------------------------

/// What a login app-server is started for.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginTarget {
    /// The profile's materialized auth.json (absolute).
    pub auth_path: PathBuf,
    /// Extra `-c key=value` overrides (non-secret).
    pub overrides: Vec<String>,
    /// Extra child environment (proxy settings).
    pub envs: Vec<(String, String)>,
}

impl std::fmt::Debug for LoginTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoginTarget")
            .field("auth_path", &self.auth_path)
            .field("overrides", &self.overrides)
            .field("envs", &self.envs.iter().map(|(key, _)| key).collect::<Vec<_>>())
            .finish()
    }
}

pub trait LoginEndpointFactory: Send + Sync {
    /// Spawn and initialize an app-server bound to `target.auth_path`.
    fn spawn(&self, target: &LoginTarget) -> Result<Box<dyn CatalogEndpoint>, CatalogError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginKind {
    Chatgpt,
    ApiKey,
}

impl LoginKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::ApiKey => "apiKey",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStatus {
    Pending,
    Succeeded,
    Failed,
    Canceled,
}

impl LoginStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

type SharedEndpoint = Arc<Mutex<Option<Box<dyn CatalogEndpoint>>>>;

struct LoginRecord {
    login_id: String,
    profile_id: String,
    kind: LoginKind,
    status: LoginStatus,
    error: Option<String>,
    warning: Option<String>,
    auth_url: Option<String>,
    started_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    deadline: Instant,
    finished: Option<Instant>,
    endpoint: Option<SharedEndpoint>,
}

impl LoginRecord {
    fn view(&self) -> Value {
        let pending = self.status == LoginStatus::Pending;
        json!({
            "loginId": self.login_id,
            "profileId": self.profile_id,
            "type": self.kind.as_str(),
            "status": self.status.as_str(),
            "error": self.error,
            "warning": self.warning,
            "authUrl": if pending { self.auth_url.clone() } else { None },
            "startedAt": self.started_at.to_rfc3339(),
            "expiresAt": if self.kind == LoginKind::Chatgpt { Some(self.expires_at.to_rfc3339()) } else { None },
            "finishedAt": self.finished_at.map(|at| at.to_rfc3339()),
        })
    }

    /// Mark finished; returns the endpoint to drop (outside any lock).
    fn finish(&mut self, status: LoginStatus, error: Option<String>) -> Option<SharedEndpoint> {
        self.status = status;
        self.error = error;
        self.finished_at = Some(Utc::now());
        self.finished = Some(Instant::now());
        self.endpoint.take()
    }
}

#[derive(Debug)]
pub enum LoginError {
    /// No app-server could be started.
    Unavailable(String),
    /// The native app-server rejected the request.
    Rejected(String),
    /// Transport failure or timeout talking to the app-server.
    Failed(String),
}

fn request_error(error: CatalogError) -> LoginError {
    match error {
        CatalogError::Rpc { error, .. } => LoginError::Rejected(error.message),
        other => LoginError::Failed(other.to_string()),
    }
}

/// Callback after a successful login of `kind` for the profile ID. A returned
/// message is surfaced as the login's `warning`.
pub type SuccessHook = Box<dyn Fn(&str, LoginKind) -> Option<String> + Send + Sync>;

pub struct LoginManager {
    factory: Box<dyn LoginEndpointFactory>,
    ttl: Duration,
    records: Mutex<HashMap<String, LoginRecord>>,
    /// Serializes starts / logouts (one native callback listener at a time).
    operations: Mutex<()>,
    on_success: SuccessHook,
    auth_poll: Duration,
    completion_grace: Duration,
}

fn close(endpoint: Option<SharedEndpoint>) {
    if let Some(endpoint) = endpoint {
        // Drops the OwnedStdioEndpoint, which stops the child.
        endpoint.lock().unwrap_or_else(|error| error.into_inner()).take();
    }
}

impl LoginManager {
    pub fn new(factory: Box<dyn LoginEndpointFactory>, ttl: Duration, on_success: SuccessHook) -> Self {
        Self {
            factory,
            ttl,
            records: Mutex::new(HashMap::new()),
            operations: Mutex::new(()),
            on_success,
            auth_poll: AUTH_POLL_INTERVAL,
            completion_grace: COMPLETION_GRACE,
        }
    }

    /// Override the auth-file fallback timing (auth file poll interval, grace
    /// for the native completion notification after the file was written).
    pub fn with_auth_fallback(mut self, poll: Duration, grace: Duration) -> Self {
        self.auth_poll = poll;
        self.completion_grace = grace;
        self
    }

    fn records(&self) -> std::sync::MutexGuard<'_, HashMap<String, LoginRecord>> {
        self.records.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn prune(records: &mut HashMap<String, LoginRecord>) {
        let now = Instant::now();
        records.retain(|_, record| {
            record
                .finished
                .is_none_or(|finished| now.duration_since(finished) < FINISHED_RETENTION)
        });
    }

    /// Cancel pending logins matching `predicate`; returns endpoints to close.
    fn supersede(&self, predicate: impl Fn(&LoginRecord) -> bool, reason: &str) -> Vec<SharedEndpoint> {
        let mut records = self.records();
        Self::prune(&mut records);
        records
            .values_mut()
            .filter(|record| record.status == LoginStatus::Pending && predicate(record))
            .filter_map(|record| record.finish(LoginStatus::Canceled, Some(reason.to_string())))
            .collect()
    }

    pub fn start_chatgpt(
        self: &Arc<Self>,
        profile_id: &str,
        target: &LoginTarget,
    ) -> Result<Value, LoginError> {
        let _operation = self.operations.lock().unwrap_or_else(|error| error.into_inner());
        // The native browser flow binds one fixed localhost callback port:
        // a newer login (any profile) replaces an older pending one.
        for endpoint in self.supersede(
            |record| record.kind == LoginKind::Chatgpt || record.profile_id == profile_id,
            "superseded by a newer login",
        ) {
            close(Some(endpoint));
        }
        // Before the native login can write it.
        let auth = AuthWatch::capture(&target.auth_path);
        let mut endpoint = self
            .factory
            .spawn(target)
            .map_err(|error| LoginError::Unavailable(error.to_string()))?;
        let response = endpoint
            .request("account/login/start", json!({ "type": "chatgpt" }))
            .map_err(request_error)?;
        let login_id = response
            .get("loginId")
            .and_then(Value::as_str)
            .filter(|id| valid_login_id(id))
            .map(str::to_string);
        let auth_url = response
            .get("authUrl")
            .and_then(Value::as_str)
            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
            .map(str::to_string);
        let (Some(login_id), Some(auth_url)) = (login_id, auth_url) else {
            return Err(LoginError::Failed(
                "app-server returned an unexpected account/login/start response".to_string(),
            ));
        };
        let now = Utc::now();
        let shared: SharedEndpoint = Arc::new(Mutex::new(Some(endpoint)));
        let record = LoginRecord {
            login_id: login_id.clone(),
            profile_id: profile_id.to_string(),
            kind: LoginKind::Chatgpt,
            status: LoginStatus::Pending,
            error: None,
            warning: None,
            auth_url: Some(auth_url),
            started_at: now,
            expires_at: now + chrono::Duration::from_std(self.ttl).unwrap_or_default(),
            finished_at: None,
            deadline: Instant::now() + self.ttl,
            finished: None,
            endpoint: Some(Arc::clone(&shared)),
        };
        let view = record.view();
        self.records().insert(login_id.clone(), record);
        let manager = Arc::clone(self);
        std::thread::spawn(move || manager.watch(&login_id, &shared, &auth));
        Ok(view)
    }

    /// Wait for `account/login/completed`, cancel, process exit or the TTL.
    /// The profile auth file gaining ChatGPT tokens also completes the login:
    /// after `completion_grace` without a notification (the native side sends
    /// it only after the browser loaded the success page), on a failure
    /// notification, on process exit, or at the TTL.
    fn watch(&self, login_id: &str, endpoint: &SharedEndpoint, auth: &AuthWatch) {
        let mut next_auth_check = Instant::now();
        let mut written_at: Option<Instant> = None;
        loop {
            let deadline = {
                let records = self.records();
                match records.get(login_id) {
                    Some(record) if record.status == LoginStatus::Pending => record.deadline,
                    _ => break,
                }
            };
            let now = Instant::now();
            if written_at.is_none() && now >= next_auth_check {
                next_auth_check = now + self.auth_poll;
                if auth.written() {
                    written_at = Some(now);
                }
            }
            if written_at.is_some_and(|at| now.duration_since(at) >= self.completion_grace) {
                self.complete(login_id, LoginStatus::Succeeded, None, None);
                break;
            }
            if now >= deadline {
                if written_at.is_some() || auth.written() {
                    self.complete(login_id, LoginStatus::Succeeded, None, None);
                    break;
                }
                if let Some(endpoint) = endpoint
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_mut()
                {
                    let _ = endpoint.request("account/login/cancel", json!({ "loginId": login_id }));
                }
                self.complete(login_id, LoginStatus::Failed, Some("login timed out".to_string()), None);
                break;
            }
            let event = {
                let mut guard = endpoint.lock().unwrap_or_else(|error| error.into_inner());
                match guard.as_mut() {
                    Some(endpoint) => endpoint.next_notification(POLL_INTERVAL),
                    None => break,
                }
            };
            match event {
                Ok(Some((method, params))) if method == "account/login/completed" => {
                    let params = params.unwrap_or(Value::Null);
                    let matches = params
                        .get("loginId")
                        .and_then(Value::as_str)
                        .is_none_or(|id| id == login_id);
                    if !matches {
                        continue;
                    }
                    if params.get("success").and_then(Value::as_bool) == Some(true) {
                        self.complete(login_id, LoginStatus::Succeeded, None, None);
                    } else {
                        let error = params
                            .get("error")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| "login failed".to_string());
                        if written_at.is_some() || auth.written() {
                            // E.g. the post-login account read failed: the
                            // credentials were saved and are usable.
                            self.complete(
                                login_id,
                                LoginStatus::Succeeded,
                                None,
                                Some(format!(
                                    "the profile auth file holds the new ChatGPT login, but the login app-server reported: {error}"
                                )),
                            );
                        } else {
                            self.complete(login_id, LoginStatus::Failed, Some(error), None);
                        }
                    }
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    if written_at.is_some() || auth.written() {
                        self.complete(login_id, LoginStatus::Succeeded, None, None);
                    } else {
                        self.complete(
                            login_id,
                            LoginStatus::Failed,
                            Some(format!("login app-server stopped: {error}")),
                            None,
                        );
                    }
                    break;
                }
            }
        }
        close(Some(Arc::clone(endpoint)));
    }

    fn complete(&self, login_id: &str, status: LoginStatus, error: Option<String>, warning: Option<String>) {
        let (endpoint, profile, kind) = {
            let mut records = self.records();
            let Some(record) = records.get_mut(login_id) else {
                return;
            };
            if record.status != LoginStatus::Pending {
                return;
            }
            record.warning = warning;
            (record.finish(status, error), record.profile_id.clone(), record.kind)
        };
        close(endpoint);
        if status == LoginStatus::Succeeded {
            if let Some(hook_warning) = (self.on_success)(&profile, kind) {
                if let Some(record) = self.records().get_mut(login_id) {
                    record.warning = Some(match record.warning.take() {
                        Some(existing) => format!("{existing}; {hook_warning}"),
                        None => hook_warning,
                    });
                }
            }
        }
    }

    pub fn login_api_key(
        &self,
        profile_id: &str,
        target: &LoginTarget,
        key: &ApiKey,
    ) -> Result<Value, LoginError> {
        let _operation = self.operations.lock().unwrap_or_else(|error| error.into_inner());
        for endpoint in self.supersede(|record| record.profile_id == profile_id, "superseded by a newer login") {
            close(Some(endpoint));
        }
        let mut endpoint = self
            .factory
            .spawn(target)
            .map_err(|error| LoginError::Unavailable(error.to_string()))?;
        let result = endpoint.request("account/login/start", json!({ "type": "apiKey", "apiKey": key.0 }));
        drop(endpoint);
        let (status, error) = match result {
            Ok(_) => (LoginStatus::Succeeded, None),
            Err(CatalogError::Rpc { error, .. }) => (LoginStatus::Failed, Some(error.message)),
            Err(other) => return Err(LoginError::Failed(other.to_string())),
        };
        let now = Utc::now();
        let mut record = LoginRecord {
            login_id: uuid::Uuid::new_v4().to_string(),
            profile_id: profile_id.to_string(),
            kind: LoginKind::ApiKey,
            status: LoginStatus::Pending,
            error: None,
            warning: None,
            auth_url: None,
            started_at: now,
            expires_at: now,
            finished_at: None,
            deadline: Instant::now(),
            finished: None,
            endpoint: None,
        };
        record.finish(status, error);
        if status == LoginStatus::Succeeded {
            record.warning = (self.on_success)(profile_id, LoginKind::ApiKey);
        }
        let view = record.view();
        self.records().insert(record.login_id.clone(), record);
        Ok(view)
    }

    pub fn logout(&self, profile_id: &str, target: &LoginTarget) -> Result<(), LoginError> {
        let _operation = self.operations.lock().unwrap_or_else(|error| error.into_inner());
        for endpoint in self.supersede(|record| record.profile_id == profile_id, "canceled by logout") {
            close(Some(endpoint));
        }
        let mut endpoint = self
            .factory
            .spawn(target)
            .map_err(|error| LoginError::Unavailable(error.to_string()))?;
        endpoint
            .request("account/logout", Value::Null)
            .map(|_| ())
            .map_err(request_error)
    }

    pub fn status(&self, profile_id: &str, login_id: &str) -> Option<Value> {
        let mut records = self.records();
        Self::prune(&mut records);
        records
            .get(login_id)
            .filter(|record| record.profile_id == profile_id)
            .map(LoginRecord::view)
    }

    /// The pending login of a profile, if any.
    pub fn active(&self, profile_id: &str) -> Option<Value> {
        self.records()
            .values()
            .find(|record| record.profile_id == profile_id && record.status == LoginStatus::Pending)
            .map(LoginRecord::view)
    }

    /// Cancel a pending login. `None` when unknown; otherwise `(changed, view)`.
    pub fn cancel(&self, profile_id: &str, login_id: &str) -> Option<(bool, Value)> {
        let endpoint = {
            let mut records = self.records();
            let record = records
                .get_mut(login_id)
                .filter(|record| record.profile_id == profile_id)?;
            if record.status != LoginStatus::Pending {
                return Some((false, record.view()));
            }
            record.finish(LoginStatus::Canceled, None)
        };
        if let Some(endpoint) = endpoint {
            let mut guard = endpoint.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(endpoint) = guard.as_mut() {
                let _ = endpoint.request("account/login/cancel", json!({ "loginId": login_id }));
            }
            guard.take();
        }
        self.records()
            .get(login_id)
            .map(|record| (true, record.view()))
    }

    pub fn has_live_process(&self, login_id: &str) -> bool {
        self.records()
            .get(login_id)
            .and_then(|record| record.endpoint.as_ref())
            .is_some_and(|endpoint| {
                endpoint
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_some()
            })
    }
}

// ---------------------------------------------------------------------------
// Real app-server factory
// ---------------------------------------------------------------------------

pub(super) struct RuntimeLoginFactory;

impl LoginEndpointFactory for RuntimeLoginFactory {
    fn spawn(&self, target: &LoginTarget) -> Result<Box<dyn CatalogEndpoint>, CatalogError> {
        let (program, transport_args, codex_home) = super::codex_config::resolve_runtime()
            .map_err(|error| CatalogError::Launch(format!("no codex runtime available: {error:#}")))?;
        if !codex_home.is_dir() {
            return Err(CatalogError::Launch(format!(
                "shared CODEX_HOME does not exist: {}",
                codex_home.display()
            )));
        }
        let auth_path = target
            .auth_path
            .to_str()
            .ok_or_else(|| CatalogError::Launch("profile auth path must be UTF-8".to_string()))?;
        let mut command = super::codex_config::base_command(&program, &codex_home);
        for (key, value) in &target.envs {
            command.env(key, value);
        }
        // Root flags first: the multitool CLI takes them before `app-server`.
        command.args(["--auth-file", auth_path, "-c", "cli_auth_credentials_store=\"file\""]);
        command.args(["-c", "analytics.enabled=false"]);
        for value in &target.overrides {
            command.args(["-c", value]);
        }
        command.args(&transport_args);
        let mut options = StdioAppServerOptions::new(program.into_os_string(), codex_home);
        options.request_timeout = REQUEST_TIMEOUT;
        options.shutdown_timeout = Duration::from_secs(2);
        // Keep `account/login/completed` (the stdio reader drops notifications
        // by default).
        options.notification_buffer = LOGIN_NOTIFICATION_BUFFER;
        let mut endpoint = OwnedStdioEndpoint::spawn_command(options, command)?;
        endpoint.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "cutex_profile_login",
                    "title": "Cutex profile login",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            }),
        )?;
        endpoint.notify("initialized", None)?;
        Ok(Box::new(endpoint))
    }
}

/// After a successful login: re-apply Cutex's private file custody to the
/// profile files (the native store writes auth.json with inherited ACLs on
/// Windows, which owner launch rejects), then, for ChatGPT, refresh the
/// stored profile's email / plan labels from the id-token claims.
fn after_login(profile_id: &str, kind: LoginKind) -> Option<String> {
    let result = (|| -> anyhow::Result<()> {
        let mut store = crate::profiles::settings_api::load_store()?;
        let Some(account) = store.accounts.iter_mut().find(|account| account.id == profile_id) else {
            return Ok(());
        };
        let files = crate::profiles::materialize::materialized_account_files(account)?;
        crate::profiles::materialize::set_materialized_file_permissions(&files)
            .map_err(|error| anyhow::anyhow!("secure profile auth file: {error:#}"))?;
        if kind != LoginKind::Chatgpt {
            return Ok(());
        }
        let raw = std::fs::read_to_string(&files.auth_path)?;
        let (email, plan_type) = crate::profiles::import::parse_auth_metadata(&raw);
        let mut changed = false;
        if email.is_some() && account.email != email {
            account.email = email;
            changed = true;
        }
        if plan_type.is_some() && account.plan_type != plan_type {
            account.plan_type = plan_type;
            changed = true;
        }
        if changed {
            crate::profiles::store::save_store(&store)?;
        }
        Ok(())
    })();
    result.err().map(|error| format!("{error:#}"))
}

static MANAGER: OnceLock<Arc<LoginManager>> = OnceLock::new();

fn manager() -> Arc<LoginManager> {
    MANAGER
        .get_or_init(|| {
            Arc::new(LoginManager::new(
                Box::new(RuntimeLoginFactory),
                LOGIN_TTL,
                Box::new(after_login),
            ))
        })
        .clone()
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    code: &str,
    message: &str,
    retryable: bool,
    details: Value,
) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, status, reason, code, message, retryable, details)
}

fn login_error(stream: &mut TcpStream, failure: LoginError) -> anyhow::Result<()> {
    match failure {
        LoginError::Unavailable(diagnostic) => error(
            stream,
            503,
            "Service Unavailable",
            "codex_app_server_unavailable",
            "the codex app-server for the profile login could not be started",
            true,
            json!({ "diagnostic": diagnostic }),
        ),
        LoginError::Rejected(message) => error(
            stream,
            400,
            "Bad Request",
            "login_rejected",
            &message,
            false,
            json!({}),
        ),
        LoginError::Failed(diagnostic) => error(
            stream,
            502,
            "Bad Gateway",
            "codex_app_server_failed",
            "the codex app-server failed during the login request",
            true,
            json!({ "diagnostic": diagnostic }),
        ),
    }
}

fn profile_ref(account: &StoredAccount) -> Value {
    json!({ "id": account.id, "name": account.name, "cliKind": account.cli_kind.to_string() })
}

fn supported_login_types(account: &StoredAccount) -> Vec<&'static str> {
    match account.cli_kind {
        CliKind::Claude | CliKind::Dsh | CliKind::Kimi => Vec::new(),
        // Third-party API-key profiles route through a custom provider; a
        // ChatGPT credential would not launch on them.
        CliKind::Codex if account.source.as_deref() == Some("api-key") => vec!["apiKey"],
        CliKind::Codex => vec!["chatgpt", "apiKey"],
    }
}

/// Profile auth file + child overrides for a login app-server.
pub(super) fn login_target(account: &StoredAccount) -> anyhow::Result<LoginTarget> {
    anyhow::ensure!(account.cli_kind == CliKind::Codex, "account login is only available for Codex profiles");
    let files = crate::profiles::materialize::materialized_account_files(account)?;
    let directory = files
        .auth_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("profile auth path has no directory"))?;
    if !directory.is_dir() {
        crate::profiles::materialize::ensure_materialized_account_dir(&files)?;
        #[cfg(windows)]
        crate::platform::private_fs::secure_directory(directory)
            .map_err(|error| anyhow::anyhow!("secure profile directory: {error:?}"))?;
    }
    let mut overrides = Vec::new();
    if let Some(method) = crate::profiles::settings_api::codex_snapshot(account).forced_login_method {
        if matches!(method.as_str(), "chatgpt" | "api") {
            overrides.push(format!("forced_login_method=\"{method}\""));
        }
    }
    let global = crate::config::store::load_codez_config();
    let envs = crate::config::proxy::proxy_envs(
        crate::config::proxy::effective_proxy_config(account, &global),
        None,
    );
    Ok(LoginTarget {
        auth_path: files.auth_path,
        overrides,
        envs,
    })
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    profile: &str,
    route: LoginRoute,
) -> anyhow::Result<()> {
    let expected = match route {
        LoginRoute::Account | LoginRoute::Status(_) => "GET",
        _ => "POST",
    };
    if request.method != expected {
        return error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            &format!("use {expected}"),
            false,
            json!({}),
        );
    }
    let store = match crate::profiles::settings_api::load_store() {
        Ok(store) => store,
        Err(failure) => {
            return error(
                stream,
                503,
                "Service Unavailable",
                "profile_store_unavailable",
                "the profile store could not be read",
                true,
                json!({ "diagnostic": format!("{failure:#}") }),
            )
        }
    };
    let Some(account) = crate::profiles::settings_api::find(&store, profile).cloned() else {
        return error(
            stream,
            404,
            "Not Found",
            "profile_not_found",
            "no profile has this ID or name",
            false,
            json!({ "profile": profile }),
        );
    };
    if account.cli_kind != CliKind::Codex {
        return error(
            stream,
            400,
            "Bad Request",
            "unsupported",
            "account login is only available for Codex profiles",
            false,
            json!({ "profile": account.id, "cliKind": account.cli_kind.to_string() }),
        );
    }
    let manager = manager();
    let files = crate::profiles::materialize::materialized_account_files(&account)?;
    let account_body = |manager: &LoginManager| {
        json!({
            "contractVersion": 2,
            "profile": profile_ref(&account),
            "account": profile_account_view(&files.auth_path, &account.id),
            "supportedLoginTypes": supported_login_types(&account),
            "activeLogin": manager.active(&account.id),
        })
    };
    match route {
        LoginRoute::Account => write_json_response(stream, 200, "OK", &account_body(&manager)),
        LoginRoute::Status(login_id) => match manager.status(&account.id, &login_id) {
            Some(login) => write_json_response(
                stream,
                200,
                "OK",
                &json!({ "contractVersion": 2, "login": login }),
            ),
            None => login_not_found(stream, &login_id),
        },
        LoginRoute::Cancel(login_id) => match manager.cancel(&account.id, &login_id) {
            Some((changed, login)) => write_json_response(
                stream,
                200,
                "OK",
                &json!({ "contractVersion": 2, "changed": changed, "login": login }),
            ),
            None => login_not_found(stream, &login_id),
        },
        LoginRoute::Start => {
            let login = match parse_login_request(&request.body) {
                Ok(login) => login,
                Err(message) => {
                    return error(stream, 400, "Bad Request", "invalid_request", &message, false, json!({}))
                }
            };
            let kind = match &login {
                LoginRequest::Chatgpt => "chatgpt",
                LoginRequest::ApiKey(_) => "apiKey",
            };
            if !supported_login_types(&account).contains(&kind) {
                return error(
                    stream,
                    400,
                    "Bad Request",
                    "unsupported",
                    "this login type is not supported for this profile",
                    false,
                    json!({ "profile": account.id, "type": kind,
                            "supportedLoginTypes": supported_login_types(&account) }),
                );
            }
            let target = match login_target(&account) {
                Ok(target) => target,
                Err(failure) => {
                    return error(
                        stream,
                        503,
                        "Service Unavailable",
                        "profile_store_unavailable",
                        "the profile auth directory could not be prepared",
                        true,
                        json!({ "diagnostic": format!("{failure:#}") }),
                    )
                }
            };
            match login {
                LoginRequest::Chatgpt => match manager.start_chatgpt(&account.id, &target) {
                    Ok(login) => write_json_response(
                        stream,
                        200,
                        "OK",
                        &json!({
                            "contractVersion": 2,
                            "loginId": login["loginId"],
                            "authUrl": login["authUrl"],
                            "login": login,
                        }),
                    ),
                    Err(failure) => login_error(stream, failure),
                },
                LoginRequest::ApiKey(key) => match manager.login_api_key(&account.id, &target, &key) {
                    Ok(login) => write_json_response(
                        stream,
                        200,
                        "OK",
                        &json!({
                            "contractVersion": 2,
                            "loginId": login["loginId"],
                            "login": login,
                            "account": profile_account_view(&files.auth_path, &account.id),
                        }),
                    ),
                    Err(failure) => login_error(stream, failure),
                },
            }
        }
        LoginRoute::Logout => {
            let target = match login_target(&account) {
                Ok(target) => target,
                Err(failure) => {
                    return error(
                        stream,
                        503,
                        "Service Unavailable",
                        "profile_store_unavailable",
                        "the profile auth directory could not be prepared",
                        true,
                        json!({ "diagnostic": format!("{failure:#}") }),
                    )
                }
            };
            match manager.logout(&account.id, &target) {
                Ok(()) => write_json_response(stream, 200, "OK", &account_body(&manager)),
                Err(failure) => login_error(stream, failure),
            }
        }
    }
}

fn login_not_found(stream: &mut TcpStream, login_id: &str) -> anyhow::Result<()> {
    error(
        stream,
        404,
        "Not Found",
        "login_not_found",
        "no login with this ID for this profile (finished logins are kept for 30 minutes)",
        false,
        json!({ "loginId": login_id }),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn dsh_profile_has_no_login_target_or_auth_directory() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let account: StoredAccount = serde_json::from_value(serde_json::json!({
            "id":"dsh-login-refusal","name":"DSH","cli_kind":"dsh"
        })).unwrap();
        assert!(supported_login_types(&account).is_empty());
        assert!(login_target(&account).is_err());
        let files = crate::profiles::materialize::materialized_account_files(&account).unwrap();
        assert!(!files.auth_path.parent().unwrap().exists());
    }

    use super::*;
    use crate::app_server::protocol::RpcError;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[test]
    fn paths_parse_strictly() {
        assert_eq!(
            parse_path("/v2/profiles/GLM%20pro/account"),
            Some(("GLM pro".to_string(), LoginRoute::Account))
        );
        assert_eq!(parse_path("/v2/profiles/p/login"), Some(("p".into(), LoginRoute::Start)));
        assert_eq!(parse_path("/v2/profiles/p/logout"), Some(("p".into(), LoginRoute::Logout)));
        assert_eq!(
            parse_path("/v2/profiles/p/login/abc-123"),
            Some(("p".into(), LoginRoute::Status("abc-123".into())))
        );
        assert_eq!(
            parse_path("/v2/profiles/p/login/abc-123/cancel"),
            Some(("p".into(), LoginRoute::Cancel("abc-123".into())))
        );
        for bad in [
            "/v2/profiles/p",
            "/v2/profiles//account",
            "/v2/profiles/p/quota",
            "/v2/profiles/p/login/",
            "/v2/profiles/p/login/a_b",
            "/v2/profiles/p/login/a/b",
            "/v2/profiles/p/account/x",
            "/v2/profiles/a%2Fb/account",
        ] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn login_requests_are_strict_and_keys_redacted() {
        assert_eq!(parse_login_request(br#"{"type":"chatgpt"}"#), Ok(LoginRequest::Chatgpt));
        let LoginRequest::ApiKey(key) =
            parse_login_request(br#"{"type":"apiKey","apiKey":" sk-test-fake "}"#).unwrap()
        else {
            panic!("apiKey");
        };
        assert_eq!(key.0, "sk-test-fake");
        assert!(!format!("{key:?}").contains("sk-test"));
        for bad in [
            &br#"{"type":"chatgpt","apiKey":"x"}"#[..],
            br#"{"type":"apiKey"}"#,
            br#"{"type":"apiKey","apiKey":""}"#,
            br#"{"type":"apiKey","apiKey":"a\nb"}"#,
            br#"{"type":"apiKey","apiKey":"x","extra":1}"#,
            br#"{"type":"chatgptDeviceCode"}"#,
            br#"{"type":"chatgptAuthTokens","accessToken":"x"}"#,
            br#"[]"#,
            br#"{}"#,
        ] {
            assert!(parse_login_request(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-profile-login-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_jwt(claims: Value) -> String {
        use base64::Engine;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            engine.encode(br#"{"alg":"none"}"#),
            engine.encode(claims.to_string())
        )
    }

    #[test]
    fn account_view_reports_mode_without_secrets() {
        let dir = temp_dir();
        let path = dir.join("auth.json");
        assert_eq!(account_view(&path)["authMode"], "none");
        assert_eq!(account_view(&path)["loggedIn"], false);
        assert_eq!(account_view(&path)["authFilePresent"], false);

        std::fs::write(&path, json!({ "auth_mode": "apikey", "OPENAI_API_KEY": "sk-fake-secret" }).to_string())
            .unwrap();
        let view = account_view(&path);
        assert_eq!(view["authMode"], "apiKey");
        assert_eq!(view["loggedIn"], true);
        assert!(!view.to_string().contains("sk-fake-secret"));

        std::fs::write(&path, json!({ "openai_api_key": "sk-fake-lower" }).to_string()).unwrap();
        let view = account_view(&path);
        assert_eq!(view["authMode"], "apiKey");
        assert!(!view.to_string().contains("sk-fake-lower"));
        std::fs::write(&path, json!({ "OPENAI_API_KEY": null, "openai_api_key": "sk-fake-lower" }).to_string())
            .unwrap();
        assert_eq!(account_view(&path)["authMode"], "apiKey");

        let id_token = fake_jwt(json!({
            "email": "owner@example.com",
            "https://api.openai.com/auth": { "chatgpt_plan_type": "pro" }
        }));
        std::fs::write(
            &path,
            json!({ "OPENAI_API_KEY": null, "last_refresh": "2026-10-06T00:00:00Z",
                    "tokens": { "id_token": id_token, "access_token": "fake-access", "refresh_token": "fake-refresh" } })
            .to_string(),
        )
        .unwrap();
        let view = account_view(&path);
        assert_eq!(view["authMode"], "chatgpt");
        assert_eq!(view["email"], "owner@example.com");
        assert_eq!(view["planType"], "pro");
        assert_eq!(view["lastRefresh"], "2026-10-06T00:00:00Z");
        let text = view.to_string();
        for secret in ["fake-access", "fake-refresh", "sig"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- fake app-server ----

    #[derive(Default)]
    struct Shared {
        log: Mutex<Vec<(String, Value)>>,
        replies: Mutex<VecDeque<Result<Value, CatalogError>>>,
        notifications: Mutex<VecDeque<(String, Option<Value>)>>,
        spawns: AtomicUsize,
        live: AtomicUsize,
        targets: Mutex<Vec<PathBuf>>,
        /// The app-server exits once the scripted notifications are drained.
        exited: std::sync::atomic::AtomicBool,
    }

    struct FakeEndpoint(Arc<Shared>);

    impl Drop for FakeEndpoint {
        fn drop(&mut self) {
            self.0.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl CatalogEndpoint for FakeEndpoint {
        fn request(&mut self, method: &str, params: Value) -> Result<Value, CatalogError> {
            self.0.log.lock().unwrap().push((method.to_string(), params));
            self.0.replies.lock().unwrap().pop_front().unwrap_or_else(|| Ok(json!({})))
        }
        fn notify(&mut self, _: &str, _: Option<Value>) -> Result<(), CatalogError> {
            Ok(())
        }
        fn next_notification(
            &mut self,
            timeout: Duration,
        ) -> Result<Option<(String, Option<Value>)>, CatalogError> {
            if let Some(next) = self.0.notifications.lock().unwrap().pop_front() {
                return Ok(Some(next));
            }
            if self.0.exited.load(Ordering::SeqCst) {
                return Err(CatalogError::Transport("app-server closed stdout (exit code: 0)".into()));
            }
            std::thread::sleep(timeout.min(Duration::from_millis(10)));
            Ok(None)
        }
    }

    struct FakeFactory(Arc<Shared>);

    impl LoginEndpointFactory for FakeFactory {
        fn spawn(&self, target: &LoginTarget) -> Result<Box<dyn CatalogEndpoint>, CatalogError> {
            self.0.spawns.fetch_add(1, Ordering::SeqCst);
            self.0.live.fetch_add(1, Ordering::SeqCst);
            self.0.targets.lock().unwrap().push(target.auth_path.clone());
            Ok(Box::new(FakeEndpoint(Arc::clone(&self.0))))
        }
    }

    type Successes = Arc<Mutex<Vec<(String, LoginKind)>>>;

    fn manager_with(ttl: Duration) -> (Arc<LoginManager>, Arc<Shared>, Successes) {
        let shared = Arc::new(Shared::default());
        let successes: Successes = Arc::default();
        let hook = Arc::clone(&successes);
        let manager = Arc::new(LoginManager::new(
            Box::new(FakeFactory(Arc::clone(&shared))),
            ttl,
            Box::new(move |profile, kind| {
                hook.lock().unwrap().push((profile.to_string(), kind));
                (profile == "warn").then(|| "secure profile auth file: denied".to_string())
            }),
        ));
        (manager, shared, successes)
    }

    fn target(name: &str) -> LoginTarget {
        LoginTarget {
            auth_path: PathBuf::from(format!("/profiles/{name}/auth.json")),
            overrides: Vec::new(),
            envs: vec![("HTTPS_PROXY".into(), "http://user:proxy-pass@h:1".into())],
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < Duration::from_secs(5), "condition not reached");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn chatgpt_reply(login_id: &str) -> Result<Value, CatalogError> {
        Ok(json!({ "type": "chatgpt", "loginId": login_id, "authUrl": "https://auth.example/authorize?x=1" }))
    }

    #[test]
    fn chatgpt_login_completes_on_notification_and_reaps() {
        let (manager, shared, successes) = manager_with(Duration::from_secs(60));
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-1"));
        let view = manager.start_chatgpt("p1", &target("p1")).unwrap();
        assert_eq!(view["status"], "pending");
        assert_eq!(view["authUrl"], "https://auth.example/authorize?x=1");
        assert_eq!(view["type"], "chatgpt");
        assert_eq!(shared.log.lock().unwrap()[0], ("account/login/start".to_string(), json!({ "type": "chatgpt" })));
        assert_eq!(shared.targets.lock().unwrap()[0], PathBuf::from("/profiles/p1/auth.json"));
        assert_eq!(manager.active("p1").unwrap()["loginId"], "l-1");
        assert!(manager.status("p2", "l-1").is_none(), "scoped to the profile");
        assert!(manager.has_live_process("l-1"));

        // A notification for another login is ignored.
        shared.notifications.lock().unwrap().extend([
            ("account/updated".to_string(), Some(json!({ "authMode": "chatgpt" }))),
            ("account/login/completed".to_string(), Some(json!({ "loginId": "other", "success": false }))),
            ("account/login/completed".to_string(), Some(json!({ "loginId": "l-1", "success": true, "error": null }))),
        ]);
        wait_until(|| manager.status("p1", "l-1").unwrap()["status"] == "succeeded");
        let done = manager.status("p1", "l-1").unwrap();
        assert_eq!(done["authUrl"], Value::Null);
        assert!(done["finishedAt"].is_string());
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);
        assert!(manager.active("p1").is_none());
        assert_eq!(successes.lock().unwrap().as_slice(), &[("p1".to_string(), LoginKind::Chatgpt)]);
    }

    #[test]
    fn failed_cancel_and_timeout_paths() {
        let (manager, shared, successes) = manager_with(Duration::from_secs(60));
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-f"));
        manager.start_chatgpt("p1", &target("p1")).unwrap();
        shared.notifications.lock().unwrap().push_back((
            "account/login/completed".to_string(),
            Some(json!({ "loginId": "l-f", "success": false, "error": "Login server error: denied" })),
        ));
        wait_until(|| manager.status("p1", "l-f").unwrap()["status"] == "failed");
        assert_eq!(manager.status("p1", "l-f").unwrap()["error"], "Login server error: denied");

        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-c"));
        manager.start_chatgpt("p1", &target("p1")).unwrap();
        let (changed, view) = manager.cancel("p1", "l-c").unwrap();
        assert!(changed);
        assert_eq!(view["status"], "canceled");
        assert!(shared
            .log
            .lock()
            .unwrap()
            .contains(&("account/login/cancel".to_string(), json!({ "loginId": "l-c" }))));
        let (changed, _) = manager.cancel("p1", "l-c").unwrap();
        assert!(!changed, "cancel is idempotent");
        assert!(manager.cancel("p1", "missing").is_none());
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);

        let (manager, shared, _) = manager_with(Duration::from_millis(50));
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-t"));
        manager.start_chatgpt("p1", &target("p1")).unwrap();
        wait_until(|| manager.status("p1", "l-t").unwrap()["status"] == "failed");
        assert_eq!(manager.status("p1", "l-t").unwrap()["error"], "login timed out");
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);
        assert!(successes.lock().unwrap().is_empty());
    }

    #[test]
    fn newer_chatgpt_login_supersedes_any_pending_one() {
        let (manager, shared, _) = manager_with(Duration::from_secs(60));
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-a"));
        manager.start_chatgpt("p1", &target("p1")).unwrap();
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-b"));
        manager.start_chatgpt("p2", &target("p2")).unwrap();
        let old = manager.status("p1", "l-a").unwrap();
        assert_eq!(old["status"], "canceled");
        assert_eq!(old["error"], "superseded by a newer login");
        assert_eq!(manager.status("p2", "l-b").unwrap()["status"], "pending");
        wait_until(|| shared.live.load(Ordering::SeqCst) == 1);
        manager.cancel("p2", "l-b").unwrap();
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);
    }

    #[test]
    fn api_key_login_is_synchronous_and_logout_uses_the_profile_file() {
        let (manager, shared, successes) = manager_with(Duration::from_secs(60));
        shared.replies.lock().unwrap().push_back(Ok(json!({ "type": "apiKey" })));
        let view = manager
            .login_api_key("p1", &target("p1"), &ApiKey("sk-test-fake".into()))
            .unwrap();
        assert_eq!(view["status"], "succeeded");
        assert_eq!(view["type"], "apiKey");
        assert_eq!(view["expiresAt"], Value::Null);
        assert!(!view.to_string().contains("sk-test-fake"));
        assert_eq!(
            shared.log.lock().unwrap()[0],
            ("account/login/start".to_string(), json!({ "type": "apiKey", "apiKey": "sk-test-fake" }))
        );
        assert_eq!(shared.live.load(Ordering::SeqCst), 0, "process reaped after the request");
        assert_eq!(manager.status("p1", view["loginId"].as_str().unwrap()).unwrap()["status"], "succeeded");
        assert_eq!(successes.lock().unwrap().as_slice(), &[("p1".to_string(), LoginKind::ApiKey)]);

        shared.replies.lock().unwrap().push_back(Err(CatalogError::Rpc {
            method: "account/login/start".into(),
            error: RpcError { code: -32600, message: "API key login is disabled. Use ChatGPT login instead.".into(), data: None },
        }));
        let view = manager.login_api_key("p1", &target("p1"), &ApiKey("sk-test-fake".into())).unwrap();
        assert_eq!(view["status"], "failed");
        assert_eq!(view["error"], "API key login is disabled. Use ChatGPT login instead.");
        assert_eq!(view["warning"], Value::Null);

        // A post-login custody failure is surfaced, the login still succeeded.
        shared.replies.lock().unwrap().push_back(Ok(json!({ "type": "apiKey" })));
        let view = manager.login_api_key("warn", &target("warn"), &ApiKey("sk-test-fake".into())).unwrap();
        assert_eq!(view["status"], "succeeded");
        assert_eq!(view["warning"], "secure profile auth file: denied");

        manager.logout("p3", &target("p3")).unwrap();
        assert_eq!(shared.log.lock().unwrap().last().unwrap(), &("account/logout".to_string(), Value::Null));
        assert_eq!(shared.targets.lock().unwrap().last().unwrap(), &PathBuf::from("/profiles/p3/auth.json"));
        assert_eq!(shared.live.load(Ordering::SeqCst), 0);
        assert_eq!(shared.spawns.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn rejected_and_malformed_starts_leave_no_record() {
        let (manager, shared, _) = manager_with(Duration::from_secs(60));
        shared.replies.lock().unwrap().push_back(Err(CatalogError::Rpc {
            method: "account/login/start".into(),
            error: RpcError { code: -32600, message: "ChatGPT login is disabled. Use API key login instead.".into(), data: None },
        }));
        assert!(matches!(manager.start_chatgpt("p1", &target("p1")), Err(LoginError::Rejected(_))));
        shared.replies.lock().unwrap().push_back(Ok(json!({ "type": "chatgpt" })));
        assert!(matches!(manager.start_chatgpt("p1", &target("p1")), Err(LoginError::Failed(_))));
        assert!(manager.active("p1").is_none());
        assert_eq!(shared.live.load(Ordering::SeqCst), 0);
    }

    // ---- auth-file fallback ----

    fn manager_with_fallback(ttl: Duration, grace: Duration) -> (Arc<LoginManager>, Arc<Shared>, Successes) {
        let shared = Arc::new(Shared::default());
        let successes: Successes = Arc::default();
        let hook = Arc::clone(&successes);
        let manager = LoginManager::new(
            Box::new(FakeFactory(Arc::clone(&shared))),
            ttl,
            Box::new(move |profile, kind| {
                hook.lock().unwrap().push((profile.to_string(), kind));
                None
            }),
        )
        .with_auth_fallback(Duration::from_millis(10), grace);
        (Arc::new(manager), shared, successes)
    }

    fn file_target(dir: &Path) -> LoginTarget {
        LoginTarget { auth_path: dir.join("auth.json"), overrides: Vec::new(), envs: Vec::new() }
    }

    fn write_chatgpt_auth(path: &Path, generation: &str) {
        let id_token = fake_jwt(json!({ "email": "owner@example.com" }));
        std::fs::write(
            path,
            json!({ "auth_mode": "chatgpt", "OPENAI_API_KEY": null,
                    "tokens": { "id_token": id_token, "access_token": format!("fake-access-{generation}"),
                                "refresh_token": "fake-refresh" } })
            .to_string(),
        )
        .unwrap();
    }

    fn status_of(manager: &LoginManager, login_id: &str) -> Value {
        manager.status("p1", login_id).unwrap()
    }

    #[test]
    fn lost_completion_notification_falls_back_to_the_written_auth_file() {
        let (manager, shared, successes) = manager_with_fallback(Duration::from_secs(60), Duration::from_millis(150));
        let dir = temp_dir();
        let target = file_target(&dir);
        // Re-login of a profile that is already signed in: the unchanged file
        // alone is not a completion.
        write_chatgpt_auth(&target.auth_path, "old");
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-1"));
        manager.start_chatgpt("p1", &target).unwrap();
        // Unrelated notifications and an API-key-only rewrite do not count.
        shared
            .notifications
            .lock()
            .unwrap()
            .push_back(("account/updated".to_string(), Some(json!({ "authMode": null }))));
        std::fs::write(&target.auth_path, json!({ "OPENAI_API_KEY": "sk-fake" }).to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(status_of(&manager, "l-1")["status"], "pending");
        assert!(successes.lock().unwrap().is_empty());

        // The browser flow wrote new tokens; no notification ever arrives.
        let written = Instant::now();
        write_chatgpt_auth(&target.auth_path, "new");
        wait_until(|| status_of(&manager, "l-1")["status"] == "succeeded");
        assert!(written.elapsed() >= Duration::from_millis(150), "the native side gets a grace period");
        let done = status_of(&manager, "l-1");
        assert_eq!(done["warning"], Value::Null);
        assert_eq!(done["error"], Value::Null);
        assert!(manager.active("p1").is_none());
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);
        assert_eq!(successes.lock().unwrap().as_slice(), &[("p1".to_string(), LoginKind::Chatgpt)]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notifications_win_within_the_grace_and_a_failure_with_saved_tokens_succeeds() {
        let (manager, shared, successes) = manager_with_fallback(Duration::from_secs(60), Duration::from_secs(60));
        let dir = temp_dir();
        let target = file_target(&dir);

        // Success notification after the file was written: completed once.
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-1"));
        manager.start_chatgpt("p1", &target).unwrap();
        write_chatgpt_auth(&target.auth_path, "a");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(status_of(&manager, "l-1")["status"], "pending");
        shared.notifications.lock().unwrap().push_back((
            "account/login/completed".to_string(),
            Some(json!({ "loginId": "l-1", "success": true, "error": null, "onboardingEntrypoint": null })),
        ));
        wait_until(|| status_of(&manager, "l-1")["status"] == "succeeded");
        assert_eq!(status_of(&manager, "l-1")["warning"], Value::Null);
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);

        // A failure report while the file gained tokens: succeeded + warning.
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-2"));
        manager.start_chatgpt("p1", &target).unwrap();
        write_chatgpt_auth(&target.auth_path, "b");
        shared.notifications.lock().unwrap().push_back((
            "account/login/completed".to_string(),
            Some(json!({ "loginId": "l-2", "success": false, "error": "account changed before sign-in completed" })),
        ));
        wait_until(|| status_of(&manager, "l-2")["status"] == "succeeded");
        let warning = status_of(&manager, "l-2")["warning"].as_str().unwrap().to_string();
        assert!(warning.contains("account changed before sign-in completed"), "{warning}");
        assert!(!warning.contains("fake-access"), "{warning}");

        // A failure report without new tokens stays a failure.
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-3"));
        manager.start_chatgpt("p1", &target).unwrap();
        shared.notifications.lock().unwrap().push_back((
            "account/login/completed".to_string(),
            Some(json!({ "loginId": "l-3", "success": false, "error": "Login server error: denied" })),
        ));
        wait_until(|| status_of(&manager, "l-3")["status"] == "failed");
        assert_eq!(status_of(&manager, "l-3")["error"], "Login server error: denied");
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);
        assert_eq!(successes.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exit_or_timeout_after_saved_tokens_succeeds() {
        // App-server exits right after writing the tokens (before notifying).
        let (manager, shared, successes) = manager_with_fallback(Duration::from_secs(60), Duration::from_secs(60));
        let dir = temp_dir();
        let target = file_target(&dir);
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-1"));
        manager.start_chatgpt("p1", &target).unwrap();
        write_chatgpt_auth(&target.auth_path, "a");
        shared.exited.store(true, Ordering::SeqCst);
        wait_until(|| status_of(&manager, "l-1")["status"] == "succeeded");
        wait_until(|| shared.live.load(Ordering::SeqCst) == 0);

        // Exit without new tokens is a failure.
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-2"));
        manager.start_chatgpt("p1", &target).unwrap();
        wait_until(|| status_of(&manager, "l-2")["status"] == "failed");
        assert!(status_of(&manager, "l-2")["error"].as_str().unwrap().starts_with("login app-server stopped"));
        assert_eq!(successes.lock().unwrap().len(), 1);

        // TTL reached after the tokens were saved (grace longer than the TTL).
        let (manager, shared, successes) = manager_with_fallback(Duration::from_millis(300), Duration::from_secs(60));
        shared.replies.lock().unwrap().push_back(chatgpt_reply("l-t"));
        manager.start_chatgpt("p1", &target).unwrap();
        write_chatgpt_auth(&target.auth_path, "b");
        wait_until(|| status_of(&manager, "l-t")["status"] != "pending");
        assert_eq!(status_of(&manager, "l-t")["status"], "succeeded");
        assert!(!shared.log.lock().unwrap().iter().any(|(method, _)| method == "account/login/cancel"));
        assert_eq!(successes.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn target_debug_hides_env_values() {
        let text = format!("{:?}", target("p1"));
        assert!(!text.contains("proxy-pass"), "{text}");
        assert!(text.contains("HTTPS_PROXY"));
    }

    /// Live: CUTEX_PROFILE_LOGIN_SMOKE_BIN=<cute-codex multitool> cargo test -- --ignored live_profile_login
    /// Starts a ChatGPT login (auth URL only, never completed), cancels it,
    /// then logs in with an obviously fake API key into a throwaway auth file.
    #[test]
    #[ignore]
    fn live_profile_login_smoke() {
        struct Explicit(PathBuf, PathBuf);
        impl LoginEndpointFactory for Explicit {
            fn spawn(&self, target: &LoginTarget) -> Result<Box<dyn CatalogEndpoint>, CatalogError> {
                let mut command = super::super::codex_config::base_command(&self.0, &self.1);
                command.args(["--auth-file", target.auth_path.to_str().unwrap(), "-c",
                              "cli_auth_credentials_store=\"file\"", "app-server", "--listen", "stdio://"]);
                let mut options = StdioAppServerOptions::new(self.0.clone().into_os_string(), self.1.clone());
                options.notification_buffer = LOGIN_NOTIFICATION_BUFFER;
                let mut endpoint = OwnedStdioEndpoint::spawn_command(options, command)?;
                endpoint.request("initialize", json!({ "clientInfo": { "name": "t", "title": "t", "version": "0" },
                                                       "capabilities": { "experimentalApi": true } }))?;
                endpoint.notify("initialized", None)?;
                Ok(Box::new(endpoint))
            }
        }
        let program = PathBuf::from(std::env::var("CUTEX_PROFILE_LOGIN_SMOKE_BIN").unwrap());
        let root = temp_dir().canonicalize().unwrap();
        let home = root.join("codex-home");
        let profile = root.join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        let auth = profile.join("auth.json");
        let manager = Arc::new(LoginManager::new(
            Box::new(Explicit(program, home.clone())),
            Duration::from_secs(60),
            Box::new(|_, _| None),
        ));
        let target = LoginTarget { auth_path: auth.clone(), overrides: Vec::new(), envs: Vec::new() };
        let view = manager.start_chatgpt("p", &target).unwrap();
        eprintln!("chatgpt start: status={} authUrl present={}", view["status"], view["authUrl"].is_string());
        assert_eq!(view["status"], "pending");
        assert!(view["authUrl"].as_str().unwrap().starts_with("https://"));
        let (changed, view) = manager.cancel("p", view["loginId"].as_str().unwrap()).unwrap();
        assert!(changed);
        assert_eq!(view["status"], "canceled");
        assert!(!auth.exists());
        let view = manager.login_api_key("p", &target, &ApiKey("sk-cutex-fake-smoke".into())).unwrap();
        assert_eq!(view["status"], "succeeded", "{view}");
        assert!(auth.exists());
        assert!(!home.join("auth.json").exists(), "shared home untouched");
        assert_eq!(account_view(&auth)["authMode"], "apiKey");
        manager.logout("p", &target).unwrap();
        assert!(!auth.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
