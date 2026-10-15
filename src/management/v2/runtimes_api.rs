//! `GET /v2/runtimes` (capability `runtimes-view`; root credential incl.
//! client / device tokens, like `/v2/runtime-packages`): a read-only view for
//! the desktop's 运行时 settings page.
//!
//! ```text
//! { contractVersion, hostId, generatedAt,
//!   cutex: {version, release?, executable?},
//!   native: Entry[], claude: Entry[],
//!   unpinnedAgentCount }
//! Entry = { release, version?, path?, releaseDir?, sha256Prefix?, selected?,
//!           latest?, missing?, agents: [{cutexSessionId, name}], agentCount }
//! ```
//!
//! - `native`: one entry per native version in the version registry
//!   (`runtime_artifacts` / `runtime_paths`), plus one per pinned executable
//!   the registry does not know (`version` absent). `selected`: the version of
//!   the selected runtime package's executable (else the registry's latest
//!   runtime); `latest`: the registry's latest runtime. Agents: non-retired
//!   explicit-launch records whose bundle manifest pins that executable.
//! - `claude`: one entry per Claude runtime directory (`claude-r5`, …) used
//!   by the selected deployment or by a non-retired Claude session.
//! - `agents` is capped at [`MAX_AGENTS`] per entry; `agentCount` is exact.
//! - Paths are reduced to `<release dir>/<file>` (`path`) and directory names;
//!   no tokens, no auth or home paths.
//!
//! Computed per request from existing stores (session store, version
//! registry, bundle manifests, Claude deployment); no threads, no polling.

use std::collections::{BTreeMap, HashMap};
use std::net::TcpStream;
use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::launch::version_registry::NativeVersionRegistry;
use crate::session::model::{CutexSessionRecord, CutexSessionStore};

pub const PATH: &str = "/v2/runtimes";
pub const MAX_AGENTS: usize = 20;
const SHA_PREFIX: usize = 12;

pub fn is_runtimes_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/runtimes/")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRef {
    pub cutex_session_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// `r50` (native) / `claude-r5` (Claude runtime directory).
    pub release: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `<release dir>/<file>` of the executable (native) or the adapter
    /// source directory name (Claude); never a full path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256_prefix: Option<String>,
    pub selected: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub latest: bool,
    /// The recorded files no longer exist (deleted release).
    pub missing: bool,
    pub agents: Vec<AgentRef>,
    pub agent_count: usize,
}

impl Entry {
    fn add_agent(&mut self, agent: AgentRef) {
        self.agent_count += 1;
        if self.agents.len() < MAX_AGENTS {
            self.agents.push(agent);
        }
    }
}

/// The pinned executable of one bundle manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedExecutable {
    pub sha256: String,
    pub path: String,
}

/// Inputs of [`build`], gathered by the handler (or a test).
pub struct Inputs<'a> {
    pub sessions: &'a CutexSessionStore,
    pub registry: Option<&'a NativeVersionRegistry>,
    /// Executable of the selected runtime package.
    pub selected_native: Option<PinnedExecutable>,
    /// Selected Claude deployment (adapter source path).
    pub selected_claude_adapter: Option<String>,
    pub read_manifest: &'a dyn Fn(&Path) -> Option<PinnedExecutable>,
    pub exists: &'a dyn Fn(&str) -> bool,
}

/// Last path component (`/` or `\`).
pub fn file_name(path: &str) -> Option<String> {
    path.rsplit(['/', '\\']).find(|part| !part.is_empty()).map(str::to_string)
}

/// Parent directory name.
fn parent_name(path: &str) -> Option<String> {
    let mut parts = path.split(['/', '\\']).filter(|part| !part.is_empty()).collect::<Vec<_>>();
    parts.pop();
    parts.pop().map(str::to_string)
}

fn short_path(path: &str) -> Option<String> {
    match (parent_name(path), file_name(path)) {
        (Some(dir), Some(file)) => Some(format!("{dir}/{file}")),
        (None, file) => file,
        (dir, None) => dir,
    }
}

fn sha_prefix(sha: &str) -> String {
    sha.chars().take(SHA_PREFIX).collect()
}

/// `claude-r5` from any component of the deployment's paths, else the
/// adapter source directory name.
pub fn claude_runtime_label(adapter_source: &str) -> String {
    adapter_source
        .split(['/', '\\'])
        .rev()
        .find(|part| {
            part.strip_prefix("claude-r")
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().take_while(u8::is_ascii_digit).count() > 0)
        })
        .map(str::to_string)
        .or_else(|| file_name(adapter_source))
        .unwrap_or_else(|| "claude".to_string())
}

fn agent_ref(record: &CutexSessionRecord) -> AgentRef {
    AgentRef {
        cutex_session_id: record.cutex_session_id.clone(),
        name: crate::session::metadata::cutex_session_presented_name(record),
    }
}

pub struct Built {
    pub native: Vec<Entry>,
    pub claude: Vec<Entry>,
    pub unpinned_agent_count: usize,
}

pub fn build(inputs: &Inputs<'_>) -> Built {
    // --- native: registry entries keyed by version
    let mut native: BTreeMap<String, Entry> = BTreeMap::new();
    let mut sha_to_version: HashMap<String, String> = HashMap::new();
    let latest = inputs.registry.map(|registry| registry.latest_runtime.version.clone());
    if let Some(registry) = inputs.registry {
        for (sha, record) in &registry.runtime_artifacts {
            sha_to_version.insert(sha.clone(), record.version.clone());
            let entry = native.entry(record.version.clone()).or_default();
            entry.sha256_prefix.get_or_insert_with(|| sha_prefix(sha));
        }
        for (path, record) in &registry.runtime_paths {
            let entry = native.entry(record.version.clone()).or_default();
            if entry.path.is_none() {
                entry.path = short_path(path);
                entry.release_dir = parent_name(path);
                entry.missing = !(inputs.exists)(path);
            } else if entry.missing && (inputs.exists)(path) {
                entry.missing = false;
            }
        }
        native.entry(registry.latest_runtime.version.clone()).or_default();
    }
    for (version, entry) in native.iter_mut() {
        entry.version = Some(version.clone());
        entry.release = crate::management::v2::runtime_packages::native_release(Some(version), entry.path.as_deref())
            .unwrap_or_else(|| version.clone());
        entry.latest = latest.as_deref() == Some(version.as_str());
    }
    let selected_version = inputs
        .selected_native
        .as_ref()
        .and_then(|pinned| sha_to_version.get(&pinned.sha256).cloned())
        .or_else(|| latest.clone());
    // Executables the registry does not know, keyed by sha.
    let mut unknown: BTreeMap<String, Entry> = BTreeMap::new();
    fn unknown_entry(unknown: &mut BTreeMap<String, Entry>, pinned: &PinnedExecutable, exists: &dyn Fn(&str) -> bool) -> String {
        unknown
            .entry(pinned.sha256.clone())
            .or_insert_with(|| Entry {
                release: crate::management::v2::runtime_packages::release_dir_label(&pinned.path)
                    .unwrap_or_else(|| sha_prefix(&pinned.sha256)),
                path: short_path(&pinned.path),
                release_dir: parent_name(&pinned.path),
                sha256_prefix: Some(sha_prefix(&pinned.sha256)),
                missing: !exists(&pinned.path),
                ..Entry::default()
            });
        pinned.sha256.clone()
    }
    let mut selected_unknown = None;
    if let (Some(pinned), None) = (&inputs.selected_native, &selected_version) {
        selected_unknown = Some(unknown_entry(&mut unknown, pinned, inputs.exists));
    }

    // --- agents
    let mut manifests: HashMap<std::path::PathBuf, Option<PinnedExecutable>> = HashMap::new();
    let mut claude: BTreeMap<String, Entry> = BTreeMap::new();
    let mut unpinned_agent_count = 0;
    let mut records: Vec<&CutexSessionRecord> =
        inputs.sessions.sessions.values().filter(|record| !record.is_retired()).collect();
    records.sort_by(|a, b| {
        crate::session::metadata::cutex_session_presented_name(a)
            .cmp(&crate::session::metadata::cutex_session_presented_name(b))
            .then_with(|| a.cutex_session_id.cmp(&b.cutex_session_id))
    });
    for record in records {
        if let Some(session) = &record.claude_session {
            let adapter = session.deployment.adapter_source.to_string_lossy().into_owned();
            let label = claude_runtime_label(&adapter);
            let entry = claude.entry(label).or_insert_with(|| Entry {
                path: file_name(&adapter),
                missing: !(inputs.exists)(&adapter),
                ..Entry::default()
            });
            entry.add_agent(agent_ref(record));
            continue;
        }
        let Some(contract) = &record.explicit_launch else {
            if record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::Host {
                unpinned_agent_count += 1;
            }
            continue;
        };
        let pinned = manifests
            .entry(contract.bundle_manifest.clone())
            .or_insert_with(|| (inputs.read_manifest)(&contract.bundle_manifest))
            .clone();
        let Some(pinned) = pinned else {
            // Unreadable manifest (deleted release): its own bucket.
            let key = format!("manifest:{}", contract.bundle_manifest.to_string_lossy());
            let entry = unknown.entry(key).or_insert_with(|| Entry {
                release: crate::management::v2::runtime_packages::release_dir_label(
                    &contract.bundle_manifest.to_string_lossy(),
                )
                .unwrap_or_else(|| "unknown".to_string()),
                release_dir: parent_name(&contract.bundle_manifest.to_string_lossy()),
                missing: true,
                ..Entry::default()
            });
            entry.add_agent(agent_ref(record));
            continue;
        };
        match sha_to_version.get(&pinned.sha256) {
            Some(version) => native.entry(version.clone()).or_default().add_agent(agent_ref(record)),
            None => {
                let key = unknown_entry(&mut unknown, &pinned, inputs.exists);
                unknown.get_mut(&key).expect("inserted").add_agent(agent_ref(record));
            }
        }
    }
    if let Some(version) = &selected_version {
        if let Some(entry) = native.get_mut(version) {
            entry.selected = true;
        }
    }
    if let Some(key) = selected_unknown {
        if let Some(entry) = unknown.get_mut(&key) {
            entry.selected = true;
        }
    }

    // --- Claude: the selected deployment
    if let Some(adapter) = &inputs.selected_claude_adapter {
        let label = claude_runtime_label(adapter);
        let entry = claude.entry(label).or_insert_with(|| Entry {
            path: file_name(adapter),
            missing: !(inputs.exists)(adapter),
            ..Entry::default()
        });
        entry.selected = true;
    }
    for (label, entry) in claude.iter_mut() {
        entry.release = label.clone();
    }

    // Newest first: registry entries by version (descending), then unknown.
    let mut native: Vec<Entry> = native.into_values().collect();
    native.sort_by(|a, b| natural_key(b.version.as_deref()).cmp(&natural_key(a.version.as_deref())));
    native.extend(unknown.into_values());
    let mut claude: Vec<Entry> = claude.into_values().collect();
    claude.sort_by(|a, b| natural_key(Some(&b.release)).cmp(&natural_key(Some(&a.release))));
    Built { native, claude, unpinned_agent_count }
}

/// Compare `0.160.0-r50` / `claude-r5` by their numbers.
fn natural_key(value: Option<&str>) -> Vec<(u64, String)> {
    let Some(value) = value else { return Vec::new() };
    let mut out = Vec::new();
    let mut digits = String::new();
    let mut text = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            if !text.is_empty() {
                out.push((0, std::mem::take(&mut text)));
            }
            digits.push(ch);
        } else {
            if !digits.is_empty() {
                out.push((digits.parse().unwrap_or(u64::MAX), String::new()));
                digits.clear();
            }
            text.push(ch);
        }
    }
    if !digits.is_empty() {
        out.push((digits.parse().unwrap_or(u64::MAX), String::new()));
    }
    if !text.is_empty() {
        out.push((0, text));
    }
    out
}

fn read_manifest(path: &Path) -> Option<PinnedExecutable> {
    let bundle: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some(PinnedExecutable {
        sha256: bundle.pointer("/executable/sha256")?.as_str()?.to_string(),
        path: bundle.pointer("/executable/path")?.as_str()?.to_string(),
    })
}

/// The selected Claude deployment's adapter source, read without validating
/// the deployment (a broken one still shows which runtime it names).
fn selected_claude_adapter() -> Option<String> {
    let path = crate::config::paths::runtime_dir().ok()?.join("claude/deployment.json");
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    value.get("adapter_source")?.as_str().map(str::to_string)
}

fn cutex_view() -> Value {
    let executable = std::env::current_exe().ok().map(|path| path.to_string_lossy().into_owned());
    json!({
        // Build identity as in /v2/capabilities cutexBuild; `release` below is
        // the label of the directory the executable runs from.
        "build": crate::build_info::build_json(),
        "release": executable.as_deref().and_then(crate::management::v2::runtime_packages::release_dir_label),
        "releaseDir": executable.as_deref().and_then(parent_name),
        "executable": executable.as_deref().and_then(file_name),
    })
}

pub fn resource() -> anyhow::Result<Value> {
    let sessions = crate::session::store::load_cutex_session_store()?;
    let registry = crate::launch::version_registry::load_relaxed();
    let selected_native = crate::launch::local_deployment::LocalDeployment::selected()
        .ok()
        .flatten()
        .and_then(|deployment| read_manifest(&deployment.bundle_manifest));
    let exists = |path: &str| Path::new(path).exists();
    let built = build(&Inputs {
        sessions: &sessions,
        registry: registry.as_ref(),
        selected_native,
        selected_claude_adapter: selected_claude_adapter(),
        read_manifest: &read_manifest,
        exists: &exists,
    });
    Ok(json!({
        "contractVersion": 2,
        "hostId": crate::platform::host::current_host_name(),
        "generatedAt": chrono::Utc::now().to_rfc3339(),
        "cutex": cutex_view(),
        "native": built.native,
        "claude": built.claude,
        "unpinnedAgentCount": built.unpinned_agent_count,
    }))
}

/// Compact selection + in-use counts for `/v2/host` `runtimes` (backend
/// auto-update RFC §9 P0): no agent names, no paths.
///
/// ```text
/// { native: {selected, latest, inUse: [Use]}, claude: {selected, inUse: [Use]},
///   unpinnedAgents }
/// Use = { release, version, agents, selected, missing }
/// ```
pub fn summary_from(built: &Built) -> Value {
    fn uses(entries: &[Entry]) -> Vec<Value> {
        entries
            .iter()
            .filter(|entry| entry.selected || entry.agent_count > 0)
            .map(|entry| {
                json!({
                    "release": entry.release,
                    "version": entry.version,
                    "agents": entry.agent_count,
                    "selected": entry.selected,
                    "missing": entry.missing,
                })
            })
            .collect()
    }
    let selected = |entries: &[Entry]| {
        entries
            .iter()
            .find(|entry| entry.selected)
            .map(|entry| entry.version.clone().unwrap_or_else(|| entry.release.clone()))
    };
    json!({
        "native": {
            "selected": selected(&built.native),
            "latest": built.native.iter().find(|entry| entry.latest).and_then(|entry| entry.version.clone()),
            "inUse": uses(&built.native),
        },
        "claude": {
            "selected": built.claude.iter().find(|entry| entry.selected).map(|entry| entry.release.clone()),
            "inUse": uses(&built.claude),
        },
        "unpinnedAgents": built.unpinned_agent_count,
    })
}

/// [`summary_from`] over the live stores.
pub fn summary() -> anyhow::Result<Value> {
    let sessions = crate::session::store::load_cutex_session_store_shared()?;
    let registry = crate::launch::version_registry::load_relaxed();
    let selected_native = crate::launch::local_deployment::LocalDeployment::selected()
        .ok()
        .flatten()
        .and_then(|deployment| read_manifest(&deployment.bundle_manifest));
    let exists = |path: &str| Path::new(path).exists();
    let built = build(&Inputs {
        sessions: &sessions,
        registry: registry.as_ref(),
        selected_native,
        selected_claude_adapter: selected_claude_adapter(),
        read_manifest: &read_manifest,
        exists: &exists,
    });
    Ok(summary_from(&built))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if path != PATH {
        return super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "route_not_found",
            "management v2 route not found",
            false,
            json!({ "path": path }),
        );
    }
    if request.method != "GET" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "only GET is allowed on /v2/runtimes",
            false,
            json!({}),
        );
    }
    match resource() {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "runtimes_unavailable",
            "the runtime view could not be read",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        ),
    }
}

#[cfg(test)]
#[path = "runtimes_api_tests.rs"]
mod tests;
