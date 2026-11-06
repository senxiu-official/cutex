//! Adapter-v1 external npm prefixes. GET reads cached results only; check/configure
//! explicitly probe dependencies without installing packages or starting a vendor CLI.
//! Callers serialize mutations with the component operation/upgrade lock.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::upgrade::layout::Layout;
pub use crate::upgrade::service::Exec;

const SCHEMA: &str = "cutex/external-runtime/v1";
const STATE_SCHEMA: &str = "cutex/external-runtime-state/v1";
const NODE_RANGE: &str = ">=24.9.0 <25";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalRuntime {
    pub schema: String,
    pub name: String,
    pub version: String,
    pub payload: String,
    pub external_root: ExternalRoot,
    pub node: NodeRequirement,
    pub checks: Checks,
    pub environment: Environment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalRoot {
    pub kind: String,
    pub packages: BTreeMap<String, String>,
    pub cli_entry: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_entry: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeRequirement {
    pub range: String,
    pub recommended: String,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checks {
    pub rg: bool,
    pub node_pty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Environment {
    pub install_root: String,
}

fn family(name: &str) -> Result<&'static str> {
    match name {
        "dsh-runtime" => Ok("DSH"),
        "kimi-runtime" => Ok("KIMI"),
        _ => bail!("unsupported external component {name}"),
    }
}

fn safe_entry(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\\', ':', '\0'])
        && value
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
        && !Path::new(value).is_absolute()
}

fn stable_version(value: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
        })
    {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

fn pinned_version(value: &str) -> bool {
    let identifiers = |text: &str, prerelease: bool| {
        text.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !(prerelease
                    && part.len() > 1
                    && part.starts_with('0')
                    && part.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    let without_build = if let Some((version, build)) = value.split_once('+') {
        if !identifiers(build, false) {
            return false;
        }
        version
    } else {
        value
    };
    let core = if let Some((core, prerelease)) = without_build.split_once('-') {
        if !identifiers(prerelease, true) {
            return false;
        }
        core
    } else {
        without_build
    };
    stable_version(core).is_some()
}

fn supported_node(value: &str) -> bool {
    stable_version(value).is_some_and(|v| v >= (24, 9, 0) && v < (25, 0, 0))
}

impl ExternalRuntime {
    pub fn validate(&self, name: &str, version: &str) -> Result<()> {
        let component = family(name)?;
        ensure!(
            self.schema == SCHEMA && self.payload == "adapter-v1",
            "unsupported external runtime schema/payload"
        );
        ensure!(
            self.name == name && self.version == version,
            "external runtime name/version mismatch"
        );
        let prefix = if component == "DSH" {
            "dsh-r"
        } else {
            "kimi-r"
        };
        let revision = version
            .strip_prefix(prefix)
            .context("invalid adapter version")?;
        ensure!(
            !revision.is_empty()
                && revision.bytes().all(|b| b.is_ascii_digit())
                && revision.parse::<u64>().unwrap_or(0) > 0,
            "invalid adapter version"
        );
        ensure!(
            self.external_root.kind == "npm-prefix",
            "externalRoot must be npm-prefix"
        );
        let (packages, cli, catalog): (&[&str], &str, Option<&str>) = if component == "DSH" {
            (
                &[
                    "@deepseek-ai/dsh",
                    "@deepseek-ai/dsh-session-format-catalog",
                    "ws",
                ],
                "node_modules/@deepseek-ai/dsh/lib/bin.js",
                Some("node_modules/@deepseek-ai/dsh-session-format-catalog/lib/index.js"),
            )
        } else {
            (
                &["@moonshot-ai/kimi-code", "ws"],
                "node_modules/@moonshot-ai/kimi-code/dist/main.mjs",
                None,
            )
        };
        ensure!(
            self.external_root.packages.len() == packages.len()
                && packages.iter().all(|p| self
                    .external_root
                    .packages
                    .get(*p)
                    .is_some_and(|v| pinned_version(v))),
            "invalid external package pins"
        );
        if component == "DSH" {
            ensure!(
                self.external_root.packages["@deepseek-ai/dsh"]
                    == self.external_root.packages["@deepseek-ai/dsh-session-format-catalog"],
                "DSH/catalog pins differ"
            );
        }
        ensure!(
            safe_entry(&self.external_root.cli_entry) && self.external_root.cli_entry == cli,
            "invalid CLI entry"
        );
        ensure!(
            self.external_root.catalog_entry.as_deref() == catalog
                && self
                    .external_root
                    .catalog_entry
                    .as_deref()
                    .is_none_or(safe_entry),
            "invalid catalog entry"
        );
        ensure!(
            self.node.range == NODE_RANGE
                && supported_node(&self.node.recommended)
                && self.node.features == ["node:sqlite"],
            "unsupported Node requirements"
        );
        ensure!(
            self.checks.rg == (component == "KIMI")
                && self.checks.node_pty == (component == "KIMI"),
            "invalid component checks"
        );
        ensure!(
            self.environment.install_root == format!("{component}_INSTALL_ROOT"),
            "invalid component environment"
        );
        Ok(())
    }
}

pub fn load_contract(dir: &Path, name: &str, version: &str) -> Result<ExternalRuntime> {
    let path = dir.join("external-runtime.json");
    let contract: ExternalRuntime = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    contract.validate(name, version)?;
    Ok(contract)
}

/// Both persisted explicit overrides and a fully resolved/pinned configuration use
/// this type. probe() never fills absent fields from the environment or PATH.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalConfig {
    pub external_root: Option<PathBuf>,
    pub node: Option<PathBuf>,
    pub rg: Option<PathBuf>,
}

impl ExternalConfig {
    pub fn validate(&self) -> Result<()> {
        for path in [&self.external_root, &self.node, &self.rg]
            .into_iter()
            .flatten()
        {
            ensure!(
                path.is_absolute() && !path.as_os_str().to_string_lossy().contains('\0'),
                "external configuration paths must be absolute"
            );
        }
        Ok(())
    }
}

/// Absent leaves the override unchanged; null removes it and restores defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PathUpdate {
    #[default]
    Unchanged,
    Clear,
    Set(PathBuf),
}

impl<'de> Deserialize<'de> for PathUpdate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(match Option::<PathBuf>::deserialize(deserializer)? {
            Some(path) => Self::Set(path),
            None => Self::Clear,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigureRequest {
    #[serde(default)]
    pub external_root: PathUpdate,
    #[serde(default)]
    pub node: PathUpdate,
    #[serde(default)]
    pub rg: PathUpdate,
}

impl ConfigureRequest {
    fn apply(self, config: &mut ExternalConfig) -> Result<()> {
        for (update, field) in [
            (self.external_root, &mut config.external_root),
            (self.node, &mut config.node),
            (self.rg, &mut config.rg),
        ] {
            match update {
                PathUpdate::Unchanged => {}
                PathUpdate::Clear => *field = None,
                PathUpdate::Set(path) => *field = Some(path),
            }
        }
        config.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalIssue {
    pub code: String,
    pub expected: Option<String>,
    pub actual: Option<String>,
    pub path: Option<PathBuf>,
    pub steps: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalStatus {
    pub version: String,
    pub root: Option<PathBuf>,
    pub node: Option<PathBuf>,
    pub rg: Option<PathBuf>,
    pub packages: Vec<String>,
    pub node_range: Option<String>,
    pub node_recommended: Option<String>,
    pub checked_at: Option<String>,
    pub issues: Vec<ExternalIssue>,
}

impl ExternalStatus {
    pub fn ready(&self) -> bool {
        self.checked_at.is_some() && self.issues.is_empty()
    }

    fn unchecked(contract: &ExternalRuntime, config: &ExternalConfig) -> Self {
        Self {
            version: contract.version.clone(),
            root: config.external_root.clone(),
            node: config.node.clone(),
            rg: config.rg.clone(),
            packages: contract
                .external_root
                .packages
                .iter()
                .map(|(p, v)| format!("{p}@{v}"))
                .collect(),
            node_range: Some(contract.node.range.clone()),
            node_recommended: Some(contract.node.recommended.clone()),
            checked_at: None,
            issues: Vec::new(),
        }
    }

    fn issue(
        &mut self,
        code: &str,
        expected: impl Into<String>,
        actual: Option<String>,
        path: Option<&Path>,
        step: impl Into<String>,
    ) {
        self.issues.push(ExternalIssue {
            code: code.into(),
            expected: Some(expected.into()),
            actual,
            path: path.map(Path::to_path_buf),
            steps: vec![step.into()],
        });
    }
}

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        path.extension()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.eq_ignore_ascii_case("exe"))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|v| v.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(any(windows, unix)))]
    {
        true
    }
}

fn rg_search_basename(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case("rg.exe")
            } else {
                name == "rg"
            }
        })
}

fn environment_value<'a>(env: &'a BTreeMap<OsString, OsString>, key: &str) -> Option<&'a OsString> {
    if cfg!(windows) {
        env.iter()
            .find(|(name, _)| name.to_string_lossy().eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    } else {
        env.get(std::ffi::OsStr::new(key))
    }
}

fn resolve_defaults(
    config: &ExternalConfig,
    contract: &ExternalRuntime,
    env: &BTreeMap<OsString, OsString>,
) -> Result<ExternalConfig> {
    let component = family(&contract.name)?;
    let from_env = |key: &str| {
        environment_value(env, key)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    let find = |name: &str| -> Option<PathBuf> {
        let filename = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        };
        std::env::split_paths(environment_value(env, "PATH")?)
            .filter(|p| p.is_absolute())
            .map(|p| p.join(&filename))
            .find(|p| executable(p))
    };
    let normalize = |path: Option<PathBuf>| path.map(|p| fs::canonicalize(&p).unwrap_or(p));
    let resolved = ExternalConfig {
        external_root: normalize(
            config
                .external_root
                .clone()
                .or_else(|| from_env(&format!("CUTEX_{component}_INSTALL_ROOT")))
                .or_else(|| from_env(&contract.environment.install_root)),
        ),
        node: normalize(
            config
                .node
                .clone()
                .or_else(|| from_env(&format!("CUTEX_{component}_NODE")))
                .or_else(|| find("node")),
        ),
        // Kimi searches PATH for rg[.exe], rather than launching this path
        // directly. Preserve the link's lexical name and parent directory:
        // canonicalizing rg -> differently-named target would break that lookup.
        rg: config.rg.clone().or_else(|| {
            if contract.checks.rg {
                from_env("CUTEX_KIMI_RG").or_else(|| find("rg"))
            } else {
                None
            }
        }),
    };
    resolved.validate()?;
    Ok(resolved)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    schema: String,
    name: String,
    config: ExternalConfig,
    contract: ExternalRuntime,
    resolved: ExternalConfig,
    status: ExternalStatus,
}

fn state_path(layout: &Layout, name: &str) -> Result<PathBuf> {
    family(name)?;
    Ok(layout
        .base
        .join("components/external")
        .join(format!("{name}.json")))
}

fn read_state(layout: &Layout, name: &str) -> Result<Option<State>> {
    Ok(read_state_with_digest(layout, name)?.map(|(state, _)| state))
}

fn read_state_with_digest(layout: &Layout, name: &str) -> Result<Option<(State, String)>> {
    let path = state_path(layout, name)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let state: State = serde_json::from_slice(&bytes)
        .with_context(|| format!("corrupt external state {}", path.display()))?;
    ensure!(
        state.schema == STATE_SCHEMA && state.name == name,
        "corrupt external state identity"
    );
    state.config.validate()?;
    state.resolved.validate()?;
    state.contract.validate(name, &state.contract.version)?;
    let mut unverified = state.status.clone();
    unverified.checked_at = None;
    unverified.issues.clear();
    ensure!(
        unverified == ExternalStatus::unchecked(&state.contract, &state.resolved),
        "corrupt external status binding"
    );
    if let Some(checked_at) = &state.status.checked_at {
        chrono::DateTime::parse_from_rfc3339(checked_at)
            .context("corrupt external status timestamp")?;
    } else {
        ensure!(
            state.status.issues.is_empty(),
            "corrupt unchecked external status"
        );
    }
    Ok(Some((state, crate::upgrade::manifest::sha256_hex(&bytes))))
}

fn resolve_with_env(
    layout: &Layout,
    name: &str,
    contract: &ExternalRuntime,
    env: &BTreeMap<OsString, OsString>,
) -> Result<(ExternalConfig, ExternalStatus)> {
    let (_, resolved, status, _) = resolved_state_with_env(layout, name, contract, env)?;
    Ok((resolved, status))
}

fn resolved_state_with_env(
    layout: &Layout,
    name: &str,
    contract: &ExternalRuntime,
    env: &BTreeMap<OsString, OsString>,
) -> Result<(
    ExternalConfig,
    ExternalConfig,
    ExternalStatus,
    Option<String>,
)> {
    contract.validate(name, &contract.version)?;
    let (state, digest) = match read_state_with_digest(layout, name)? {
        Some((state, digest)) => (Some(state), Some(digest)),
        None => (None, None),
    };
    let config = state.as_ref().map(|s| s.config.clone()).unwrap_or_default();
    let resolved = resolve_defaults(&config, contract, env)?;
    let status = match state {
        Some(state) if state.contract == *contract && state.resolved == resolved => state.status,
        _ => ExternalStatus::unchecked(contract, &resolved),
    };
    Ok((config, resolved, status, digest))
}

/// Resolve effective paths and cached status without executing any dependency.
/// Environment/PATH changes invalidate readiness, including when overrides are absent.
pub fn resolve(
    layout: &Layout,
    name: &str,
    contract: &ExternalRuntime,
) -> Result<(ExternalConfig, ExternalStatus)> {
    resolve_with_env(layout, name, contract, &std::env::vars_os().collect())
}

pub fn cached(layout: &Layout, name: &str, contract: &ExternalRuntime) -> Result<ExternalStatus> {
    Ok(resolve(layout, name, contract)?.1)
}

/// Owner-facing cached status. This reads one saved state and never probes,
/// persists status, or changes the paths used by the runtime.
pub fn view(layout: &Layout, name: &str, contract: &ExternalRuntime) -> Result<serde_json::Value> {
    view_with_env(layout, name, contract, &std::env::vars_os().collect())
}

fn view_with_env(
    layout: &Layout,
    name: &str,
    contract: &ExternalRuntime,
    env: &BTreeMap<OsString, OsString>,
) -> Result<serde_json::Value> {
    let (stored, resolved, status, digest) = resolved_state_with_env(layout, name, contract, env)?;
    let node_version = status
        .checked_at
        .as_ref()
        .and_then(|_| observed_node_version(layout, name, &resolved, digest.as_deref()?));
    let mut value = serde_json::to_value(status)?;
    for field in ["root", "node", "rg"] {
        display_path_value(&mut value[field]);
    }
    value["storedConfig"] = serde_json::to_value(stored)?;
    let mut paths = serde_json::to_value(resolved)?;
    for field in ["externalRoot", "node", "rg"] {
        display_path_value(&mut paths[field]);
    }
    value["resolved"] = paths;
    value["nodeVersion"] = serde_json::to_value(node_version)?;
    Ok(diagnostics_view(value))
}

// Presentation only: do not strip arbitrary Windows device/volume prefixes.
fn display_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\'
        {
            return rest.to_owned();
        }
        if let Some(unc) = rest.strip_prefix(r"UNC\") {
            let mut parts = unc.split('\\');
            let ordinary = |part: &str| {
                !part.is_empty()
                    && !matches!(part, "." | ".." | "?")
                    && !part.contains(['/', ':', '\0'])
            };
            if parts.next().is_some_and(ordinary) && parts.next().is_some_and(ordinary) {
                return format!(r"\\{unc}");
            }
        }
    }
    path.to_owned()
}

fn display_path_value(value: &mut serde_json::Value) {
    if let Some(path) = value.as_str() {
        *value = serde_json::Value::String(display_path(path));
    }
}

// Error text may quote a path containing spaces, or contain a bare path token.
// Apply the same conservative presentation rules as structured path fields.
fn display_path_text(text: &str) -> String {
    let mut output = String::new();
    let mut remaining = text;
    while let Some(start) = remaining.find(r"\\?\") {
        output.push_str(&remaining[..start]);
        let path = &remaining[start..];
        let quote = remaining[..start]
            .chars()
            .next_back()
            .filter(|c| matches!(c, '\'' | '"'));
        let end = path
            .char_indices()
            .find_map(|(offset, c)| {
                let boundary = match quote {
                    Some(quote) => c == quote || c == '\n' || c == '\r',
                    None => {
                        c.is_whitespace() || matches!(c, '\'' | '"' | ',' | ';' | ')' | ']' | '}')
                    }
                };
                boundary.then_some(offset)
            })
            .unwrap_or(path.len());
        output.push_str(&display_path(&path[..end]));
        remaining = &path[end..];
    }
    output.push_str(remaining);
    output
}

fn compact_actual(actual: &str) -> String {
    let useful = |line: &&str| {
        !line.is_empty()
            && !line.starts_with("at ")
            && !line.starts_with("node:")
            && !line.starts_with("Node.js ")
            && !line.chars().all(|c| c == '^' || c.is_whitespace())
    };
    let error = |line: &&str| {
        line.contains("Error:")
            || line.contains("Error [")
            || line.starts_with("MODULE_")
            || line.starts_with("ERR_")
    };
    let selected = actual
        .lines()
        .map(str::trim)
        .filter(useful)
        .find(error)
        .or_else(|| actual.lines().map(str::trim).find(useful))
        .unwrap_or("");
    let compact = display_path_text(selected)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.chars().count() <= 240 {
        compact
    } else {
        compact
            .chars()
            .take(239)
            .chain(std::iter::once('…'))
            .collect()
    }
}

fn bounded_diagnostics(value: &str) -> String {
    const LIMIT: usize = 64 * 1024;
    const MARKER: &str = "\n… [truncated]";
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT - MARKER.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &value[..end])
}

/// Sanitize owner-facing issue JSON, including fallback errors. Persisted typed
/// issues are deliberately unchanged; repeated application preserves diagnostics.
pub fn diagnostics_view(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(issues) = value.get_mut("issues").and_then(|v| v.as_array_mut()) {
        for issue in issues {
            let Some(issue) = issue.as_object_mut() else {
                continue;
            };
            if let Some(path) = issue.get_mut("path") {
                display_path_value(path);
            }
            if let Some(actual) = issue.get("actual").and_then(|v| v.as_str()) {
                let compact = compact_actual(actual);
                let diagnostics = (compact != actual
                    && !issue.get("diagnostics").is_some_and(|v| v.is_string()))
                .then(|| bounded_diagnostics(actual));
                if let Some(diagnostics) = diagnostics {
                    issue.insert("diagnostics".into(), diagnostics.into());
                }
                issue.insert("actual".into(), compact.into());
            }
            if let Some(diagnostics) = issue.get("diagnostics").and_then(|v| v.as_str()) {
                let bounded = bounded_diagnostics(diagnostics);
                issue.insert("diagnostics".into(), bounded.into());
            }
        }
    }
    value
}

/// Last explicitly checked/configured candidate, including an unselected source
/// build awaiting dependencies. Reading it never probes or changes defaults.
pub fn current_contract(layout: &Layout, name: &str) -> Result<Option<ExternalRuntime>> {
    Ok(read_state(layout, name)?.map(|state| state.contract))
}

const NODE_VERSION_SCHEMA: &str = "cutex/external-node-version/v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NodeFingerprint {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
}

impl NodeFingerprint {
    fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(Self {
            len: metadata.len(),
            modified_secs: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NodeVersionObservation {
    schema: String,
    node: PathBuf,
    fingerprint: NodeFingerprint,
    state_sha256: String,
    observed_node_version: String,
}

fn node_version_path(layout: &Layout, name: &str) -> Result<PathBuf> {
    Ok(state_path(layout, name)?.with_file_name(format!("{name}.node-version.json")))
}

fn valid_node_version(value: &str) -> bool {
    value.strip_prefix('v').is_some_and(pinned_version)
}

fn observed_node_version(
    layout: &Layout,
    name: &str,
    resolved: &ExternalConfig,
    digest: &str,
) -> Option<String> {
    let node = resolved.node.as_ref()?;
    let bytes = fs::read(node_version_path(layout, name).ok()?).ok()?;
    let observation: NodeVersionObservation = serde_json::from_slice(&bytes).ok()?;
    (observation.schema == NODE_VERSION_SCHEMA
        && observation.node == *node
        && observation.state_sha256 == digest
        && Some(observation.fingerprint) == NodeFingerprint::read(node)
        && valid_node_version(&observation.observed_node_version))
    .then_some(observation.observed_node_version)
}

struct ObserveNodeExec<'a> {
    inner: &'a dyn Exec,
    node: Option<&'a Path>,
    fingerprint: Option<NodeFingerprint>,
    observed: std::cell::RefCell<Option<String>>,
    version_checked: std::cell::Cell<bool>,
}

impl Exec for ObserveNodeExec<'_> {
    fn run(&self, program: &str, args: &[String]) -> Result<String> {
        let result = self.inner.run(program, args);
        if self.node == Some(Path::new(program))
            && args == ["--version"]
            && !self.version_checked.replace(true)
        {
            *self.observed.borrow_mut() = result
                .as_ref()
                .ok()
                .map(|output| output.trim())
                .filter(|output| valid_node_version(output))
                .map(str::to_owned);
        }
        result
    }
}

fn save_node_observation(
    layout: &Layout,
    name: &str,
    state: &State,
    exec: &ObserveNodeExec<'_>,
) -> Result<()> {
    let Some(version) = exec.observed.borrow().clone() else {
        return Ok(());
    };
    let Some(node) = state.resolved.node.as_ref() else {
        return Ok(());
    };
    let Some(fingerprint) = exec.fingerprint.as_ref() else {
        return Ok(());
    };
    ensure!(
        Some(fingerprint.clone()) == NodeFingerprint::read(node),
        "Node changed during external check"
    );
    let bytes = fs::read(state_path(layout, name)?)?;
    // A concurrent replacement must not associate this probe with another state.
    ensure!(
        serde_json::from_slice::<serde_json::Value>(&bytes)? == serde_json::to_value(state)?,
        "external state changed during check"
    );
    let observation = NodeVersionObservation {
        schema: NODE_VERSION_SCHEMA.into(),
        node: node.clone(),
        fingerprint: fingerprint.clone(),
        state_sha256: crate::upgrade::manifest::sha256_hex(&bytes),
        observed_node_version: version,
    };
    write_private_pretty_json_atomic(
        &node_version_path(layout, name)?,
        &observation,
        "observed external Node version",
    )
}

pub fn configure(
    layout: &Layout,
    name: &str,
    contract: &ExternalRuntime,
    request: ConfigureRequest,
    exec: &dyn Exec,
) -> Result<ExternalStatus> {
    contract.validate(name, &contract.version)?;
    let mut config = read_state(layout, name)?
        .map(|s| s.config)
        .unwrap_or_default();
    request.apply(&mut config)?;
    let resolved = resolve_defaults(&config, contract, &std::env::vars_os().collect())?;
    let path = state_path(layout, name)?;
    let mut state = State {
        schema: STATE_SCHEMA.into(),
        name: name.into(),
        config,
        contract: contract.clone(),
        status: ExternalStatus::unchecked(contract, &resolved),
        resolved,
    };
    // Durable invalidation MUST precede the first process spawn, even if a probe
    // errors or the daemon exits before the completed result can be written.
    write_private_pretty_json_atomic(&path, &state, "external runtime configuration")?;
    let observed = ObserveNodeExec {
        inner: exec,
        node: state.resolved.node.as_deref(),
        fingerprint: state
            .resolved
            .node
            .as_deref()
            .and_then(NodeFingerprint::read),
        observed: std::cell::RefCell::new(None),
        version_checked: std::cell::Cell::new(false),
    };
    state.status = probe(contract, &state.resolved, &observed)?;
    write_private_pretty_json_atomic(&path, &state, "external runtime status")?;
    // Display-only evidence is optional; its failure must never fail configure.
    let _ = save_node_observation(layout, name, &state, &observed);
    Ok(state.status)
}

pub fn check(
    layout: &Layout,
    name: &str,
    contract: &ExternalRuntime,
    exec: &dyn Exec,
) -> Result<ExternalStatus> {
    configure(layout, name, contract, ConfigureRequest::default(), exec)
}

/// Probe exactly these pinned paths: no persisted configuration, environment
/// defaults, npm, vendor CLI, network, credentials or PTY spawn is consulted.
// Resolve without importing vendor code: CLI packages may expose only `bin`
// (no bare-package export), while ws/catalog are loaded via createRequire by
// the adapters. Let Node apply its real main/exports conditions and fallbacks.
const PACKAGE_ENTRY_SCRIPT: &str = r#"
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { statSync } from 'node:fs';
const [root, primary, cli, catalog, packages] = process.argv.slice(1);
const require = createRequire(pathToFileURL(join(root, 'package.json')));
const issues = [];
function check(packageName, specifier, path) {
  try {
    const resolved = require.resolve(specifier);
    if (!statSync(resolved).isFile()) throw Error('resolved entry is not a file');
  } catch (error) {
    issues.push({package: packageName, path, actual: `${error.code ?? 'ENTRY_INVALID'}: ${error.message}`});
  }
}
for (const name of JSON.parse(packages)) {
  if (name === primary) check(name, join(root, cli), join(root, cli));
  else check(name, name, join(root, 'node_modules', name));
}
if (catalog) check('catalogEntry', join(root, catalog), join(root, catalog));
console.log(JSON.stringify({marker: 'cutex-entries-v1', issues}));
"#;

fn probe_package_entries(
    contract: &ExternalRuntime,
    root: &Path,
    node: &Path,
    exec: &dyn Exec,
    status: &mut ExternalStatus,
) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EntryIssue {
        package: String,
        path: PathBuf,
        actual: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EntryResult {
        marker: String,
        issues: Vec<EntryIssue>,
    }
    let primary = if contract.name == "dsh-runtime" {
        "@deepseek-ai/dsh"
    } else {
        "@moonshot-ai/kimi-code"
    };
    let args = vec![
        "--input-type=module".into(),
        "-e".into(),
        PACKAGE_ENTRY_SCRIPT.into(),
        root.to_str().context("external root is not UTF-8")?.into(),
        primary.into(),
        contract.external_root.cli_entry.clone(),
        contract
            .external_root
            .catalog_entry
            .clone()
            .unwrap_or_default(),
        serde_json::to_string(&contract.external_root.packages.keys().collect::<Vec<_>>())?,
    ];
    let result = exec
        .run(node.to_str().context("Node path is not UTF-8")?, &args)
        .and_then(|output| {
            serde_json::from_str::<EntryResult>(&output)
                .context("invalid Node entry probe response")
        })
        .and_then(|result| {
            ensure!(
                result.marker == "cutex-entries-v1",
                "unexpected Node entry probe response"
            );
            Ok(result)
        });
    match result {
        Ok(result) => for issue in result.issues {
            status.issue("external_root_invalid", format!("Node-resolvable entry for {}", issue.package),
                Some(issue.actual), Some(&issue.path), "Repair or reinstall the exact package in this npm prefix so its main/exports and required CLI/catalog entry resolve to existing files, then check again.");
        },
        Err(error) => status.issue("external_root_invalid", "Node-resolvable external package entries",
            Some(error.to_string()), Some(root), "Check the external package main/exports and required CLI/catalog files, repair the npm prefix, then check again."),
    }
    Ok(())
}

pub fn probe(
    contract: &ExternalRuntime,
    config: &ExternalConfig,
    exec: &dyn Exec,
) -> Result<ExternalStatus> {
    contract.validate(&contract.name, &contract.version)?;
    config.validate()?;
    let mut status = ExternalStatus::unchecked(contract, config);
    if let Some(root) = &config.external_root {
        if !root.is_dir() {
            status.issue(
                "external_root_invalid",
                "existing npm prefix directory",
                None,
                Some(root),
                "Configure the absolute npm prefix containing the pinned packages.",
            );
        } else {
            for (name, version) in &contract.external_root.packages {
                let path = root.join("node_modules").join(name).join("package.json");
                let value = fs::read(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| {
                        serde_json::from_slice::<serde_json::Value>(&bytes).map_err(Into::into)
                    });
                let actual = value.and_then(|value| {
                    ensure!(
                        value["name"].as_str() == Some(name),
                        "package name missing or name mismatch"
                    );
                    let version = value["version"]
                        .as_str()
                        .context("package version missing or invalid")?;
                    ensure!(
                        pinned_version(version),
                        "package version is not a valid exact version"
                    );
                    Ok(version.to_owned())
                });
                match actual {
                    Ok(actual) if actual != *version => {
                        status.issue("upstream_version_mismatch", format!("{name}@{version}"), Some(actual), Some(&path), "Install the exact package versions into this npm prefix, then check again.");
                    }
                    Err(error) => {
                        status.issue("external_root_invalid", format!("valid package.json for {name}@{version}"), Some(error.to_string()), Some(&path), "Repair the missing, unreadable or invalid package metadata in this npm prefix, then check again.");
                    }
                    _ => {}
                }
            }
            for entry in std::iter::once(&contract.external_root.cli_entry)
                .chain(contract.external_root.catalog_entry.iter())
            {
                let path = root.join(entry);
                if !path.is_file() {
                    status.issue(
                        "external_root_invalid",
                        "required adapter entry file",
                        None,
                        Some(&path),
                        "Repair the external npm prefix, then check again.",
                    );
                }
            }
        }
    } else {
        status.issue(
            "external_root_missing",
            "absolute npm prefix",
            None,
            None,
            "Configure externalRoot or the component install-root environment variable.",
        );
    }
    if let Some(node) = config.node.as_deref().filter(|p| executable(p)) {
        let output = exec.run(
            node.to_str().context("Node path is not UTF-8")?,
            &["--version".into()],
        );
        let supported = match output {
            Ok(output) if output.trim().strip_prefix('v').is_some_and(supported_node) => true,
            other => {
                status.issue(
                    "node_version_unsupported",
                    &contract.node.range,
                    Some(match other {
                        Ok(v) => v.trim().to_owned(),
                        Err(e) => e.to_string(),
                    }),
                    Some(node),
                    format!(
                        "Use Node {} ({}).",
                        contract.node.recommended, contract.node.range
                    ),
                );
                false
            }
        };
        if supported {
            if let Some(root) = config.external_root.as_ref().filter(|root| root.is_dir()) {
                probe_package_entries(contract, root, node, exec, &mut status)?;
            }
            let script = "import { DatabaseSync } from 'node:sqlite'; if (typeof DatabaseSync !== 'function') throw Error('node:sqlite unavailable'); console.log('cutex-sqlite-ok');";
            let result = exec.run(
                node.to_str().context("Node path is not UTF-8")?,
                &["--input-type=module".into(), "-e".into(), script.into()],
            );
            if !result.as_ref().is_ok_and(|s| s.trim() == "cutex-sqlite-ok") {
                status.issue(
                    "node_feature_missing",
                    "node:sqlite DatabaseSync",
                    Some(
                        result
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "unexpected sqlite probe output".into()),
                    ),
                    Some(node),
                    format!(
                        "Use the standard Node {} build with node:sqlite.",
                        contract.node.recommended
                    ),
                );
            }
            if contract.checks.node_pty {
                if let Some(root) = config.external_root.as_ref().filter(|r| r.is_dir()) {
                    let anchor = root.join("node_modules/@moonshot-ai/kimi-code/package.json");
                    // Windows node-pty defers native loading until PTY creation.
                    // Load its native module explicitly without constructing a PTY.
                    let script = "import { createRequire } from 'node:module'; import { pathToFileURL } from 'node:url'; const require = createRequire(pathToFileURL(process.argv[1])); const pty = require('node-pty'); if (typeof pty.spawn !== 'function') throw Error('node-pty binding unavailable'); if (process.platform === 'win32') { const ptyRequire = createRequire(require.resolve('node-pty')); const binding = ptyRequire('./utils').loadNativeModule('conpty').module; if (!binding || typeof binding.startProcess !== 'function') throw Error('ConPTY binding unavailable'); } console.log('cutex-pty-ok');";
                    let result = exec.run(
                        node.to_str().context("Node path is not UTF-8")?,
                        &[
                            "--input-type=module".into(),
                            "-e".into(),
                            script.into(),
                            anchor.to_str().context("Kimi path is not UTF-8")?.into(),
                        ],
                    );
                    if !result.as_ref().is_ok_and(|s| s.trim() == "cutex-pty-ok") {
                        status.issue("kimi_pty_unavailable", "loadable node-pty for this Node ABI", Some(result.err().map(|e| e.to_string()).unwrap_or_else(|| "unexpected node-pty probe output".into())), Some(&anchor), "Install/rebuild the Kimi node-pty native binding for this host and configured Node, then check again.");
                    }
                }
            }
        }
    } else {
        status.issue(
            "node_missing",
            "absolute Node executable (no shell shim)",
            None,
            config.node.as_deref(),
            "Configure node or put the Node executable on the daemon PATH.",
        );
    }
    if contract.checks.rg {
        let result = match config.rg.as_deref() {
            Some(rg) if !rg_search_basename(rg) => Err(anyhow::anyhow!(
                "Kimi searches PATH for {}; configured ripgrep has a different filename",
                if cfg!(windows) { "rg.exe" } else { "rg" }
            )),
            Some(rg) if executable(rg) => exec.run(
                rg.to_str().context("rg path is not UTF-8")?,
                &["--version".into()],
            ),
            _ => Err(anyhow::anyhow!("ripgrep executable missing")),
        };
        if !result.as_ref().is_ok_and(|s| s.starts_with("ripgrep ")) {
            status.issue(
                "kimi_rg_missing",
                format!("ripgrep executable named {}", if cfg!(windows) { "rg.exe" } else { "rg" }),
                Some(
                    result
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unexpected ripgrep version output".into()),
                ),
                config.rg.as_deref(),
                "Configure a ripgrep executable or symlink named rg.exe (Windows) / rg (Unix). Rename the executable or provide that standard name in its directory, then check again; Kimi searches its parent directory on PATH.",
            );
        }
    }
    status.checked_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    Ok(status)
}

/// The service executor has no timeout. Dependency probes instead use this
/// bounded implementation (10 seconds, at most 64 KiB per output stream).
pub struct RealExec;

impl Exec for RealExec {
    fn run(&self, program: &str, args: &[String]) -> Result<String> {
        ensure!(
            Path::new(program).is_absolute() && executable(Path::new(program)),
            "probe program must be an absolute executable"
        );
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("start {program}"))?;
        let (sender, receiver) = std::sync::mpsc::channel();
        for (index, pipe) in [
            (
                0,
                Box::new(child.stdout.take().context("stdout")?) as Box<dyn Read + Send>,
            ),
            (
                1,
                Box::new(child.stderr.take().context("stderr")?) as Box<dyn Read + Send>,
            ),
        ] {
            let sender = sender.clone();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe.take(65_537).read_to_end(&mut bytes);
                let _ = sender.send((index, bytes, result));
            });
        }
        drop(sender);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut outputs = [Vec::new(), Vec::new()];
        let mut received = 0;
        let result = (|| -> Result<String> {
            loop {
                while let Ok((index, bytes, read_result)) = receiver.try_recv() {
                    read_result?;
                    ensure!(bytes.len() <= 65_536, "probe output exceeded 64 KiB");
                    outputs[index] = bytes;
                    received += 1;
                }
                if let Some(exit) = child.try_wait()? {
                    if received == 2 {
                        ensure!(
                            exit.success(),
                            "probe failed: {}",
                            String::from_utf8_lossy(&outputs[1])
                        );
                        return String::from_utf8(outputs[0].clone())
                            .context("probe output is not UTF-8");
                    }
                }
                ensure!(
                    Instant::now() < deadline,
                    "probe timed out after 10 seconds"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    mod view_tests {
        include!("external_runtime_view_tests.rs");
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("cutex-external-runtime-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn contract(kimi: bool) -> ExternalRuntime {
        let (name, version, packages, cli, catalog, env) = if kimi {
            (
                "kimi-runtime",
                "kimi-r3",
                json!({"@moonshot-ai/kimi-code":"2.3.3", "ws":"8.22.0"}),
                "node_modules/@moonshot-ai/kimi-code/dist/main.mjs",
                None,
                "KIMI_INSTALL_ROOT",
            )
        } else {
            (
                "dsh-runtime",
                "dsh-r3",
                json!({"@deepseek-ai/dsh":"0.2.1-rc.3", "@deepseek-ai/dsh-session-format-catalog":"0.2.1-rc.3", "ws":"8.22.0"}),
                "node_modules/@deepseek-ai/dsh/lib/bin.js",
                Some("node_modules/@deepseek-ai/dsh-session-format-catalog/lib/index.js"),
                "DSH_INSTALL_ROOT",
            )
        };
        serde_json::from_value(json!({
            "schema": SCHEMA, "name": name, "version": version, "payload":"adapter-v1",
            "externalRoot":{"kind":"npm-prefix", "packages":packages, "cliEntry":cli, "catalogEntry":catalog},
            "node":{"range":NODE_RANGE,"recommended":"24.21.0","features":["node:sqlite"]},
            "checks":{"rg":kimi,"nodePty":kimi}, "environment":{"installRoot":env}
        })).unwrap()
    }

    fn put(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn fake_binary(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        });
        put(&path, b"fixture only; must never execute");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::canonicalize(path).unwrap()
    }

    struct Fixture {
        _dir: TestDir,
        layout: Layout,
        contract: ExternalRuntime,
        config: ExternalConfig,
    }

    impl Fixture {
        fn new(kimi: bool) -> Self {
            let dir = TestDir::new();
            let root = dir.path().join("prefix");
            let contract = contract(kimi);
            for (name, version) in &contract.external_root.packages {
                let mut metadata = json!({"name": name, "version": version});
                if name == "ws" {
                    metadata["main"] = json!("index.js");
                }
                if name == "@deepseek-ai/dsh-session-format-catalog" {
                    metadata["main"] = json!("lib/index.js");
                }
                put(
                    &root.join("node_modules").join(name).join("package.json"),
                    &serde_json::to_vec(&metadata).unwrap(),
                );
            }
            put(
                &root.join("node_modules/ws/index.js"),
                b"throw Error('entry probe must never execute package code');\n",
            );
            for entry in std::iter::once(&contract.external_root.cli_entry)
                .chain(contract.external_root.catalog_entry.iter())
            {
                put(&root.join(entry), b"// fixture, do not execute\n");
            }
            let root = fs::canonicalize(root).unwrap();
            let node = fake_binary(&dir.path().join("bin"), "node");
            let rg = fake_binary(&dir.path().join("bin"), "rg");
            Self {
                layout: Layout::at(dir.path().join("managed")),
                contract,
                config: ExternalConfig {
                    external_root: Some(root),
                    node: Some(node),
                    rg: Some(rg),
                },
                _dir: dir,
            }
        }

        fn request(&self) -> ConfigureRequest {
            serde_json::from_value(json!({"externalRoot":self.config.external_root,"node":self.config.node,"rg":self.config.rg})).unwrap()
        }
    }

    struct FakeExec {
        version: String,
        fail: Option<&'static str>,
        calls: RefCell<Vec<(String, Vec<String>)>>,
        invalidation_path: Option<PathBuf>,
    }

    impl Default for FakeExec {
        fn default() -> Self {
            Self {
                version: "v24.21.0\n".into(),
                fail: None,
                calls: RefCell::new(Vec::new()),
                invalidation_path: None,
            }
        }
    }

    impl Exec for FakeExec {
        fn run(&self, program: &str, args: &[String]) -> Result<String> {
            self.calls
                .borrow_mut()
                .push((program.into(), args.to_vec()));
            if let Some(path) = &self.invalidation_path {
                let state: State = serde_json::from_slice(&fs::read(path)?).unwrap();
                assert!(
                    state.status.checked_at.is_none(),
                    "invalidation must be durable before EVERY spawn"
                );
            }
            if Path::new(program).file_stem().unwrap() == "rg" {
                ensure!(self.fail != Some("rg"), "fixture rg failure");
                assert_eq!(args, ["--version"]);
                return Ok("ripgrep 14.1.1\n".into());
            }
            if args == ["--version"] {
                return Ok(self.version.clone());
            }
            assert_eq!(&args[..2], ["--input-type=module", "-e"]);
            if args[2].contains("cutex-entries-v1") {
                ensure!(
                    self.fail != Some("entries"),
                    "fixture entry resolution failure"
                );
                Ok(json!({"marker": "cutex-entries-v1", "issues": []}).to_string())
            } else if args[2].contains("node:sqlite") {
                ensure!(self.fail != Some("sqlite"), "fixture sqlite failure");
                assert_eq!(args.len(), 3);
                Ok("cutex-sqlite-ok\n".into())
            } else {
                ensure!(self.fail != Some("pty"), "fixture pty failure");
                assert!(args[2].contains("createRequire(pathToFileURL(process.argv[1]))"));
                assert!(!args[2].contains("pty.spawn("));
                assert!(args[2].contains("loadNativeModule('conpty').module"));
                assert!(!args[2].contains("binding.startProcess("));
                assert!(Path::new(&args[3])
                    .ends_with("node_modules/@moonshot-ai/kimi-code/package.json"));
                Ok("cutex-pty-ok\n".into())
            }
        }
    }

    #[test]
    fn strict_contract_identity_paths_packages_features_checks_environment() {
        for kimi in [false, true] {
            let original = contract(kimi);
            original
                .validate(&original.name, &original.version)
                .unwrap();
            assert!(original.validate(&original.name, "wrong-r3").is_err());
            for change in [
                json!({"schema":"bad"}),
                json!({"payload":"bundled"}),
                json!({"externalRoot":{"kind":"directory"}}),
                json!({"externalRoot":{"cliEntry":"../escape.js"}}),
                json!({"externalRoot":{"cliEntry":"C:/escape.js"}}),
                json!({"node":{"range":">=22"}}),
                json!({"node":{"recommended":"25.0.0"}}),
                json!({"node":{"features":[]}}),
                json!({"checks":{"rg":!kimi}}),
                json!({"checks":{"nodePty":!kimi}}),
                json!({"environment":{"installRoot":"OTHER_INSTALL_ROOT"}}),
            ] {
                let mut value = serde_json::to_value(&original).unwrap();
                for (key, new) in change.as_object().unwrap() {
                    if let Some(fields) = new.as_object() {
                        for (field, v) in fields {
                            value[key][field] = v.clone();
                        }
                    } else {
                        value[key] = new.clone();
                    }
                }
                let changed: ExternalRuntime = serde_json::from_value(value).unwrap();
                assert!(changed.validate(&original.name, &original.version).is_err());
            }
            let mut changed = original.clone();
            changed
                .external_root
                .packages
                .insert("extra".into(), "1.0.0".into());
            assert!(changed.validate(&original.name, &original.version).is_err());
            changed = original.clone();
            changed
                .external_root
                .packages
                .insert("ws".into(), "^8.22.0".into());
            assert!(changed.validate(&original.name, &original.version).is_err());
            let mut value = serde_json::to_value(original).unwrap();
            value["command"] = json!("not allowed");
            assert!(serde_json::from_value::<ExternalRuntime>(value).is_err());
        }
    }

    #[test]
    fn load_contract_checks_requested_identity() {
        let fixture = Fixture::new(false);
        let dir = fixture._dir.path().join("adapter");
        put(
            &dir.join("external-runtime.json"),
            &serde_json::to_vec(&fixture.contract).unwrap(),
        );
        assert_eq!(
            load_contract(&dir, "dsh-runtime", "dsh-r3").unwrap(),
            fixture.contract
        );
        assert!(load_contract(&dir, "dsh-runtime", "dsh-r4").is_err());
        assert!(load_contract(&dir, "kimi-runtime", "kimi-r3").is_err());
    }

    #[test]
    fn future_revision_accepts_exact_new_pins_but_not_other_component_identity() {
        for kimi in [false, true] {
            let mut next = contract(kimi);
            next.version = if kimi { "kimi-r4" } else { "dsh-r4" }.into();
            next.validate(&next.name, &next.version).unwrap();
            next.external_root
                .packages
                .insert("ws".into(), "8.23.0".into());
            next.validate(&next.name, &next.version).unwrap();
            for invalid in [
                "kimi-r",
                "dsh-r",
                "kimi-r3/escape",
                "dsh-r-3",
                "dsh-r0",
                "kimi-r99999999999999999999999999",
            ] {
                let mut invalid_contract = next.clone();
                invalid_contract.version = invalid.into();
                assert!(invalid_contract.validate(&next.name, invalid).is_err());
            }
        }
        for version in ["0.2.1-rc.3", "2.3.3", "1.2.3+build.4"] {
            assert!(pinned_version(version));
        }
        for version in [
            "^2.3.3",
            "1.2.3-rc..1",
            "1.2.3-01",
            "1.2.3+",
            "1.2.3+a+b",
            "01.2.3",
        ] {
            assert!(!pinned_version(version));
        }
    }

    #[test]
    fn absent_null_absolute_and_default_restoration() {
        let fixture = Fixture::new(true);
        let mut config = fixture.config.clone();
        let absent: ConfigureRequest = serde_json::from_str("{}").unwrap();
        absent.apply(&mut config).unwrap();
        assert_eq!(config, fixture.config);
        let clear: ConfigureRequest =
            serde_json::from_str(r#"{"externalRoot":null,"node":null,"rg":null}"#).unwrap();
        clear.apply(&mut config).unwrap();
        assert_eq!(config, ExternalConfig::default());
        let env = BTreeMap::from([
            (
                OsString::from("KIMI_INSTALL_ROOT"),
                fixture
                    .config
                    .external_root
                    .as_ref()
                    .unwrap()
                    .as_os_str()
                    .into(),
            ),
            (
                OsString::from("CUTEX_KIMI_NODE"),
                fixture.config.node.as_ref().unwrap().as_os_str().into(),
            ),
            (
                OsString::from("PATH"),
                std::env::join_paths([fixture.config.rg.as_ref().unwrap().parent().unwrap()])
                    .unwrap(),
            ),
        ]);
        assert_eq!(
            resolve_defaults(&config, &fixture.contract, &env).unwrap(),
            fixture.config
        );
        let relative: ConfigureRequest =
            serde_json::from_str(r#"{"node":"relative/node"}"#).unwrap();
        assert!(relative.apply(&mut config).is_err());
        assert!(serde_json::from_str::<ConfigureRequest>(r#"{"command":"anything"}"#).is_err());
        assert!(serde_json::from_str::<ConfigureRequest>(r#"{"node":24}"#).is_err());
    }

    #[test]
    fn successful_probe_pins_paths_and_never_invokes_vendor_cli() {
        for kimi in [false, true] {
            let fixture = Fixture::new(kimi);
            let exec = FakeExec::default();
            let status = probe(&fixture.contract, &fixture.config, &exec).unwrap();
            assert!(status.ready(), "{:?}", status.issues);
            assert_eq!(status.root, fixture.config.external_root);
            assert_eq!(status.node, fixture.config.node);
            assert_eq!(status.rg, fixture.config.rg);
            assert_eq!(status.packages.len(), if kimi { 2 } else { 3 });
            let calls = exec.calls.borrow();
            assert_eq!(calls.len(), if kimi { 5 } else { 3 });
            assert!(calls.iter().all(|(p, _)| Some(Path::new(p))
                == fixture.config.node.as_deref()
                || Some(Path::new(p)) == fixture.config.rg.as_deref()));
        }
    }

    struct RealEntryExec {
        node: PathBuf,
        fake: FakeExec,
    }
    impl Exec for RealEntryExec {
        fn run(&self, program: &str, args: &[String]) -> Result<String> {
            if args
                .get(2)
                .is_some_and(|script| script.contains("cutex-entries-v1"))
            {
                RealExec.run(self.node.to_str().unwrap(), args)
            } else {
                self.fake.run(program, args)
            }
        }
    }

    fn real_entry_exec() -> Option<RealEntryExec> {
        std::env::var_os("CUTEX_TEST_EXTERNAL_NODE").map(|node| RealEntryExec {
            node: PathBuf::from(node),
            fake: FakeExec::default(),
        })
    }

    fn broken_ws_entries_are_not_ready(kimi: bool) {
        let Some(exec) = real_entry_exec() else {
            return;
        };
        let fixture = Fixture::new(kimi);
        let contract = &fixture.contract;
        assert!(configure(
            &fixture.layout,
            &contract.name,
            contract,
            fixture.request(),
            &exec
        )
        .unwrap()
        .ready());
        let ws = fixture
            .config
            .external_root
            .as_ref()
            .unwrap()
            .join("node_modules/ws");
        fs::remove_file(ws.join("index.js")).unwrap();
        // Metadata remains pinned, but main's file is absent. Exercise both
        // owner mutations, including their persisted needsUser result.
        let status = configure(
            &fixture.layout,
            &contract.name,
            contract,
            fixture.request(),
            &exec,
        )
        .unwrap();
        assert!(!status.ready());
        assert!(status
            .issues
            .iter()
            .any(|issue| issue.code == "external_root_invalid"
                && issue.path.as_ref() == Some(&ws)
                && !issue.steps.is_empty()));
        assert!(!cached(&fixture.layout, &contract.name, contract)
            .unwrap()
            .ready());
        assert!(!check(&fixture.layout, &contract.name, contract, &exec)
            .unwrap()
            .ready());
        put(
            &ws.join("index.js"),
            b"throw Error('must not execute ws');\n",
        );
        // A healthy main must not hide a broken exports target: Node resolves
        // exports first, rather than a hand-written package.json main check.
        // Conditional exports are ordered. A sorted JSON object would move
        // default before require and legitimately select the healthy index.js.
        put(&ws.join("package.json"), br#"{"name":"ws","version":"8.22.0","main":"index.js","exports":{".":{"default":"./index.js","require":"./missing.cjs"}}}"#);
        assert!(check(&fixture.layout, &contract.name, contract, &exec)
            .unwrap()
            .ready());
        put(&ws.join("package.json"), br#"{"name":"ws","version":"8.22.0","main":"index.js","exports":{".":{"require":"./missing.cjs","default":"./index.js"}}}"#);
        assert!(!check(&fixture.layout, &contract.name, contract, &exec)
            .unwrap()
            .ready());
        put(
            &ws.join("missing.cjs"),
            b"throw Error('must not execute exports entry');\n",
        );
        assert!(check(&fixture.layout, &contract.name, contract, &exec)
            .unwrap()
            .ready());
    }

    #[test]
    fn dsh_broken_ws_main_and_exports_are_needs_user() {
        broken_ws_entries_are_not_ready(false);
    }

    #[test]
    fn kimi_broken_ws_main_and_exports_are_needs_user() {
        broken_ws_entries_are_not_ready(true);
    }

    #[test]
    fn real_node_resolves_cli_and_catalog_without_loading_them() {
        let Some(exec) = real_entry_exec() else {
            return;
        };
        for kimi in [false, true] {
            let fixture = Fixture::new(kimi);
            let root = fixture.config.external_root.as_ref().unwrap();
            let contract = &fixture.contract;
            assert!(probe(contract, &fixture.config, &exec).unwrap().ready());
            let cli = root.join(&contract.external_root.cli_entry);
            fs::remove_file(&cli).unwrap();
            assert!(!probe(contract, &fixture.config, &exec).unwrap().ready());
            put(&cli, b"throw Error('CLI must not run');\n");
            if let Some(entry) = &contract.external_root.catalog_entry {
                let metadata =
                    root.join("node_modules/@deepseek-ai/dsh-session-format-catalog/package.json");
                put(&metadata, &serde_json::to_vec(&json!({"name":"@deepseek-ai/dsh-session-format-catalog", "version":"0.2.1-rc.3",
                    "main":"lib/index.js", "exports":{".":"./lib/missing.js"}})).unwrap());
                assert!(root.join(entry).is_file());
                let status = probe(contract, &fixture.config, &exec).unwrap();
                assert!(!status.ready());
                assert!(status
                    .issues
                    .iter()
                    .any(|issue| issue.code == "external_root_invalid"
                        && issue.path == Some(metadata.parent().unwrap().to_path_buf())));
            }
        }
    }

    #[test]
    fn failed_entry_probe_is_needs_user_instead_of_ready() {
        let fixture = Fixture::new(false);
        let exec = FakeExec {
            fail: Some("entries"),
            ..FakeExec::default()
        };
        let status = probe(&fixture.contract, &fixture.config, &exec).unwrap();
        assert!(!status.ready());
        assert!(status
            .issues
            .iter()
            .any(|issue| issue.code == "external_root_invalid"));
    }

    #[test]
    fn exact_package_versions_names_and_entries_are_required() {
        let fixture = Fixture::new(false);
        let root = fixture.config.external_root.as_ref().unwrap();
        let package = root.join("node_modules/ws/package.json");
        for (value, code) in [
            (
                json!({"name":"ws","version":"8.21.0"}),
                "upstream_version_mismatch",
            ),
            (
                json!({"name":"other","version":"8.22.0"}),
                "external_root_invalid",
            ),
        ] {
            put(&package, &serde_json::to_vec(&value).unwrap());
            let status = probe(&fixture.contract, &fixture.config, &FakeExec::default()).unwrap();
            assert!(!status.ready());
            assert!(status
                .issues
                .iter()
                .any(|i| i.code == code && i.path.as_ref() == Some(&package)));
        }
        put(&package, br#"{"name":"ws","version":"8.22.0"}"#);
        fs::remove_file(
            root.join(
                fixture
                    .contract
                    .external_root
                    .catalog_entry
                    .as_ref()
                    .unwrap(),
            ),
        )
        .unwrap();
        assert!(
            probe(&fixture.contract, &fixture.config, &FakeExec::default())
                .unwrap()
                .issues
                .iter()
                .any(|i| i.code == "external_root_invalid")
        );
    }

    #[test]
    fn node_version_bounds_and_sqlite_feature_are_enforced() {
        let fixture = Fixture::new(false);
        for (version, ready) in [
            ("v24.8.9", false),
            ("v24.9.0", true),
            ("v24.21.0", true),
            ("v24.99.0", true),
            ("v25.0.0", false),
            ("v24.21.0-rc.1", false),
            ("24.21.0", false),
        ] {
            let exec = FakeExec {
                version: version.into(),
                ..Default::default()
            };
            let status = probe(&fixture.contract, &fixture.config, &exec).unwrap();
            assert_eq!(status.ready(), ready, "{version}");
            if !ready {
                assert_eq!(exec.calls.borrow().len(), 1);
            }
        }
        let exec = FakeExec {
            fail: Some("sqlite"),
            ..Default::default()
        };
        let status = probe(&fixture.contract, &fixture.config, &exec).unwrap();
        assert!(status
            .issues
            .iter()
            .any(|i| i.code == "node_feature_missing"));
    }

    #[test]
    fn kimi_pty_and_rg_are_required_but_dsh_does_not_probe_them() {
        for (failure, code) in [("pty", "kimi_pty_unavailable"), ("rg", "kimi_rg_missing")] {
            let fixture = Fixture::new(true);
            let exec = FakeExec {
                fail: Some(failure),
                ..Default::default()
            };
            let status = probe(&fixture.contract, &fixture.config, &exec).unwrap();
            assert!(!status.ready());
            assert!(status.issues.iter().any(|i| i.code == code));
            let fixture = Fixture::new(false);
            assert!(probe(&fixture.contract, &fixture.config, &exec)
                .unwrap()
                .ready());
        }
    }

    #[test]
    fn renamed_ripgrep_is_not_ready_even_when_it_reports_ripgrep() {
        struct RenamedRgExec {
            path: PathBuf,
            calls: RefCell<usize>,
            other: FakeExec,
        }
        impl Exec for RenamedRgExec {
            fn run(&self, program: &str, args: &[String]) -> Result<String> {
                if Path::new(program) == self.path {
                    *self.calls.borrow_mut() += 1;
                    assert_eq!(args, ["--version"]);
                    Ok("ripgrep 15.0.0\n".into())
                } else {
                    self.other.run(program, args)
                }
            }
        }
        let fixture = Fixture::new(true);
        let renamed = fake_binary(&fixture._dir.path().join("custom-bin"), "rg-custom");
        let exec = RenamedRgExec {
            path: renamed.clone(),
            calls: RefCell::new(0),
            other: FakeExec::default(),
        };
        assert!(exec
            .run(renamed.to_str().unwrap(), &["--version".into()])
            .unwrap()
            .starts_with("ripgrep "));
        *exec.calls.borrow_mut() = 0;
        let mut config = fixture.config.clone();
        config.rg = Some(renamed.clone());
        let status = probe(&fixture.contract, &config, &exec).unwrap();
        assert!(!status.ready());
        assert_eq!(
            *exec.calls.borrow(),
            0,
            "the wrong basename must not be executed"
        );
        assert_eq!(status.issues.len(), 1);
        let issue = &status.issues[0];
        assert_eq!(issue.code, "kimi_rg_missing");
        assert_eq!(issue.path.as_ref(), Some(&renamed));
        assert!(issue
            .actual
            .as_ref()
            .unwrap()
            .contains("different filename"));
        assert!(issue.steps[0].contains("Rename"));
    }

    #[test]
    fn rg_defaults_preserve_lexical_path_for_explicit_env_and_path_lookup() {
        let fixture = Fixture::new(true);
        let rg = fixture.config.rg.as_ref().unwrap();
        let lookup_dir = rg.parent().unwrap().join(".");
        let lexical = lookup_dir.join(rg.file_name().unwrap());
        let mut config = fixture.config.clone();
        config.rg = Some(lexical.clone());
        let explicit = resolve_defaults(&config, &fixture.contract, &BTreeMap::new()).unwrap();
        assert_eq!(explicit.rg.unwrap().as_os_str(), lexical.as_os_str());
        config.rg = None;
        let env = BTreeMap::from([(
            OsString::from("CUTEX_KIMI_RG"),
            lexical.as_os_str().to_owned(),
        )]);
        let from_env = resolve_defaults(&config, &fixture.contract, &env).unwrap();
        assert_eq!(from_env.rg.unwrap().as_os_str(), lexical.as_os_str());
        let env = BTreeMap::from([(
            OsString::from("PATH"),
            std::env::join_paths([&lookup_dir]).unwrap(),
        )]);
        let from_path = resolve_defaults(&config, &fixture.contract, &env).unwrap();
        assert_eq!(from_path.rg.unwrap().as_os_str(), lexical.as_os_str());
    }

    #[cfg(unix)]
    #[test]
    fn standard_rg_symlink_to_differently_named_target_remains_usable() {
        let fixture = Fixture::new(true);
        let target = fake_binary(&fixture._dir.path().join("libexec"), "ripgrep-15");
        let link = fixture._dir.path().join("search-bin/rg");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(fs::canonicalize(&link).unwrap(), target);
        let mut config = fixture.config.clone();
        config.rg = Some(link.clone());
        let resolved = resolve_defaults(&config, &fixture.contract, &BTreeMap::new()).unwrap();
        assert_eq!(resolved.rg.as_ref(), Some(&link));
        assert!(probe(&fixture.contract, &resolved, &FakeExec::default())
            .unwrap()
            .ready());
        config.rg = None;
        for env in [
            BTreeMap::from([(OsString::from("CUTEX_KIMI_RG"), link.as_os_str().to_owned())]),
            BTreeMap::from([(
                OsString::from("PATH"),
                std::env::join_paths([link.parent().unwrap()]).unwrap(),
            )]),
        ] {
            let resolved = resolve_defaults(&config, &fixture.contract, &env).unwrap();
            assert_eq!(resolved.rg.as_ref(), Some(&link));
            let exec = FakeExec::default();
            assert!(probe(&fixture.contract, &resolved, &exec).unwrap().ready());
            assert!(exec
                .calls
                .borrow()
                .iter()
                .any(|(program, _)| Path::new(program) == link));
        }
    }

    #[test]
    fn configure_durably_invalidates_before_probe_and_cached_get_has_no_exec() {
        let fixture = Fixture::new(true);
        let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
        let exec = FakeExec {
            invalidation_path: Some(path.clone()),
            ..Default::default()
        };
        let status = configure(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            fixture.request(),
            &exec,
        )
        .unwrap();
        assert!(status.ready());
        let count = exec.calls.borrow().len();
        assert_eq!(
            cached(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap(),
            status
        );
        assert_eq!(
            resolve(&fixture.layout, &fixture.contract.name, &fixture.contract)
                .unwrap()
                .0,
            fixture.config
        );
        assert_eq!(exec.calls.borrow().len(), count);
        let bytes = fs::read(&path).unwrap();
        let mut next = fixture.contract.clone();
        next.version = "kimi-r4".into();
        let next_status = cached(&fixture.layout, &next.name, &next).unwrap();
        assert_eq!(next_status.version, "kimi-r4");
        assert!(next_status.checked_at.is_none() && !next_status.ready());
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "GET must not rewrite state"
        );
        let mut new_contract = fixture.contract.clone();
        new_contract
            .external_root
            .packages
            .insert("ws".into(), "8.23.0".into());
        assert!(cached(&fixture.layout, &new_contract.name, &new_contract)
            .unwrap()
            .checked_at
            .is_none());
        check(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            &exec,
        )
        .unwrap();
    }

    #[test]
    fn env_and_path_resolved_changes_invalidate_cached_status() {
        let fixture = Fixture::new(false);
        let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
        let status = probe(&fixture.contract, &fixture.config, &FakeExec::default()).unwrap();
        let state = State {
            schema: STATE_SCHEMA.into(),
            name: fixture.contract.name.clone(),
            config: ExternalConfig {
                external_root: fixture.config.external_root.clone(),
                node: None,
                rg: fixture.config.rg.clone(),
            },
            contract: fixture.contract.clone(),
            resolved: fixture.config.clone(),
            status,
        };
        write_private_pretty_json_atomic(&path, &state, "test").unwrap();
        let mut env = BTreeMap::from([(
            OsString::from("PATH"),
            std::env::join_paths([fixture.config.node.as_ref().unwrap().parent().unwrap()])
                .unwrap(),
        )]);
        assert!(resolve_with_env(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            &env
        )
        .unwrap()
        .1
        .ready());
        let other_node = fake_binary(&fixture._dir.path().join("other-bin"), "node");
        env.insert(
            "PATH".into(),
            std::env::join_paths([other_node.parent().unwrap()]).unwrap(),
        );
        let (config, status) = resolve_with_env(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            &env,
        )
        .unwrap();
        assert_eq!(config.node.as_ref(), Some(&other_node));
        assert!(!status.ready() && status.checked_at.is_none());
        env.insert(
            "CUTEX_DSH_NODE".into(),
            fixture.config.node.as_ref().unwrap().as_os_str().into(),
        );
        assert!(resolve_with_env(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            &env
        )
        .unwrap()
        .1
        .ready());
    }

    #[test]
    fn explicit_missing_files_produce_persisted_issues_not_configuration_rejection() {
        let fixture = Fixture::new(true);
        let missing_node = fixture._dir.path().join("absent-node.exe");
        let missing_rg = fixture._dir.path().join("absent-rg.exe");
        let request = serde_json::from_value(json!({"externalRoot":fixture.config.external_root,"node":missing_node,"rg":missing_rg})).unwrap();
        let exec = FakeExec::default();
        let status = configure(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            request,
            &exec,
        )
        .unwrap();
        assert!(status.checked_at.is_some() && !status.ready());
        assert!(status.issues.iter().any(|i| i.code == "node_missing"));
        assert!(status.issues.iter().any(|i| i.code == "kimi_rg_missing"));
        assert_eq!(
            cached(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap(),
            status
        );
        assert!(exec.calls.borrow().is_empty());
    }

    #[test]
    fn pinned_probe_does_not_fall_back_and_corrupt_state_is_not_reset() {
        let fixture = Fixture::new(false);
        let exec = FakeExec::default();
        let status = probe(&fixture.contract, &ExternalConfig::default(), &exec).unwrap();
        assert!(status
            .issues
            .iter()
            .any(|i| i.code == "external_root_missing"));
        assert!(status.issues.iter().any(|i| i.code == "node_missing"));
        assert!(exec.calls.borrow().is_empty());
        let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
        put(&path, b"{broken");
        assert!(cached(&fixture.layout, &fixture.contract.name, &fixture.contract).is_err());
        assert!(check(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            &exec
        )
        .is_err());
        assert_eq!(fs::read(path).unwrap(), b"{broken");
    }

    #[test]
    fn cache_binding_tampering_is_corruption_and_unchecked_is_not_ready() {
        let fixture = Fixture::new(false);
        assert!(!ExternalStatus::unchecked(&fixture.contract, &fixture.config).ready());
        configure(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            fixture.request(),
            &FakeExec::default(),
        )
        .unwrap();
        let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
        let mut state = read_state(&fixture.layout, &fixture.contract.name)
            .unwrap()
            .unwrap();
        state.status.root = None;
        write_private_pretty_json_atomic(&path, &state, "test").unwrap();
        assert!(cached(&fixture.layout, &fixture.contract.name, &fixture.contract).is_err());
    }
}
