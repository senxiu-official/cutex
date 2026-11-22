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
//! - `native`: one entry per exact executable SHA in the version registry
//!   (`runtime_artifacts` / `runtime_paths`), plus one per pinned executable
//!   the registry does not know (`version` absent). `selected`: the version of
//!   the selected runtime package's exact executable SHA; absent deployment
//!   selects nothing, and an unregistered SHA selects an unknown entry.
//!   `latest`: the registry's latest runtime. Agents: all durable
//!   roster-backed persistent records whose bundle manifest pins that executable.
//! - `claude`: one entry per Claude runtime directory (`claude-r5`, …) used
//!   by the selected deployment or any durable Claude session, including retired rows.
//! - Hidden local-only, ephemeral and orphan session rows are excluded.
//!   `activeAgentCount` includes offline agents; `archivedAgentCount` counts
//!   retired durable agents. Their sum is `agentCount`, independently of list caps.
//! - `agents` is capped at [`MAX_AGENTS`] per entry; `agentCount` is exact.
//! - Paths are reduced to `<release dir>/<file>` (`path`) and directory names;
//!   no tokens, no auth or home paths.
//!
//! Computed per request from existing stores (session store, version
//! registry, bundle manifests, Claude deployment). Historical executable bytes
//! are verified by one bounded worker; summary never requests that inventory.

#[path = "runtime_digest_inventory.rs"]
mod digests;

/// Start the advisory inventory without waiting for registry or executable I/O.
pub fn start_inventory() {
    digests::start();
}

pub(super) fn maintain_runtime_inventory(work: Box<dyn FnOnce() + Send>) -> bool {
    digests::maintain(work)
}

use std::collections::{BTreeMap, HashMap, HashSet};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_runtime_path: Option<bool>,
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
    pub active_agent_count: usize,
    pub archived_agent_count: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub verification_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_managed: Option<bool>,
    #[serde(skip_serializing_if = "is_zero")]
    pub external_agent_count: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub managed_agent_count: usize,
}

fn is_zero(value: &usize) -> bool { *value == 0 }

impl Entry {
    fn add_agent(&mut self, agent: AgentRef, archived: bool) {
        self.agent_count += 1;
        if archived { self.archived_agent_count += 1; } else { self.active_agent_count += 1; }
        match agent.external_runtime_path {
            Some(true) => self.external_agent_count += 1,
            Some(false) => self.managed_agent_count += 1,
            None => {},
        }
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
    /// Authoritative managed-agent roster, including archived identities.
    pub managed_agent_ids: &'a HashSet<String>,
    /// Current bytes, never a path-only version label.
    pub executable_sha256: &'a dyn Fn(&str) -> Option<String>,
    pub registry: Option<&'a NativeVersionRegistry>,
    /// Executable of the selected runtime package.
    pub selected_native: Option<PinnedExecutable>,
    /// Selected Claude deployment (adapter source path).
    pub selected_claude_adapter: Option<String>,
    pub read_manifest: &'a dyn Fn(&Path) -> Option<PinnedExecutable>,
    pub exists: &'a dyn Fn(&str) -> bool,
    pub managed_native_root: Option<&'a Path>,
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
        external_runtime_path: None,
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
    build_inventory(inputs, InventoryMode::Verified)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InventoryMode { Verified, Pending, Summary }

fn build_inventory(inputs: &Inputs<'_>, mode: InventoryMode) -> Built {
    // Native build identity is the full SHA, never a version or directory label.
    let mut native: BTreeMap<String, Entry> = BTreeMap::new();
    let mut verified_shas = HashSet::new();
    let latest = inputs.registry.map(|registry| registry.latest_runtime.version.as_str());
    if let Some(registry) = inputs.registry {
        for (sha, record) in &registry.runtime_artifacts {
            native.insert(sha.clone(), Entry {
                version: Some(record.version.clone()),
                release: super::runtime_packages::native_release(Some(&record.version), None)
                    .unwrap_or_else(|| record.version.clone()),
                sha256_prefix: Some(sha_prefix(sha)),
                latest: latest == Some(record.version.as_str()),
                missing: true,
                verification_pending: mode == InventoryMode::Pending,
                ..Entry::default()
            });
        }
        for path in registry.runtime_paths.keys().filter(|_| mode != InventoryMode::Summary) {
            // Resolve current bytes; stale path labels cannot relabel replacements.
            if mode == InventoryMode::Verified && !(inputs.exists)(path) { continue; }
            let Some(sha) = (inputs.executable_sha256)(path) else { continue };
            verified_shas.insert(sha.clone());
            let Some(entry) = native.get_mut(&sha) else { continue };
            if entry.path.is_none() {
                entry.path = short_path(path);
                entry.release_dir = parent_name(path);
            }
            entry.missing = false;
            entry.verification_pending = false;
        }
    }
    let selected_sha = inputs.selected_native.as_ref()
        .filter(|pinned| native.contains_key(&pinned.sha256))
        .map(|pinned| pinned.sha256.clone());
    // Executables the registry does not know, keyed by sha.
    let mut unknown: BTreeMap<String, Entry> = BTreeMap::new();
    fn unknown_entry(unknown: &mut BTreeMap<String, Entry>, pinned: &PinnedExecutable, exists: &dyn Fn(&str) -> bool, unverified: bool) -> String {
        unknown
            .entry(pinned.sha256.clone())
            .or_insert_with(|| Entry {
                release: "unknown".to_string(),
                path: short_path(&pinned.path),
                release_dir: parent_name(&pinned.path),
                sha256_prefix: Some(sha_prefix(&pinned.sha256)),
                missing: !exists(&pinned.path),
                verification_pending: unverified,
                ..Entry::default()
            });
        pinned.sha256.clone()
    }
    let mut selected_unknown = None;
    if let (Some(pinned), None) = (&inputs.selected_native, &selected_sha) {
        selected_unknown = Some(unknown_entry(&mut unknown, pinned, inputs.exists,
            mode == InventoryMode::Pending && !verified_shas.contains(&pinned.sha256)));
    }

    // --- agents
    let mut manifests: HashMap<std::path::PathBuf, Option<PinnedExecutable>> = HashMap::new();
    let mut claude: BTreeMap<String, Entry> = BTreeMap::new();
    let mut unpinned_agent_count = 0;
    let mut records: Vec<&CutexSessionRecord> =
        inputs.sessions.sessions.values().collect();
    records.sort_by(|a, b| {
        crate::session::metadata::cutex_session_presented_name(a)
            .cmp(&crate::session::metadata::cutex_session_presented_name(b))
            .then_with(|| a.cutex_session_id.cmp(&b.cutex_session_id))
    });
    for record in records {
        if record.registration_class != crate::agent_bus::model::AgentRegistrationClass::Persistent
            || !inputs.managed_agent_ids.contains(&record.cutex_session_id) {
            continue;
        }
        let archived = record.archive_state == crate::session::model::CutexSessionArchiveState::Retired;
        if let Some(session) = &record.claude_session {
            let adapter = session.deployment.adapter_source.to_string_lossy().into_owned();
            let label = claude_runtime_label(&adapter);
            let entry = claude.entry(label).or_insert_with(|| Entry {
                path: file_name(&adapter),
                missing: !(inputs.exists)(&adapter),
                ..Entry::default()
            });
            entry.add_agent(agent_ref(record), archived);
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
                release: "unknown".to_string(),
                release_dir: parent_name(&contract.bundle_manifest.to_string_lossy()),
                missing: true,
                verification_pending: mode == InventoryMode::Pending,
                ..Entry::default()
            });
            entry.add_agent(agent_ref(record), archived);
            continue;
        };
        let mut agent = agent_ref(record);
        agent.external_runtime_path = inputs.managed_native_root
            .map(|root| !crate::launch::native_managed::is_inside(Path::new(&pinned.path), root));
        if let Some(entry) = native.get_mut(&pinned.sha256) {
            if entry.path.is_none() || (entry.missing && (inputs.exists)(&pinned.path)) {
                entry.path = short_path(&pinned.path);
                entry.release_dir = parent_name(&pinned.path);
                entry.missing = !(inputs.exists)(&pinned.path);
            }
            entry.add_agent(agent, archived);
        } else {
            let key = unknown_entry(&mut unknown, &pinned, inputs.exists,
                mode == InventoryMode::Pending && !verified_shas.contains(&pinned.sha256));
            unknown.get_mut(&key).expect("inserted").add_agent(agent, archived);
        }
    }
    if let Some(sha) = &selected_sha {
        if let Some(entry) = native.get_mut(sha) {
            entry.selected = true;
            entry.sha256_prefix = inputs.selected_native.as_ref().map(|pinned| sha_prefix(&pinned.sha256));
            if let Some(pinned) = &inputs.selected_native {
                entry.path = short_path(&pinned.path);
                entry.release_dir = parent_name(&pinned.path);
                entry.missing = !(inputs.exists)(&pinned.path);
                entry.selected_managed = inputs.managed_native_root
                    .map(|root| crate::launch::native_managed::is_inside(Path::new(&pinned.path), root));
            }
        }
    }
    if let Some(key) = selected_unknown {
        if let Some(entry) = unknown.get_mut(&key) {
            entry.selected = true;
            entry.selected_managed = inputs.selected_native.as_ref().and_then(|pinned| inputs.managed_native_root
                .map(|root| crate::launch::native_managed::is_inside(Path::new(&pinned.path), root)));
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
    let mut native: Vec<Entry> = native.into_values().filter(|entry|
        (mode == InventoryMode::Summary && entry.latest)
            || entry.selected || entry.agent_count > 0 || (entry.path.is_some() && !entry.missing)).collect();
    native.sort_by(|a, b| natural_key(b.version.as_deref()).cmp(&natural_key(a.version.as_deref())));
    native.extend(unknown.into_values());
    let mut claude: Vec<Entry> = claude.into_values().collect();
    claude.sort_by(|a, b| natural_key(Some(&b.release)).cmp(&natural_key(Some(&a.release))));
    Built { native, claude, unpinned_agent_count }
}

/// Compare `0.160.0-r50` / `claude-r5` by their numbers.
pub(crate) fn natural_key(value: Option<&str>) -> Vec<(u64, String)> {
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

fn managed_agent_ids() -> anyhow::Result<HashSet<String>> {
    let directory = crate::agent_management::AgentManagementStore::open_default()?
        .directory_snapshot_shared()?;
    Ok(directory.agents.keys().map(|id| id.as_str().to_string()).collect())
}

/// A damaged/unavailable roster must not hide installed runtimes. The fallback
/// total describes durable persistent session rows, without claiming an exact
/// roster-backed active/archive split.
fn roster_or_session_ids(
    sessions: &CutexSessionStore,
    roster: anyhow::Result<HashSet<String>>,
) -> (HashSet<String>, bool) {
    match roster {
        Ok(ids) => (ids, true),
        Err(_) => (sessions.sessions.values()
            .filter(|row| row.registration_class == crate::agent_bus::model::AgentRegistrationClass::Persistent)
            .map(|row| row.cutex_session_id.clone()).collect(), false),
    }
}

fn entries_wire(entries: &[Entry], roster_available: bool) -> Value {
    let mut value = json!(entries);
    if !roster_available {
        for entry in value.as_array_mut().expect("entry array") {
            let entry = entry.as_object_mut().expect("entry object");
            entry.remove("activeAgentCount");
            entry.remove("archivedAgentCount");
        }
    }
    value
}

pub fn resource() -> anyhow::Result<Value> {
    let managed_root = crate::launch::native_managed::root()?;
    let sessions = crate::session::store::load_cutex_session_store()?;
    let registry = crate::launch::version_registry::load_relaxed();
    let selected_native = crate::launch::local_deployment::LocalDeployment::selected()
        .ok()
        .flatten()
        .and_then(|deployment| read_manifest(&deployment.bundle_manifest));
    let exists = |path: &str| Path::new(path).exists();
    let (managed_agent_ids, roster_available) = roster_or_session_ids(&sessions, managed_agent_ids());
    let ready = |path: &str| {
        match digests::ready(path) {
            digests::Observation::Verified(sha) => Some(sha),
            digests::Observation::Missing => None,
            _ => None,
        }
    };
    let built = build_inventory(&Inputs {
        managed_agent_ids: &managed_agent_ids,
        executable_sha256: &ready,
        sessions: &sessions,
        registry: registry.as_ref(),
        selected_native,
        selected_claude_adapter: selected_claude_adapter(),
        read_manifest: &read_manifest,
        exists: &exists,
        managed_native_root: Some(&managed_root),
    }, InventoryMode::Pending);
    Ok(json!({
        "contractVersion": 2,
        "hostId": crate::platform::host::current_host_name(),
        "generatedAt": chrono::Utc::now().to_rfc3339(),
        "cutex": cutex_view(),
        "native": entries_wire(&built.native, roster_available),
        "claude": entries_wire(&built.claude, roster_available),
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
                let mut value = json!({
                    "release": entry.release,
                    "version": entry.version,
                    "agents": entry.agent_count,
                    "selected": entry.selected,
                    "missing": entry.missing,
                });
                if let Some(managed) = entry.selected_managed { value["selectedManaged"] = json!(managed); }
                if entry.external_agent_count > 0 { value["externalAgentCount"] = json!(entry.external_agent_count); }
                if entry.managed_agent_count > 0 { value["managedAgentCount"] = json!(entry.managed_agent_count); }
                value
            })
            .collect()
    }
    let selected_native = built.native.iter().find(|entry| entry.selected);
    json!({
        "native": {
            "selected": selected_native.and_then(|entry| entry.version.clone()),
            "selectedSha256Prefix": selected_native.and_then(|entry| entry.sha256_prefix.clone()),
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
    let managed_root = crate::launch::native_managed::root()?;
    let sessions = crate::session::store::load_cutex_session_store_shared()?;
    let registry = crate::launch::version_registry::load_relaxed();
    let selected_native = crate::launch::local_deployment::LocalDeployment::selected()
        .ok()
        .flatten()
        .and_then(|deployment| read_manifest(&deployment.bundle_manifest));
    let exists = |path: &str| Path::new(path).exists();
    let (managed_agent_ids, _) = roster_or_session_ids(&sessions, managed_agent_ids());
    let built = build_inventory(&Inputs {
        managed_agent_ids: &managed_agent_ids,
        executable_sha256: &|_| None,
        sessions: &sessions,
        registry: registry.as_ref(),
        selected_native,
        selected_claude_adapter: selected_claude_adapter(),
        read_manifest: &read_manifest,
        exists: &exists,
        managed_native_root: Some(&managed_root),
    }, InventoryMode::Summary);
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
