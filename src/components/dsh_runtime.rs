//! Offline, optional DSH runtime. Selection changes only the default for NEW
//! sessions; session-owned homes and pinned deployments are never modified.
//! Source payload: src/, package{,-lock}.json, runtime/package{,-lock}.json,
//! runtime/node_modules (DSH 0.2.1-rc.3 and ws 8.22.0). No npm, downloads,
//! credentials, login-home discovery or provider request occurs here.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context as _};
use serde_json::{json, Value};

use super::{states, Context, Entry, Handler, Metadata};
use crate::runtime::dsh::DshDeployment;
use crate::upgrade::layout::Layout;
use crate::upgrade::service::{Exec, RealExec};

pub const NAME: &str = "dsh-runtime";
pub const DSH_VERSION: &str = "0.2.1-rc.3";
pub const NODE_MINIMUM: &str = "24.9.0";
pub const NODE_ENV: &str = "CUTEX_DSH_NODE";

/// Presence, not health: capability polling must never execute Node or hash
/// the npm tree. Registered, damaged installs keep their repair card.
pub fn advertised() -> bool {
    advertised_with(Layout::detect().as_ref())
}

/// An absent layout still permits r260's external, hand-selected deployment.
/// Never consult the platform's default install base for an unmanaged host.
pub fn advertised_with(layout: Option<&Layout>) -> bool {
    if let Some(layout) = layout {
        if let Some(entry) = super::installed(layout, NAME) {
            if !crate::upgrade::layout::valid_label(&entry.selected) {
                return false;
            }
            let dir = super::version_dir(layout, NAME, &entry.selected);
            // A small identity document is enough to identify an installed
            // version. Missing Node, adapter or npm internals are repairable.
            return dir.is_absolute()
                && dir.is_dir()
                && std::fs::read(dir.join("component.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Metadata>(&bytes).ok())
                    .is_some_and(|meta| registered_identity(&meta, &entry));
        }
    }
    let Some(deployment) = selected_raw().and_then(|value| serde_json::from_value::<DshDeployment>(value).ok()) else {
        return false;
    };
    // r260's hand-selected deployments may also live under components/ without
    // an Entry. Normal remove deselects them; retained files alone stay hidden.
    manual_presence(&deployment)
}

fn registered_identity(meta: &Metadata, entry: &Entry) -> bool {
    if meta.schema != super::COMPONENT_SCHEMA || meta.name != NAME || !crate::upgrade::layout::valid_label(&meta.version) {
        return false;
    }
    if meta.version == entry.selected {
        return true;
    }
    // Source staging preserves component.json's base version and names the
    // immutable directory <base>-source-<12 lowercase hex digest characters>.
    entry.origin == super::ORIGIN_SOURCE
        && entry.selected.strip_prefix(&format!("{}-source-", meta.version)).is_some_and(|digest| {
            digest.len() == 12 && digest.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn manual_presence(deployment: &DshDeployment) -> bool {
    deployment.version == DSH_VERSION
        && deployment.node.is_absolute()
        && deployment.adapter.is_absolute()
        && deployment.adapter.is_file()
        && deployment.dsh_cli.is_absolute()
        && deployment.dsh_cli.is_file()
        && deployment.install_root.is_absolute()
        && deployment.install_root.is_dir()
        && same_path(&deployment.dsh_cli, &deployment.install_root.join("node_modules/@deepseek-ai/dsh/lib/bin.js"))
        && package(&deployment.install_root, "node_modules/@deepseek-ai/dsh/package.json", "@deepseek-ai/dsh", DSH_VERSION).is_ok()
}

pub fn requirements() -> Value {
    json!({"version": DSH_VERSION, "node": {"minimum": NODE_MINIMUM, "version": null, "ready": false}})
}

/// Unmanaged/manual deployments have no component Entry/probe. Their card
/// must still report the selected Node rather than showing a false failure.
pub fn status_requirements() -> Value { status_requirements_with(&CachedNodeExec) }
fn status_requirements_with(exec: &dyn Exec) -> Value {
    let node = selected_raw().and_then(|value| value["node"].as_str().map(PathBuf::from));
    match node {
        Some(node) => json!({"version": DSH_VERSION, "node": node_detail(Some(&node), exec)}),
        None => requirements(),
    }
}

pub fn node_version_supported(text: &str) -> bool {
    node_version_ready(text)
}

pub fn node_version(path: &Path) -> anyhow::Result<String> {
    ensure!(path.is_absolute() && path.is_file(), "absolute Node executable required");
    let version = CachedNodeExec.run(path.to_str().context("Node path must be UTF-8")?, &["--version".into()])?;
    Ok(version.trim().to_string())
}
pub const REQUIRED_FILES: &[&str] = &[
    "component.json",
    "package.json",
    "package-lock.json",
    "src/server.mjs",
    "src/history-reader.mjs",
    "src/runtime.mjs",
    "src/rpc.mjs",
    "src/store.mjs",
    "src/reconcile.mjs",
    "src/nativeProjection.mjs",
    "runtime/package.json",
    "runtime/package-lock.json",
    "runtime/node_modules/@deepseek-ai/dsh/package.json",
    "runtime/node_modules/@deepseek-ai/dsh/lib/bin.js",
    "runtime/node_modules/@deepseek-ai/dsh-session-format-catalog/package.json",
    "runtime/node_modules/@deepseek-ai/dsh-session-format-catalog/lib/index.js",
    "runtime/node_modules/ws/package.json",
    "runtime/node_modules/ws/index.js",
    "node_modules/ws/package.json",
    "node_modules/ws/index.js",
];

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// An explicit absolute binary; never a shell command or npm shim.
    pub node: Option<PathBuf>,
}
impl Options {
    pub fn from_env() -> Self {
        Self {
            node: std::env::var_os(NODE_ENV).filter(|s| !s.is_empty()).map(PathBuf::from),
        }
    }
}

pub struct DshRuntime {
    metadata: Metadata,
    pub options: Option<Options>,
}
impl Default for DshRuntime {
    fn default() -> Self {
        Self {
            metadata: Metadata::parse(include_str!("../../components/dsh-runtime/component.json")),
            options: None,
        }
    }
}
impl DshRuntime {
    fn options(&self) -> Options {
        self.options.clone().unwrap_or_else(Options::from_env)
    }
}

/// Strict release semver, compared numerically. Prereleases are deliberately
/// refused: 24.9.0-rc.1 is below the required stable 24.9.0.
pub fn node_version_ready(text: &str) -> bool {
    let text = text.trim().strip_prefix('v').unwrap_or(text.trim());
    let core = if let Some((core, build)) = text.split_once('+') {
        if !build
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
        {
            return false;
        }
        core
    } else {
        text
    };
    let parts: Vec<_> = core.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    let mut v = Vec::new();
    for part in parts {
        if part.is_empty() || (part.len() > 1 && part.starts_with('0')) || !part.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        let Ok(n) = part.parse::<u64>() else {
            return false;
        };
        v.push(n);
    }
    (v[0], v[1], v[2]) >= (24, 9, 0)
}

fn package(dir: &Path, relative: &str, name: &str, version: &str) -> anyhow::Result<()> {
    let value: Value = serde_json::from_slice(&std::fs::read(dir.join(relative))?)?;
    ensure!(
        value["name"] == name && value["version"] == version,
        "{relative} must be {name}@{version}"
    );
    Ok(())
}

/// A source tree must carry this binary's component version label (r267:
/// `dsh-r2` / `kimi-r2`). Staging an older tree would name the directory
/// `<this label>-source-<digest>` around foreign metadata: the install would
/// "succeed" yet never be advertised, and pins would mix adapter generations.
pub(crate) fn same_component_version(adapter: &Path, expected: &Metadata) -> anyhow::Result<()> {
    let found: Metadata = serde_json::from_slice(&std::fs::read(adapter.join("component.json")).context("read the source component.json")?)
        .context("parse the source component.json")?;
    ensure!(
        found.name == expected.name && found.version == expected.version,
        "the source tree is {} {}, this cutex installs {} {}; use the source of the matching release",
        found.name, found.version, expected.name, expected.version
    );
    Ok(())
}

pub fn check_layout(dir: &Path) -> anyhow::Result<()> {
    ensure!(dir.is_absolute(), "DSH component directory must be absolute");
    for file in REQUIRED_FILES {
        ensure!(dir.join(file).is_file(), "DSH component lacks {file}");
    }
    let meta: Metadata = serde_json::from_slice(&std::fs::read(dir.join("component.json"))?)?;
    ensure!(
        meta.name == NAME && meta.schema == super::COMPONENT_SCHEMA,
        "invalid DSH component identity"
    );
    package(
        dir,
        "runtime/node_modules/@deepseek-ai/dsh/package.json",
        "@deepseek-ai/dsh",
        DSH_VERSION,
    )?;
    package(dir, "runtime/node_modules/ws/package.json", "ws", "8.22.0")?;
    package(dir, "node_modules/ws/package.json", "ws", "8.22.0")?;
    package(
        dir,
        "runtime/node_modules/@deepseek-ai/dsh-session-format-catalog/package.json",
        "@deepseek-ai/dsh-session-format-catalog",
        DSH_VERSION,
    )?;
    Ok(())
}

fn node_path(dir: &Path, options: &Options) -> Option<PathBuf> {
    if let Some(path) = &options.node {
        return Some(path.clone());
    }
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    let bundled = dir.join("runtime").join(exe);
    if bundled.is_file() {
        return Some(bundled);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .filter(|p| p.is_absolute())
            .map(|p| p.join(exe))
            .find(|p| p.is_file())
    })
}

fn node_detail(path: Option<&Path>, exec: &dyn Exec) -> Value {
    let version = path
        .filter(|p| p.is_absolute() && p.is_file())
        .and_then(|p| p.to_str())
        .and_then(|p| exec.run(p, &["--version".into()]).ok())
        .map(|v| v.trim().to_string());
    json!({"minimum": NODE_MINIMUM, "version": version,
        "ready": version.as_deref().is_some_and(node_version_ready), "path": path})
}

fn user_step() -> Value {
    json!({"code": "dsh_node_required", "message": "DSH requires Node.js 24.9.0 or newer",
        "steps": ["Install Node.js 24.9.0 or newer, or include runtime/node (Windows: node.exe) in the offline kit.",
            "Set CUTEX_DSH_NODE to an absolute Node executable if it is not on the daemon PATH, then install the DSH component again."]})
}

fn selected_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("dsh/deployment.json"))
}
fn selected_raw() -> Option<Value> {
    serde_json::from_slice(&std::fs::read(selected_path().ok()?).ok()?).ok()
}
fn same_path(a: &Path, b: &Path) -> bool {
    a.canonicalize().unwrap_or_else(|_| a.to_path_buf()) == b.canonicalize().unwrap_or_else(|_| b.to_path_buf())
}
fn selected_is(dir: &Path) -> bool {
    selected_raw()
        .and_then(|v| v["install_root"].as_str().map(PathBuf::from))
        .is_some_and(|root| same_path(&root, &dir.join("runtime")))
}

fn verify_payload(dir: &Path) -> anyhow::Result<()> {
    check_layout(dir)?;
    let receipt = super::read_receipt(dir).context("DSH component has no receipt")?;
    for file in REQUIRED_FILES {
        ensure!(receipt.files.contains_key(*file), "DSH receipt lacks {file}");
    }
    // A full npm tree has tens of thousands of files. Read the receipt ONCE;
    // verify_file reloads and parses it per call (quadratic for this payload).
    for (file, expected) in &receipt.files {
        let actual = crate::upgrade::manifest::file_sha256_hex(&dir.join(file))?;
        ensure!(&actual == expected, "DSH payload {file} does not match its receipt");
    }
    Ok(())
}

impl Handler for DshRuntime {
    fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    fn required_files(&self) -> &'static [&'static str] {
        REQUIRED_FILES
    }
    /// Explicit input only. A checkout's .dsh-install is staging, never the
    /// user's npm prefix. A prebuilt component payload is accepted unchanged.
    fn from_source(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        ensure!(source.is_absolute(), "source tree must be absolute");
        let adapter = if source.join("components").join(NAME).join("component.json").is_file() {
            source.join("components").join(NAME)
        } else {
            source.to_path_buf()
        };
        same_component_version(&adapter, &self.metadata)?;
        let runtime = if adapter.join("runtime").is_dir() {
            adapter.join("runtime")
        } else {
            source.join(".dsh-install")
        };
        ensure!(
            runtime.join("node_modules").is_dir(),
            "explicit source needs runtime/node_modules or checkout .dsh-install/node_modules; no packages are downloaded"
        );
        ensure!(
            !dest.exists() || (dest.is_dir() && std::fs::read_dir(dest)?.next().is_none()),
            "source output must be empty"
        );
        ensure!(
            !dest.starts_with(source) && !source.starts_with(dest),
            "source output must be separate from input"
        );
        std::fs::create_dir_all(dest)?;
        for file in ["component.json", "package.json", "package-lock.json"] {
            copy_file(&adapter.join(file), &dest.join(file))?;
        }
        copy_tree(&adapter.join("src"), &dest.join("src"))?;
        std::fs::create_dir_all(dest.join("runtime"))?;
        for file in ["package.json", "package-lock.json"] {
            copy_file(&runtime.join(file), &dest.join("runtime").join(file))?;
        }
        copy_tree(&runtime.join("node_modules"), &dest.join("runtime/node_modules"))?;
        let node = if cfg!(windows) { "node.exe" } else { "node" };
        if runtime.join(node).is_file() {
            copy_file(&runtime.join(node), &dest.join("runtime").join(node))?;
        }
        // Resolve the adapter's ws import without depending on fallback rules.
        copy_tree(&runtime.join("node_modules/ws"), &dest.join("node_modules/ws"))?;
        check_layout(dest)
    }
    fn external(&self, _layout: &Layout) -> bool {
        false
    }
    fn configured_by_hand(&self) -> bool {
        DshDeployment::selected().and_then(|d| d.validate()).is_ok()
    }
    fn prepare(&self, ctx: &Context) -> anyhow::Result<Value> {
        if ctx.dry_run {
            return Ok(json!({"dryRun": true, "unpacked": check_layout(&ctx.dir).is_ok()}));
        }
        verify_payload(&ctx.dir)?;
        let path = node_path(&ctx.dir, &self.options());
        let node = node_detail(path.as_deref(), ctx.exec);
        if node["ready"] != true {
            return Ok(json!({"prepared": false, "needsUser": user_step(), "dsh": {"version": DSH_VERSION, "node": node}}));
        }
        Ok(json!({"prepared": true, "dsh": {"version": DSH_VERSION, "node": node}}))
    }
    fn select(&self, ctx: &Context) -> anyhow::Result<Value> {
        if ctx.dry_run {
            return Ok(json!({"selected": false, "dryRun": true}));
        }
        verify_payload(&ctx.dir)?;
        let path = node_path(&ctx.dir, &self.options());
        let node = node_detail(path.as_deref(), ctx.exec);
        if node["ready"] != true {
            return Ok(json!({"selected": false, "needsUser": user_step(), "dsh": {"version": DSH_VERSION, "node": node}}));
        }
        let deployment = DshDeployment {
            node: path.context("Node path missing")?,
            adapter: ctx.dir.join("src/server.mjs"),
            dsh_cli: ctx.dir.join("runtime/node_modules/@deepseek-ai/dsh/lib/bin.js"),
            install_root: ctx.dir.join("runtime"),
            version: DSH_VERSION.to_string(),
        };
        deployment.validate()?;
        deployment.install()?;
        Ok(json!({"selected": true, "deployment": deployment, "dsh": {"version": DSH_VERSION, "node": node}}))
    }
    fn deselect(&self, ctx: &Context, _force: bool) -> anyhow::Result<Vec<String>> {
        if !selected_is(&ctx.dir) {
            return Ok(vec![]);
        }
        if !ctx.dry_run {
            std::fs::remove_file(selected_path()?)?;
        }
        Ok(vec![
            "unselect DSH for new sessions; existing sessions keep their deployment and data".into(),
        ])
    }
    fn probe(&self, layout: &Layout, entry: &Entry) -> Value {
        probe_dir(&super::version_dir(layout, NAME, &entry.selected), &self.options(), &CachedNodeExec)
    }
    fn pins(&self, layout: &Layout) -> BTreeMap<String, usize> {
        self.pins_checked(layout).unwrap_or_default()
    }
    fn pins_checked(&self, layout: &Layout) -> anyhow::Result<BTreeMap<String, usize>> {
        let store = crate::session::store::load_cutex_session_store_shared()
            .context("cannot read durable DSH session pins; refusing to prune")?;
        let root = super::components_dir(layout).join(NAME);
        // Archived sessions can be restored with their original deployment.
        // Collect every durable binding, without a health or archive filter.
        let sessions = store.sessions.values().filter_map(|s| s.dsh_session.as_ref());
        let mut saved = Vec::new();
        for session in sessions {
            saved.push(serde_json::to_value(session).context("cannot decode durable DSH session pin")?);
        }
        Ok(pins_for(&root, saved))
    }
    fn restart_gate(&self, _layout: &Layout) -> Result<(), String> {
        Ok(())
    }
}

pub fn probe_dir(dir: &Path, options: &Options, exec: &dyn Exec) -> Value {
    if let Err(error) = check_layout(dir) {
        return json!({"dir": dir, "dsh": requirements(), "state": states::DEGRADED,
            "problem": format!("{error:#}; install the component again")});
    }
    // A selected system Node stays pinned even if PATH changes later.
    let selected_node = if selected_is(dir) {
        selected_raw().and_then(|v| v["node"].as_str().map(PathBuf::from))
    } else {
        None
    };
    let path = selected_node.or_else(|| node_path(dir, options));
    let node = node_detail(path.as_deref(), exec);
    let mut value = json!({"dir": dir, "dsh": {"version": DSH_VERSION, "node": node}});
    if node["ready"] != true {
        value["state"] = json!(states::NEEDS_USER);
        value["needsUser"] = user_step();
        value["steps"] = user_step()["steps"].clone();
    } else if !selected_is(dir) {
        value["state"] = json!(states::DEGRADED);
        value["problem"] = json!("not selected for new DSH sessions; install again");
    } else if let Err(error) = DshDeployment::selected() {
        value["state"] = json!(states::DEGRADED);
        value["problem"] = json!(format!("{error:#}"));
    } else {
        value["state"] = json!(states::READY);
    }
    value
}

/// Accepts both the Phase 1 flattened pin and the product's saved deployment.
pub fn pins_for(root: &Path, sessions: impl IntoIterator<Item = Value>) -> BTreeMap<String, usize> {
    let mut pins = BTreeMap::new();
    for session in sessions {
        let deployment = session.get("deployment").unwrap_or(&session);
        let Some(install) = deployment["install_root"].as_str() else {
            continue;
        };
        let install = PathBuf::from(install);
        let Some(version) = install.parent() else { continue };
        if version.parent().is_some_and(|p| same_path(p, root)) && install.file_name().is_some_and(|s| s == "runtime") {
            if let Some(label) = version.file_name().and_then(|n| n.to_str()) {
                *pins.entry(label.into()).or_insert(0) += 1;
            }
        }
    }
    pins
}

fn copy_tree(source: &Path, dest: &Path) -> anyhow::Result<()> {
    ensure!(std::fs::symlink_metadata(source)?.is_dir(), "source directory required (no links)");
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == ".bin" {
            continue;
        }
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "source payload must not contain links: {}",
            entry.path().display()
        );
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        } else {
            ensure!(kind.is_file(), "source payload must contain only ordinary files");
            std::fs::copy(entry.path(), dest.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, dest: &Path) -> anyhow::Result<()> {
    ensure!(
        std::fs::symlink_metadata(source)?.is_file(),
        "ordinary source file required (no links): {}",
        source.display()
    );
    std::fs::copy(source, dest)?;
    Ok(())
}

/// Health lists are polled frequently. Cache only the harmless version command,
/// for at most 30s and only while the pinned binary's size/mtime stay unchanged.
/// Component prepare/select deliberately bypass this cache through Context.exec.
/// Runtime validation uses node_version so selected() cannot duplicate a probe.
struct CachedNodeExec;
struct CachedVersion {
    at: std::time::Instant,
    modified: Option<std::time::SystemTime>,
    len: u64,
    result: Result<String, String>,
}
impl Exec for CachedNodeExec {
    fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        ensure!(args == ["--version"], "only the Node version probe may be cached");
        static CACHE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, CachedVersion>>> = std::sync::OnceLock::new();
        let metadata = std::fs::metadata(program)?;
        let modified = metadata.modified().ok();
        let path = PathBuf::from(program);
        let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache
            .get(&path)
            .filter(|v| v.at.elapsed().as_secs() < 30 && v.modified == modified && v.len == metadata.len())
        {
            return hit.result.clone().map_err(anyhow::Error::msg);
        }
        let result = RealExec.run(program, args).map_err(|error| format!("{error:#}"));
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(
            path,
            CachedVersion {
                at: std::time::Instant::now(),
                modified,
                len: metadata.len(),
                result: result.clone(),
            },
        );
        result.map_err(anyhow::Error::msg)
    }
}

#[cfg(test)]
#[path = "dsh_runtime_tests.rs"]
mod tests;
