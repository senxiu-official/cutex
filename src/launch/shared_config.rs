//! Shared Codex configuration for agents with their own native home
//! (DESIGN §4.5 Plugins / MCP / Skills).
//!
//! Managed Codex agents run on a per-agent native home
//! (`runtime/light/agents/<native_id>`) whose `config.toml` is a one-time copy
//! of the shared home's (the selected `LocalDeployment::native_home`) taken at
//! adoption, and whose `skills/`, `memories/` and `plugins/` are links to the
//! shared ones. Settings (plugins, MCP servers, skill toggles, hook trust)
//! edit the shared `config.toml`, so without help those edits never reach an
//! existing agent.
//!
//! At launch Cutex therefore layers four parts of the shared config over the
//! agent's own, as `-c` session overrides (native's highest user-writable
//! layer). The agent's `config.toml` is never rewritten.
//!
//! | part            | override          | native merge                         |
//! |-----------------|-------------------|--------------------------------------|
//! | `[plugins]`     | `plugins={..}`    | deep merge over the user layer       |
//! | `[mcp_servers]` | `mcp_servers={..}`| deep merge over the user layer       |
//! | `skills.config` | `skills.config=[]`| rules per layer, later layer wins per selector |
//! | `hooks.state`   | `hooks.state={..}`| per layer, later layer wins per field |
//!
//! Merge rules (entry = plugin id / MCP server name / skill selector
//! `path:<p>` or `name:<n>` / hook state key):
//! - **In both**: the shared entry wins for every key it defines. Only the
//!   keys whose value differs from the agent's are emitted (so an unchanged
//!   entry costs nothing on the command line). Native merges cannot delete a
//!   key, so a sub-key that only the agent's copy still has survives, except
//!   `enabled = false` on a plugin / MCP server whose shared entry omits
//!   `enabled` (native default true): that is reset to `true`.
//! - **Agent only, agent's own**: kept (nothing emitted).
//! - **Agent only, copied from shared** (removed from shared since): turned
//!   off as if absent — plugin / MCP server `enabled = false`; a skill rule
//!   `enabled = false` is countered with `enabled = true` (native default);
//!   a hook state's `trusted_hash` becomes `""` (no longer trusted) and its
//!   `enabled = false` becomes `true`.
//!
//! "Copied from shared" is told apart from "agent's own" by a baseline file,
//! `<agent home>/cutex-shared-config.json`, written once on the first launch
//! that applies the overlay: the entries present in *both* the shared config
//! and the agent's config at that moment. Only baseline entries are ever
//! turned off. For a new agent (adopted from the shared home just before its
//! first launch) that is exactly its adopted copy. For an agent adopted
//! before this overlay existed the history is not recoverable: entries it
//! has and the shared home no longer has are treated as its own and kept
//! (e.g. Windows-host agents with an extra `sites@openai-bundled`).
//!
//! The agent view used for comparisons is the agent's `config.toml` plus the
//! `-c` overrides already on the command line (a selected profile's
//! `plugins` / `skills` projection). An emitted table / array therefore
//! starts from those earlier overrides, because a later `-c plugins=` would
//! replace an earlier one inside native's override layer.
//!
//! Ordering: the overlay is applied before Cutex's own `mcp_servers.cutex` /
//! `mcp_servers.cutex_job` and before scoped MCP (`mcp_servers.<name>`), which
//! therefore replace any same-named shared entry. `cutex` and `cutex_job` are
//! never taken from the shared config.
//!
//! Secrets: an MCP `env` / `http_headers` value that has to be emitted is
//! passed as process environment (`env_vars` / `env_http_headers`, like scoped
//! MCP) unless the agent's copy defines that key inline (native prefers
//! inline values), the name is reserved, or another source sets the same
//! variable differently; only then does it go inline.
//!
//! `plugins/` links: a missing `<agent home>/plugins` is linked to the shared
//! `plugins/` as adoption does. Marketplaces are not projected: loading an
//! installed plugin needs only its config entry and the cached bundle.
//!
//! Settings `codexConfigFollowShared` (default true) turns all of this off.
//! Failures only warn; they never block a launch.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use toml::map::Map;
use toml::Value;

use super::scoped_mcp::LaunchInjection;

/// Baseline of entries copied from the shared home (see module docs).
pub const BASELINE_FILE: &str = "cutex-shared-config.json";
const BASELINE_SCHEMA: &str = "cutex.shared-config-baseline.v1";
/// MCP servers Cutex itself injects at launch; never taken from shared.
pub const RESERVED_MCP_SERVERS: &[&str] = &["cutex", "cutex_job"];
const HEADER_ENV_PREFIX: &str = "CUTEX_SHARED_MCP_HDR_";

/// Result of making sure the agent home sees the shared `plugins/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginsLink {
    /// Already a link (or the same directory).
    Linked,
    /// Created now.
    Created,
    /// The shared home has no `plugins/` yet (nothing is installed).
    NoSharedPlugins,
    /// The agent home has its own real `plugins/` directory (created before
    /// the shared one existed); Cutex leaves it alone.
    SeparateDirectory,
}

/// Entries known to have been copied from the shared home.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub schema: String,
    #[serde(default)]
    pub plugins: BTreeSet<String>,
    #[serde(default)]
    pub mcp_servers: BTreeSet<String>,
    #[serde(default)]
    pub skills: BTreeSet<String>,
    #[serde(default)]
    pub hooks_state: BTreeSet<String>,
}

impl Baseline {
    /// Entries present in both configs (the copy that came from shared).
    pub fn initial(shared: &Map<String, Value>, agent: &Map<String, Value>) -> Self {
        let both = |part: fn(&Map<String, Value>) -> BTreeSet<String>| {
            part(shared).intersection(&part(agent)).cloned().collect()
        };
        Self {
            schema: BASELINE_SCHEMA.to_string(),
            plugins: both(plugin_names),
            mcp_servers: both(mcp_names),
            skills: both(skill_names),
            hooks_state: both(hook_state_names),
        }
    }
}

fn table_at<'a>(root: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Map<String, Value>> {
    let (first, rest) = path.split_first()?;
    let mut current = root.get(*first)?.as_table()?;
    for key in rest {
        current = current.get(*key)?.as_table()?;
    }
    Some(current)
}

fn table_entries<'a>(
    root: &'a Map<String, Value>,
    path: &[&str],
) -> impl Iterator<Item = (&'a String, &'a Map<String, Value>)> {
    table_at(root, path)
        .into_iter()
        .flat_map(|table| table.iter())
        .filter_map(|(key, value)| value.as_table().map(|entry| (key, entry)))
}

fn plugin_names(root: &Map<String, Value>) -> BTreeSet<String> {
    table_entries(root, &["plugins"])
        .map(|(id, _)| id.clone())
        .collect()
}

fn mcp_names(root: &Map<String, Value>) -> BTreeSet<String> {
    table_entries(root, &["mcp_servers"])
        .map(|(name, _)| name.clone())
        .filter(|name| !RESERVED_MCP_SERVERS.contains(&name.as_str()))
        .collect()
}

fn hook_state_names(root: &Map<String, Value>) -> BTreeSet<String> {
    table_entries(root, &["hooks", "state"])
        .map(|(key, _)| key.clone())
        .collect()
}

fn skill_rules(root: &Map<String, Value>) -> Vec<(String, &Map<String, Value>)> {
    root.get("skills")
        .and_then(|skills| skills.get("config"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.as_table()?;
            entry.get("enabled")?.as_bool()?;
            Some((skill_selector(entry)?, entry))
        })
        .collect()
}

/// `path:<p>` or `name:<n>`, as native selects (exactly one of the two).
fn skill_selector(entry: &Map<String, Value>) -> Option<String> {
    match (
        entry.get("path").and_then(Value::as_str),
        entry.get("name").and_then(Value::as_str),
    ) {
        (Some(path), None) => Some(format!("path:{path}")),
        (None, Some(name)) if !name.trim().is_empty() => Some(format!("name:{}", name.trim())),
        _ => None,
    }
}

fn skill_names(root: &Map<String, Value>) -> BTreeSet<String> {
    skill_rules(root)
        .into_iter()
        .map(|(selector, _)| selector)
        .collect()
}

/// Each selector's effective rule (later rules win, like native).
fn effective_skill_rules(root: &Map<String, Value>) -> BTreeMap<String, bool> {
    skill_rules(root)
        .into_iter()
        .map(|(selector, entry)| (selector, entry["enabled"].as_bool().unwrap_or(true)))
        .collect()
}

/// Deep merge like native: tables merge per key, anything else replaces.
fn merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Table(base), Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

/// What `shared` must add over `agent` so that, after a deep merge, every key
/// `shared` defines has the shared value. `None` when nothing differs.
fn diff(shared: &Value, agent: Option<&Value>) -> Option<Value> {
    match (shared, agent) {
        (Value::Table(shared), Some(Value::Table(agent))) => {
            let mut out = Map::new();
            for (key, value) in shared {
                if let Some(changed) = diff(value, agent.get(key)) {
                    out.insert(key.clone(), changed);
                }
            }
            (!out.is_empty()).then_some(Value::Table(out))
        }
        (shared, Some(agent)) if shared == agent => None,
        (shared, _) => Some(shared.clone()),
    }
}

/// `-c key=value` pairs already on a command line, as native's override
/// layer (dotted paths, later wins).
pub fn cli_layer(args: &[String]) -> Map<String, Value> {
    let mut root = Value::Table(Map::new());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg != "-c" && arg != "--config" {
            continue;
        }
        let Some((key, raw)) = iter.next().and_then(|pair| pair.split_once('=')) else {
            continue;
        };
        let value = toml::from_str::<Map<String, Value>>(&format!("v = {raw}"))
            .ok()
            .and_then(|mut table| table.remove("v"))
            .unwrap_or_else(|| Value::String(raw.to_string()));
        let segments: Vec<&str> = key.trim().split('.').collect();
        let mut current = &mut root;
        for (index, segment) in segments.iter().enumerate() {
            if !current.is_table() {
                *current = Value::Table(Map::new());
            }
            let table = current.as_table_mut().expect("table");
            if index + 1 == segments.len() {
                table.insert(segment.to_string(), value.clone());
                break;
            }
            current = table
                .entry(segment.to_string())
                .or_insert_with(|| Value::Table(Map::new()));
        }
    }
    match root {
        Value::Table(table) => table,
        _ => Map::new(),
    }
}

fn enabled_of(entry: Option<&Map<String, Value>>) -> Option<bool> {
    entry
        .and_then(|entry| entry.get("enabled"))
        .and_then(Value::as_bool)
}

/// Shared-wins / removed-off overlay for a table of entries whose default
/// is `enabled = true` (plugins, MCP servers).
fn entries_overlay(
    shared: &Map<String, Value>,
    agent: &Map<String, Value>,
    path: &str,
    tracked: &BTreeSet<String>,
    skip: &[&str],
) -> Map<String, Value> {
    let empty = Map::new();
    let shared_entries = table_at(shared, &[path]).unwrap_or(&empty);
    let agent_entries = table_at(agent, &[path]).unwrap_or(&empty);
    let mut overlay = Map::new();
    for (name, entry) in shared_entries {
        let Some(entry_table) = entry.as_table() else {
            continue;
        };
        if skip.contains(&name.as_str()) {
            continue;
        }
        let agent_entry = agent_entries.get(name);
        let mut changed = diff(entry, agent_entry);
        if !entry_table.contains_key("enabled")
            && enabled_of(agent_entry.and_then(Value::as_table)) == Some(false)
        {
            let table = changed.get_or_insert_with(|| Value::Table(Map::new()));
            if let Some(table) = table.as_table_mut() {
                table.insert("enabled".to_string(), Value::Boolean(true));
            }
        }
        if let Some(changed) = changed {
            overlay.insert(name.clone(), changed);
        }
    }
    for (name, entry) in agent_entries {
        if shared_entries.get(name).is_some_and(Value::is_table)
            || skip.contains(&name.as_str())
            || !tracked.contains(name)
        {
            continue;
        }
        if enabled_of(entry.as_table()) != Some(false) {
            overlay.insert(
                name.clone(),
                Value::Table(Map::from_iter([(
                    "enabled".to_string(),
                    Value::Boolean(false),
                )])),
            );
        }
    }
    overlay
}

/// Move emitted MCP `env` / `http_headers` values to process environment
/// where native would still use them (see module docs).
fn route_mcp_secrets(
    overlay: &mut Map<String, Value>,
    agent: &Map<String, Value>,
    shared: &Map<String, Value>,
    taken_envs: &BTreeSet<String>,
    envs: &mut Vec<(String, String)>,
) {
    let agent_entry = |name: &str, field: &str| {
        table_at(agent, &["mcp_servers", name, field])
            .cloned()
            .unwrap_or_default()
    };
    // Same variable name with different values across servers: inline.
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut conflicting: BTreeSet<String> = BTreeSet::new();
    for entry in overlay.values() {
        for (key, value) in entry
            .get("env")
            .and_then(Value::as_table)
            .into_iter()
            .flatten()
        {
            let value = value.as_str().unwrap_or_default().to_string();
            match values.get(key) {
                Some(existing) if *existing != value => {
                    conflicting.insert(key.clone());
                }
                _ => {
                    values.insert(key.clone(), value);
                }
            }
        }
    }
    let mut header_index = 0usize;
    for (name, entry) in overlay.iter_mut() {
        let Some(entry) = entry.as_table_mut() else {
            continue;
        };
        if let Some(Value::Table(env)) = entry.remove("env") {
            let agent_env = agent_entry(name, "env");
            let mut inline = Map::new();
            let mut routed = Vec::new();
            for (key, value) in env {
                let Some(text) = value.as_str() else {
                    inline.insert(key, value);
                    continue;
                };
                if agent_env.contains_key(&key)
                    || conflicting.contains(&key)
                    || taken_envs.contains(&key)
                    || super::scoped_mcp::reserved_process_env(&key)
                {
                    inline.insert(key, value);
                } else {
                    if !envs.iter().any(|(existing, _)| *existing == key) {
                        envs.push((key.clone(), text.to_string()));
                    }
                    routed.push(key);
                }
            }
            if !inline.is_empty() {
                entry.insert("env".to_string(), Value::Table(inline));
            }
            if !routed.is_empty() {
                // Arrays replace on merge: start from what native would use.
                let mut vars = table_at(shared, &["mcp_servers", name.as_str()])
                    .and_then(|shared| shared.get("env_vars"))
                    .or_else(|| {
                        table_at(agent, &["mcp_servers", name.as_str()])
                            .and_then(|agent| agent.get("env_vars"))
                    })
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for key in routed {
                    let present = vars.iter().any(|item| {
                        item.as_str() == Some(key.as_str())
                            || item.get("name").and_then(Value::as_str) == Some(key.as_str())
                    });
                    if !present {
                        vars.push(Value::String(key));
                    }
                }
                entry.insert("env_vars".to_string(), Value::Array(vars));
            }
        }
        if let Some(Value::Table(headers)) = entry.remove("http_headers") {
            let agent_headers = agent_entry(name, "http_headers");
            let mut inline = Map::new();
            let mut env_headers = Map::new();
            for (header, value) in headers {
                match value.as_str() {
                    Some(text) if !agent_headers.contains_key(&header) => {
                        header_index += 1;
                        let variable = format!("{HEADER_ENV_PREFIX}{header_index}");
                        envs.push((variable.clone(), text.to_string()));
                        env_headers.insert(header, Value::String(variable));
                    }
                    _ => {
                        inline.insert(header, value);
                    }
                }
            }
            if !inline.is_empty() {
                entry.insert("http_headers".to_string(), Value::Table(inline));
            }
            if !env_headers.is_empty() {
                let target = entry
                    .entry("env_http_headers".to_string())
                    .or_insert_with(|| Value::Table(Map::new()));
                merge(target, &Value::Table(env_headers));
            }
        }
    }
}

fn skills_overlay(
    shared: &Map<String, Value>,
    agent: &Map<String, Value>,
    tracked: &BTreeSet<String>,
) -> Vec<Value> {
    let agent_rules = effective_skill_rules(agent);
    let shared_rules = effective_skill_rules(shared);
    let mut out = Vec::new();
    for (selector, entry) in skill_rules(shared) {
        let enabled = shared_rules[&selector];
        // Emit each selector once, at its last (effective) position.
        if entry["enabled"].as_bool() != Some(enabled) {
            continue;
        }
        match agent_rules.get(&selector) {
            Some(current) if *current == enabled => {}
            None if enabled => {}
            _ => {
                let mut rule = Map::new();
                for key in ["path", "name"] {
                    if let Some(value) = entry.get(key) {
                        rule.insert(key.to_string(), value.clone());
                    }
                }
                rule.insert("enabled".to_string(), Value::Boolean(enabled));
                if !out.contains(&Value::Table(rule.clone())) {
                    out.push(Value::Table(rule));
                }
            }
        }
    }
    for (selector, enabled) in &agent_rules {
        if shared_rules.contains_key(selector) || !tracked.contains(selector) || *enabled {
            continue;
        }
        let (key, value) = selector.split_once(':').expect("selector");
        out.push(Value::Table(Map::from_iter([
            (key.to_string(), Value::String(value.to_string())),
            ("enabled".to_string(), Value::Boolean(true)),
        ])));
    }
    out
}

fn hooks_overlay(
    shared: &Map<String, Value>,
    agent: &Map<String, Value>,
    tracked: &BTreeSet<String>,
) -> Map<String, Value> {
    let empty = Map::new();
    let shared_state = table_at(shared, &["hooks", "state"]).unwrap_or(&empty);
    let agent_state = table_at(agent, &["hooks", "state"]).unwrap_or(&empty);
    let mut overlay = Map::new();
    for (key, entry) in shared_state {
        if !entry.is_table() {
            continue;
        }
        if let Some(changed) = diff(entry, agent_state.get(key)) {
            overlay.insert(key.clone(), changed);
        }
    }
    for (key, entry) in agent_state {
        if shared_state.get(key).is_some_and(Value::is_table) || !tracked.contains(key) {
            continue;
        }
        let Some(entry) = entry.as_table() else {
            continue;
        };
        let mut neutral = Map::new();
        if entry
            .get("trusted_hash")
            .and_then(Value::as_str)
            .is_some_and(|hash| !hash.is_empty())
        {
            neutral.insert("trusted_hash".to_string(), Value::String(String::new()));
        }
        if enabled_of(Some(entry)) == Some(false) {
            neutral.insert("enabled".to_string(), Value::Boolean(true));
        }
        if !neutral.is_empty() {
            overlay.insert(key.clone(), Value::Table(neutral));
        }
    }
    overlay
}

/// The overlay for an agent whose own config is `agent_config` and whose
/// command line already carries `cli` overrides, following `shared`.
/// `taken_envs`: process variables other launch parts set.
pub fn overlay(
    shared: &Map<String, Value>,
    agent_config: &Map<String, Value>,
    cli: &Map<String, Value>,
    baseline: &Baseline,
    taken_envs: &BTreeSet<String>,
) -> LaunchInjection {
    let mut agent = Value::Table(agent_config.clone());
    merge(&mut agent, &Value::Table(cli.clone()));
    let agent = match agent {
        Value::Table(table) => table,
        _ => Map::new(),
    };
    let mut injection = LaunchInjection::default();
    // A later `-c key=` replaces an earlier one: start from it.
    let merged = |base: Option<&Value>, overlay: Map<String, Value>| {
        let mut value = base.cloned().unwrap_or_else(|| Value::Table(Map::new()));
        merge(&mut value, &Value::Table(overlay));
        value
    };

    let plugins = entries_overlay(shared, &agent, "plugins", &baseline.plugins, &[]);
    if !plugins.is_empty() {
        injection
            .options
            .push(("plugins".to_string(), merged(cli.get("plugins"), plugins)));
    }

    let mut mcp = entries_overlay(
        shared,
        &agent,
        "mcp_servers",
        &baseline.mcp_servers,
        RESERVED_MCP_SERVERS,
    );
    let mut envs = Vec::new();
    route_mcp_secrets(&mut mcp, &agent, shared, taken_envs, &mut envs);
    if !mcp.is_empty() {
        injection.options.push((
            "mcp_servers".to_string(),
            merged(cli.get("mcp_servers"), mcp),
        ));
    }

    let skills = skills_overlay(shared, &agent, &baseline.skills);
    if !skills.is_empty() {
        let mut rules = cli
            .get("skills")
            .and_then(|skills| skills.get("config"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        rules.extend(skills);
        injection
            .options
            .push(("skills.config".to_string(), Value::Array(rules)));
    }

    let hooks = hooks_overlay(shared, &agent, &baseline.hooks_state);
    if !hooks.is_empty() {
        let base = cli.get("hooks").and_then(|hooks| hooks.get("state"));
        injection
            .options
            .push(("hooks.state".to_string(), merged(base, hooks)));
    }
    injection.envs = envs;
    injection
}

fn same_dir(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Make `<agent_home>/plugins` the shared `plugins/` when it is missing.
pub fn ensure_plugins_link(agent_home: &Path, shared_home: &Path) -> anyhow::Result<PluginsLink> {
    let shared = shared_home.join("plugins");
    if !shared.is_dir() {
        return Ok(PluginsLink::NoSharedPlugins);
    }
    let local = agent_home.join("plugins");
    match std::fs::symlink_metadata(&local) {
        Ok(_) if same_dir(&local, &shared) => Ok(PluginsLink::Linked),
        Ok(_) => Ok(PluginsLink::SeparateDirectory),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::platform::shared_assets::link_directory(&shared, &local)
                .context("link the shared plugins directory into the agent home")?;
            Ok(PluginsLink::Created)
        }
        Err(error) => Err(error.into()),
    }
}

fn read_config(home: &Path, what: &str) -> anyhow::Result<Map<String, Value>> {
    let path = home.join("config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    toml::from_str(&raw)
        .map_err(|error| anyhow::anyhow!("invalid {what} config.toml: {}", error.message()))
}

/// The agent's baseline; written on first use (see module docs).
pub fn load_or_init_baseline(
    agent_home: &Path,
    shared: &Map<String, Value>,
    agent_view: &Map<String, Value>,
) -> Baseline {
    let path = agent_home.join(BASELINE_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Baseline>(&bytes) {
            Ok(baseline) if baseline.schema == BASELINE_SCHEMA => return baseline,
            _ => eprintln!(
                "codex shared config: {} is invalid; recording a new baseline",
                path.display()
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("codex shared config: read {}: {error}", path.display());
        }
    }
    let baseline = Baseline::initial(shared, agent_view);
    if let Err(error) = crate::config::atomic::write_private_pretty_json_atomic(
        &path,
        &baseline,
        "shared config baseline",
    ) {
        eprintln!("codex shared config: baseline not saved: {error:#}");
    }
    baseline
}

/// Launch overrides for a Codex agent on `agent_home` against `shared_home`.
/// `existing_args`: the command line so far; `taken_envs`: process variables
/// set by other launch parts (scoped MCP).
pub fn injection_for_homes(
    agent_home: &Path,
    shared_home: &Path,
    existing_args: &[String],
    taken_envs: &BTreeSet<String>,
) -> anyhow::Result<LaunchInjection> {
    if same_dir(agent_home, shared_home) {
        return Ok(LaunchInjection::default());
    }
    let shared = read_config(shared_home, "shared")?;
    let agent = read_config(agent_home, "agent")?;
    match ensure_plugins_link(agent_home, shared_home) {
        Ok(PluginsLink::SeparateDirectory) => eprintln!(
            "codex shared config: {} has its own plugins/ directory; shared plugin bundles are not visible there",
            agent_home.display()
        ),
        Ok(_) => {}
        Err(error) => eprintln!("codex shared config: plugins link: {error:#}"),
    }
    let cli = cli_layer(existing_args);
    let mut view = Value::Table(agent.clone());
    merge(&mut view, &Value::Table(cli.clone()));
    let view = match view {
        Value::Table(table) => table,
        _ => Map::new(),
    };
    let baseline = load_or_init_baseline(agent_home, &shared, &view);
    Ok(overlay(&shared, &agent, &cli, &baseline, taken_envs))
}

/// Launch overrides for a Codex agent on `agent_home`: empty when the setting
/// is off, no local deployment is selected, or the agent runs on the shared
/// home itself.
pub fn launch_injection(
    agent_home: &Path,
    follow_shared: bool,
    existing_args: &[String],
    taken_envs: &BTreeSet<String>,
) -> anyhow::Result<LaunchInjection> {
    if !follow_shared {
        return Ok(LaunchInjection::default());
    }
    let Some(deployment) = super::local_deployment::LocalDeployment::selected()? else {
        return Ok(LaunchInjection::default());
    };
    injection_for_homes(
        agent_home,
        &deployment.native_home,
        existing_args,
        taken_envs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Map<String, Value> {
        toml::from_str(raw).unwrap()
    }

    fn inline(raw: &str) -> Value {
        parse(&format!("v = {raw}"))["v"].clone()
    }

    fn option<'a>(injection: &'a LaunchInjection, key: &str) -> Option<&'a Value> {
        injection
            .options
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    fn no_envs() -> BTreeSet<String> {
        BTreeSet::new()
    }

    const SHARED: &str = r#"
model = "gpt"
[marketplaces.debug]
source_type = "git"
source = "https://example.com/debug.git"
[plugins."github@openai-curated"]
enabled = true
[plugins."docs.helper@debug"]
enabled = false
[plugins."docs.helper@debug".mcp_servers.docs]
enabled_tools = ["read"]
[plugins."same@x"]
enabled = true
[mcp_servers.node_repl]
command = "node"
args = ["repl.js"]
[mcp_servers.fresh]
command = "fresh-mcp"
[mcp_servers.cutex]
command = "stale-shared-cutex"
[mcp_servers.cutex_job]
command = "stale-shared-job"
"#;
    const AGENT: &str = r#"
model = "glm"
[plugins."github@openai-curated"]
enabled = false
[plugins."same@x"]
enabled = true
[plugins."sites@openai-bundled"]
enabled = true
[plugins."gone@debug"]
enabled = true
[mcp_servers.node_repl]
command = "node"
args = ["old.js"]
enabled = false
[mcp_servers.own]
command = "own-mcp"
[mcp_servers.removed]
command = "removed-mcp"
[mcp_servers.cutex]
command = "agent-cutex"
"#;

    fn baseline() -> Baseline {
        Baseline {
            schema: BASELINE_SCHEMA.into(),
            plugins: ["github@openai-curated", "same@x", "gone@debug"]
                .map(String::from)
                .into(),
            mcp_servers: ["node_repl", "removed", "cutex"].map(String::from).into(),
            ..Baseline::default()
        }
    }

    #[test]
    fn shared_wins_in_both_agent_own_kept_and_removed_copies_turn_off() {
        let injection = overlay(
            &parse(SHARED),
            &parse(AGENT),
            &Map::new(),
            &baseline(),
            &no_envs(),
        );
        let plugins = option(&injection, "plugins").unwrap().as_table().unwrap();
        // Both sides, differing: shared value.
        assert_eq!(
            plugins["github@openai-curated"]["enabled"].as_bool(),
            Some(true)
        );
        // Shared only: as is (dotted ids kept as one key).
        assert_eq!(
            plugins["docs.helper@debug"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(
            plugins["docs.helper@debug"]["mcp_servers"]["docs"]["enabled_tools"][0].as_str(),
            Some("read")
        );
        // Identical: nothing emitted.
        assert!(!plugins.contains_key("same@x"));
        // Agent's own extra (not in the baseline): kept as is.
        assert!(!plugins.contains_key("sites@openai-bundled"));
        // Copied from shared, removed there since: off.
        assert_eq!(plugins["gone@debug"]["enabled"].as_bool(), Some(false));

        let servers = option(&injection, "mcp_servers")
            .unwrap()
            .as_table()
            .unwrap();
        // Only the differing keys, plus the stale `enabled = false` reset.
        let node = servers["node_repl"].as_table().unwrap();
        assert_eq!(node.len(), 2, "{node:?}");
        assert_eq!(node["args"][0].as_str(), Some("repl.js"));
        assert_eq!(node["enabled"].as_bool(), Some(true));
        assert_eq!(servers["fresh"]["command"].as_str(), Some("fresh-mcp"));
        assert!(!servers.contains_key("own"));
        assert_eq!(servers["removed"]["enabled"].as_bool(), Some(false));
        // Cutex's own servers are never taken from (or disabled by) shared.
        assert!(!servers.contains_key("cutex"));
        assert!(!servers.contains_key("cutex_job"));
        assert!(injection.envs.is_empty());
    }

    #[test]
    fn overlay_precedes_cutex_servers_on_the_command_line() {
        let mut injection = overlay(
            &parse(SHARED),
            &parse(AGENT),
            &Map::new(),
            &baseline(),
            &no_envs(),
        );
        // What stock_lifecycle appends after the overlay.
        injection
            .options
            .push(("mcp_servers.cutex".into(), inline("{ command = 'facade' }")));
        injection
            .options
            .push(("mcp_servers.fresh".into(), inline("{ command = 'scoped' }")));
        let layer = cli_layer(&injection.args());
        let servers = layer["mcp_servers"].as_table().unwrap();
        assert_eq!(servers["cutex"]["command"].as_str(), Some("facade"));
        // Scoped MCP replaces a same-named shared server.
        assert_eq!(servers["fresh"]["command"].as_str(), Some("scoped"));
        assert_eq!(servers["node_repl"]["args"][0].as_str(), Some("repl.js"));
        // Dotted plugin ids survive the round trip.
        assert!(layer["plugins"]
            .as_table()
            .unwrap()
            .contains_key("docs.helper@debug"));
    }

    #[test]
    fn earlier_projection_overrides_are_kept_in_emitted_values() {
        let earlier = vec![
            "-c".to_string(),
            r#"plugins={"proj@x" = { enabled = true }}"#.to_string(),
            "-c".to_string(),
            r#"skills={ config = [{ path = "/p/a/SKILL.md", enabled = false }] }"#.to_string(),
        ];
        let cli = cli_layer(&earlier);
        let shared = parse(
            r#"
[plugins."new@x"]
enabled = true
[[skills.config]]
path = "/p/b/SKILL.md"
enabled = false
"#,
        );
        let injection = overlay(&shared, &Map::new(), &cli, &Baseline::default(), &no_envs());
        let plugins = option(&injection, "plugins").unwrap().as_table().unwrap();
        assert!(plugins.contains_key("proj@x") && plugins.contains_key("new@x"));
        let rules = option(&injection, "skills.config")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["path"].as_str(), Some("/p/a/SKILL.md"));
        assert_eq!(rules[1]["path"].as_str(), Some("/p/b/SKILL.md"));
        let layer = cli_layer(&[earlier, injection.args()].concat());
        assert_eq!(layer["plugins"]["proj@x"]["enabled"].as_bool(), Some(true));
        assert_eq!(layer["skills"]["config"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn skill_rules_follow_shared_and_removed_copies_are_countered() {
        let shared = parse(
            r#"
[[skills.config]]
path = "/s/off/SKILL.md"
enabled = false
[[skills.config]]
name = "same"
enabled = false
[[skills.config]]
name = "noop"
enabled = true
"#,
        );
        let agent = parse(
            r#"
[[skills.config]]
name = "same"
enabled = false
[[skills.config]]
path = "/s/copied/SKILL.md"
enabled = false
[[skills.config]]
path = "/s/own/SKILL.md"
enabled = false
"#,
        );
        let baseline = Baseline {
            skills: ["name:same", "path:/s/copied/SKILL.md"]
                .map(String::from)
                .into(),
            ..baseline()
        };
        let injection = overlay(&shared, &agent, &Map::new(), &baseline, &no_envs());
        let rules = option(&injection, "skills.config")
            .unwrap()
            .as_array()
            .unwrap();
        let rendered: Vec<String> = rules.iter().map(|rule| rule.to_string()).collect();
        assert_eq!(rules.len(), 2, "{rendered:?}");
        assert_eq!(rules[0]["path"].as_str(), Some("/s/off/SKILL.md"));
        assert_eq!(rules[0]["enabled"].as_bool(), Some(false));
        // Copied rule removed from shared: back to the native default.
        assert_eq!(rules[1]["path"].as_str(), Some("/s/copied/SKILL.md"));
        assert_eq!(rules[1]["enabled"].as_bool(), Some(true));
        // The agent's own rule is left to its config file.
        assert!(!rendered.iter().any(|rule| rule.contains("/s/own/")));
    }

    #[test]
    fn hook_trust_follows_shared_and_revoked_copies_lose_trust() {
        let shared = parse(
            r#"
[hooks.state."sample@smoke:hooks/hooks.json:pre_tool_use:0:0"]
trusted_hash = "sha256:new"
[hooks.state."both@x:hooks.json:stop:0:0"]
enabled = false
"#,
        );
        let agent = parse(
            r#"
[hooks.state."both@x:hooks.json:stop:0:0"]
trusted_hash = "sha256:agent"
[hooks.state."revoked@x:hooks.json:stop:0:0"]
trusted_hash = "sha256:old"
enabled = false
[hooks.state."own@x:hooks.json:stop:0:0"]
trusted_hash = "sha256:own"
"#,
        );
        let baseline = Baseline {
            hooks_state: [
                "both@x:hooks.json:stop:0:0",
                "revoked@x:hooks.json:stop:0:0",
            ]
            .map(String::from)
            .into(),
            ..baseline()
        };
        let injection = overlay(&shared, &agent, &Map::new(), &baseline, &no_envs());
        let state = option(&injection, "hooks.state")
            .unwrap()
            .as_table()
            .unwrap();
        assert_eq!(
            state["sample@smoke:hooks/hooks.json:pre_tool_use:0:0"]["trusted_hash"].as_str(),
            Some("sha256:new")
        );
        // Field level: shared's `enabled`; the agent's own trust stays.
        let both = state["both@x:hooks.json:stop:0:0"].as_table().unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both["enabled"].as_bool(), Some(false));
        let revoked = state["revoked@x:hooks.json:stop:0:0"].as_table().unwrap();
        assert_eq!(revoked["trusted_hash"].as_str(), Some(""));
        assert_eq!(revoked["enabled"].as_bool(), Some(true));
        assert!(!state.contains_key("own@x:hooks.json:stop:0:0"));
        // `-c hooks.state=` renders and parses back.
        let layer = cli_layer(&injection.args());
        assert_eq!(layer["hooks"]["state"], Value::Table(state.clone()));
    }

    #[test]
    fn mcp_secrets_go_through_process_environment_when_native_would_use_them() {
        let shared = parse(
            r#"
[mcp_servers.gh]
command = "gh-mcp"
env = { GH_TOKEN = "s3cret", SAME = "v", INLINE = "new", TAKEN = "t", PATH = "/x" }
[mcp_servers.web]
url = "https://example.invalid/mcp"
http_headers = { Authorization = "Bearer x", Kept = "k2" }
"#,
        );
        let agent = parse(
            r#"
[mcp_servers.gh]
command = "gh-mcp"
env = { SAME = "v", INLINE = "old" }
[mcp_servers.web]
url = "https://example.invalid/mcp"
http_headers = { Kept = "k1" }
"#,
        );
        let taken: BTreeSet<String> = ["TAKEN".to_string()].into();
        let injection = overlay(&shared, &agent, &Map::new(), &Baseline::default(), &taken);
        let servers = option(&injection, "mcp_servers").unwrap();
        let gh = servers["gh"].as_table().unwrap();
        assert_eq!(
            gh["env_vars"],
            Value::Array(vec![Value::String("GH_TOKEN".into())])
        );
        let kept_inline: Vec<String> = gh["env"].as_table().unwrap().keys().cloned().collect();
        assert_eq!(kept_inline, ["INLINE", "PATH", "TAKEN"]);
        assert_eq!(
            injection.envs,
            [
                ("GH_TOKEN".to_string(), "s3cret".to_string()),
                ("CUTEX_SHARED_MCP_HDR_1".to_string(), "Bearer x".to_string()),
            ]
        );
        assert_eq!(
            servers["web"]["env_http_headers"]["Authorization"].as_str(),
            Some("CUTEX_SHARED_MCP_HDR_1")
        );
        assert_eq!(servers["web"]["http_headers"]["Kept"].as_str(), Some("k2"));
        assert!(!injection.args().join(" ").contains("s3cret"));
    }

    #[test]
    fn baseline_is_the_copy_present_on_both_sides() {
        let baseline = Baseline::initial(&parse(SHARED), &parse(AGENT));
        assert_eq!(
            baseline.plugins,
            ["github@openai-curated", "same@x"].map(String::from).into()
        );
        assert_eq!(baseline.mcp_servers, ["node_repl".to_string()].into());
    }

    #[test]
    fn missing_and_invalid_configs() {
        let root =
            std::env::temp_dir().join(format!("cutex-shared-config-{}", uuid::Uuid::new_v4()));
        let (shared, agent) = (root.join("shared"), root.join("agent"));
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        assert_eq!(
            injection_for_homes(&agent, &shared, &[], &no_envs()).unwrap(),
            LaunchInjection::default()
        );
        std::fs::write(shared.join("config.toml"), "plugins = [").unwrap();
        assert!(injection_for_homes(&agent, &shared, &[], &no_envs()).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn homes_get_a_plugins_link_a_baseline_and_the_override() {
        let root =
            std::env::temp_dir().join(format!("cutex-shared-config-{}", uuid::Uuid::new_v4()));
        let shared = root.join("shared");
        let agent = root.join("agent");
        let separate = root.join("separate");
        std::fs::create_dir_all(shared.join("plugins").join("cache")).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(separate.join("plugins")).unwrap();
        std::fs::write(shared.join("config.toml"), SHARED).unwrap();
        std::fs::write(agent.join("config.toml"), AGENT).unwrap();

        // Same home: nothing to do.
        assert_eq!(
            injection_for_homes(&shared, &shared, &[], &no_envs()).unwrap(),
            LaunchInjection::default()
        );

        let injection = injection_for_homes(&agent, &shared, &[], &no_envs()).unwrap();
        let plugins = option(&injection, "plugins").unwrap();
        // First launch: legacy extras are the agent's own, never disabled.
        assert!(plugins.get("sites@openai-bundled").is_none());
        assert!(plugins.get("gone@debug").is_none());
        assert!(option(&injection, "mcp_servers")
            .unwrap()
            .get("removed")
            .is_none());
        let saved: Baseline =
            serde_json::from_slice(&std::fs::read(agent.join(BASELINE_FILE)).unwrap()).unwrap();
        assert_eq!(saved, Baseline::initial(&parse(SHARED), &parse(AGENT)));
        assert!(
            agent.join("plugins").join("cache").is_dir(),
            "shared plugins/ linked"
        );

        // Settings removes a plugin and a server the agent had copied.
        let edited = SHARED
            .replace("[plugins.\"same@x\"]\nenabled = true\n", "")
            .replace(
                "[mcp_servers.node_repl]\ncommand = \"node\"\nargs = [\"repl.js\"]\n",
                "",
            );
        assert_ne!(edited, SHARED);
        std::fs::write(shared.join("config.toml"), &edited).unwrap();
        let injection = injection_for_homes(&agent, &shared, &[], &no_envs()).unwrap();
        assert_eq!(
            option(&injection, "plugins").unwrap()["same@x"]["enabled"].as_bool(),
            Some(false)
        );
        // node_repl is already `enabled = false` in the agent's copy.
        assert!(option(&injection, "mcp_servers")
            .unwrap()
            .get("node_repl")
            .is_none());
        // The baseline is recorded once.
        let again: Baseline =
            serde_json::from_slice(&std::fs::read(agent.join(BASELINE_FILE)).unwrap()).unwrap();
        assert_eq!(again, saved);

        assert_eq!(
            ensure_plugins_link(&agent, &shared).unwrap(),
            PluginsLink::Linked
        );
        assert_eq!(
            ensure_plugins_link(&separate, &shared).unwrap(),
            PluginsLink::SeparateDirectory
        );
        assert_eq!(
            ensure_plugins_link(&agent, &root.join("missing")).unwrap(),
            PluginsLink::NoSharedPlugins
        );
        // The agent config itself is never rewritten.
        assert_eq!(
            std::fs::read_to_string(agent.join("config.toml")).unwrap(),
            AGENT
        );
        assert_eq!(
            launch_injection(&agent, false, &[], &no_envs()).unwrap(),
            LaunchInjection::default()
        );

        #[cfg(windows)]
        {
            // Remove the junction without following it.
            std::fs::remove_dir(agent.join("plugins")).unwrap();
        }
        #[cfg(unix)]
        std::fs::remove_file(agent.join("plugins")).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
