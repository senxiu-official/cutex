//! Root-only access to the *global* cute-codex configuration that managed
//! agents share: `POST /v2/codex/requests`.
//!
//! Managed (v4 local-deployment) agents all run with one native home: the
//! selected `LocalDeployment::native_home` (normally `~/.cutex/codex-home`,
//! falling back to `host_codex_home_dir()` when no deployment is selected).
//! Profiles do not get their own CODEX_HOME; they are applied at launch as
//! `--auth-file` / `-c key=value` overrides on top of that home's
//! `config.toml`. MCP servers, skills and hooks therefore live in the shared
//! home, and this route edits exactly that home.
//!
//! Requests are forwarded to a Cutex-owned app-server process for that home,
//! spawned on demand from the selected runtime bundle, kept warm for a few
//! minutes and then stopped. No thread is created. All requests (reads and
//! writes) are serialized through one process.
//!
//! Body: `{ "requestId": string, "method": string, "params"?: object|null }`.
//! Success: `200 { contractVersion, requestId, method, codexHome, result }`.
//! Native JSON-RPC error: `200 { contractVersion, requestId, method,
//! codexHome, error: { code, message, data } }`.
//! Cutex failures use the usual v2 error envelope (400/403/405/502/503/504).

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;
use serde_json::Map;
use serde_json::Value;

use crate::catalog::CatalogEndpoint;
use crate::catalog::CatalogError;
use crate::catalog::OwnedStdioEndpoint;
use crate::catalog::StdioAppServerOptions;
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;

pub const CODEX_REQUESTS_PATH: &str = "/v2/codex/requests";

/// Allowlisted global app-server methods. All of them are present in the
/// pinned app-server 0.144.1 experimental ClientRequest schema and are
/// validated against it (the schema's param objects are open, so newer
/// optional fields such as `clientRegistration` still pass).
pub const CODEX_CONFIG_METHODS: &[&str] = &[
    "config/read",
    "config/value/write",
    "config/batchWrite",
    "config/mcpServer/reload",
    "mcpServerStatus/list",
    "mcpServer/oauth/login",
    "skills/list",
    "skills/config/write",
    "skills/extraRoots/set",
    "hooks/list",
    // Codex plugins (capability `codex-plugins`): the local, profile-agnostic
    // catalog of the shared home. Installs land in the shared config.toml +
    // plugins/cache; per-agent homes follow them at launch
    // (`crate::launch::shared_config`). Remote-only and publishing methods
    // (plugin/share/*, plugin/skill/read, plugin/reconcile) stay denied.
    "plugin/list",
    "plugin/read",
    "plugin/search",
    "plugin/install",
    "plugin/uninstall",
    "marketplace/add",
    "marketplace/remove",
    "marketplace/upgrade",
];

/// Allowlisted methods newer than the pinned 0.144.1 schema (experimental in
/// the selected runtime). Only the envelope and an object `params` are
/// checked here; the native app-server deserializes them strictly.
pub const UNPINNED_METHODS: &[&str] = &["plugin/search"];

/// Plugin methods whose `cwds` would discover repo marketplaces relative to
/// a working directory: the config route is global, so they are refused.
const PLUGIN_CWDS_METHODS: &[&str] = &["plugin/list", "plugin/search"];

/// App-server config overrides for the Cutex-owned config process. The remote
/// plugin catalog is bound to whichever ChatGPT account the shared home is
/// logged in with and records installs server-side, so they would reach only
/// agents on that account; Cutex manages the local catalog instead, whose
/// installs live in the shared config.toml and reach every Codex agent.
pub const CONFIG_APP_SERVER_OVERRIDES: &[&str] = &["features.remote_plugin=false"];

pub const REDACTED: &str = "<redacted>";

const KEEP_WARM: Duration = Duration::from_secs(180);
const REAPER_INTERVAL: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const OAUTH_DEFAULT_KEEP: Duration = Duration::from_secs(600);
const OAUTH_MAX_KEEP: Duration = Duration::from_secs(1800);

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCodexRequest {
    pub request_id: String,
    pub method: String,
    /// Forwarded params (`Value::Null` for `config/mcpServer/reload`).
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexRequestError {
    Invalid(String),
    MethodDenied(String),
}

pub fn validate_codex_request(body: &[u8]) -> Result<ValidatedCodexRequest, CodexRequestError> {
    let invalid = |message: &str| CodexRequestError::Invalid(message.to_string());
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| CodexRequestError::Invalid(format!("invalid JSON request body: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("request body must be a JSON object"))?;
    if object.contains_key("profile") {
        return Err(invalid(
            "profile is not supported: managed agents share one CODEX_HOME; profiles only add launch overrides",
        ));
    }
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "requestId" | "method" | "params"))
    {
        return Err(CodexRequestError::Invalid(format!(
            "unknown request field: {key} (allowed: requestId, method, params)"
        )));
    }
    let request_id = object
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.chars().count() <= 256)
        .ok_or_else(|| invalid("requestId must be a non-empty string of at most 256 characters"))?
        .to_string();
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| invalid("method must be a non-empty string"))?
        .to_string();
    if !CODEX_CONFIG_METHODS.contains(&method.as_str()) {
        return Err(CodexRequestError::MethodDenied(method));
    }
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    let params = if method == "config/mcpServer/reload" {
        if !params.is_null() {
            return Err(invalid("config/mcpServer/reload takes no params"));
        }
        Value::Null
    } else {
        match params {
            Value::Null => Value::Object(Map::new()),
            Value::Object(_) => params,
            _ => return Err(invalid("params must be an object or null")),
        }
    };
    apply_fences(&method, &params)?;

    if UNPINNED_METHODS.contains(&method.as_str()) {
        return Ok(ValidatedCodexRequest {
            request_id,
            method,
            params,
        });
    }

    let mut message = Map::new();
    message.insert("id".to_string(), json!("cutex-codex-config"));
    message.insert("method".to_string(), json!(method));
    if !params.is_null() {
        message.insert("params".to_string(), params.clone());
    }
    super::native_requests::validate_pinned_client_request_schema(&Value::Object(message))
        .map_err(|error| match error {
            super::native_requests::NativeRequestValidationError::Invalid(message) => {
                CodexRequestError::Invalid(message.replace("native.message", "request"))
            }
            other => CodexRequestError::Invalid(format!("{other:?}")),
        })?;

    Ok(ValidatedCodexRequest {
        request_id,
        method,
        params,
    })
}

/// Cutex-specific fences on top of the native schema.
fn apply_fences(method: &str, params: &Value) -> Result<(), CodexRequestError> {
    let field_is_set = |name: &str| params.get(name).is_some_and(|value| !value.is_null());
    // Writes go only to the shared home's user config.toml.
    if matches!(method, "config/value/write" | "config/batchWrite") && field_is_set("filePath") {
        return Err(CodexRequestError::Invalid(
            "filePath is not allowed: writes always target the shared CODEX_HOME config.toml"
                .to_string(),
        ));
    }
    // The config app-server never has a loaded thread.
    if matches!(method, "mcpServerStatus/list" | "mcpServer/oauth/login") && field_is_set("threadId")
    {
        return Err(CodexRequestError::Invalid(
            "threadId is not allowed: global config requests are not bound to a thread".to_string(),
        ));
    }
    if PLUGIN_CWDS_METHODS.contains(&method) && field_is_set("cwds") {
        return Err(CodexRequestError::Invalid(
            "cwds is not allowed: the codex config route manages the shared home's plugins only"
                .to_string(),
        ));
    }
    if method == "plugin/search"
        && !params.get("searchTerm").is_some_and(Value::is_string)
    {
        return Err(CodexRequestError::Invalid(
            "plugin/search requires a string searchTerm".to_string(),
        ));
    }
    // A redacted placeholder written back would clobber the real secret.
    if matches!(method, "config/value/write" | "config/batchWrite") && contains_redacted(params) {
        return Err(CodexRequestError::Invalid(format!(
            "write contains the {REDACTED} placeholder; omit secret values you do not want to change"
        )));
    }
    Ok(())
}

fn contains_redacted(value: &Value) -> bool {
    match value {
        Value::String(text) => text == REDACTED,
        Value::Array(items) => items.iter().any(contains_redacted),
        Value::Object(map) => map.values().any(contains_redacted),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// Keys whose object value is a map of secret values (keys kept).
const SECRET_MAP_KEYS: &[&str] = &["env", "http_headers", "headers", "query_params"];

fn is_secret_leaf_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name.ends_with("_env_var") || name.ends_with("_env") || name.ends_with("_limit") {
        return false;
    }
    if name == "token" {
        return true;
    }
    [
        "bearer_token",
        "api_key",
        "apikey",
        "secret",
        "password",
        "access_token",
        "refresh_token",
        "id_token",
        "authorization",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

/// Whether a scalar at `path` (config key path, segments) is secret.
/// Name-based matches only apply to strings (feature flags such as
/// `features.api_key_model_discovery = true` are not secrets); every scalar
/// below a secret map (env, headers, ...) is redacted.
fn is_secret_config_path(path: &[String], scalar: &Value) -> bool {
    let Some(last) = path.last() else {
        return false;
    };
    // env_http_headers maps header names to env var *names*; never secret.
    if path.iter().any(|segment| segment == "env_http_headers") {
        return false;
    }
    if scalar.is_string() && is_secret_leaf_name(last) {
        return true;
    }
    // Any value nested below a secret map: env.X, http_headers.X, ...
    for (index, segment) in path.iter().enumerate() {
        if index + 1 >= path.len() {
            break;
        }
        if SECRET_MAP_KEYS.contains(&segment.as_str()) {
            return true;
        }
        if segment == "shell_environment_policy" && path.get(index + 1).map(String::as_str) == Some("set")
            && index + 2 < path.len()
        {
            return true;
        }
    }
    false
}

/// Redact secret scalars in a config-shaped value whose root sits at `path`.
pub fn redact_config_value(value: &mut Value, path: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                path.push(key.clone());
                redact_config_value(child, path);
                path.pop();
            }
        }
        Value::Array(items) => {
            // Arrays (args, header lists) inherit the parent's secrecy.
            for item in items.iter_mut() {
                redact_config_value(item, path);
            }
        }
        Value::Null => {}
        scalar => {
            if is_secret_config_path(path, scalar) {
                *scalar = Value::String(REDACTED.to_string());
            }
        }
    }
}

fn split_key_path(key_path: &str) -> Vec<String> {
    key_path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

/// Redact an allowlisted method's result before it leaves Cutex.
pub fn redact_result(method: &str, params: &Value, mut result: Value) -> Value {
    match method {
        "config/read" => {
            if let Some(config) = result.get_mut("config") {
                redact_config_value(config, &mut Vec::new());
            }
            if let Some(Value::Array(layers)) = result.get_mut("layers") {
                for layer in layers {
                    if let Some(config) = layer.get_mut("config") {
                        redact_config_value(config, &mut Vec::new());
                    }
                }
            }
            // origins: keyPath -> metadata; keys are not secret.
        }
        "config/value/write" | "config/batchWrite" => {
            if let Some(effective) = result.pointer_mut("/overriddenMetadata/effectiveValue") {
                let key_path = params
                    .get("keyPath")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        params
                            .pointer("/edits/0/keyPath")
                            .and_then(Value::as_str)
                            .filter(|_| params["edits"].as_array().is_some_and(|e| e.len() == 1))
                    });
                match key_path {
                    Some(key_path) => redact_config_value(effective, &mut split_key_path(key_path)),
                    None => redact_all_scalars(effective),
                }
            }
        }
        "mcpServerStatus/list" => {
            if let Some(Value::Array(servers)) = result.get_mut("data") {
                for server in servers {
                    if let Value::Object(map) = server {
                        for (key, child) in map.iter_mut() {
                            // Tool / resource schemas are not config; only
                            // redact config-like fields on the server entry.
                            if matches!(
                                key.as_str(),
                                "tools" | "resources" | "resourceTemplates" | "serverCapabilities" | "serverInfo"
                            ) {
                                continue;
                            }
                            let mut path = vec![key.clone()];
                            redact_config_value(child, &mut path);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    result
}

fn redact_all_scalars(value: &mut Value) {
    match value {
        Value::Object(map) => map.values_mut().for_each(redact_all_scalars),
        Value::Array(items) => items.iter_mut().for_each(redact_all_scalars),
        Value::Null => {}
        scalar => *scalar = Value::String(REDACTED.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Warm app-server host
// ---------------------------------------------------------------------------

pub struct SpawnedEndpoint {
    pub endpoint: Box<dyn CatalogEndpoint>,
    pub codex_home: PathBuf,
}

pub trait CodexEndpointFactory: Send + Sync {
    /// Spawn and fully initialize (initialize + initialized) an app-server.
    fn spawn(&self) -> Result<SpawnedEndpoint, CatalogError>;
}

struct WarmEndpoint {
    spawned: SpawnedEndpoint,
    generation: u64,
    keep_until: Instant,
}

pub struct CodexConfigHost {
    factory: Box<dyn CodexEndpointFactory>,
    keep_warm: Duration,
    state: Mutex<HostState>,
}

#[derive(Default)]
struct HostState {
    warm: Option<WarmEndpoint>,
    next_generation: u64,
}

#[derive(Debug)]
pub struct CodexCallOutcome {
    pub codex_home: PathBuf,
    pub result: Result<Value, CatalogError>,
}

impl CodexConfigHost {
    pub fn new(factory: Box<dyn CodexEndpointFactory>, keep_warm: Duration) -> Self {
        Self {
            factory,
            keep_warm,
            state: Mutex::new(HostState::default()),
        }
    }

    /// Forward one request. Returns `Err` only when no app-server could be
    /// started; native errors and transport failures are in the outcome.
    /// The second value is a generation to reap when a new process started.
    pub fn call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(CodexCallOutcome, Option<u64>), CatalogError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut started = None;
        if state.warm.is_none() {
            let spawned = self.factory.spawn()?;
            state.next_generation += 1;
            let generation = state.next_generation;
            state.warm = Some(WarmEndpoint {
                spawned,
                generation,
                keep_until: Instant::now() + self.keep_warm,
            });
            started = Some(generation);
        }
        let warm = state.warm.as_mut().expect("warm endpoint present");
        let oauth_keep = (method == "mcpServer/oauth/login").then(|| {
            params
                .get("timeoutSecs")
                .and_then(Value::as_u64)
                .map(|secs| Duration::from_secs(secs.saturating_add(30)))
                .unwrap_or(OAUTH_DEFAULT_KEEP)
                .min(OAUTH_MAX_KEEP)
        });
        let result = warm.spawned.endpoint.request(method, params);
        let codex_home = warm.spawned.codex_home.clone();
        match &result {
            Ok(_) | Err(CatalogError::Rpc { .. }) => {
                let now = Instant::now();
                let mut keep_until = now + self.keep_warm;
                if let (Some(keep), Ok(_)) = (oauth_keep, &result) {
                    // Keep the OAuth callback listener alive for the login.
                    keep_until = keep_until.max(now + keep);
                }
                warm.keep_until = warm.keep_until.max(keep_until);
            }
            Err(_) => {
                // Transport is unusable after any non-RPC failure.
                state.warm = None;
            }
        }
        Ok((CodexCallOutcome { codex_home, result }, started))
    }

    /// Stop the process when idle. Returns false once `generation` is gone.
    pub fn reap(&self, generation: u64, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match &state.warm {
            Some(warm) if warm.generation == generation => {
                if now >= warm.keep_until {
                    state.warm = None;
                    false
                } else {
                    true
                }
            }
            _ => false,
        }
    }

    /// Stop the warm process now (if any).
    pub fn shutdown(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .warm = None;
    }

    pub fn is_warm(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .warm
            .is_some()
    }
}

pub fn spawn_reaper(host: Arc<CodexConfigHost>, generation: u64) {
    std::thread::spawn(move || loop {
        std::thread::sleep(REAPER_INTERVAL);
        if !host.reap(generation, Instant::now()) {
            break;
        }
    });
}

/// Real factory: the selected runtime bundle's app-server on its native home.
struct RuntimeEndpointFactory {
    /// Test-only override: (program, args, codex_home).
    explicit: Option<(PathBuf, Vec<String>, PathBuf)>,
}

impl RuntimeEndpointFactory {
    fn resolve(&self) -> anyhow::Result<(PathBuf, Vec<String>, PathBuf)> {
        if let Some(explicit) = &self.explicit {
            return Ok(explicit.clone());
        }
        resolve_runtime()
    }
}

/// The selected runtime bundle's app-server (program, transport args, shared
/// native home); falls back to the host codex program and host CODEX_HOME
/// when no local deployment is selected.
pub(crate) fn resolve_runtime() -> anyhow::Result<(PathBuf, Vec<String>, PathBuf)> {
    use anyhow::Context;
    if let Some(deployment) = crate::launch::local_deployment::LocalDeployment::selected()? {
        let bundle: crate::launch::stock::StockBundle =
            serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)
                .context("invalid selected runtime bundle manifest")?;
        bundle.executable.validate()?;
        Ok((
            bundle.executable.path.clone(),
            vec!["--listen".to_string(), "stdio://".to_string()],
            deployment.native_home,
        ))
    } else {
        Ok((
            PathBuf::from(crate::launch::program::codex_program()),
            vec!["app-server".to_string(), "--stdio".to_string()],
            crate::config::paths::host_codex_home_dir()?,
        ))
    }
}

/// OS context only: no Cutex credentials, profile auth or overrides.
const CHILD_ENV_KEYS: &[&str] = &[
    "PATH",
    // Instance selection (docs/INSTANCES.md), not a credential.
    "CUTEX_HOME",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "SYSTEMROOT",
    "SystemDrive",
    "WINDIR",
    "COMSPEC",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TEMP",
    "TMP",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// A cleared-environment app-server command rooted at `codex_home`.
pub(crate) fn base_command(program: &std::path::Path, codex_home: &std::path::Path) -> Command {
    let mut command = Command::new(program);
    command.env_clear().current_dir(codex_home);
    for key in CHILD_ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

impl CodexEndpointFactory for RuntimeEndpointFactory {
    fn spawn(&self) -> Result<SpawnedEndpoint, CatalogError> {
        let (program, args, codex_home) = self
            .resolve()
            .map_err(|error| CatalogError::Launch(format!("no codex runtime available: {error:#}")))?;
        if !codex_home.is_dir() {
            return Err(CatalogError::Launch(format!(
                "shared CODEX_HOME does not exist: {}",
                codex_home.display()
            )));
        }
        let mut command = base_command(&program, &codex_home);
        for option in CONFIG_APP_SERVER_OVERRIDES {
            command.args(["-c", option]);
        }
        command.args(&args);
        let mut options = StdioAppServerOptions::new(program.into_os_string(), codex_home.clone());
        options.request_timeout = REQUEST_TIMEOUT;
        options.shutdown_timeout = Duration::from_secs(2);
        let mut endpoint = OwnedStdioEndpoint::spawn_command(options, command)?;
        let initialized = endpoint.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "cutex_codex_config",
                    "title": "Cutex codex config",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            }),
        )?;
        let reported = initialized
            .get("codexHome")
            .and_then(Value::as_str)
            .and_then(|home| std::path::Path::new(home).canonicalize().ok());
        if let Some(reported) = reported {
            if Some(reported.clone()) != codex_home.canonicalize().ok() {
                return Err(CatalogError::ProviderIncompatible(format!(
                    "app-server reported CODEX_HOME {} instead of {}",
                    reported.display(),
                    codex_home.display()
                )));
            }
        }
        endpoint.notify("initialized", None)?;
        Ok(SpawnedEndpoint {
            endpoint: Box::new(endpoint),
            codex_home,
        })
    }
}

static HOST: OnceLock<Arc<CodexConfigHost>> = OnceLock::new();

fn host() -> Arc<CodexConfigHost> {
    HOST.get_or_init(|| Arc::new(CodexConfigHost::new(Box::new(RuntimeEndpointFactory { explicit: None }), KEEP_WARM)))
        .clone()
}

/// Stop the warm config app-server (if any) so the next request re-scans the
/// shared home (skills archive moves skill directories behind its back).
pub(crate) fn stop_warm_host() {
    if let Some(host) = HOST.get() {
        host.shutdown();
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn display_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

pub fn is_codex_requests_path(path: &str) -> bool {
    path == CODEX_REQUESTS_PATH
}

/// Build the HTTP status + body for one validated request.
pub fn respond(
    host: &Arc<CodexConfigHost>,
    request: &ValidatedCodexRequest,
) -> (u16, &'static str, Value, bool) {
    let (outcome, started) = match host.call(&request.method, request.params.clone()) {
        Ok(value) => value,
        Err(error) => {
            return (
                503,
                "Service Unavailable",
                json!({
                    "code": "codex_app_server_unavailable",
                    "message": "the codex app-server for the shared CODEX_HOME could not be started",
                    "details": { "diagnostic": error.to_string() },
                    "retryable": true,
                }),
                true,
            )
        }
    };
    if let Some(generation) = started {
        spawn_reaper(Arc::clone(host), generation);
    }
    let base = |extra: (&str, Value)| {
        let mut body = json!({
            "contractVersion": 2,
            "requestId": request.request_id,
            "method": request.method,
            "codexHome": display_path(&outcome.codex_home),
        });
        body[extra.0] = extra.1;
        body
    };
    match outcome.result {
        Ok(result) => (
            200,
            "OK",
            base(("result", redact_result(&request.method, &request.params, result))),
            false,
        ),
        Err(CatalogError::Rpc { error, .. }) => (
            200,
            "OK",
            base((
                "error",
                json!({ "code": error.code, "message": error.message, "data": error.data }),
            )),
            false,
        ),
        Err(CatalogError::Timeout { method }) => (
            504,
            "Gateway Timeout",
            json!({
                "code": "codex_app_server_timeout",
                "message": format!("timed out waiting for app-server {method}; the outcome is unknown"),
                "details": {},
                "retryable": false,
            }),
            true,
        ),
        Err(error) => (
            502,
            "Bad Gateway",
            json!({
                "code": "codex_app_server_failed",
                "message": "the codex app-server failed; it will be restarted on the next request",
                "details": { "diagnostic": error.to_string() },
                "retryable": true,
            }),
            true,
        ),
    }
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "POST" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use POST",
            false,
            json!({}),
        );
    }
    let validated = match validate_codex_request(&request.body) {
        Ok(validated) => validated,
        Err(CodexRequestError::Invalid(message)) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &message,
                false,
                json!({}),
            )
        }
        Err(CodexRequestError::MethodDenied(method)) => {
            return super::server::write_v2_error(
                stream,
                403,
                "Forbidden",
                "method_denied",
                "this app-server method is not allowed on the codex config route",
                false,
                json!({ "method": method, "allowed": CODEX_CONFIG_METHODS }),
            )
        }
    };
    let (status, reason, body, is_error) = respond(&host(), &validated);
    if is_error {
        let code = body["code"].as_str().unwrap_or("codex_app_server_failed").to_string();
        let message = body["message"].as_str().unwrap_or_default().to_string();
        let retryable = body["retryable"].as_bool().unwrap_or(false);
        return super::server::write_v2_error(
            stream,
            status,
            reason,
            &code,
            &message,
            retryable,
            body["details"].clone(),
        );
    }
    write_json_response(stream, status, reason, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::protocol::RpcError;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    fn body(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn allowlist_and_envelope_are_strict() {
        let ok = validate_codex_request(&body(json!({
            "requestId": "r1", "method": "config/read", "params": { "includeLayers": true }
        })))
        .unwrap();
        assert_eq!(ok.method, "config/read");
        assert_eq!(ok.params, json!({ "includeLayers": true }));

        for method in ["thread/start", "turn/start", "account/read", "plugin/share/save", "plugin/skill/read", "plugin/reconcile", "plugin/installed", "cutex/x"] {
            assert_eq!(
                validate_codex_request(&body(json!({ "requestId": "r", "method": method }))),
                Err(CodexRequestError::MethodDenied(method.to_string())),
            );
        }
        for bad in [
            json!({ "method": "config/read" }),
            json!({ "requestId": "", "method": "config/read" }),
            json!({ "requestId": "r", "method": "config/read", "extra": 1 }),
            json!({ "requestId": "r", "method": "config/read", "profile": "GLM" }),
            json!({ "requestId": "r", "method": "config/read", "params": [] }),
            json!({ "requestId": "r", "method": "config/mcpServer/reload", "params": {} }),
        ] {
            assert!(
                matches!(validate_codex_request(&body(bad.clone())), Err(CodexRequestError::Invalid(_))),
                "{bad}"
            );
        }
        let reload = validate_codex_request(&body(json!({
            "requestId": "r", "method": "config/mcpServer/reload"
        })))
        .unwrap();
        assert_eq!(reload.params, Value::Null);
        // Absent params default to {} for object-param methods.
        let skills = validate_codex_request(&body(json!({ "requestId": "r", "method": "skills/list" })))
            .unwrap();
        assert_eq!(skills.params, json!({}));
        let hooks = validate_codex_request(&body(json!({
            "requestId": "r", "method": "hooks/list", "params": null
        })))
        .unwrap();
        assert_eq!(hooks.params, json!({}));
    }

    #[test]
    fn every_allowlisted_method_is_in_the_pinned_schema() {
        let minimal = |method: &str| match method {
            "config/value/write" => json!({ "keyPath": "a", "value": 1, "mergeStrategy": "replace" }),
            "config/batchWrite" => json!({ "edits": [] }),
            "config/mcpServer/reload" => Value::Null,
            "mcpServer/oauth/login" => json!({ "name": "x" }),
            "skills/config/write" => json!({ "enabled": true, "name": "x" }),
            "skills/extraRoots/set" => json!({ "extraRoots": [] }),
            "marketplace/add" => json!({ "source": "owner/repo" }),
            "marketplace/remove" => json!({ "marketplaceName": "m" }),
            "plugin/read" | "plugin/install" => json!({ "pluginName": "p" }),
            "plugin/uninstall" => json!({ "pluginId": "p@m" }),
            "plugin/search" => json!({ "searchTerm": "git" }),
            _ => json!({}),
        };
        for method in CODEX_CONFIG_METHODS {
            let mut request = json!({ "requestId": "r", "method": method });
            let params = minimal(method);
            if !params.is_null() {
                request["params"] = params;
            }
            validate_codex_request(&body(request)).unwrap_or_else(|error| panic!("{method}: {error:?}"));
        }
    }

    #[test]
    fn plugin_methods_are_global_and_validated() {
        for method in UNPINNED_METHODS {
            assert!(CODEX_CONFIG_METHODS.contains(method));
        }
        let ok = validate_codex_request(&body(json!({
            "requestId": "r", "method": "plugin/list",
            "params": { "marketplaceKinds": ["local"], "forceRefetch": true, "cwds": null }
        })))
        .unwrap();
        assert_eq!(ok.method, "plugin/list");
        let search = validate_codex_request(&body(json!({
            "requestId": "r", "method": "plugin/search", "params": { "searchTerm": "git", "limit": 20 }
        })))
        .unwrap();
        assert_eq!(search.params["searchTerm"], "git");
        for bad in [
            json!({ "requestId": "r", "method": "plugin/list", "params": { "cwds": ["/repo"] } }),
            json!({ "requestId": "r", "method": "plugin/search", "params": { "searchTerm": "x", "cwds": ["/r"] } }),
            json!({ "requestId": "r", "method": "plugin/search", "params": {} }),
            json!({ "requestId": "r", "method": "plugin/search", "params": [] }),
            json!({ "requestId": "r", "method": "plugin/install", "params": {} }),
            json!({ "requestId": "r", "method": "plugin/uninstall", "params": { "pluginId": 3 } }),
            json!({ "requestId": "r", "method": "marketplace/add", "params": {} }),
            json!({ "requestId": "r", "method": "marketplace/remove", "params": {} }),
        ] {
            assert!(
                matches!(validate_codex_request(&body(bad.clone())), Err(CodexRequestError::Invalid(_))),
                "{bad}"
            );
        }
        assert_eq!(CONFIG_APP_SERVER_OVERRIDES, &["features.remote_plugin=false"]);
    }

    #[test]
    fn params_are_schema_validated_and_fenced() {
        for bad in [
            // schema: missing required / wrong types
            json!({ "requestId": "r", "method": "config/value/write", "params": { "keyPath": "a", "value": 1 } }),
            json!({ "requestId": "r", "method": "config/value/write",
                    "params": { "keyPath": "a", "value": 1, "mergeStrategy": "nope" } }),
            json!({ "requestId": "r", "method": "skills/config/write", "params": { "name": "x" } }),
            json!({ "requestId": "r", "method": "skills/extraRoots/set", "params": { "extraRoots": "x" } }),
            json!({ "requestId": "r", "method": "mcpServer/oauth/login", "params": {} }),
            json!({ "requestId": "r", "method": "config/read", "params": { "includeLayers": "yes" } }),
            // fences
            json!({ "requestId": "r", "method": "config/value/write",
                    "params": { "keyPath": "a", "value": 1, "mergeStrategy": "replace", "filePath": "C:/x.toml" } }),
            json!({ "requestId": "r", "method": "config/batchWrite",
                    "params": { "edits": [], "filePath": "/etc/x" } }),
            json!({ "requestId": "r", "method": "mcpServerStatus/list", "params": { "threadId": "t" } }),
            json!({ "requestId": "r", "method": "mcpServer/oauth/login", "params": { "name": "x", "threadId": "t" } }),
            json!({ "requestId": "r", "method": "config/value/write",
                    "params": { "keyPath": "mcp_servers.x", "mergeStrategy": "upsert",
                                "value": { "env": { "TOKEN": "<redacted>" } } } }),
            json!({ "requestId": "r", "method": "config/batchWrite",
                    "params": { "edits": [{ "keyPath": "mcp_servers.x.bearer_token",
                                            "value": "<redacted>", "mergeStrategy": "replace" }] } }),
        ] {
            assert!(
                matches!(validate_codex_request(&body(bad.clone())), Err(CodexRequestError::Invalid(_))),
                "{bad}"
            );
        }
        // Null filePath / threadId are fine; newer optional fields pass the open schema.
        validate_codex_request(&body(json!({ "requestId": "r", "method": "mcpServer/oauth/login",
            "params": { "name": "x", "threadId": null, "clientRegistration": null, "scopes": ["a"] } })))
        .unwrap();
        validate_codex_request(&body(json!({ "requestId": "r", "method": "config/value/write",
            "params": { "keyPath": "mcp_servers.x.env.TOKEN", "value": "new-secret",
                        "mergeStrategy": "replace", "filePath": null, "expectedVersion": "v1" } })))
        .unwrap();
    }

    #[test]
    fn config_read_redacts_secrets_but_keeps_keys() {
        let result = json!({
            "config": {
                "model": "gpt-5",
                "model_auto_compact_token_limit": 1000,
                "features": { "api_key_model_discovery": true },
                "mcp_servers": {
                    "gh": { "command": "npx", "args": ["-y", "gh-mcp"],
                            "env": { "GITHUB_TOKEN": "ghp_secret", "MODE": "x" },
                            "env_vars": ["PATH"] },
                    "web": { "url": "https://x", "bearer_token": "tok",
                             "bearer_token_env_var": "WEB_TOKEN",
                             "http_headers": { "Authorization": "Bearer abc" },
                             "env_http_headers": { "X-Key": "WEB_KEY", "Authorization": "WEB_AUTH" } }
                },
                "model_providers": { "p": { "experimental_bearer_token": "sk-1",
                                            "query_params": { "api-key": "k" }, "base_url": "https://p" } },
                "shell_environment_policy": { "set": { "SECRET": "v" }, "inherit": "all" }
            },
            "origins": { "mcp_servers.gh.env.GITHUB_TOKEN": { "name": { "type": "user" }, "version": "1" } },
            "layers": [{ "name": { "type": "user" }, "version": "1", "disabledReason": null,
                         "config": { "mcp_servers": { "gh": { "env": { "GITHUB_TOKEN": "ghp_secret" } } } } }]
        });
        let out = redact_result("config/read", &json!({}), result);
        let text = out.to_string();
        for secret in ["ghp_secret", "\"tok\"", "Bearer abc", "sk-1", "\"k\"", "\"v\""] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        let gh = &out["config"]["mcp_servers"]["gh"];
        assert_eq!(gh["env"]["GITHUB_TOKEN"], REDACTED);
        assert_eq!(gh["env"]["MODE"], REDACTED);
        assert_eq!(gh["command"], "npx");
        assert_eq!(gh["args"], json!(["-y", "gh-mcp"]));
        assert_eq!(gh["env_vars"], json!(["PATH"]));
        let web = &out["config"]["mcp_servers"]["web"];
        assert_eq!(web["bearer_token"], REDACTED);
        assert_eq!(web["bearer_token_env_var"], "WEB_TOKEN");
        assert_eq!(web["http_headers"]["Authorization"], REDACTED);
        assert_eq!(web["env_http_headers"]["X-Key"], "WEB_KEY");
        assert_eq!(web["env_http_headers"]["Authorization"], "WEB_AUTH");
        assert_eq!(out["config"]["model"], "gpt-5");
        assert_eq!(out["config"]["model_auto_compact_token_limit"], 1000);
        assert_eq!(out["config"]["features"]["api_key_model_discovery"], true);
        assert_eq!(out["config"]["shell_environment_policy"]["inherit"], "all");
        assert_eq!(out["layers"][0]["config"]["mcp_servers"]["gh"]["env"]["GITHUB_TOKEN"], REDACTED);
        assert!(out["origins"].get("mcp_servers.gh.env.GITHUB_TOKEN").is_some());
    }

    #[test]
    fn write_and_status_results_are_redacted_in_context() {
        let write = redact_result(
            "config/value/write",
            &json!({ "keyPath": "mcp_servers.gh.env" }),
            json!({ "status": "okOverridden", "version": "2", "filePath": "/h/config.toml",
                    "overriddenMetadata": { "message": "m", "overridingLayer": {},
                                            "effectiveValue": { "GITHUB_TOKEN": "ghp" } } }),
        );
        assert_eq!(write["overriddenMetadata"]["effectiveValue"]["GITHUB_TOKEN"], REDACTED);
        let model = redact_result(
            "config/value/write",
            &json!({ "keyPath": "model" }),
            json!({ "overriddenMetadata": { "effectiveValue": "gpt-5" } }),
        );
        assert_eq!(model["overriddenMetadata"]["effectiveValue"], "gpt-5");
        let batch = redact_result(
            "config/batchWrite",
            &json!({ "edits": [{ "keyPath": "a" }, { "keyPath": "b" }] }),
            json!({ "overriddenMetadata": { "effectiveValue": { "x": "y" } } }),
        );
        assert_eq!(batch["overriddenMetadata"]["effectiveValue"]["x"], REDACTED);

        let status = redact_result(
            "mcpServerStatus/list",
            &json!({}),
            json!({ "data": [{ "name": "web", "httpOrigin": "https://x",
                                "env": { "K": "secret" }, "http_headers": { "Authorization": "x" },
                                "tools": { "t": { "inputSchema": { "properties": {
                                    "headers": { "type": "object" }, "api_key": { "type": "string" } } } } } }],
                    "nextCursor": null }),
        );
        let server = &status["data"][0];
        assert_eq!(server["env"]["K"], REDACTED);
        assert_eq!(server["http_headers"]["Authorization"], REDACTED);
        assert_eq!(server["httpOrigin"], "https://x");
        assert_eq!(
            server["tools"]["t"]["inputSchema"]["properties"]["headers"]["type"],
            "object"
        );
        // skills/hooks results pass through unchanged.
        let skills = json!({ "data": [{ "cwd": "/x", "skills": [], "errors": [] }] });
        assert_eq!(redact_result("skills/list", &json!({}), skills.clone()), skills);
    }

    /// Live smoke test against a real app-server binary and a throwaway home:
    /// CUTEX_CODEX_CONFIG_SMOKE_BIN=<codex-app-server> cargo test -- --ignored live_app_server
    #[test]
    #[ignore]
    fn live_app_server_smoke() {
        let program = PathBuf::from(std::env::var("CUTEX_CODEX_CONFIG_SMOKE_BIN").unwrap());
        let home = std::env::temp_dir().join(format!("cutex-codexcfg-smoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let factory = RuntimeEndpointFactory {
            explicit: Some((program, vec!["--listen".into(), "stdio://".into()], home.clone())),
        };
        let host = Arc::new(CodexConfigHost::new(Box::new(factory), Duration::from_secs(5)));
        let run = |value: Value| {
            let (status, _, body, _) = respond(&host, &validated(value));
            eprintln!("{status} {body:#}");
            assert_eq!(status, 200, "{body}");
            body
        };
        let write = run(json!({ "requestId": "w", "method": "config/value/write", "params": {
            "keyPath": "mcp_servers.smoke", "mergeStrategy": "upsert",
            "value": { "command": "smoke-mcp-does-not-exist", "env": { "SMOKE_TOKEN": "s3cret-smoke" } } } }));
        assert!(write.get("result").is_some(), "{write}");
        let read = run(json!({ "requestId": "r", "method": "config/read", "params": { "includeLayers": true } }));
        assert!(!read.to_string().contains("s3cret-smoke"));
        assert_eq!(read["result"]["config"]["mcp_servers"]["smoke"]["env"]["SMOKE_TOKEN"], REDACTED);
        assert!(std::fs::read_to_string(home.join("config.toml")).unwrap().contains("s3cret-smoke"));
        run(json!({ "requestId": "s", "method": "skills/list", "params": { "cwds": [home] } }));
        run(json!({ "requestId": "h", "method": "hooks/list", "params": { "cwds": [home] } }));
        run(json!({ "requestId": "m", "method": "config/mcpServer/reload" }));
        run(json!({ "requestId": "l", "method": "mcpServerStatus/list", "params": { "detail": "toolsAndAuthOnly" } }));
        drop(host);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Plugin round trip on a real app-server (temp shared home, local
    /// marketplace), then agent homes launched with the
    /// `launch::shared_config` overlay: a fresh one sees the installed
    /// plugin's skill, a Settings MCP server (secret through process env) and
    /// a Settings skill toggle; an "adopted" one (config copied from shared)
    /// follows the server's removal and the toggle's revert, keeping its own
    /// server. `CUTEX_CODEX_CONFIG_SMOKE_BIN` = a bundle app-server executable.
    #[test]
    #[ignore]
    fn live_plugins_smoke() {
        let program = PathBuf::from(std::env::var("CUTEX_CODEX_CONFIG_SMOKE_BIN").unwrap());
        let root = std::env::temp_dir().join(format!("cutex-plugins-smoke-{}", uuid::Uuid::new_v4()));
        let (shared, agent, source) = (root.join("shared"), root.join("agent"), root.join("market"));
        let adopted = root.join("adopted");
        for dir in [&shared, &agent, &adopted, &source.join(".agents/plugins"), &source.join("plugins/sample/.codex-plugin"), &source.join("plugins/sample/skills/hello")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(source.join(".agents/plugins/marketplace.json"),
            r#"{"name":"smoke","plugins":[{"name":"sample","source":{"source":"local","path":"./plugins/sample"}}]}"#).unwrap();
        std::fs::write(source.join("plugins/sample/.codex-plugin/plugin.json"),
            r#"{"name":"sample","description":"Smoke plugin","skills":"./skills"}"#).unwrap();
        std::fs::write(source.join("plugins/sample/skills/hello/SKILL.md"),
            "---\nname: hello\ndescription: Says hello for the smoke test.\n---\nSay hello.\n").unwrap();
        let shared = shared.canonicalize().unwrap();
        let agent = agent.canonicalize().unwrap();
        let adopted = adopted.canonicalize().unwrap();
        let host_for = |home: &PathBuf, extra: Vec<String>| {
            let mut args = extra;
            args.extend(["--listen".to_string(), "stdio://".to_string()]);
            Arc::new(CodexConfigHost::new(
                Box::new(RuntimeEndpointFactory { explicit: Some((program.clone(), args, home.clone())) }),
                Duration::from_secs(5),
            ))
        };
        let host = host_for(&shared, vec![]);
        let run = |host: &Arc<CodexConfigHost>, value: Value| {
            let (status, _, body, _) = respond(host, &validated(value));
            eprintln!("{status} {body:#}");
            assert_eq!(status, 200, "{body}");
            assert!(body.get("error").is_none(), "{body}");
            body["result"].clone()
        };
        let added = run(&host, json!({ "requestId": "a", "method": "marketplace/add",
            "params": { "source": source.to_string_lossy() } }));
        assert_eq!(added["marketplaceName"], "smoke");
        let list = run(&host, json!({ "requestId": "l", "method": "plugin/list", "params": {} }));
        let market = list["marketplaces"].as_array().unwrap().iter()
            .find(|m| m["name"] == "smoke").cloned().expect("smoke marketplace listed");
        let path = market["path"].clone();
        let read = run(&host, json!({ "requestId": "r", "method": "plugin/read",
            "params": { "marketplacePath": path, "pluginName": "sample" } }));
        assert_eq!(read["plugin"]["skills"][0]["name"].as_str().map(|n| n.ends_with("hello")), Some(true));
        run(&host, json!({ "requestId": "i", "method": "plugin/install",
            "params": { "marketplacePath": path, "pluginName": "sample" } }));
        let config = std::fs::read_to_string(shared.join("config.toml")).unwrap();
        assert!(config.contains("sample@smoke"), "{config}");
        run(&host, json!({ "requestId": "s", "method": "plugin/search", "params": { "searchTerm": "sample" } }));

        // Settings edits on the shared home: an MCP server with a secret and
        // a skill toggle (the plugin's skill, by name).
        let hello = |skills: &Value| -> (String, bool) {
            skills["data"].as_array().unwrap().iter()
                .flat_map(|entry| entry["skills"].as_array().cloned().unwrap_or_default())
                .find(|skill| skill["name"].as_str().is_some_and(|name| name.ends_with("hello")))
                .map(|skill| (skill["name"].as_str().unwrap().to_string(), skill["enabled"].as_bool().unwrap()))
                .unwrap_or_else(|| panic!("hello skill listed: {skills}"))
        };
        let listed = run(&host, json!({ "requestId": "k0", "method": "skills/list",
            "params": { "cwds": [shared], "forceReload": true } }));
        let (skill_name, _) = hello(&listed);
        run(&host, json!({ "requestId": "w", "method": "config/value/write", "params": {
            "keyPath": "mcp_servers.smoke_shared", "mergeStrategy": "upsert",
            "value": { "command": "smoke-mcp-does-not-exist", "env": { "SMOKE_TOKEN": "s3cret-smoke" } } } }));
        run(&host, json!({ "requestId": "t", "method": "skills/config/write",
            "params": { "name": skill_name, "enabled": false } }));
        let shared_config = std::fs::read_to_string(shared.join("config.toml")).unwrap();
        assert!(shared_config.contains("smoke_shared") && shared_config.contains("skills"), "{shared_config}");
        let none = std::collections::BTreeSet::new();
        let overlay = |home: &PathBuf| {
            let injection = crate::launch::shared_config::injection_for_homes(home, &shared, &[], &none).unwrap();
            assert!(!injection.args().join(" ").contains("s3cret-smoke"), "{:?}", injection.args());
            injection
        };
        let effective = |home: &PathBuf, injection: &crate::launch::scoped_mcp::LaunchInjection| {
            let agent_host = host_for(home, injection.args());
            let config = run(&agent_host, json!({ "requestId": "c", "method": "config/read" }));
            let skills = run(&agent_host, json!({ "requestId": "k", "method": "skills/list",
                "params": { "cwds": [home], "forceReload": true } }));
            drop(agent_host);
            (config["config"].clone(), skills)
        };

        // Fresh agent home: own (empty) config, everything via the overlay.
        let injection = overlay(&agent);
        assert_eq!(injection.envs, [("SMOKE_TOKEN".to_string(), "s3cret-smoke".to_string())]);
        let (config, skills) = effective(&agent, &injection);
        assert!(skills.to_string().contains("sample@smoke"), "{skills}");
        assert_eq!(hello(&skills).1, false, "shared skill toggle reached the agent");
        assert_eq!(config["mcp_servers"]["smoke_shared"]["command"], "smoke-mcp-does-not-exist");
        assert_eq!(config["mcp_servers"]["smoke_shared"]["env_vars"], json!(["SMOKE_TOKEN"]));

        // Adopted agent home: the shared config as copied at adoption plus
        // its own server. First launch records the baseline; nothing differs.
        std::fs::write(adopted.join("config.toml"), format!(
            "{shared_config}\n[mcp_servers.agent_own]\ncommand = \"own-mcp-does-not-exist\"\n")).unwrap();
        let injection = overlay(&adopted);
        assert!(injection.options.is_empty(), "{:?}", injection.args());
        assert!(adopted.join(crate::launch::shared_config::BASELINE_FILE).is_file());
        // Settings then removes the server and turns the skill back on.
        run(&host, json!({ "requestId": "w2", "method": "config/value/write", "params": {
            "keyPath": "mcp_servers.smoke_shared", "mergeStrategy": "replace", "value": null } }));
        run(&host, json!({ "requestId": "t2", "method": "skills/config/write",
            "params": { "name": skill_name, "enabled": true } }));
        assert!(!std::fs::read_to_string(shared.join("config.toml")).unwrap().contains("smoke_shared"));
        let injection = overlay(&adopted);
        let (config, skills) = effective(&adopted, &injection);
        assert_eq!(config["mcp_servers"]["smoke_shared"]["enabled"], false, "{config:#}");
        assert_eq!(config["mcp_servers"]["agent_own"]["command"], "own-mcp-does-not-exist");
        assert_eq!(hello(&skills).1, true, "skill toggle revert reached the agent");

        run(&host, json!({ "requestId": "u", "method": "plugin/uninstall", "params": { "pluginId": "sample@smoke" } }));
        run(&host, json!({ "requestId": "x", "method": "marketplace/remove", "params": { "marketplaceName": "smoke" } }));
        assert!(!std::fs::read_to_string(shared.join("config.toml")).unwrap().contains("sample@smoke"));
        drop(host);
        let _ = std::fs::remove_dir(agent.join("plugins"));
        let _ = std::fs::remove_dir(adopted.join("plugins"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Scoped MCP launch overrides are accepted by a real app-server.
    #[test]
    #[ignore]
    fn live_scoped_mcp_overrides_smoke() {
        use crate::launch::scoped_mcp::{render_launch, ScopeEntry};
        let program = PathBuf::from(std::env::var("CUTEX_CODEX_CONFIG_SMOKE_BIN").unwrap());
        let home = std::env::temp_dir().join(format!("cutex-scoped-smoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "[mcp_servers.global_one]
command = \"g\"
").unwrap();
        let home = home.canonicalize().unwrap();
        let agent = ScopeEntry {
            servers: [
                ("gh".to_string(), json!({ "command": "gh-mcp", "env": { "GH_TOKEN": "s3cret" } })),
                ("web".to_string(), json!({ "url": "https://example.invalid/mcp",
                                             "http_headers": { "Authorization": "Bearer x" } })),
            ].into(),
            disabled: vec!["global_one".into()],
            ..ScopeEntry::default()
        };
        let injection = render_launch(None, Some(&agent)).unwrap();
        let mut args = injection.args();
        args.extend(["--listen".to_string(), "stdio://".to_string()]);
        let factory = RuntimeEndpointFactory { explicit: Some((program, args, home.clone())) };
        let host = Arc::new(CodexConfigHost::new(Box::new(factory), Duration::from_secs(5)));
        let (status, _, body, _) = respond(&host, &validated(json!({ "requestId": "r", "method": "config/read" })));
        eprintln!("{status} {:#}", body["result"]["config"]["mcp_servers"]);
        assert_eq!(status, 200, "{body}");
        let servers = &body["result"]["config"]["mcp_servers"];
        assert_eq!(servers["global_one"]["enabled"], false);
        assert_eq!(servers["gh"]["env_vars"], json!(["GH_TOKEN"]));
        assert_eq!(servers["web"]["env_http_headers"]["Authorization"], "CUTEX_MCP_HDR_1");
        drop(host);
        let _ = std::fs::remove_dir_all(&home);
    }

    // ---- fake app-server plumbing ----

    struct FakeEndpoint {
        log: Arc<Mutex<Vec<(String, Value)>>>,
        replies: Arc<Mutex<VecDeque<Result<Value, CatalogError>>>>,
    }

    impl CatalogEndpoint for FakeEndpoint {
        fn request(&mut self, method: &str, params: Value) -> Result<Value, CatalogError> {
            self.log.lock().unwrap().push((method.to_string(), params));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(json!({})))
        }
        fn notify(&mut self, _method: &str, _params: Option<Value>) -> Result<(), CatalogError> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeFactory {
        spawns: Arc<AtomicUsize>,
        fail: bool,
        log: Arc<Mutex<Vec<(String, Value)>>>,
        replies: Arc<Mutex<VecDeque<Result<Value, CatalogError>>>>,
    }

    impl CodexEndpointFactory for FakeFactory {
        fn spawn(&self) -> Result<SpawnedEndpoint, CatalogError> {
            if self.fail {
                return Err(CatalogError::Launch("no runtime".to_string()));
            }
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(SpawnedEndpoint {
                endpoint: Box::new(FakeEndpoint {
                    log: Arc::clone(&self.log),
                    replies: Arc::clone(&self.replies),
                }),
                codex_home: PathBuf::from("/fake/codex-home"),
            })
        }
    }

    fn validated(value: Value) -> ValidatedCodexRequest {
        validate_codex_request(&body(value)).unwrap()
    }

    #[test]
    fn requests_reuse_one_warm_process_and_redact_results() {
        let factory = FakeFactory::default();
        factory.replies.lock().unwrap().extend([
            Ok(json!({ "config": { "mcp_servers": { "gh": { "env": { "T": "s3cret" } } } },
                       "origins": {}, "layers": null })),
            Err(CatalogError::Rpc {
                method: "config/value/write".to_string(),
                error: RpcError { code: -32600, message: "version conflict".to_string(),
                                  data: Some(json!({ "configWriteErrorCode": "configVersionConflict" })) },
            }),
            Ok(json!({ "data": [] })),
        ]);
        let host = Arc::new(CodexConfigHost::new(Box::new(factory.clone()), Duration::from_secs(60)));

        let (status, _, body, is_error) = respond(
            &host,
            &validated(json!({ "requestId": "a", "method": "config/read" })),
        );
        assert_eq!((status, is_error), (200, false));
        assert_eq!(body["requestId"], "a");
        assert_eq!(body["codexHome"], "/fake/codex-home");
        assert_eq!(body["result"]["config"]["mcp_servers"]["gh"]["env"]["T"], REDACTED);

        let (status, _, body, _) = respond(
            &host,
            &validated(json!({ "requestId": "b", "method": "config/value/write",
                "params": { "keyPath": "model", "value": "m", "mergeStrategy": "replace",
                            "expectedVersion": "old" } })),
        );
        assert_eq!(status, 200);
        assert_eq!(body["error"]["code"], -32600);
        assert_eq!(body["error"]["data"]["configWriteErrorCode"], "configVersionConflict");
        assert!(body.get("result").is_none());

        let (status, _, _, _) = respond(
            &host,
            &validated(json!({ "requestId": "c", "method": "config/mcpServer/reload" })),
        );
        assert_eq!(status, 200);
        assert_eq!(factory.spawns.load(Ordering::SeqCst), 1, "native errors keep the process");
        let log = factory.log.lock().unwrap().clone();
        assert_eq!(log[0], ("config/read".to_string(), json!({})));
        assert_eq!(log[1].0, "config/value/write");
        assert_eq!(log[2], ("config/mcpServer/reload".to_string(), Value::Null));
    }

    #[test]
    fn transport_failures_restart_and_launch_failures_are_503() {
        let factory = FakeFactory::default();
        factory
            .replies
            .lock()
            .unwrap()
            .push_back(Err(CatalogError::Transport("closed".to_string())));
        let host = Arc::new(CodexConfigHost::new(Box::new(factory.clone()), Duration::from_secs(60)));
        let request = validated(json!({ "requestId": "a", "method": "hooks/list" }));
        let (status, _, body, is_error) = respond(&host, &request);
        assert_eq!((status, is_error), (502, true));
        assert_eq!(body["code"], "codex_app_server_failed");
        assert!(!host.is_warm());
        let (status, _, _, _) = respond(&host, &request);
        assert_eq!(status, 200);
        assert_eq!(factory.spawns.load(Ordering::SeqCst), 2);

        let failing = FakeFactory { fail: true, ..FakeFactory::default() };
        let host = Arc::new(CodexConfigHost::new(Box::new(failing), Duration::from_secs(60)));
        let (status, _, body, _) = respond(&host, &request);
        assert_eq!(status, 503);
        assert_eq!(body["code"], "codex_app_server_unavailable");
    }

    #[test]
    fn idle_process_is_reaped_but_oauth_login_keeps_it_alive() {
        let factory = FakeFactory::default();
        let host = CodexConfigHost::new(Box::new(factory.clone()), Duration::from_secs(60));
        let (_, started) = host.call("skills/list", json!({})).unwrap();
        let generation = started.unwrap();
        let now = Instant::now();
        assert!(host.reap(generation, now));
        assert!(!host.reap(generation, now + Duration::from_secs(61)));
        assert!(!host.is_warm());

        factory
            .replies
            .lock()
            .unwrap()
            .push_back(Ok(json!({ "authorizationUrl": "https://auth" })));
        let (_, started) = host
            .call("mcpServer/oauth/login", json!({ "name": "x", "timeoutSecs": 300 }))
            .unwrap();
        let generation = started.unwrap();
        let now = Instant::now();
        assert!(host.reap(generation, now + Duration::from_secs(120)));
        assert!(!host.reap(generation, now + Duration::from_secs(331)));
        // A stale reaper for an old generation stops without touching state.
        assert!(!host.reap(generation, now));
    }
}
