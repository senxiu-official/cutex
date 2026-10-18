//! Claude Code settings sources and the host's MCP layers for one Claude
//! session (capability `claude-code-sources`).
//!
//! At launch cutex tells the adapter which Claude Code settings sources to
//! read (the profile's `claude_code.setting_sources`, `profiles::claude_code`),
//! whether to load the login's claude.ai connectors, and which external MCP
//! servers to add next to its in-process cutex server: the same three layers
//! Codex agents get (DESIGN §4.11) — the shared CODEX_HOME `[mcp_servers]`
//! (global), the project layer and the agent layer of `scoped-mcp.json`,
//! merged per name (agent > project > global), minus the agent's `disabled`
//! names, `enabled = false` entries and Cutex's own built-ins.
//!
//! Each server is translated to Claude Code's `mcpServers` format and written
//! to `mcp-servers.json` in the session state dir. Secrets never enter that
//! file or the command line: stdio `env` values, `http_headers` values and an
//! inline `bearer_token` become `${CUTEX_CLAUDE_MCP_<n>}` references that
//! Claude Code expands from its environment, and cutex passes the values only
//! in the adapter process environment (as Codex launches pass them to the
//! app-server). Any other literal containing `${` is passed the same way, so
//! Claude Code never expands text the owner wrote. `env_http_headers` /
//! `bearer_token_env_var` become `${VAR}` references to the inherited
//! environment. Fields Claude Code has no equivalent for (stdio `cwd`, OAuth,
//! header helpers, timeouts) are reported as ignored; `enabled_tools` /
//! `disabled_tools` are enforced by the adapter.
//!
//! Every adapter option is gated on its `features.json` (`settingSources`,
//! `mcpServers`, `claudeAiConnectors`): an older adapter keeps its previous
//! behaviour. A non-secret summary of what one launch passed is kept in
//! `claude-code-launch.json` for `GET /v2/sessions/{id}/claude-code`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::launch::scoped_mcp::{self, ScopeEntry};
use crate::profiles::claude_code::{self, SettingSource};
use crate::session::model::CutexSessionRecord;

pub const CAPABILITY: &str = "claude-code-sources";
pub const MCP_CONFIG_FILE: &str = "mcp-servers.json";
pub const LAUNCH_RECORD_FILE: &str = "claude-code-launch.json";
pub const ENV_PREFIX: &str = "CUTEX_CLAUDE_MCP_";
/// The adapter's own limit (config.MAX_MCP_SERVERS).
pub const MAX_SERVERS: usize = 64;
/// The adapter's in-process server, and Claude Code's connector prefix.
const RESERVED: &[&str] = &["cutex"];
const CONNECTOR_PREFIX: &str = "claude_ai";

/// What the adapter at hand understands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdapterSupport {
    pub setting_sources: bool,
    pub mcp_servers: bool,
    pub claude_ai_connectors: bool,
}

impl AdapterSupport {
    pub fn from_features(features: &Value) -> Self {
        let sources = features["settingSources"].as_array();
        Self {
            setting_sources: sources.is_some_and(|names| {
                ["project", "user"].iter().all(|name| names.iter().any(|value| value == name))
            }),
            mcp_servers: features["mcpServers"] == true,
            claude_ai_connectors: features["claudeAiConnectors"] == true,
        }
    }

    pub fn json(self) -> Value {
        json!({
            "settingSources": self.setting_sources,
            "mcpServers": self.mcp_servers,
            "claudeAiConnectors": self.claude_ai_connectors,
        })
    }
}

/// One merged server and what happens to it for a Claude agent.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerEntry {
    pub name: String,
    /// Layers defining the name, lowest first (global, project, agent).
    pub layers: Vec<&'static str>,
    pub transport: &'static str,
    /// included | disabled | skipped
    pub status: &'static str,
    /// disabled: agent | config; skipped: reserved | name | transport |
    /// unsupportedRuntime | limit
    pub reason: Option<String>,
    pub ignored_fields: Vec<String>,
}

impl ServerEntry {
    pub fn json(&self) -> Value {
        json!({
            "name": self.name,
            "layers": self.layers,
            "transport": self.transport,
            "status": self.status,
            "reason": self.reason,
            "ignoredFields": self.ignored_fields,
        })
    }
}

/// Everything one launch passes (or would pass) to the adapter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub support: AdapterSupport,
    /// The profile's sources (before adapter gating).
    pub setting_sources: Vec<SettingSource>,
    pub claude_ai_connectors: bool,
    /// Claude Code format, secrets as `${CUTEX_CLAUDE_MCP_<n>}`.
    pub servers: BTreeMap<String, Value>,
    pub tool_filters: BTreeMap<String, Value>,
    /// Secret values for the adapter process environment. Never logged.
    pub envs: Vec<(String, String)>,
    pub entries: Vec<ServerEntry>,
}

impl Plan {
    /// Sources the adapter actually gets.
    pub fn passed_sources(&self) -> Vec<SettingSource> {
        if self.support.setting_sources { self.setting_sources.clone() } else { Vec::new() }
    }

    pub fn passed_connectors(&self) -> bool {
        self.support.claude_ai_connectors && self.claude_ai_connectors
    }

    /// Whether the project source is on for this launch (CLAUDE.md is read).
    pub fn project_settings_active(&self) -> bool {
        self.passed_sources().contains(&SettingSource::Project)
    }

    /// `--setting-sources` / `--claude-ai-connectors` / `--mcp-servers-config`
    /// (the last only with servers; `config` is where the file was written).
    pub fn args(&self, config: Option<&Path>) -> Vec<std::ffi::OsString> {
        let mut args = Vec::new();
        if self.support.setting_sources {
            args.push("--setting-sources".into());
            args.push(claude_code::names(&self.setting_sources).join(",").into());
        }
        if self.support.claude_ai_connectors {
            args.push("--claude-ai-connectors".into());
            args.push(if self.claude_ai_connectors { "enabled" } else { "disabled" }.into());
        }
        if let Some(config) = config {
            args.push("--mcp-servers-config".into());
            args.push(config.as_os_str().to_owned());
        }
        args
    }

    /// The `mcp-servers.json` body (no secret values).
    pub fn config_json(&self) -> Value {
        json!({"version": 1, "servers": self.servers, "toolFilters": self.tool_filters})
    }

    /// Non-secret launch summary; also the comparison key for restart hints.
    pub fn summary(&self) -> Value {
        let included: Vec<&str> = self.entries.iter().filter(|e| e.status == "included").map(|e| e.name.as_str()).collect();
        let skipped: Vec<Value> = self
            .entries
            .iter()
            .filter(|e| e.status == "skipped")
            .map(|e| json!({"name": e.name, "reason": e.reason}))
            .collect();
        json!({
            "settingSources": claude_code::names(&self.passed_sources()),
            "claudeAiConnectors": self.passed_connectors(),
            "mcpServers": if self.support.mcp_servers { json!(included) } else { json!([]) },
            "skipped": skipped,
            // Changes of a server's (non-secret) config also need a restart.
            "mcpConfigDigest": digest(&self.config_json()),
        })
    }
}

fn digest(value: &Value) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(value.to_string().as_bytes()))[..16].to_string()
}

struct Secrets<'a> {
    envs: &'a mut Vec<(String, String)>,
}

impl Secrets<'_> {
    /// A `${CUTEX_CLAUDE_MCP_<n>}` reference to a value passed in the env.
    fn reference(&mut self, value: &str) -> String {
        if let Some((name, _)) = self.envs.iter().find(|(_, existing)| existing == value) {
            return format!("${{{name}}}");
        }
        let name = format!("{ENV_PREFIX}{}", self.envs.len() + 1);
        self.envs.push((name.clone(), value.to_string()));
        format!("${{{name}}}")
    }

    /// Owner text Claude Code must not expand: by reference when it has `${`.
    fn literal(&mut self, value: &str) -> String {
        if value.contains("${") { self.reference(value) } else { value.to_string() }
    }
}

/// Fields consumed by the translation (or meaningless for Claude).
const MAPPED: &[&str] = &[
    "command", "args", "env", "env_vars", "url", "http_headers", "env_http_headers",
    "bearer_token", "bearer_token_env_var", "enabled", "enabled_tools", "disabled_tools", "name",
];

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn string_map(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_object)
        .map(|map| map.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
        .unwrap_or_default()
}

/// One merged Codex server config -> (Claude Code config, tool filter,
/// ignored fields), or why it cannot be passed.
fn translate(
    config: &Value,
    envs: &mut Vec<(String, String)>,
) -> Result<(&'static str, Value, Option<Value>, Vec<String>), (&'static str, String)> {
    let map = config.as_object().cloned().unwrap_or_default();
    let mut secrets = Secrets { envs };
    let ignored: Vec<String> = map.keys().filter(|key| !MAPPED.contains(&key.as_str())).cloned().collect();
    let enabled = map.get("enabled_tools").map(|value| strings(Some(value)));
    let disabled = strings(map.get("disabled_tools"));
    let filter = (enabled.is_some() || !disabled.is_empty()).then(|| json!({"enabled": enabled, "disabled": disabled}));
    if let Some(command) = map.get("command").and_then(Value::as_str) {
        let mut out = Map::new();
        out.insert("type".into(), json!("stdio"));
        out.insert("command".into(), json!(secrets.literal(command)));
        let args: Vec<String> = strings(map.get("args")).iter().map(|arg| secrets.literal(arg)).collect();
        if !args.is_empty() {
            out.insert("args".into(), json!(args));
        }
        // `env_vars` name variables forwarded from the parent: Claude Code
        // already starts stdio servers with its whole environment.
        let env: Map<String, Value> = string_map(map.get("env"))
            .into_iter()
            .map(|(key, value)| (key, json!(secrets.reference(&value))))
            .collect();
        if !env.is_empty() {
            out.insert("env".into(), Value::Object(env));
        }
        return Ok(("stdio", Value::Object(out), filter, ignored));
    }
    if let Some(url) = map.get("url").and_then(Value::as_str) {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(("http", "transport".into()));
        }
        let mut headers = Map::new();
        for (header, value) in string_map(map.get("http_headers")) {
            headers.insert(header, json!(secrets.reference(&value)));
        }
        for (header, variable) in string_map(map.get("env_http_headers")) {
            headers.insert(header, json!(format!("${{{variable}}}")));
        }
        if let Some(token) = map.get("bearer_token").and_then(Value::as_str) {
            let reference = secrets.reference(token);
            headers.insert("Authorization".into(), json!(format!("Bearer {reference}")));
        } else if let Some(variable) = map.get("bearer_token_env_var").and_then(Value::as_str) {
            headers.insert("Authorization".into(), json!(format!("Bearer ${{{variable}}}")));
        }
        let mut out = Map::new();
        out.insert("type".into(), json!("http"));
        out.insert("url".into(), json!(secrets.literal(url)));
        if !headers.is_empty() {
            out.insert("headers".into(), Value::Object(headers));
        }
        return Ok(("http", Value::Object(out), filter, ignored));
    }
    Err(("unknown", "transport".into()))
}

fn claude_name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Merge the three layers and translate them (pure).
pub fn plan_from(
    support: AdapterSupport,
    setting_sources: Vec<SettingSource>,
    claude_ai_connectors: bool,
    global: &BTreeMap<String, Value>,
    project: Option<&ScopeEntry>,
    agent: Option<&ScopeEntry>,
) -> Plan {
    let mut merged: BTreeMap<String, (Vec<&'static str>, Value)> = BTreeMap::new();
    let mut add = |layer: &'static str, servers: &BTreeMap<String, Value>| {
        for (name, config) in servers {
            if scoped_mcp::BUILTIN_SERVER_NAMES.iter().any(|builtin| builtin.eq_ignore_ascii_case(name)) {
                continue;
            }
            let entry = merged.entry(name.clone()).or_insert_with(|| (Vec::new(), Value::Object(Map::new())));
            entry.0.push(layer);
            scoped_mcp::deep_merge(&mut entry.1, config);
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
    let mut plan = Plan { support, setting_sources, claude_ai_connectors, ..Plan::default() };
    for (name, (layers, config)) in merged {
        let transport = if config.get("command").is_some() { "stdio" } else if config.get("url").is_some() { "http" } else { "unknown" };
        let mut entry = ServerEntry { name: name.clone(), layers, transport, status: "included", reason: None, ignored_fields: Vec::new() };
        if disabled.iter().any(|item| **item == name) {
            (entry.status, entry.reason) = ("disabled", Some("agent".into()));
        } else if config.get("enabled") == Some(&Value::Bool(false)) {
            (entry.status, entry.reason) = ("disabled", Some("config".into()));
        } else if RESERVED.contains(&name.as_str()) || name.starts_with(CONNECTOR_PREFIX) {
            (entry.status, entry.reason) = ("skipped", Some("reserved".into()));
        } else if !claude_name_ok(&name) {
            (entry.status, entry.reason) = ("skipped", Some("name".into()));
        } else if !support.mcp_servers {
            (entry.status, entry.reason) = ("skipped", Some("unsupportedRuntime".into()));
        } else if plan.servers.len() >= MAX_SERVERS {
            (entry.status, entry.reason) = ("skipped", Some("limit".into()));
        } else {
            let mut envs = plan.envs.clone();
            match translate(&config, &mut envs) {
                Ok((transport, claude, filter, ignored)) => {
                    plan.envs = envs;
                    entry.transport = transport;
                    entry.ignored_fields = ignored;
                    plan.servers.insert(name.clone(), claude);
                    if let Some(filter) = filter {
                        plan.tool_filters.insert(name.clone(), filter);
                    }
                }
                Err((transport, reason)) => {
                    entry.transport = transport;
                    (entry.status, entry.reason) = ("skipped", Some(reason));
                }
            }
        }
        plan.entries.push(entry);
    }
    plan
}

/// The global layer's home: the selected local deployment's native home,
/// else the host CODEX_HOME (as `GET /v2/sessions/{id}/mcp/effective`).
fn global_home() -> Option<std::path::PathBuf> {
    match crate::launch::local_deployment::LocalDeployment::selected() {
        Ok(Some(deployment)) => Some(deployment.native_home),
        _ => crate::config::paths::host_codex_home_dir().ok(),
    }
}

/// The profile's sources / connectors (Claude profiles; anything else: none).
fn profile_choice(record: &CutexSessionRecord) -> anyhow::Result<(Vec<SettingSource>, bool)> {
    let Some(name) = record.profile.as_deref() else { return Ok((Vec::new(), false)) };
    let store = crate::profiles::settings_api::load_store()?;
    let Some(account) = crate::profiles::lookup::find_account(&store, name)? else { return Ok((Vec::new(), false)) };
    Ok((
        claude_code::setting_sources(account).unwrap_or_default(),
        claude_code::claude_ai_connectors(account).unwrap_or(false),
    ))
}

/// The plan for this session now (launch input and the "next" view).
pub fn plan_for(record: &CutexSessionRecord, features: &Value) -> anyhow::Result<Plan> {
    let support = AdapterSupport::from_features(features);
    let (sources, connectors) = profile_choice(record)?;
    let store = scoped_mcp::load_store()?;
    let project = scoped_mcp::session_project_id(&record.cutex_session_id)?;
    let global = match global_home() {
        Some(home) => scoped_mcp::global_servers(&home)?,
        None => BTreeMap::new(),
    };
    Ok(plan_from(
        support,
        sources,
        connectors,
        &global,
        project.as_ref().and_then(|project| store.projects.get(project)),
        store.sessions.get(&record.cutex_session_id),
    ))
}

/// Whether CLAUDE.md is read for this Claude session at its next launch
/// (the harness then leaves out project AGENTS.md files CLAUDE.md imports).
/// Any failure reads as "no" (the AGENTS.md text stays: never lose it).
pub fn project_settings_active(record: &CutexSessionRecord) -> bool {
    let Some(session) = record.claude_session.as_ref() else { return false };
    if !AdapterSupport::from_features(&super::adapter_features(&session.deployment.adapter_source)).setting_sources {
        return false;
    }
    profile_choice(record).is_ok_and(|(sources, _)| sources.contains(&SettingSource::Project))
}

/// Write `mcp-servers.json` (or remove a stale one) and return the adapter
/// args plus the env values; called by the launch.
pub fn prepare_launch(
    record: &CutexSessionRecord,
    state_dir: &Path,
    features: &Value,
) -> anyhow::Result<(Plan, Vec<std::ffi::OsString>)> {
    let plan = plan_for(record, features)?;
    let path = state_dir.join(MCP_CONFIG_FILE);
    let config = if plan.support.mcp_servers && !plan.servers.is_empty() {
        crate::config::atomic::write_private_pretty_json_atomic(&path, &plan.config_json(), "Claude MCP servers")?;
        Some(path.as_path())
    } else {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        None
    };
    let args = plan.args(config);
    Ok((plan, args))
}

/// After a confirmed start: what this launch passed (a hint only).
pub fn record_launch(state_dir: &Path, plan: &Plan) {
    let mut summary = plan.summary();
    summary["launchedAt"] = json!(chrono::Utc::now().to_rfc3339());
    if let Err(error) = crate::config::atomic::write_private_pretty_json_atomic(
        &state_dir.join(LAUNCH_RECORD_FILE),
        &summary,
        "Claude Code launch summary",
    ) {
        eprintln!("claude: launch summary not written to {}: {error:#}", state_dir.display());
    }
}

/// A running adapter launched with something else than `next` would pass.
/// No launch record (started by an older cutex): it got none of these.
pub fn restart_needed(launched: Option<&Value>, next: &Value) -> bool {
    match launched {
        Some(launched) => ["settingSources", "claudeAiConnectors", "mcpServers", "mcpConfigDigest"]
            .iter()
            .any(|key| launched.get(*key) != next.get(*key)),
        None => {
            next["settingSources"].as_array().is_some_and(|v| !v.is_empty())
                || next["claudeAiConnectors"] == true
                || next["mcpServers"].as_array().is_some_and(|v| !v.is_empty())
        }
    }
}

/// `GET /v2/sessions/{id}/claude-code` body.
pub fn view(record: &CutexSessionRecord) -> anyhow::Result<Value> {
    let session = record.claude_session.as_ref().ok_or_else(|| anyhow::anyhow!("not a Claude session"))?;
    let features = super::adapter_features(&session.deployment.adapter_source);
    let plan = plan_for(record, &features)?;
    let launched: Option<Value> = std::fs::read(session.state_dir.join(LAUNCH_RECORD_FILE))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok());
    let next = plan.summary();
    let running = record.runtime_pid.is_some();
    let restart_needed = running && restart_needed(launched.as_ref(), &next);
    Ok(json!({
        "contractVersion": 1,
        "cutexSessionId": record.cutex_session_id,
        "profile": record.profile,
        "adapterSupports": plan.support.json(),
        "profileSettingSources": claude_code::names(&plan.setting_sources),
        "profileClaudeAiConnectors": plan.claude_ai_connectors,
        "next": next,
        "servers": plan.entries.iter().map(ServerEntry::json).collect::<Vec<_>>(),
        "launched": launched,
        "running": running,
        "restartNeeded": restart_needed,
        "appliesOn": "next launch",
    }))
}

#[cfg(test)]
#[path = "code_sources_tests.rs"]
mod tests;
