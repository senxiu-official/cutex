//! Scoped MCP servers (DESIGN §4.11): project- and agent-level MCP server
//! configs that Cutex injects at managed launch on top of the shared
//! CODEX_HOME `[mcp_servers]` (global layer).
//!
//! Storage is a Cutex-owned sidecar, `~/.cutex/scoped-mcp.json`, keyed by
//! project ID and cutex session ID. It deliberately does not extend the
//! Project state / session records: those are `deny_unknown_fields` stores
//! shared with other writers, and an additive field would make older
//! binaries (e.g. a rolled-back live deployment) refuse to load them.
//!
//! Precedence per server name: global < project < agent. A `-c` override is
//! a config layer that native codex deep-merges, so a scoped server that
//! reuses a lower layer's name overrides that entry field by field. Agent
//! `disabled` names render as `mcp_servers.<name>.enabled=false`.
//!
//! Secrets: stdio `env` values are passed to the app-server process
//! environment and referenced by name through `env_vars`; `http_headers`
//! values are passed as `CUTEX_MCP_HDR_<n>` variables referenced through
//! `env_http_headers`. Values therefore never appear on the command line,
//! except stdio env keys that collide with a reserved process variable or
//! with a different value for the same key on another server, which are
//! rendered inline.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

use crate::launch::command::LaunchCommand;

pub const STORE_FILE: &str = "scoped-mcp.json";
pub const REDACTED: &str = "<redacted>";
/// Built-in servers Cutex injects itself; never overridable or disableable.
pub const BUILTIN_SERVER_NAMES: &[&str] = &["cutex", "cutex_job"];
pub const MAX_SERVERS_PER_SCOPE: usize = 64;

static STORE_PROCESS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeEntry {
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub servers: BTreeMap<String, Value>,
    /// Session scope only: inherited (global/project) servers to turn off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedMcpStore {
    #[serde(default = "store_version")]
    pub version: u32,
    #[serde(default)]
    pub projects: BTreeMap<String, ScopeEntry>,
    #[serde(default)]
    pub sessions: BTreeMap<String, ScopeEntry>,
}

fn store_version() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    Session,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Session => "session",
        }
    }
}

// ---------------------------------------------------------------------------
// Validation (mirrors codex RawMcpServerConfig -> McpServerConfig)
// ---------------------------------------------------------------------------

const KNOWN_FIELDS: &[&str] = &[
    "command",
    "args",
    "env",
    "env_vars",
    "cwd",
    "http_headers",
    "env_http_headers",
    "url",
    "bearer_token",
    "bearer_token_env_var",
    "http_headers_helper",
    "environment_id",
    "auth",
    "startup_timeout_sec",
    "startup_timeout_ms",
    "tool_timeout_sec",
    "enabled",
    "required",
    "startup_readiness",
    "supports_parallel_tool_calls",
    "tool_input_schema_max_bytes",
    "omit_tools_from",
    "default_tools_approval_mode",
    "enabled_tools",
    "disabled_tools",
    "scopes",
    "oauth",
    "oauth_resource",
    "name",
    "tools",
];

/// Server names usable as one `-c mcp_servers.<name>` key segment.
pub fn validate_server_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "invalid MCP server name {name:?}: use 1-64 characters [A-Za-z0-9_-]"
        ));
    }
    if BUILTIN_SERVER_NAMES
        .iter()
        .any(|builtin| builtin.eq_ignore_ascii_case(name))
    {
        return Err(format!("{name} is a Cutex built-in MCP server and cannot be overridden"));
    }
    Ok(())
}

fn reject_nulls(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::Null => Err(format!("{path}: null is not representable in config.toml")),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(index, item)| reject_nulls(item, &format!("{path}[{index}]"))),
        Value::Object(map) => map
            .iter()
            .try_for_each(|(key, item)| reject_nulls(item, &format!("{path}.{key}"))),
        _ => Ok(()),
    }
}

fn expect_string(map: &Map<String, Value>, key: &str, non_empty: bool) -> Result<(), String> {
    match map.get(key) {
        None => Ok(()),
        Some(Value::String(text)) if !non_empty || !text.trim().is_empty() => Ok(()),
        Some(_) => Err(format!(
            "{key} must be a {}string",
            if non_empty { "non-empty " } else { "" }
        )),
    }
}

fn expect_string_array(map: &Map<String, Value>, key: &str) -> Result<(), String> {
    match map.get(key) {
        None => Ok(()),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => Ok(()),
        Some(_) => Err(format!("{key} must be an array of strings")),
    }
}

fn expect_string_map(map: &Map<String, Value>, key: &str) -> Result<(), String> {
    match map.get(key) {
        None => Ok(()),
        Some(Value::Object(entries)) => {
            for (name, value) in entries {
                if name.is_empty() || name.contains('=') || name.contains('\0') {
                    return Err(format!("{key} has an invalid name {name:?}"));
                }
                if !value.is_string() {
                    return Err(format!("{key}.{name} must be a string"));
                }
            }
            Ok(())
        }
        Some(_) => Err(format!("{key} must be a table of strings")),
    }
}

fn expect_bool(map: &Map<String, Value>, key: &str) -> Result<(), String> {
    match map.get(key) {
        None | Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

fn expect_seconds(map: &Map<String, Value>, key: &str) -> Result<(), String> {
    match map.get(key) {
        None => Ok(()),
        Some(Value::Number(number))
            if number.as_f64().is_some_and(|value| value.is_finite() && value >= 0.0) =>
        {
            Ok(())
        }
        Some(_) => Err(format!("{key} must be a non-negative number of seconds")),
    }
}

fn expect_uint(map: &Map<String, Value>, key: &str, min: u64) -> Result<(), String> {
    match map.get(key) {
        None => Ok(()),
        Some(value) if value.as_u64().is_some_and(|value| value >= min) => Ok(()),
        Some(_) => Err(format!("{key} must be an integer >= {min}")),
    }
}

/// Validate one `mcp_servers.<name>` entry with codex's transport rules.
pub fn validate_server_config(name: &str, config: &Value) -> Result<(), String> {
    let prefix = |message: String| format!("mcp server {name}: {message}");
    let map = config
        .as_object()
        .ok_or_else(|| prefix("config must be a table".to_string()))?;
    reject_nulls(config, name).map_err(prefix)?;
    if let Some(key) = map.keys().find(|key| !KNOWN_FIELDS.contains(&key.as_str())) {
        return Err(prefix(format!("unknown field {key}")));
    }
    let check = || -> Result<(), String> {
        for key in [
            "cwd",
            "bearer_token_env_var",
            "environment_id",
            "oauth_resource",
            "name",
        ] {
            expect_string(map, key, false)?;
        }
        expect_string(map, "command", true)?;
        expect_string(map, "url", true)?;
        expect_string(map, "http_headers_helper", true)?;
        for key in ["args", "enabled_tools", "disabled_tools", "scopes"] {
            expect_string_array(map, key)?;
        }
        for key in ["env", "http_headers", "env_http_headers"] {
            expect_string_map(map, key)?;
        }
        for key in ["enabled", "required", "supports_parallel_tool_calls"] {
            expect_bool(map, key)?;
        }
        expect_seconds(map, "startup_timeout_sec")?;
        expect_seconds(map, "tool_timeout_sec")?;
        expect_uint(map, "startup_timeout_ms", 0)?;
        expect_uint(map, "tool_input_schema_max_bytes", 1)?;
        if let Some(mode) = map.get("default_tools_approval_mode") {
            if !matches!(mode.as_str(), Some("auto" | "prompt" | "writes" | "approve")) {
                return Err(
                    "default_tools_approval_mode must be auto, prompt, writes or approve".to_string(),
                );
            }
        }
        for key in ["oauth", "tools"] {
            if map.get(key).is_some_and(|value| !value.is_object()) {
                return Err(format!("{key} must be a table"));
            }
        }
        if map.get("omit_tools_from").is_some_and(|value| !value.is_array()) {
            return Err("omit_tools_from must be an array".to_string());
        }
        if let Some(env_vars) = map.get("env_vars") {
            let items = env_vars
                .as_array()
                .ok_or_else(|| "env_vars must be an array".to_string())?;
            for item in items {
                match item {
                    Value::String(name) if !name.is_empty() => {}
                    Value::Object(entry)
                        if entry.get("name").and_then(Value::as_str).is_some_and(|n| !n.is_empty())
                            && entry.keys().all(|key| key == "name" || key == "source") =>
                    {
                        match entry.get("source").map(|source| source.as_str()) {
                            None | Some(Some("local" | "remote")) => {}
                            _ => {
                                return Err(
                                    "env_vars source must be `local` or `remote`".to_string()
                                )
                            }
                        }
                    }
                    _ => return Err("env_vars entries must be a name or {name, source}".to_string()),
                }
            }
        }
        if map.contains_key("bearer_token") {
            return Err(
                "bearer_token is not supported; set bearer_token_env_var instead".to_string()
            );
        }
        let set = |key: &str| map.contains_key(key);
        if set("command") {
            for key in [
                "url",
                "bearer_token_env_var",
                "http_headers_helper",
                "http_headers",
                "env_http_headers",
                "oauth",
                "oauth_resource",
                "auth",
            ] {
                if set(key) {
                    return Err(format!("{key} is not supported for stdio"));
                }
            }
        } else if set("url") {
            for key in ["args", "env", "env_vars", "cwd"] {
                if set(key) {
                    return Err(format!("{key} is not supported for streamable_http"));
                }
            }
        } else {
            return Err("invalid transport: set command (stdio) or url (streamable_http)".to_string());
        }
        Ok(())
    };
    check().map_err(prefix)
}

/// Validate a full scope replacement (after redacted values are restored).
pub fn validate_scope(scope: Scope, entry: &ScopeEntry) -> Result<(), String> {
    if entry.servers.len() > MAX_SERVERS_PER_SCOPE {
        return Err(format!("at most {MAX_SERVERS_PER_SCOPE} servers per scope"));
    }
    for (name, config) in &entry.servers {
        validate_server_name(name)?;
        validate_server_config(name, config)?;
    }
    if scope == Scope::Project && !entry.disabled.is_empty() {
        return Err("disabled is only supported on the session (agent) scope".to_string());
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in &entry.disabled {
        validate_server_name(name)?;
        if !seen.insert(name) {
            return Err(format!("disabled lists {name} twice"));
        }
        if entry.servers.contains_key(name) {
            return Err(format!(
                "{name} is both defined and disabled; remove it from servers instead"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Redaction / write-only secrets
// ---------------------------------------------------------------------------

pub fn redact_server(name: &str, config: &Value) -> Value {
    let mut config = config.clone();
    let mut path = vec!["mcp_servers".to_string(), name.to_string()];
    crate::management::v2::codex_config::redact_config_value(&mut config, &mut path);
    config
}

pub fn redact_servers(servers: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    servers
        .iter()
        .map(|(name, config)| (name.clone(), redact_server(name, config)))
        .collect()
}

/// Replace every `"<redacted>"` in `new` with the value at the same path of
/// `old` (the stored config of the same server in the same scope).
pub fn restore_redacted(name: &str, new: &mut Value, old: Option<&Value>) -> Result<(), String> {
    fn walk(new: &mut Value, old: Option<&Value>, path: &str) -> Result<(), String> {
        match new {
            Value::String(text) if text == REDACTED => match old {
                Some(previous) if previous.as_str() != Some(REDACTED) && !previous.is_null() => {
                    *new = previous.clone();
                    Ok(())
                }
                _ => Err(format!(
                    "{path} is {REDACTED} but no stored value exists; send the real value"
                )),
            },
            Value::Object(map) => map.iter_mut().try_for_each(|(key, child)| {
                walk(child, old.and_then(|old| old.get(key)), &format!("{path}.{key}"))
            }),
            Value::Array(items) => items.iter_mut().enumerate().try_for_each(|(index, child)| {
                walk(child, old.and_then(|old| old.get(index)), &format!("{path}[{index}]"))
            }),
            _ => Ok(()),
        }
    }
    walk(new, old, &format!("mcp server {name}"))
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub fn store_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(STORE_FILE))
}

pub fn load_store_at(path: &Path) -> anyhow::Result<ScopedMcpStore> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("invalid scoped MCP store: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ScopedMcpStore {
            version: 1,
            ..ScopedMcpStore::default()
        }),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn load_store() -> anyhow::Result<ScopedMcpStore> {
    load_store_at(&store_path()?)
}

#[derive(Debug)]
pub enum UpdateError {
    Invalid(String),
    RevisionConflict { expected: u64, actual: u64 },
    Store(anyhow::Error),
}

/// Replace one scope under process + file locks. `servers` may contain
/// `"<redacted>"` placeholders that keep the stored secret.
pub fn replace_scope_at(
    path: &Path,
    scope: Scope,
    id: &str,
    mut servers: BTreeMap<String, Value>,
    disabled: Vec<String>,
    expected_revision: Option<u64>,
) -> Result<ScopeEntry, UpdateError> {
    use fs2::FileExt;
    let _guard = STORE_PROCESS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| UpdateError::Store(error.into()))?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| UpdateError::Store(error.into()))?;
    lock.lock_exclusive()
        .map_err(|error| UpdateError::Store(error.into()))?;
    let result = (|| {
        let mut store = load_store_at(path).map_err(UpdateError::Store)?;
        let map = match scope {
            Scope::Project => &mut store.projects,
            Scope::Session => &mut store.sessions,
        };
        let current = map.get(id).cloned().unwrap_or_default();
        if let Some(expected) = expected_revision {
            if expected != current.revision {
                return Err(UpdateError::RevisionConflict {
                    expected,
                    actual: current.revision,
                });
            }
        }
        for (name, config) in servers.iter_mut() {
            restore_redacted(name, config, current.servers.get(name)).map_err(UpdateError::Invalid)?;
        }
        let next = ScopeEntry {
            revision: current.revision + 1,
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            servers,
            disabled,
        };
        validate_scope(scope, &next).map_err(UpdateError::Invalid)?;
        if next.servers.is_empty() && next.disabled.is_empty() {
            // Keep the revision so CAS stays monotonic.
            map.insert(id.to_string(), ScopeEntry { servers: BTreeMap::new(), ..next.clone() });
        } else {
            map.insert(id.to_string(), next.clone());
        }
        crate::config::atomic::write_private_pretty_json_atomic(path, &store, "scoped MCP store")
            .map_err(UpdateError::Store)?;
        Ok(next)
    })();
    let _ = FileExt::unlock(&lock);
    result
}

pub fn replace_scope(
    scope: Scope,
    id: &str,
    servers: BTreeMap<String, Value>,
    disabled: Vec<String>,
    expected_revision: Option<u64>,
) -> Result<ScopeEntry, UpdateError> {
    let path = store_path().map_err(UpdateError::Store)?;
    replace_scope_at(&path, scope, id, servers, disabled, expected_revision)
}

// ---------------------------------------------------------------------------
// Launch rendering
// ---------------------------------------------------------------------------

/// Process variables a stdio `env` key must never overwrite on the
/// app-server process; such keys are rendered inline instead.
pub(crate) fn reserved_process_env(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.starts_with("CUTEX_")
        || upper.starts_with("CODEX_")
        || matches!(
            upper.as_str(),
            "PATH"
                | "PATHEXT"
                | "HOME"
                | "USERPROFILE"
                | "APPDATA"
                | "LOCALAPPDATA"
                | "SYSTEMROOT"
                | "SYSTEMDRIVE"
                | "WINDIR"
                | "COMSPEC"
                | "TEMP"
                | "TMP"
                | "TMPDIR"
                | "PROGRAMDATA"
                | "PROGRAMFILES"
                | "PROGRAMFILES(X86)"
                | "OPENAI_API_KEY"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "LD_PRELOAD"
                | "LD_LIBRARY_PATH"
        )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaunchInjection {
    /// `-c key=value` pairs in order.
    pub options: Vec<(String, toml::Value)>,
    /// Environment for the app-server process (secret values).
    pub envs: Vec<(String, String)>,
}

impl LaunchInjection {
    pub fn args(&self) -> Vec<String> {
        self.options
            .iter()
            .flat_map(|(key, value)| ["-c".to_string(), format!("{key}={value}")])
            .collect()
    }

    pub fn apply(&self, mut launch: LaunchCommand) -> LaunchCommand {
        for arg in self.args() {
            launch = launch.arg(arg);
        }
        for (key, value) in &self.envs {
            launch = launch.env(key, value);
        }
        launch
    }
}

/// Effective scoped servers (project then agent; agent wins per name by
/// deep merge, mirroring native layering) and the agent's disabled names.
pub fn render_launch(
    project: Option<&ScopeEntry>,
    agent: Option<&ScopeEntry>,
) -> anyhow::Result<LaunchInjection> {
    // Per-layer, in precedence order; each is its own -c override so native
    // codex performs the same deep merge it does for any layer.
    let mut layers: Vec<&BTreeMap<String, Value>> = Vec::new();
    if let Some(project) = project {
        layers.push(&project.servers);
    }
    if let Some(agent) = agent {
        layers.push(&agent.servers);
    }
    let mut injection = LaunchInjection::default();
    let mut env_values: BTreeMap<String, String> = BTreeMap::new();
    // First pass: which stdio env keys can be passed by name (no conflict).
    let mut conflicting = std::collections::BTreeSet::new();
    for servers in &layers {
        for config in servers.values() {
            if let Some(env) = config.get("env").and_then(Value::as_object) {
                for (key, value) in env {
                    let value = value.as_str().unwrap_or_default();
                    match env_values.get(key) {
                        Some(existing) if existing != value => {
                            conflicting.insert(key.clone());
                        }
                        _ => {
                            env_values.insert(key.clone(), value.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut passed = std::collections::BTreeSet::new();
    let mut header_index = 0usize;
    for servers in &layers {
        for (name, config) in servers.iter() {
            validate_server_name(name).map_err(anyhow::Error::msg)?;
            validate_server_config(name, config).map_err(anyhow::Error::msg)?;
            let mut rendered = config.as_object().cloned().unwrap_or_default();
            if let Some(Value::Object(env)) = rendered.remove("env") {
                let mut inline = Map::new();
                let mut env_vars = rendered
                    .remove("env_vars")
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default();
                for (key, value) in env {
                    if reserved_process_env(&key) || conflicting.contains(&key) {
                        inline.insert(key, value);
                    } else {
                        if passed.insert(key.clone()) {
                            injection
                                .envs
                                .push((key.clone(), value.as_str().unwrap_or_default().to_string()));
                        }
                        if !env_vars.iter().any(|item| {
                            item.as_str() == Some(key.as_str())
                                || item.get("name").and_then(Value::as_str) == Some(key.as_str())
                        }) {
                            env_vars.push(Value::String(key));
                        }
                    }
                }
                if !inline.is_empty() {
                    rendered.insert("env".to_string(), Value::Object(inline));
                }
                if !env_vars.is_empty() {
                    rendered.insert("env_vars".to_string(), Value::Array(env_vars));
                }
            }
            if let Some(Value::Object(headers)) = rendered.remove("http_headers") {
                let mut env_headers = rendered
                    .remove("env_http_headers")
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                for (header, value) in headers {
                    header_index += 1;
                    let variable = format!("CUTEX_MCP_HDR_{header_index}");
                    injection
                        .envs
                        .push((variable.clone(), value.as_str().unwrap_or_default().to_string()));
                    env_headers.insert(header, Value::String(variable));
                }
                rendered.insert("env_http_headers".to_string(), Value::Object(env_headers));
            }
            let value = toml::Value::try_from(Value::Object(rendered))
                .with_context(|| format!("mcp server {name} is not representable as TOML"))?;
            injection.options.push((format!("mcp_servers.{name}"), value));
        }
    }
    if let Some(agent) = agent {
        for name in &agent.disabled {
            validate_server_name(name).map_err(anyhow::Error::msg)?;
            injection
                .options
                .push((format!("mcp_servers.{name}.enabled"), toml::Value::Boolean(false)));
        }
    }
    Ok(injection)
}

/// The session's current active project, if any.
pub fn session_project_id(cutex_session_id: &str) -> anyhow::Result<Option<String>> {
    let store = crate::agent_management::AgentManagementStore::open_default()?;
    let snapshot = store.snapshot().map_err(anyhow::Error::new)?;
    let Ok(session_id) = crate::role_revision::CutexSessionId::new(cutex_session_id.to_string())
    else {
        return Ok(None);
    };
    let project = match snapshot.current_project_memberships.get(&session_id) {
        Some(membership) => membership.project_id.clone(),
        None => snapshot
            .agents
            .get(&session_id)
            .and_then(|agent| crate::agent_management::current_project_id(&snapshot, agent)),
    };
    Ok(project.filter(|project| {
        crate::agent_management::project_exists(&snapshot, project)
            && snapshot.project_states.get(project).is_none_or(|state| {
                state.lifecycle == crate::agent_management::ProjectLifecycle::Active
            })
    })
    .map(|project| project.as_str().to_string()))
}

/// Everything Cutex injects for this session's scoped MCP at launch.
pub fn launch_injection_for_session(cutex_session_id: &str) -> anyhow::Result<LaunchInjection> {
    let store = load_store()?;
    let project = session_project_id(cutex_session_id)?;
    render_launch(
        project.as_ref().and_then(|project| store.projects.get(project)),
        store.sessions.get(cutex_session_id),
    )
}

// ---------------------------------------------------------------------------
// Effective view
// ---------------------------------------------------------------------------

fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveServer {
    pub name: String,
    /// Highest layer defining the server: builtin | global | project | agent.
    pub layer: &'static str,
    /// Every layer that defines this name, lowest first.
    pub layers: Vec<&'static str>,
    pub enabled: bool,
    /// Why it is off: "agent" (agent `disabled`), "config" (`enabled=false`),
    /// "cutex" (built-in not active for this session).
    pub disabled_by: Option<&'static str>,
    /// Merged config, secrets redacted (null for built-ins without config).
    pub config: Value,
}

pub struct BuiltinServer {
    pub name: &'static str,
    pub enabled: bool,
}

pub fn effective_servers(
    global: &BTreeMap<String, Value>,
    project: Option<&ScopeEntry>,
    agent: Option<&ScopeEntry>,
    builtins: &[BuiltinServer],
) -> Vec<EffectiveServer> {
    let mut merged: BTreeMap<String, (Vec<&'static str>, Value)> = BTreeMap::new();
    let mut add = |layer: &'static str, servers: &BTreeMap<String, Value>| {
        for (name, config) in servers {
            if BUILTIN_SERVER_NAMES.contains(&name.as_str()) {
                continue;
            }
            let entry = merged
                .entry(name.clone())
                .or_insert_with(|| (Vec::new(), Value::Object(Map::new())));
            entry.0.push(layer);
            deep_merge(&mut entry.1, config);
        }
    };
    add("global", global);
    if let Some(project) = project {
        add("project", &project.servers);
    }
    if let Some(agent) = agent {
        add("agent", &agent.servers);
    }
    let disabled: Vec<&String> = agent.map(|agent| agent.disabled.iter().collect()).unwrap_or_default();
    let mut result: Vec<EffectiveServer> = builtins
        .iter()
        .map(|builtin| EffectiveServer {
            name: builtin.name.to_string(),
            layer: "builtin",
            layers: vec!["builtin"],
            enabled: builtin.enabled,
            disabled_by: (!builtin.enabled).then_some("cutex"),
            config: Value::Null,
        })
        .collect();
    for (name, (layers, config)) in merged {
        let agent_disabled = disabled.iter().any(|disabled| **disabled == name);
        let config_disabled = config.get("enabled") == Some(&Value::Bool(false));
        result.push(EffectiveServer {
            layer: layers.last().copied().unwrap_or("global"),
            layers,
            enabled: !agent_disabled && !config_disabled,
            disabled_by: if agent_disabled {
                Some("agent")
            } else if config_disabled {
                Some("config")
            } else {
                None
            },
            config: redact_server(&name, &config),
            name,
        });
    }
    // Disabled names that match nothing are still reported.
    for name in disabled {
        if !result.iter().any(|server| &server.name == name) {
            result.push(EffectiveServer {
                name: name.clone(),
                layer: "agent",
                layers: vec![],
                enabled: false,
                disabled_by: Some("agent"),
                config: json!({}),
            });
        }
    }
    result
}

/// `[mcp_servers]` of the shared CODEX_HOME config.toml as JSON.
pub fn global_servers(codex_home: &Path) -> anyhow::Result<BTreeMap<String, Value>> {
    let path = codex_home.join("config.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let value: toml::Value = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let servers = value
        .get("mcp_servers")
        .cloned()
        .map(serde_json::to_value)
        .transpose()?
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    Ok(servers.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers(value: Value) -> BTreeMap<String, Value> {
        value.as_object().unwrap().clone().into_iter().collect()
    }

    #[test]
    fn server_validation_follows_codex_transport_rules() {
        let ok = [
            json!({ "command": "npx", "args": ["-y", "x"], "env": { "T": "v" },
                    "env_vars": ["PATH", { "name": "X", "source": "remote" }],
                    "startup_timeout_sec": 10.5, "tool_timeout_sec": 30, "enabled": true,
                    "default_tools_approval_mode": "prompt", "enabled_tools": ["a"] }),
            json!({ "url": "https://mcp.example/x", "bearer_token_env_var": "TOK",
                    "http_headers": { "X-A": "b" }, "env_http_headers": { "X-B": "VAR" },
                    "oauth": { "client_id": "c" }, "scopes": ["read"] }),
        ];
        for config in ok {
            validate_server_config("s", &config).unwrap();
        }
        let bad = [
            json!("npx"),
            json!({}),
            json!({ "command": "" }),
            json!({ "command": "x", "url": "https://x" }),
            json!({ "command": "x", "http_headers": { "a": "b" } }),
            json!({ "url": "https://x", "env": { "a": "b" } }),
            json!({ "url": "https://x", "bearer_token": "secret" }),
            json!({ "command": "x", "args": "a b" }),
            json!({ "command": "x", "env": { "A": 1 } }),
            json!({ "command": "x", "env": { "A=B": "1" } }),
            json!({ "command": "x", "env_vars": [{ "name": "A", "source": "cloud" }] }),
            json!({ "command": "x", "default_tools_approval_mode": "always" }),
            json!({ "command": "x", "startup_timeout_sec": -1 }),
            json!({ "command": "x", "tool_input_schema_max_bytes": 0 }),
            json!({ "command": "x", "typo": true }),
            json!({ "command": "x", "cwd": null }),
        ];
        for config in bad {
            assert!(validate_server_config("s", &config).is_err(), "{config}");
        }
        for name in ["ok_name", "a-b", "A1"] {
            validate_server_name(name).unwrap();
        }
        for name in ["", "a.b", "a b", "cutex", "CUTEX_JOB", "x/y", &"a".repeat(65)] {
            assert!(validate_server_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn scope_validation_rejects_builtins_project_disabled_and_overlap() {
        let entry = |servers_value: Value, disabled: &[&str]| ScopeEntry {
            servers: servers(servers_value),
            disabled: disabled.iter().map(|s| s.to_string()).collect(),
            ..ScopeEntry::default()
        };
        validate_scope(Scope::Session, &entry(json!({ "a": { "command": "x" } }), &["g"])).unwrap();
        assert!(validate_scope(Scope::Project, &entry(json!({}), &["g"])).is_err());
        assert!(validate_scope(Scope::Session, &entry(json!({ "cutex": { "command": "x" } }), &[])).is_err());
        assert!(validate_scope(Scope::Session, &entry(json!({}), &["cutex"])).is_err());
        assert!(validate_scope(Scope::Session, &entry(json!({ "a": { "command": "x" } }), &["a"])).is_err());
        assert!(validate_scope(Scope::Session, &entry(json!({}), &["g", "g"])).is_err());
    }

    #[test]
    fn redaction_and_write_only_restore() {
        let stored = json!({ "command": "x", "env": { "TOKEN": "s3cret", "MODE": "m" } });
        let shown = redact_server("gh", &stored);
        assert_eq!(shown["env"]["TOKEN"], REDACTED);
        assert_eq!(shown["command"], "x");
        let http = redact_server("w", &json!({ "url": "https://x", "http_headers": { "Authorization": "Bearer z" },
                                                "oauth": { "client_id": "id", "client_secret": "cs" } }));
        assert_eq!(http["http_headers"]["Authorization"], REDACTED);
        assert_eq!(http["oauth"]["client_secret"], REDACTED);
        assert_eq!(http["oauth"]["client_id"], "id");

        // Round-trip of the redacted view keeps the stored secret; a changed value wins.
        let mut put = shown.clone();
        put["env"]["MODE"] = json!("new");
        restore_redacted("gh", &mut put, Some(&stored)).unwrap();
        assert_eq!(put["env"]["TOKEN"], "s3cret");
        assert_eq!(put["env"]["MODE"], "new");
        let mut orphan = json!({ "command": "x", "env": { "NEW": REDACTED } });
        assert!(restore_redacted("gh", &mut orphan, Some(&stored)).is_err());
        assert!(restore_redacted("gh", &mut shown.clone(), None).is_err());
    }

    #[test]
    fn store_replace_is_cas_fenced_and_restores_secrets() {
        let dir = std::env::temp_dir().join(format!("cutex-scoped-mcp-{}", uuid::Uuid::new_v4()));
        let path = dir.join(STORE_FILE);
        let first = replace_scope_at(
            &path,
            Scope::Session,
            "cutex.s1",
            servers(json!({ "gh": { "command": "x", "env": { "T": "s3cret" } } })),
            vec!["global_one".into()],
            Some(0),
        )
        .unwrap();
        assert_eq!(first.revision, 1);
        let conflict = replace_scope_at(&path, Scope::Session, "cutex.s1", BTreeMap::new(), vec![], Some(0));
        assert!(matches!(conflict, Err(UpdateError::RevisionConflict { expected: 0, actual: 1 })));
        let second = replace_scope_at(
            &path,
            Scope::Session,
            "cutex.s1",
            servers(json!({ "gh": { "command": "y", "env": { "T": REDACTED } } })),
            vec![],
            Some(1),
        )
        .unwrap();
        assert_eq!(second.revision, 2);
        let store = load_store_at(&path).unwrap();
        assert_eq!(store.sessions["cutex.s1"].servers["gh"]["env"]["T"], "s3cret");
        assert_eq!(store.sessions["cutex.s1"].servers["gh"]["command"], "y");
        let invalid = replace_scope_at(
            &path,
            Scope::Project,
            "p1",
            servers(json!({ "cutex": { "command": "x" } })),
            vec![],
            None,
        );
        assert!(matches!(invalid, Err(UpdateError::Invalid(_))));
        assert!(load_store_at(&path).unwrap().projects.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn launch_rendering_layers_project_then_agent_and_keeps_secrets_off_argv() {
        let project = ScopeEntry {
            servers: servers(json!({
                "db": { "command": "db-mcp", "args": ["--ro"], "env": { "DB_PASSWORD": "pw", "PATH": "/opt/bin" } },
                "issues": { "url": "https://issues/mcp", "http_headers": { "Authorization": "Bearer t" } }
            })),
            ..ScopeEntry::default()
        };
        let agent = ScopeEntry {
            servers: servers(json!({
                "db": { "command": "db-mcp", "env": { "DB_PASSWORD": "other" } },
                "notes": { "command": "notes", "enabled": false }
            })),
            disabled: vec!["github".into()],
            ..ScopeEntry::default()
        };
        let injection = render_launch(Some(&project), Some(&agent)).unwrap();
        let args = injection.args();
        let joined = args.join(" ");
        assert!(!joined.contains("Bearer t"), "{joined}");
        // Conflicting DB_PASSWORD values across layers fall back inline; PATH is reserved.
        let keys: Vec<&str> = injection.options.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "mcp_servers.db",
                "mcp_servers.issues",
                "mcp_servers.db",
                "mcp_servers.notes",
                "mcp_servers.github.enabled"
            ]
        );
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("mcp_servers.db="));
        assert!(args[1].contains("PATH = \"/opt/bin\""), "{}", args[1]);
        let issues = &injection.options[1].1;
        assert_eq!(issues["env_http_headers"]["Authorization"].as_str(), Some("CUTEX_MCP_HDR_1"));
        assert!(issues.get("http_headers").is_none());
        assert!(injection
            .envs
            .contains(&("CUTEX_MCP_HDR_1".to_string(), "Bearer t".to_string())));
        assert_eq!(args.last().unwrap(), "mcp_servers.github.enabled=false");

        // Without a conflict a secret env value goes to the process env, referenced by name.
        let single = ScopeEntry {
            servers: servers(json!({ "gh": { "command": "gh-mcp", "env": { "GITHUB_TOKEN": "ghp" },
                                             "env_vars": ["HOME"] } })),
            ..ScopeEntry::default()
        };
        let injection = render_launch(None, Some(&single)).unwrap();
        let rendered = &injection.options[0].1;
        assert!(rendered.get("env").is_none());
        let names: Vec<&str> = rendered["env_vars"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(names, ["HOME", "GITHUB_TOKEN"]);
        assert_eq!(injection.envs, [("GITHUB_TOKEN".to_string(), "ghp".to_string())]);
        assert!(!injection.args().join(" ").contains("ghp"));
        let launch = injection.apply(LaunchCommand::new("app-server"));
        assert_eq!(launch.args[0], "-c");
        assert!(launch.envs.contains(&("GITHUB_TOKEN".to_string(), "ghp".to_string())));

        assert_eq!(render_launch(None, None).unwrap(), LaunchInjection::default());
    }

    #[test]
    fn effective_view_merges_layers_and_reports_disabled() {
        let global = servers(json!({
            "github": { "command": "gh", "env": { "T": "g" } },
            "db": { "command": "db", "args": ["--global"] },
            "cutex": { "command": "spoofed" }
        }));
        let project = ScopeEntry {
            servers: servers(json!({ "db": { "args": ["--project"], "env": { "PW": "p" } } })),
            ..ScopeEntry::default()
        };
        let agent = ScopeEntry {
            servers: servers(json!({ "mine": { "command": "m", "enabled": false } })),
            disabled: vec!["github".into(), "ghost".into()],
            ..ScopeEntry::default()
        };
        let builtins = [
            BuiltinServer { name: "cutex", enabled: true },
            BuiltinServer { name: "cutex_job", enabled: false },
        ];
        let view = effective_servers(&global, Some(&project), Some(&agent), &builtins);
        let get = |name: &str| view.iter().find(|server| server.name == name).unwrap();
        assert_eq!(get("cutex").layer, "builtin");
        assert!(get("cutex").enabled);
        assert_eq!(get("cutex_job").disabled_by, Some("cutex"));
        assert_eq!(view.iter().filter(|server| server.name == "cutex").count(), 1);
        let db = get("db");
        assert_eq!((db.layer, db.layers.clone()), ("project", vec!["global", "project"]));
        assert_eq!(db.config["command"], "db");
        assert_eq!(db.config["args"], json!(["--project"]));
        assert_eq!(db.config["env"]["PW"], REDACTED);
        let github = get("github");
        assert_eq!((github.enabled, github.disabled_by), (false, Some("agent")));
        assert_eq!(github.config["env"]["T"], REDACTED);
        assert_eq!(get("mine").disabled_by, Some("config"));
        assert_eq!(get("ghost").layers.len(), 0);
    }
}
