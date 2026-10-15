//! Durable services registry.
//!
//! `<cutex home>/services/registry.json` holds the records and the registry
//! settings. Every mutation takes the `registry.lock` file lock (fs2), so the
//! Management daemon (desktop routes) and the Agent Bus (agent MCP tools) can
//! both write it; the supervisor notices changes through `revision`.
//! Runtime facts (health, last seen, pid) live in `state.json`, written only by
//! the daemon's supervisor.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use fs2::FileExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use super::health::RuntimeSnapshot;
use super::health::ServiceHealth;

pub const REGISTRY_FILE: &str = "registry.json";
pub const STATE_FILE: &str = "state.json";
pub const DEFAULT_PORT_RANGE_START: u16 = 24400;
pub const DEFAULT_PORT_RANGE_END: u16 = 24999;
pub const MAX_SERVICES: usize = 256;
const MAX_ENV_ENTRIES: usize = 64;
const MAX_COMMAND_CHARS: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceRuntime {
    /// Registered only; cutex just probes it.
    External,
    /// cutex runs the command, restarts it with backoff and keeps logs.
    /// `open` starts a stopped one.
    Managed,
    Static,
}

impl ServiceRuntime {
    pub fn label(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Managed => "managed",
            Self::Static => "static",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "external" => Some(Self::External),
            "managed" => Some(Self::Managed),
            "static" => Some(Self::Static),
            _ => None,
        }
    }

    pub fn supervised(self) -> bool {
        matches!(self, Self::Managed)
    }
}

/// A shell command line (run by `cmd /C` / `sh -c`) or an argv array.
/// `{port}` in either form is replaced by the service port; `PORT` is also set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServiceCommand {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRecord {
    #[serde(default = "default_proxy_host_header")]
    pub proxy_host_header: String,
    pub slug: String,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_cutex_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub runtime: ServiceRuntime,
    #[serde(default)]
    pub lifetime: ServiceLifetime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<ServiceCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Managed, owner only: a command (run like `command`, same cwd / env) that prints a
    /// one-time login URL of the running service (e.g. `claude-science url`). A view ticket
    /// for the service runs it and sends the browser to that URL's path and query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_link_command: Option<ServiceCommand>,
    /// Managed only: the owner stopped it, so the supervisor leaves it down
    /// (also across daemon restarts) until it is started again.
    #[serde(default)]
    pub stopped: bool,
    /// The port came from the allocator (informational).
    #[serde(default)]
    pub port_allocated: bool,
    /// Managed only: the execution ceiling. cutex runs managed commands
    /// unsandboxed, so it is `unrestricted` (as the owner) until a sandboxed
    /// service runner exists (asks design §4.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<String>,
    /// Who registered it, and the ask (委托) whose turn it was registered in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ServiceProvenance>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceLifetime {
    Ephemeral,
    #[default]
    Persistent,
}
impl ServiceLifetime {
    pub fn parse(value: &str) -> ServicesResult<Self> {
        match value {
            "ephemeral" => Ok(Self::Ephemeral),
            "persistent" => Ok(Self::Persistent),
            _ => Err(ServicesError::bad_request(
                "lifetime must be ephemeral or persistent",
            )),
        }
    }
}

/// Owner setting `agentPersistentApproval`: whether an agent's request for a
/// persistent lifetime raises a `service_lifetime` approval (`ask`, default)
/// or is granted mechanically (`off`). The managed-execution gate is separate
/// and unaffected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentPersistentApproval {
    #[default]
    Ask,
    Off,
}
impl AgentPersistentApproval {
    pub fn parse(value: &str) -> ServicesResult<Self> {
        match value {
            "ask" => Ok(Self::Ask),
            "off" => Ok(Self::Off),
            _ => Err(ServicesError::bad_request(
                "agentPersistentApproval must be ask or off",
            )),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceTombstone {
    pub slug: String,
    pub display_name: String,
    pub owner_cutex_session_id: Option<String>,
    pub lifetime: ServiceLifetime,
    pub reason: String,
    pub reclaimed_at: String,
}

/// Registration provenance of a service (asks design §4.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceProvenance {
    /// `owner` or the registering agent's durable session.
    pub registrant: String,
    /// The ask (委托) the registrant's turn was handling, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<String>,
}

/// The ceiling of a runtime: managed commands run unrestricted.
pub fn ceiling_for(runtime: ServiceRuntime) -> Option<String> {
    (runtime == ServiceRuntime::Managed).then(|| "unrestricted".to_string())
}

fn provenance_for(caller: &Caller) -> ServiceProvenance {
    match caller {
        Caller::Owner => ServiceProvenance {
            registrant: "owner".into(),
            ask_id: None,
        },
        Caller::Agent {
            cutex_session_id, ..
        } => ServiceProvenance {
            registrant: cutex_session_id.clone(),
            ask_id: crate::asks::store::AskStore::open_default()
                .ok()
                .and_then(|store| store.peek().ok().flatten())
                .and_then(|state| match &state.leases.get(cutex_session_id)?.holder {
                    crate::asks::model::LeaseHolder::Ask { ask_id, .. } => Some(ask_id.clone()),
                    _ => None,
                }),
        },
    }
}

impl ServiceRecord {
    /// What a supervised process depends on; a change restarts it.
    pub fn launch_fingerprint(&self) -> String {
        serde_json::to_string(&json!([self.command, self.cwd, self.env, self.port]))
            .unwrap_or_default()
    }

    pub fn url_for_host(&self, host: &str) -> String {
        format!(
            "http://{}:{}{}",
            host,
            self.port,
            self.path.as_deref().unwrap_or("/")
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServicesSettings {
    pub agent_external_default_lifetime: ServiceLifetime,
    pub agent_persistent_approval: AgentPersistentApproval,
    #[serde(default = "default_proxy_max_connections")]
    pub proxy_max_connections: u16,
    pub port_range_start: u16,
    pub port_range_end: u16,
    pub ephemeral_idle_minutes: u32,
    pub ephemeral_ttl_hours: u32,
    pub reclaim_on_owner_archive: bool,
    pub tombstone_days: u32,
    pub ephemeral_max_restarts: u32,
}

impl Default for ServicesSettings {
    fn default() -> Self {
        Self {
            agent_external_default_lifetime: ServiceLifetime::Ephemeral,
            agent_persistent_approval: AgentPersistentApproval::Ask,
            port_range_start: DEFAULT_PORT_RANGE_START,
            port_range_end: DEFAULT_PORT_RANGE_END,
            proxy_max_connections: default_proxy_max_connections(),
            ephemeral_idle_minutes: 30,
            ephemeral_ttl_hours: 24,
            reclaim_on_owner_archive: true,
            tombstone_days: 7,
            ephemeral_max_restarts: 2,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub settings: ServicesSettings,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceRecord>,
    #[serde(default)]
    pub tombstones: BTreeMap<String, ServiceTombstone>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicesError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ServicesError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }

    pub fn not_found(slug: &str) -> Self {
        Self::new(
            404,
            "service_not_found",
            format!("no service `{slug}` on this host"),
        )
    }

    pub fn unavailable(error: impl std::fmt::Display) -> Self {
        Self::new(503, "services_store_unavailable", error.to_string())
    }
}

impl std::fmt::Display for ServicesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ServicesError {}

pub type ServicesResult<T> = Result<T, ServicesError>;

/// Who is mutating the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// Management root credential (the desktop): any record, any field.
    Owner,
    /// An agent through MCP: owns what it registers, touches only its own.
    Agent {
        cutex_session_id: String,
        project_id: Option<String>,
    },
}

/// `POST /v2/services` and `service_register`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterRequest {
    #[serde(default)]
    pub proxy_host_header: Option<String>,
    pub slug: String,
    #[serde(default)]
    pub reveal: bool,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub lifetime: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub command: Option<ServiceCommand>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    /// Owner only, managed only (see `ServiceRecord::login_link_command`).
    #[serde(default)]
    pub login_link_command: Option<ServiceCommand>,
    /// Owner only (agents get their own session/project).
    #[serde(default)]
    pub owner_cutex_session_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

/// Distinguishes an absent field from an explicit `null`.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `PATCH /v2/services/{slug}` and `service_update`. `null` clears
/// optional fields; `env` merges (`"KEY": null` removes a key).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateRequest {
    #[serde(default)]
    pub proxy_host_header: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub path: Option<Option<String>>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub lifetime: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub command: Option<Option<ServiceCommand>>,
    #[serde(default, deserialize_with = "nullable")]
    pub cwd: Option<Option<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, Option<String>>>,
    /// Owner only; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub login_link_command: Option<Option<ServiceCommand>>,
    /// Owner only.
    #[serde(default, deserialize_with = "nullable")]
    pub owner_cutex_session_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub project_id: Option<Option<String>>,
}

/// `PATCH /v2/services` body (registry settings).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsPatch {
    pub agent_external_default_lifetime: Option<String>,
    pub agent_persistent_approval: Option<String>,
    #[serde(default)]
    pub proxy_max_connections: Option<u16>,
    #[serde(default)]
    pub port_range_start: Option<u16>,
    #[serde(default)]
    pub port_range_end: Option<u16>,
    pub ephemeral_idle_minutes: Option<u32>,
    pub ephemeral_ttl_hours: Option<u32>,
    pub reclaim_on_owner_archive: Option<bool>,
    pub tombstone_days: Option<u32>,
    pub ephemeral_max_restarts: Option<u32>,
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn default_proxy_host_header() -> String {
    "preserve".into()
}
fn default_proxy_max_connections() -> u16 {
    192
}
fn validate_proxy_host_header(value: &str) -> ServicesResult<String> {
    match value {
        "preserve" | "upstream" => Ok(value.into()),
        _ => Err(ServicesError::bad_request(
            "proxyHostHeader must be preserve or upstream",
        )),
    }
}

pub fn validate_slug(slug: &str) -> ServicesResult<()> {
    let bytes = slug.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if ok {
        Ok(())
    } else {
        Err(ServicesError::new(
            400,
            "invalid_slug",
            "slug must be 1-63 chars of a-z, 0-9 and '-', starting with a letter or digit",
        ))
    }
}

fn validate_display_name(name: &str) -> ServicesResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err(ServicesError::bad_request(
            "displayName must be 1-120 printable characters",
        ));
    }
    Ok(name.to_string())
}

fn validate_path(path: &str) -> ServicesResult<String> {
    let path = path.trim();
    if !path.starts_with('/')
        || path.len() > 512
        || path.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ServicesError::bad_request(
            "path must start with '/' and contain no whitespace",
        ));
    }
    Ok(path.to_string())
}

fn validate_command(command: &ServiceCommand) -> ServicesResult<()> {
    let bad = match command {
        ServiceCommand::Shell(line) => {
            line.trim().is_empty() || line.len() > MAX_COMMAND_CHARS || line.contains('\0')
        }
        ServiceCommand::Argv(argv) => {
            argv.is_empty()
                || argv[0].trim().is_empty()
                || argv.len() > 256
                || argv.iter().map(String::len).sum::<usize>() > MAX_COMMAND_CHARS
                || argv.iter().any(|arg| arg.contains('\0'))
        }
    };
    if bad {
        return Err(ServicesError::bad_request(
            "command must be a non-empty command line or argv array",
        ));
    }
    Ok(())
}

/// A login-link command runs at view time without an approval card: owner only.
fn owner_only_login_link(caller: &Caller) -> ServicesResult<()> {
    match caller {
        Caller::Owner => Ok(()),
        Caller::Agent { .. } => Err(ServicesError::new(
            403,
            "owner_only",
            "loginLinkCommand can only be set by the owner",
        )),
    }
}

fn validate_cwd(cwd: &str) -> ServicesResult<String> {
    let cwd = cwd.trim();
    let path = Path::new(cwd);
    if cwd.is_empty() || !path.is_absolute() {
        return Err(ServicesError::bad_request("cwd must be an absolute path"));
    }
    if !path.is_dir() {
        return Err(ServicesError::new(
            400,
            "cwd_not_found",
            format!("cwd `{cwd}` is not a directory on this host"),
        ));
    }
    Ok(cwd.to_string())
}

fn validate_env_entry(key: &str, value: Option<&str>) -> ServicesResult<()> {
    if key.is_empty()
        || key.len() > 128
        || key.contains(['=', '\0'])
        || key.chars().any(char::is_whitespace)
        || value.is_some_and(|value| value.contains('\0') || value.len() > 8192)
    {
        return Err(ServicesError::bad_request(format!(
            "invalid env entry `{key}`"
        )));
    }
    Ok(())
}

fn validate_session_id(id: &str) -> ServicesResult<String> {
    crate::role_revision::CutexSessionId::new(id.to_string())
        .map(|_| id.to_string())
        .map_err(|_| ServicesError::bad_request("invalid ownerCutexSessionId"))
}

fn validate_project_id(id: &str) -> ServicesResult<String> {
    crate::agent_management::ProjectId::new(id.to_string())
        .map(|_| id.to_string())
        .map_err(|_| ServicesError::bad_request("invalid projectId"))
}

/// Supervised runtimes need a command; external ones must not have one.
fn validate_runtime_fields(record: &ServiceRecord) -> ServicesResult<()> {
    if record.runtime == ServiceRuntime::Static {
        if record.root.is_none()
            || record.port != 0
            || record.command.is_some()
            || record.cwd.is_some()
            || !record.env.is_empty()
        {
            return Err(ServicesError::bad_request(
                "static requires root and forbids port, command, cwd and env",
            ));
        }
    } else if record.root.is_some() {
        return Err(ServicesError::bad_request(
            "root is only for static services",
        ));
    }
    if record.login_link_command.is_some() && record.runtime != ServiceRuntime::Managed {
        return Err(ServicesError::bad_request(
            "loginLinkCommand is only for managed services",
        ));
    }
    if record.runtime.supervised() {
        if record.command.is_none() {
            return Err(ServicesError::bad_request(format!(
                "a {} service needs a command",
                record.runtime.label()
            )));
        }
    } else if record.command.is_some() || record.cwd.is_some() || !record.env.is_empty() {
        return Err(ServicesError::bad_request(
            "command, cwd and env are only for managed services",
        ));
    }
    Ok(())
}

/// The lowest port in the configured range that no record on this host uses
/// and that `is_free` (the OS check) accepts.
pub fn allocate_port(
    settings: &ServicesSettings,
    used: &HashSet<u16>,
    is_free: impl Fn(u16) -> bool,
) -> Option<u16> {
    (settings.port_range_start..=settings.port_range_end)
        .filter(|port| *port != 0 && !used.contains(port))
        .find(|port| is_free(*port))
}

/// Free on this host: nothing accepts on loopback and the IPv4 loopback
/// address can be bound. Never binds a wildcard/LAN address (that would raise
/// a Windows Firewall prompt for the daemon).
pub fn os_port_free(port: u16) -> bool {
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&loopback, Duration::from_millis(50)).is_ok() {
        return false;
    }
    TcpListener::bind(loopback).is_ok()
}

fn ensure_owns(record: &ServiceRecord, caller: &Caller) -> ServicesResult<()> {
    match caller {
        Caller::Owner => Ok(()),
        Caller::Agent {
            cutex_session_id, ..
        } => {
            if record.owner_cutex_session_id.as_deref() == Some(cutex_session_id.as_str()) {
                Ok(())
            } else {
                Err(ServicesError::new(
                    403,
                    "not_service_owner",
                    format!(
                        "service `{}` belongs to another agent or the owner",
                        record.slug
                    ),
                ))
            }
        }
    }
}

fn port_conflict(registry: &Registry, slug: &str, host: &str, port: u16) -> ServicesResult<()> {
    if port == 0 {
        return Ok(());
    }
    if let Some(other) = registry
        .services
        .values()
        .find(|other| other.slug != slug && other.host == host && other.port == port)
    {
        return Err(ServicesError::new(
            409,
            "port_in_use",
            format!("port {port} is already registered to `{}`", other.slug),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ServiceStore {
    dir: PathBuf,
    host: String,
    port_probe: fn(u16) -> bool,
}

impl ServiceStore {
    pub fn new(dir: impl Into<PathBuf>, host: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            host: host.into(),
            port_probe: os_port_free,
        }
    }

    /// `<cutex home>/services` for this host.
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::new(
            crate::config::paths::config_dir()?.join("services"),
            crate::platform::host::current_host_name(),
        ))
    }

    #[cfg(test)]
    pub fn with_port_probe(mut self, probe: fn(u16) -> bool) -> Self {
        self.port_probe = probe;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn registry_path(&self) -> PathBuf {
        self.dir.join(REGISTRY_FILE)
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn log_path(&self, slug: &str) -> PathBuf {
        self.logs_dir().join(format!("{slug}.log"))
    }

    pub fn load(&self) -> ServicesResult<Registry> {
        read_registry(&self.registry_path()).map_err(ServicesError::unavailable)
    }

    pub fn get(&self, slug: &str) -> ServicesResult<ServiceRecord> {
        let mut registry = self.load()?;
        registry.services.remove(slug).ok_or_else(|| {
            if let Some(tombstone) = registry.tombstones.get(slug) {
                ServicesError::new(
                    410,
                    "service_reclaimed",
                    format!(
                        "service was reclaimed: {} at {}",
                        tombstone.reason, tombstone.reclaimed_at
                    ),
                )
            } else {
                ServicesError::not_found(slug)
            }
        })
    }

    pub fn get_tombstone(&self, slug: &str) -> ServicesResult<Option<ServiceTombstone>> {
        Ok(self.load()?.tombstones.remove(slug))
    }

    /// Locked read-modify-write; bumps `revision` when anything changed.
    pub fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Registry) -> ServicesResult<T>,
    ) -> ServicesResult<T> {
        std::fs::create_dir_all(&self.dir).map_err(ServicesError::unavailable)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("registry.lock"))
            .map_err(ServicesError::unavailable)?;
        lock.lock_exclusive().map_err(ServicesError::unavailable)?;
        let path = self.registry_path();
        let mut registry = read_registry(&path).map_err(ServicesError::unavailable)?;
        let before = registry.clone();
        if registry.schema_version < 1 {
            // Under the registry lock, preserve the exact old bytes once, before
            // writing any new-format record. A restart never replaces the backup.
            if path.exists() {
                let backup = self.dir.join("registry.json.pre-services-proxy");
                if !backup.exists() {
                    let bytes = std::fs::read(&path).map_err(ServicesError::unavailable)?;
                    crate::config::atomic::write_private_bytes_atomic(&backup, &bytes)
                        .map_err(ServicesError::unavailable)?;
                }
            }
            registry.schema_version = 1;
        }
        let result = f(&mut registry)?;
        if registry != before {
            registry.revision = before.revision + 1;
            crate::config::atomic::write_private_pretty_json_atomic(
                &path,
                &registry,
                "services registry",
            )
            .map_err(ServicesError::unavailable)?;
        }
        let _ = fs2::FileExt::unlock(&lock);
        Ok(result)
    }

    /// Register (or, for an agent re-registering its own slug, update) a
    /// service. Returns the record and whether it was newly created.
    pub fn register(
        &self,
        request: RegisterRequest,
        caller: &Caller,
    ) -> ServicesResult<(ServiceRecord, bool)> {
        validate_slug(&request.slug)?;
        let runtime = match request.runtime.as_deref() {
            None => ServiceRuntime::External,
            Some(value) => ServiceRuntime::parse(value).ok_or_else(|| {
                ServicesError::bad_request("runtime must be external, managed or static")
            })?,
        };
        if runtime == ServiceRuntime::Static
            && (request.port.is_some()
                || request.command.is_some()
                || request.env.is_some()
                || request.cwd.is_some())
        {
            return Err(ServicesError::bad_request(
                "static forbids port, command, cwd and env",
            ));
        }
        let root = request
            .root
            .as_deref()
            .map(|root| super::static_files::validate_root(root, &self.dir, caller))
            .transpose()?;
        let requested_lifetime = request
            .lifetime
            .as_deref()
            .map(ServiceLifetime::parse)
            .transpose()?;
        let display_name =
            validate_display_name(request.display_name.as_deref().unwrap_or(&request.slug))?;
        let path = request.path.as_deref().map(validate_path).transpose()?;
        let cwd = request.cwd.as_deref().map(validate_cwd).transpose()?;
        if let Some(command) = &request.command {
            validate_command(command)?;
        }
        if let Some(command) = &request.login_link_command {
            owner_only_login_link(caller)?;
            validate_command(command)?;
        }
        let env = request.env.clone().unwrap_or_default();
        if env.len() > MAX_ENV_ENTRIES {
            return Err(ServicesError::bad_request("too many env entries"));
        }
        for (key, value) in &env {
            validate_env_entry(key, Some(value))?;
        }
        if request.port == Some(0) {
            return Err(ServicesError::bad_request("port must be 1-65535"));
        }
        let (owner, project) = match caller {
            Caller::Owner => (
                request
                    .owner_cutex_session_id
                    .as_deref()
                    .map(validate_session_id)
                    .transpose()?,
                request
                    .project_id
                    .as_deref()
                    .map(validate_project_id)
                    .transpose()?,
            ),
            Caller::Agent {
                cutex_session_id,
                project_id,
            } => {
                if request.owner_cutex_session_id.is_some() || request.project_id.is_some() {
                    return Err(ServicesError::bad_request(
                        "owner and project come from the calling agent",
                    ));
                }
                (Some(cutex_session_id.clone()), project_id.clone())
            }
        };
        let host = self.host.clone();
        let probe = self.port_probe;
        self.mutate(|registry| {
            let existing = registry.services.get(&request.slug).cloned();
            if let Some(existing) = &existing {
                match caller {
                    Caller::Agent { .. } => ensure_owns(existing, caller).map_err(|_| {
                        ServicesError::new(
                            409,
                            "slug_taken",
                            format!(
                                "slug `{}` is registered by someone else; pick another slug",
                                request.slug
                            ),
                        )
                    })?,
                    Caller::Owner => {
                        return Err(ServicesError::new(
                            409,
                            "slug_taken",
                            format!("slug `{}` is already registered", request.slug),
                        ))
                    }
                }
            } else if registry.services.len() >= MAX_SERVICES {
                return Err(ServicesError::new(
                    409,
                    "too_many_services",
                    format!("at most {MAX_SERVICES} services per host"),
                ));
            }
            let (port, port_allocated) = if runtime == ServiceRuntime::Static {
                (0, false)
            } else {
                match (request.port, &existing) {
                    (Some(port), _) => (port, false),
                    (None, Some(existing)) if existing.port != 0 => {
                        (existing.port, existing.port_allocated)
                    }
                    (None, _) => {
                        let used: HashSet<u16> = registry
                            .services
                            .values()
                            .filter(|record| record.host == host)
                            .map(|record| record.port)
                            .collect();
                        let port =
                            allocate_port(&registry.settings, &used, probe).ok_or_else(|| {
                                ServicesError::new(
                                    409,
                                    "no_free_port",
                                    format!(
                                        "no free port in {}-{}",
                                        registry.settings.port_range_start,
                                        registry.settings.port_range_end
                                    ),
                                )
                            })?;
                        (port, true)
                    }
                }
            };
            port_conflict(registry, &request.slug, &host, port)?;
            let now = now_rfc3339();
            let record = ServiceRecord {
                proxy_host_header: validate_proxy_host_header(
                    request.proxy_host_header.as_deref().unwrap_or("preserve"),
                )?,
                slug: request.slug.clone(),
                display_name: display_name.clone(),
                host: host.clone(),
                port,
                path: path.clone(),
                owner_cutex_session_id: owner.clone(),
                project_id: project.clone(),
                runtime,
                lifetime: requested_lifetime.unwrap_or_else(|| {
                    existing.as_ref().map(|r| r.lifetime).unwrap_or(
                        if matches!(caller, Caller::Owner) {
                            ServiceLifetime::Persistent
                        } else if runtime == ServiceRuntime::External {
                            registry.settings.agent_external_default_lifetime
                        } else {
                            ServiceLifetime::Ephemeral
                        },
                    )
                }),
                root: root.clone(),
                command: request.command.clone(),
                cwd: cwd.clone(),
                env: env.clone(),
                login_link_command: request.login_link_command.clone(),
                stopped: false,
                port_allocated,
                ceiling: ceiling_for(runtime),
                provenance: Some(provenance_for(caller)),
                created_at: existing
                    .as_ref()
                    .map(|record| record.created_at.clone())
                    .unwrap_or_else(|| now.clone()),
                updated_at: now,
            };
            validate_runtime_fields(&record)?;
            let unchanged = existing.as_ref().is_some_and(|existing| {
                ServiceRecord {
                    updated_at: existing.updated_at.clone(),
                    ..record.clone()
                } == *existing
            });
            if unchanged {
                return Ok((existing.unwrap(), false));
            }
            registry.tombstones.remove(&record.slug);
            if existing.is_none() {
                super::lifecycle::for_store(self).forget(&record.slug);
            }
            registry
                .services
                .insert(record.slug.clone(), record.clone());
            Ok((record, existing.is_none()))
        })
    }

    pub fn update(
        &self,
        slug: &str,
        request: UpdateRequest,
        caller: &Caller,
    ) -> ServicesResult<ServiceRecord> {
        if matches!(caller, Caller::Agent { .. })
            && (request.owner_cutex_session_id.is_some() || request.project_id.is_some())
        {
            return Err(ServicesError::bad_request(
                "owner and project come from the calling agent",
            ));
        }
        let host = self.host.clone();
        self.mutate(|registry| {
            let mut record = registry
                .services
                .get(slug)
                .cloned()
                .ok_or_else(|| ServicesError::not_found(slug))?;
            ensure_owns(&record, caller)?;
            let before = record.clone();
            if let Some(value) = &request.proxy_host_header {
                record.proxy_host_header = validate_proxy_host_header(value)?;
            }
            if let Some(lifetime) = &request.lifetime {
                record.lifetime = ServiceLifetime::parse(lifetime)?;
            }
            if let Some(root) = &request.root {
                record.root = Some(super::static_files::validate_root(root, &self.dir, caller)?);
            }
            if let Some(name) = &request.display_name {
                record.display_name = validate_display_name(name)?;
            }
            if let Some(path) = &request.path {
                record.path = path.as_deref().map(validate_path).transpose()?;
            }
            if let Some(port) = request.port {
                if port == 0 {
                    return Err(ServicesError::bad_request("port must be 1-65535"));
                }
                port_conflict(registry, slug, &host, port)?;
                record.port = port;
                record.port_allocated = false;
            }
            if let Some(runtime) = &request.runtime {
                record.runtime = ServiceRuntime::parse(runtime).ok_or_else(|| {
                    ServicesError::bad_request("runtime must be external, managed or static")
                })?;
            }
            if let Some(command) = &request.command {
                if let Some(command) = command {
                    validate_command(command)?;
                }
                record.command = command.clone();
            }
            if let Some(cwd) = &request.cwd {
                record.cwd = cwd.as_deref().map(validate_cwd).transpose()?;
            }
            if let Some(command) = &request.login_link_command {
                owner_only_login_link(caller)?;
                if let Some(command) = command {
                    validate_command(command)?;
                }
                record.login_link_command = command.clone();
            }
            if let Some(env) = &request.env {
                for (key, value) in env {
                    validate_env_entry(key, value.as_deref())?;
                    match value {
                        Some(value) => {
                            record.env.insert(key.clone(), value.clone());
                        }
                        None => {
                            record.env.remove(key);
                        }
                    }
                }
                if record.env.len() > MAX_ENV_ENTRIES {
                    return Err(ServicesError::bad_request("too many env entries"));
                }
            }
            if let Some(owner) = &request.owner_cutex_session_id {
                record.owner_cutex_session_id =
                    owner.as_deref().map(validate_session_id).transpose()?;
            }
            if let Some(project) = &request.project_id {
                record.project_id = project.as_deref().map(validate_project_id).transpose()?;
            }
            if record.runtime != ServiceRuntime::External
                && before.runtime == ServiceRuntime::External
            {
                record.stopped = false;
            }
            if record.runtime == ServiceRuntime::Static {
                if request.port.is_some() || request.env.is_some() {
                    return Err(ServicesError::bad_request("static forbids port and env"));
                }
                record.port = 0;
                record.port_allocated = false;
            }
            validate_runtime_fields(&record)?;
            record.ceiling = ceiling_for(record.runtime);
            if record != before {
                record.updated_at = now_rfc3339();
                registry.services.insert(slug.to_string(), record.clone());
            }
            Ok(record)
        })
    }

    pub fn remove(&self, slug: &str, caller: &Caller) -> ServicesResult<ServiceRecord> {
        self.mutate(|registry| {
            let record = registry
                .services
                .get(slug)
                .cloned()
                .ok_or_else(|| ServicesError::not_found(slug))?;
            ensure_owns(&record, caller)?;
            registry.services.remove(slug);
            super::lifecycle::for_store(self).forget(slug);
            Ok(record)
        })
    }

    /// Persist the managed `stopped` flag (start/stop from the owner).
    pub fn set_stopped(&self, slug: &str, stopped: bool) -> ServicesResult<ServiceRecord> {
        self.mutate(|registry| {
            let record = registry
                .services
                .get_mut(slug)
                .ok_or_else(|| ServicesError::not_found(slug))?;
            if record.stopped != stopped {
                record.stopped = stopped;
                record.updated_at = now_rfc3339();
            }
            Ok(record.clone())
        })
    }

    pub fn update_settings(&self, patch: SettingsPatch) -> ServicesResult<ServicesSettings> {
        self.mutate(|registry| {
            let mut settings = registry.settings.clone();
            if let Some(value) = &patch.agent_external_default_lifetime {
                settings.agent_external_default_lifetime = ServiceLifetime::parse(value)?;
            }
            if let Some(value) = &patch.agent_persistent_approval {
                settings.agent_persistent_approval = AgentPersistentApproval::parse(value)?;
            }
            if let Some(value) = patch.proxy_max_connections {
                if !(16..=1024).contains(&value) {
                    return Err(ServicesError::bad_request(
                        "proxyMaxConnections must be 16-1024",
                    ));
                }
                settings.proxy_max_connections = value;
            }
            if let Some(start) = patch.port_range_start {
                settings.port_range_start = start;
            }
            if let Some(end) = patch.port_range_end {
                settings.port_range_end = end;
            }
            if settings.port_range_start < 1024
                || settings.port_range_start > settings.port_range_end
            {
                return Err(ServicesError::bad_request(
                    "port range must be within 1024-65535 with start <= end",
                ));
            }
            if let Some(v) = patch.ephemeral_idle_minutes {
                settings.ephemeral_idle_minutes = v;
            }
            if let Some(v) = patch.ephemeral_ttl_hours {
                settings.ephemeral_ttl_hours = v;
            }
            if let Some(v) = patch.reclaim_on_owner_archive {
                settings.reclaim_on_owner_archive = v;
            }
            if let Some(v) = patch.tombstone_days {
                settings.tombstone_days = v;
            }
            if let Some(v) = patch.ephemeral_max_restarts {
                settings.ephemeral_max_restarts = v;
            }
            if !(5..=1440).contains(&settings.ephemeral_idle_minutes)
                || !(1..=720).contains(&settings.ephemeral_ttl_hours)
                || settings.tombstone_days > 90
                || settings.ephemeral_max_restarts > 10
            {
                return Err(ServicesError::bad_request(
                    "invalid ephemeral lifecycle setting range",
                ));
            }
            registry.settings = settings.clone();
            Ok(settings)
        })
    }

    pub fn load_state(&self) -> BTreeMap<String, RuntimeSnapshot> {
        std::fs::read(self.state_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, state: &BTreeMap<String, RuntimeSnapshot>) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.state_path(),
            state,
            "services state",
        )
    }
}

fn read_registry(path: &Path) -> anyhow::Result<Registry> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut value: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid services registry {}", path.display()))?;
            migrate_legacy_registry(&mut value);
            serde_json::from_value(value)
                .with_context(|| format!("invalid services registry {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(error) => Err(error.into()),
    }
}

/// Upgrade records written before the services simplification in place:
/// `pinned` and `settings.idleTimeoutMinutes` are dropped, and the retired
/// `onDemand` runtime becomes `managed` with `stopped: true` (no autostart;
/// `open` / `start` bring it up). Persisted on the next registry write.
fn migrate_legacy_registry(value: &mut Value) {
    if let Some(settings) = value.get_mut("settings").and_then(Value::as_object_mut) {
        settings.remove("idleTimeoutMinutes");
    }
    let Some(services) = value.get_mut("services").and_then(Value::as_object_mut) else {
        return;
    };
    for record in services.values_mut() {
        let Some(record) = record.as_object_mut() else {
            continue;
        };
        record.remove("pinned");
        let legacy_on_demand = record
            .get("runtime")
            .and_then(Value::as_str)
            .is_some_and(|runtime| matches!(runtime, "onDemand" | "on-demand" | "on_demand"));
        if legacy_on_demand {
            record.insert("runtime".into(), json!("managed"));
            record.insert("stopped".into(), json!(true));
        }
    }
}

/// Client view of a record: env values redacted (keys only), runtime facts
/// merged in, and the URLs a client opens.
pub fn service_view(record: &ServiceRecord, state: Option<&RuntimeSnapshot>) -> Value {
    let traffic = super::traffic::snapshot(&record.slug);
    let default_health = if record.runtime.supervised() {
        ServiceHealth::Stopped
    } else {
        ServiceHealth::Unreachable
    };
    let health = state.map(|state| state.health).unwrap_or(default_health);
    let mut view = json!({
        "proxyHost": format!("{}.svc.localhost", record.slug),
        "proxyHostHeader": record.proxy_host_header,
        "activeConnections": traffic.active_connections,
        "lastTrafficAt": traffic.last_traffic_at.or_else(|| state.and_then(|s|s.last_traffic_at.clone())),
        "slug": record.slug,
        "displayName": record.display_name,
        "host": record.host,
        "port": record.port,
        "path": record.path,
        "ownerCutexSessionId": record.owner_cutex_session_id,
        "projectId": record.project_id,
        "runtime": record.runtime.label(),
        "portAllocated": record.port_allocated,
        "createdAt": record.created_at,
        "updatedAt": record.updated_at,
        "lastSeenAt": state.and_then(|state| state.last_seen_at.clone()),
        "lastProbeAt": state.and_then(|state| state.last_probe_at.clone()),
        "health": health.label(),
        "url": record.url_for_host(&record.host),
        "localUrl": record.url_for_host("127.0.0.1"),
    });
    if record.runtime.supervised() {
        view["command"] = serde_json::to_value(&record.command).unwrap_or(Value::Null);
        view["cwd"] = json!(record.cwd);
        view["envKeys"] = json!(record.env.keys().collect::<Vec<_>>());
        if record.login_link_command.is_some() {
            view["loginLinkCommand"] =
                serde_json::to_value(&record.login_link_command).unwrap_or(Value::Null);
        }
        view["stopped"] = json!(record.stopped);
        view["process"] = json!({
            "pid": state.and_then(|state| state.pid),
            "startedAt": state.and_then(|state| state.started_at.clone()),
            "restarts": state.map(|state| state.restarts).unwrap_or(0),
            "lastExitCode": state.and_then(|state| state.last_exit_code),
            "lastExitAt": state.and_then(|state| state.last_exit_at.clone()),
            "nextRestartAt": state.and_then(|state| state.next_restart_at.clone()),
        });
    }
    if record.runtime == ServiceRuntime::Static {
        view["port"] = Value::Null;
        view["url"] = Value::Null;
        view["localUrl"] = Value::Null;
        view["root"] = json!(record.root);
        if state.is_none() {
            view["health"] = json!(if record
                .root
                .as_deref()
                .is_some_and(|p| Path::new(p).exists())
            {
                "reachable"
            } else {
                "unreachable"
            });
        }
    }
    view["lifetime"] = json!(record.lifetime);
    view["link"] = json!(service_link(record));
    view
}

/// Module API v1.7 decodes the key once; encode URL delimiters and existing
/// percent escapes so query strings and encoded paths survive that decoding.
pub fn service_link(record: &ServiceRecord) -> String {
    let mut link = format!("cutex:service/{}", record.slug);
    for byte in record.path.as_deref().unwrap_or("/").bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            link.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(link, "%{byte:02X}");
        }
    }
    link
}

/// Filters for `GET /v2/services` / `service_list`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFilter {
    pub project: Option<String>,
    pub owner: Option<String>,
    pub health: Option<ServiceHealth>,
}

/// Records matching `filter`, by display name.
pub fn list_views(
    registry: &Registry,
    state: &BTreeMap<String, RuntimeSnapshot>,
    filter: &ListFilter,
) -> Vec<Value> {
    let mut records: Vec<&ServiceRecord> = registry
        .services
        .values()
        .filter(|record| {
            filter
                .project
                .as_deref()
                .is_none_or(|project| record.project_id.as_deref() == Some(project))
        })
        .filter(|record| {
            filter
                .owner
                .as_deref()
                .is_none_or(|owner| record.owner_cutex_session_id.as_deref() == Some(owner))
        })
        .collect();
    records.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
            .then_with(|| left.slug.cmp(&right.slug))
    });
    records
        .into_iter()
        .map(|record| service_view(record, state.get(&record.slug)))
        .filter(|view| {
            filter
                .health
                .is_none_or(|health| view["health"] == health.label())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> ServiceStore {
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-store-{}",
            uuid::Uuid::new_v4().simple()
        ));
        ServiceStore::new(dir, "testhost").with_port_probe(|_| true)
    }

    fn agent(session: &str) -> Caller {
        Caller::Agent {
            cutex_session_id: session.to_string(),
            project_id: Some("project-a".to_string()),
        }
    }

    #[test]
    fn login_link_command_is_owner_only_and_managed_only() {
        let store = temp_store();
        let login = || Some(ServiceCommand::Shell("app url".into()));
        let managed = |slug: &str| RegisterRequest {
            slug: slug.into(),
            runtime: Some("managed".into()),
            command: Some(ServiceCommand::Shell("app serve {port}".into())),
            login_link_command: login(),
            ..Default::default()
        };
        let error = store
            .register(managed("agent-one"), &agent(S1))
            .unwrap_err();
        assert_eq!((error.status, error.code), (403, "owner_only"));
        let error = store
            .register(
                RegisterRequest {
                    slug: "ext".into(),
                    port: Some(31001),
                    login_link_command: login(),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap_err();
        assert_eq!(error.status, 400);
        let (record, _) = store.register(managed("owned"), &Caller::Owner).unwrap();
        assert_eq!(record.login_link_command, login());
        let view = service_view(&record, None);
        assert_eq!(view["loginLinkCommand"], "app url");
        // Reloaded from disk unchanged; PATCH null clears it; agents cannot set it.
        assert_eq!(store.get("owned").unwrap().login_link_command, login());
        let error = store
            .update(
                "owned",
                serde_json::from_value(serde_json::json!({"loginLinkCommand": "x"})).unwrap(),
                &agent(S1),
            )
            .unwrap_err();
        assert_eq!(error.status, 403);
        let cleared = store
            .update(
                "owned",
                serde_json::from_value(serde_json::json!({"loginLinkCommand": null})).unwrap(),
                &Caller::Owner,
            )
            .unwrap();
        assert_eq!(cleared.login_link_command, None);
        assert!(service_view(&cleared, None)
            .get("loginLinkCommand")
            .is_none());
        let _ = std::fs::remove_dir_all(store.dir());
    }

    const S1: &str = "cutex.00000000-0000-0000-0000-000000000001";
    const S2: &str = "cutex.00000000-0000-0000-0000-000000000002";

    fn register(slug: &str) -> RegisterRequest {
        RegisterRequest {
            slug: slug.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn slug_validation() {
        for ok in ["a", "web", "my-app-2", "0x"] {
            assert!(validate_slug(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a b", &"x".repeat(64)] {
            assert!(validate_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lifetime_defaults_settings_and_slug_reuse() {
        let store = temp_store();
        let (owner, _) = store.register(register("owner"), &Caller::Owner).unwrap();
        let (agent, _) = store.register(register("agent"), &agent(S1)).unwrap();
        assert_eq!(owner.lifetime, ServiceLifetime::Persistent);
        assert_eq!(agent.lifetime, ServiceLifetime::Ephemeral);
        let mut legacy = serde_json::to_value(&owner).unwrap();
        legacy.as_object_mut().unwrap().remove("lifetime");
        assert_eq!(
            serde_json::from_value::<ServiceRecord>(legacy)
                .unwrap()
                .lifetime,
            ServiceLifetime::Persistent
        );
        for invalid in [
            json!({"ephemeralIdleMinutes":4}),
            json!({"ephemeralTtlHours":721}),
            json!({"tombstoneDays":91}),
            json!({"ephemeralMaxRestarts":11}),
        ] {
            assert!(store
                .update_settings(serde_json::from_value(invalid).unwrap())
                .is_err());
        }
        store
            .mutate(|registry| {
                registry.services.remove("agent");
                registry.tombstones.insert(
                    "agent".into(),
                    super::super::lifecycle::tombstone(&agent, "idle", chrono::Utc::now()),
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(store.get("agent").unwrap_err().status, 410);
        store.register(register("agent"), &Caller::Owner).unwrap();
        assert!(store.get_tombstone("agent").unwrap().is_none());
        std::fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn static_services_have_no_port_and_cannot_publish_home() {
        let base =
            std::env::temp_dir().join(format!("cutex-static-store-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(base.join("home/services"), "host");
        let pages = base.join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(pages.join("report.html"), "ok").unwrap();
        let mut request = register("report");
        request.runtime = Some("static".into());
        request.root = Some(pages.display().to_string());
        let (record, _) = store.register(request.clone(), &Caller::Owner).unwrap();
        assert_eq!(record.port, 0);
        assert!(!record.port_allocated);
        let view = service_view(&record, None);
        assert!(view["port"].is_null() && view["url"].is_null() && view["localUrl"].is_null());
        assert_eq!(view["health"], "reachable");
        request.slug = "second".into();
        request.root = Some(pages.join("report.html").display().to_string());
        store.register(request.clone(), &Caller::Owner).unwrap();
        request.slug = "forbidden".into();
        request.root = Some(base.join("home").display().to_string());
        assert_eq!(
            store
                .register(request.clone(), &Caller::Owner)
                .unwrap_err()
                .code,
            "static_root_forbidden"
        );
        request.root = Some(pages.display().to_string());
        request.port = Some(12345);
        assert!(store.register(request, &Caller::Owner).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn migration_preserves_original_once_and_keeps_existing_persistent() {
        let store = temp_store();
        std::fs::create_dir_all(store.dir()).unwrap();
        let legacy = br#"{"revision":7,"settings":{"portRangeStart":26180,"portRangeEnd":26199},"services":{"old":{"slug":"old","displayName":"Old","host":"host","port":26180,"runtime":"external","createdAt":"2026-10-11T00:00:00Z","updatedAt":"2026-10-11T00:00:00Z"}}}"#;
        std::fs::write(store.registry_path(), legacy).unwrap();
        store.mutate(|_| Ok(())).unwrap();
        assert_eq!(store.load().unwrap().schema_version, 1);
        assert_eq!(
            store.get("old").unwrap().lifetime,
            ServiceLifetime::Persistent
        );
        let backup = store.dir().join("registry.json.pre-services-proxy");
        assert_eq!(std::fs::read(&backup).unwrap(), legacy);
        let migrated = std::fs::read(store.registry_path()).unwrap();
        store.mutate(|_| Ok(())).unwrap();
        assert_eq!(std::fs::read(store.registry_path()).unwrap(), migrated);
        store.register(register("new"), &Caller::Owner).unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), legacy);
        std::fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn external_default_setting_and_module_link_roundtrip() {
        let store = temp_store();
        store
            .update_settings(SettingsPatch {
                agent_external_default_lifetime: Some("persistent".into()),
                ..Default::default()
            })
            .unwrap();
        let caller = Caller::Agent {
            cutex_session_id: "cutex.00000000-0000-0000-0000-000000000001".into(),
            project_id: None,
        };
        let (mut record, _) = store
            .register(register("external-default"), &caller)
            .unwrap();
        assert_eq!(record.lifetime, ServiceLifetime::Persistent);
        assert!(store
            .update_settings(SettingsPatch {
                agent_external_default_lifetime: Some("forever".into()),
                ..Default::default()
            })
            .is_err());
        // agentPersistentApproval: default ask, accepts ask / off only, and a
        // registry written before the setting existed reads as ask.
        assert_eq!(
            store.load().unwrap().settings.agent_persistent_approval,
            AgentPersistentApproval::Ask
        );
        let settings = store
            .update_settings(SettingsPatch {
                agent_persistent_approval: Some("off".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(settings.agent_persistent_approval, AgentPersistentApproval::Off);
        assert_eq!(
            serde_json::to_value(&settings).unwrap()["agentPersistentApproval"],
            "off"
        );
        assert!(store
            .update_settings(SettingsPatch {
                agent_persistent_approval: Some("never".into()),
                ..Default::default()
            })
            .is_err());
        let legacy: ServicesSettings =
            serde_json::from_value(json!({"agentExternalDefaultLifetime": "persistent"})).unwrap();
        assert_eq!(legacy.agent_persistent_approval, AgentPersistentApproval::Ask);
        record.path = Some("/report%20one?q=x#part".into());
        assert_eq!(
            service_link(&record),
            "cutex:service/external-default/report%2520one%3Fq%3Dx%23part"
        );
        std::fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn allocation_skips_registry_and_os_ports() {
        let settings = ServicesSettings {
            port_range_start: 30000,
            port_range_end: 30005,
            ..Default::default()
        };
        let used: HashSet<u16> = [30000, 30001].into_iter().collect();
        assert_eq!(allocate_port(&settings, &used, |_| true), Some(30002));
        assert_eq!(
            allocate_port(&settings, &used, |port| port > 30003),
            Some(30004)
        );
        assert_eq!(allocate_port(&settings, &used, |_| false), None);
    }

    #[test]
    fn os_port_check_sees_a_bound_listener() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!os_port_free(port));
    }

    #[test]
    fn register_allocates_persists_and_rejects_conflicts() {
        let store = temp_store();
        let (first, created) = store.register(register("web"), &Caller::Owner).unwrap();
        assert!(created);
        assert_eq!(first.port, DEFAULT_PORT_RANGE_START);
        assert!(first.port_allocated);
        assert_eq!(first.host, "testhost");
        assert_eq!(first.runtime, ServiceRuntime::External);
        let (second, _) = store.register(register("api"), &Caller::Owner).unwrap();
        assert_eq!(second.port, DEFAULT_PORT_RANGE_START + 1);
        // Same slug again from the owner: conflict.
        assert_eq!(
            store
                .register(register("web"), &Caller::Owner)
                .unwrap_err()
                .code,
            "slug_taken"
        );
        // Explicit port already registered.
        let mut clash = register("clash");
        clash.port = Some(first.port);
        assert_eq!(
            store.register(clash, &Caller::Owner).unwrap_err().code,
            "port_in_use"
        );
        let registry = store.load().unwrap();
        assert_eq!(registry.services.len(), 2);
        assert_eq!(registry.revision, 2);
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn supervised_runtime_requires_command_and_external_rejects_it() {
        let store = temp_store();
        let mut managed = register("m");
        managed.runtime = Some("managed".into());
        assert_eq!(
            store
                .register(managed.clone(), &Caller::Owner)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        managed.command = Some(ServiceCommand::Shell("python -m http.server {port}".into()));
        managed.env = Some([("TOKEN".to_string(), "secret".to_string())].into());
        let (record, _) = store.register(managed, &Caller::Owner).unwrap();
        let view = service_view(&record, None);
        assert_eq!(view["envKeys"], json!(["TOKEN"]));
        assert!(!view.to_string().contains("secret"));
        assert_eq!(view["health"], "stopped");
        // Asks design §8.5: the record carries its execution ceiling and
        // who registered it.
        assert_eq!(record.ceiling.as_deref(), Some("unrestricted"));
        assert_eq!(
            record.provenance.as_ref().map(|p| p.registrant.as_str()),
            Some("owner")
        );
        let (plain, _) = store.register(register("plain"), &Caller::Owner).unwrap();
        assert_eq!(plain.ceiling, None);
        let mut external = register("e");
        external.command = Some(ServiceCommand::Argv(vec!["x".into()]));
        assert!(store.register(external, &Caller::Owner).is_err());
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn agents_own_their_services() {
        let store = temp_store();
        let (record, created) = store.register(register("mine"), &agent(S1)).unwrap();
        assert!(created);
        assert_eq!(record.owner_cutex_session_id.as_deref(), Some(S1));
        assert_eq!(record.project_id.as_deref(), Some("project-a"));
        // Re-registering its own slug is an idempotent update.
        let mut again = register("mine");
        again.path = Some("/health".into());
        let (updated, created) = store.register(again, &agent(S1)).unwrap();
        assert!(!created);
        assert_eq!(updated.port, record.port);
        assert_eq!(updated.path.as_deref(), Some("/health"));
        // Another agent cannot take, update or remove it.
        assert_eq!(
            store
                .register(register("mine"), &agent(S2))
                .unwrap_err()
                .code,
            "slug_taken"
        );
        assert_eq!(
            store
                .update("mine", UpdateRequest::default(), &agent(S2))
                .unwrap_err()
                .code,
            "not_service_owner"
        );
        assert_eq!(
            store.remove("mine", &agent(S2)).unwrap_err().code,
            "not_service_owner"
        );
        // Agents cannot claim an owner/project.
        let mut spoof = register("spoof");
        spoof.owner_cutex_session_id = Some(S2.into());
        assert!(store.register(spoof, &agent(S1)).is_err());
        // Owner-registered services are not agent-editable.
        store.register(register("owned"), &Caller::Owner).unwrap();
        assert!(store.remove("owned", &agent(S1)).is_err());
        // The owner can edit anything; the agent removes its own.
        store
            .update(
                "mine",
                UpdateRequest {
                    display_name: Some("Renamed".into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        store.remove("mine", &agent(S1)).unwrap();
        assert!(store.get("mine").is_err());
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn update_merges_env_and_clears_nullable_fields() {
        let store = temp_store();
        let mut request = register("svc");
        request.runtime = Some("managed".into());
        request.command = Some(ServiceCommand::Shell("serve".into()));
        request.path = Some("/x".into());
        request.env = Some([("A".to_string(), "1".to_string())].into());
        store.register(request, &Caller::Owner).unwrap();
        let patch: UpdateRequest = serde_json::from_value(
            json!({"path": null, "env": {"A": null, "B": "2"}, "port": 31000}),
        )
        .unwrap();
        let record = store.update("svc", patch, &Caller::Owner).unwrap();
        assert_eq!(record.path, None);
        assert_eq!(record.port, 31000);
        assert_eq!(record.env, [("B".to_string(), "2".to_string())].into());
        assert!(serde_json::from_value::<UpdateRequest>(json!({"bogus": 1})).is_err());
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn settings_patch_is_validated_and_list_filters_and_sorts_by_name() {
        let store = temp_store();
        assert!(store
            .update_settings(SettingsPatch {
                port_range_start: Some(500),
                ..Default::default()
            })
            .is_err());
        let settings = store
            .update_settings(SettingsPatch {
                port_range_start: Some(40000),
                port_range_end: Some(40010),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(settings.port_range_end, 40010);
        assert!(serde_json::from_value::<SettingsPatch>(json!({"idleTimeoutMinutes": 5})).is_err());
        let (b, _) = store.register(register("b"), &agent(S1)).unwrap();
        assert_eq!(b.port, 40000);
        let mut a = register("a");
        a.project_id = Some("project-x".into());
        store.register(a, &Caller::Owner).unwrap();
        let registry = store.load().unwrap();
        let views = list_views(&registry, &BTreeMap::new(), &ListFilter::default());
        assert_eq!(views[0]["slug"], "a");
        assert_eq!(views[1]["slug"], "b");
        assert!(views[0].get("pinned").is_none());
        let owned = list_views(
            &registry,
            &BTreeMap::new(),
            &ListFilter {
                owner: Some(S1.into()),
                ..Default::default()
            },
        );
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0]["slug"], "b");
        let filtered = list_views(
            &registry,
            &BTreeMap::new(),
            &ListFilter {
                project: Some("project-x".into()),
                ..Default::default()
            },
        );
        assert_eq!(filtered.len(), 1);
        let by_health = list_views(
            &registry,
            &BTreeMap::new(),
            &ListFilter {
                health: Some(ServiceHealth::Reachable),
                ..Default::default()
            },
        );
        assert!(by_health.is_empty());
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn legacy_pinned_and_on_demand_records_load_as_managed_and_stopped() {
        let store = temp_store();
        std::fs::create_dir_all(store.dir()).unwrap();
        let legacy = json!({
            "revision": 7,
            "settings": {"portRangeStart": 24400, "portRangeEnd": 24999, "idleTimeoutMinutes": 30},
            "services": {
                "docs": {
                    "slug": "docs", "displayName": "Docs", "host": "testhost", "port": 24401,
                    "runtime": "onDemand", "command": "serve {port}", "pinned": true,
                    "stopped": false, "portAllocated": true,
                    "createdAt": "2026-10-01T00:00:00.000Z", "updatedAt": "2026-10-01T00:00:00.000Z"
                },
                "web": {
                    "slug": "web", "displayName": "Web", "host": "testhost", "port": 24402,
                    "runtime": "external", "pinned": false,
                    "createdAt": "2026-10-01T00:00:00.000Z", "updatedAt": "2026-10-01T00:00:00.000Z"
                }
            }
        });
        std::fs::write(store.registry_path(), legacy.to_string()).unwrap();
        let registry = store.load().unwrap();
        assert_eq!(registry.revision, 7);
        let docs = &registry.services["docs"];
        assert_eq!(docs.runtime, ServiceRuntime::Managed);
        assert!(docs.stopped);
        assert_eq!(registry.services["web"].runtime, ServiceRuntime::External);
        // The next write persists the migrated shape.
        store
            .update(
                "web",
                UpdateRequest {
                    display_name: Some("Web 2".into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let raw = std::fs::read_to_string(store.registry_path()).unwrap();
        assert!(
            !raw.contains("pinned") && !raw.contains("onDemand") && !raw.contains("idleTimeout")
        );
        // The retired runtime is no longer accepted on input.
        let mut on_demand = register("od");
        on_demand.runtime = Some("onDemand".into());
        on_demand.command = Some(ServiceCommand::Shell("serve".into()));
        assert_eq!(
            store.register(on_demand, &Caller::Owner).unwrap_err().code,
            "invalid_request"
        );
        assert!(serde_json::from_value::<UpdateRequest>(json!({"pinned": true})).is_err());
        let _ = std::fs::remove_dir_all(store.dir());
    }
}
