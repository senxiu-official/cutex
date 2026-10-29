//! Offline, optional Kimi Code runtime (r266). Selection changes only the
//! default for NEW Kimi agents; agent homes and pinned deployments are never
//! modified. Payload: src/, package{,-lock}.json, runtime/package{,-lock}.json,
//! runtime/node_modules (@moonshot-ai/kimi-code 2.3.3 with its dependencies,
//! ws 8.22.0), node_modules/ws. No npm, download, login or provider request
//! occurs here.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context as _};
use serde_json::{json, Value};

use super::{states, Context, Entry, Handler, Metadata};
use crate::runtime::kimi::{KimiDeployment, PINNED_KIMI_VERSION};
use crate::upgrade::layout::Layout;
use crate::upgrade::service::{Exec, RealExec};

pub const NAME: &str = "kimi-runtime";
pub const NODE_MINIMUM: &str = "22.19.0";
pub const NODE_ENV: &str = "CUTEX_KIMI_NODE";
pub const KIMI_MAIN: &str = "runtime/node_modules/@moonshot-ai/kimi-code/dist/main.mjs";

pub const REQUIRED_FILES: &[&str] = &[
    "component.json",
    "package.json",
    "package-lock.json",
    "src/server.mjs",
    "src/runtime.mjs",
    "src/rpc.mjs",
    "src/store.mjs",
    "src/projection.mjs",
    "src/toolItems.mjs",
    "src/wire.mjs",
    "src/delivery.mjs",
    "src/history-reader.mjs",
    "src/session-ops.mjs",
    "src/redaction.mjs",
    "runtime/package.json",
    "runtime/package-lock.json",
    "runtime/node_modules/@moonshot-ai/kimi-code/package.json",
    KIMI_MAIN,
    "runtime/node_modules/ws/package.json",
    "node_modules/ws/package.json",
    "node_modules/ws/index.js",
];

/// Presence, not health (capability polling never runs Node).
pub fn advertised() -> bool {
    advertised_with(Layout::detect().as_ref())
}

pub fn advertised_with(layout: Option<&Layout>) -> bool {
    if let Some(layout) = layout {
        if let Some(entry) = super::installed(layout, NAME) {
            if !crate::upgrade::layout::valid_label(&entry.selected) {
                return false;
            }
            let dir = super::version_dir(layout, NAME, &entry.selected);
            return dir.is_absolute()
                && dir.is_dir()
                && std::fs::read(dir.join("component.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Metadata>(&bytes).ok())
                    .is_some_and(|meta| meta.schema == super::COMPONENT_SCHEMA && meta.name == NAME);
        }
    }
    // A hand-selected deployment (unmanaged hosts, private tests).
    selected_raw().and_then(|value| serde_json::from_value::<KimiDeployment>(value).ok()).is_some_and(|deployment| {
        deployment.version == PINNED_KIMI_VERSION && deployment.adapter.is_file() && deployment.kimi_cli.is_file() && deployment.install_root.is_dir()
    })
}

/// K2's hooks (`profiles::kimi`): the component status of the Kimi runtime
/// on this host, and where the pinned Kimi CLI is (Kimi account login).
pub fn register_profile_hooks() {
    crate::profiles::kimi::set_component_probe(component_probe);
    crate::profiles::kimi::set_cli_locator(cli_locator);
}

fn component_probe() -> (crate::profiles::runtime_variants::Status, Option<&'static str>) {
    use crate::profiles::runtime_variants::Status;
    if !advertised() {
        return (Status::ComponentNotInstalled, Some("componentNotInstalled"));
    }
    match KimiDeployment::selected() {
        Ok(_) => (Status::Supported, None),
        Err(_) => (Status::ComponentNotInstalled, Some("componentNeedsRepair")),
    }
}

fn cli_locator() -> anyhow::Result<crate::profiles::kimi::KimiCli> {
    let deployment = KimiDeployment::selected()?;
    Ok(crate::profiles::kimi::KimiCli {
        program: deployment.node,
        args: vec![deployment.kimi_cli.to_string_lossy().into_owned()],
        env: crate::runtime::kimi::KIMI_SWITCHES.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect(),
    })
}

pub fn requirements() -> Value {
    json!({"version": PINNED_KIMI_VERSION, "node": {"minimum": NODE_MINIMUM, "version": null, "ready": false}})
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub node: Option<PathBuf>,
}
impl Options {
    pub fn from_env() -> Self {
        Self { node: std::env::var_os(NODE_ENV).filter(|s| !s.is_empty()).map(PathBuf::from) }
    }
}

pub struct KimiRuntime {
    metadata: Metadata,
    pub options: Option<Options>,
}
impl Default for KimiRuntime {
    fn default() -> Self {
        Self { metadata: Metadata::parse(include_str!("../../components/kimi-runtime/component.json")), options: None }
    }
}
impl KimiRuntime {
    fn options(&self) -> Options {
        self.options.clone().unwrap_or_else(Options::from_env)
    }
}

fn package(dir: &Path, relative: &str, name: &str, version: &str) -> anyhow::Result<()> {
    let value: Value = serde_json::from_slice(&std::fs::read(dir.join(relative))?)?;
    ensure!(value["name"] == name && value["version"] == version, "{relative} must be {name}@{version}");
    Ok(())
}

pub fn check_layout(dir: &Path) -> anyhow::Result<()> {
    ensure!(dir.is_absolute(), "Kimi component directory must be absolute");
    for file in REQUIRED_FILES {
        ensure!(dir.join(file).is_file(), "Kimi component lacks {file}");
    }
    let meta: Metadata = serde_json::from_slice(&std::fs::read(dir.join("component.json"))?)?;
    ensure!(meta.name == NAME && meta.schema == super::COMPONENT_SCHEMA, "invalid Kimi component identity");
    package(dir, "runtime/node_modules/@moonshot-ai/kimi-code/package.json", crate::runtime::kimi::KIMI_PACKAGE, PINNED_KIMI_VERSION)?;
    package(dir, "runtime/node_modules/ws/package.json", "ws", "8.22.0")?;
    package(dir, "node_modules/ws/package.json", "ws", "8.22.0")?;
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
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).filter(|p| p.is_absolute()).map(|p| p.join(exe)).find(|p| p.is_file()))
}

fn node_detail(path: Option<&Path>, exec: &dyn Exec) -> Value {
    let version = path
        .filter(|p| p.is_absolute() && p.is_file())
        .and_then(|p| p.to_str())
        .and_then(|p| exec.run(p, &["--version".into()]).ok())
        .map(|v| v.trim().to_string());
    json!({"minimum": NODE_MINIMUM, "version": version,
        "ready": version.as_deref().is_some_and(crate::runtime::kimi::node_version_ready), "path": path})
}

fn user_step() -> Value {
    json!({"code": "kimi_node_required", "message": "Kimi Code requires Node.js 22.19.0 or newer",
        "steps": ["Install Node.js 22.19.0 or newer, or include runtime/node (Windows: node.exe) in the offline kit.",
            "Set CUTEX_KIMI_NODE to an absolute Node executable if it is not on the daemon PATH, then install the Kimi component again."]})
}

fn selected_path() -> anyhow::Result<PathBuf> {
    crate::runtime::kimi::deployment_path()
}
fn selected_raw() -> Option<Value> {
    serde_json::from_slice(&std::fs::read(selected_path().ok()?).ok()?).ok()
}
fn same_path(a: &Path, b: &Path) -> bool {
    a.canonicalize().unwrap_or_else(|_| a.to_path_buf()) == b.canonicalize().unwrap_or_else(|_| b.to_path_buf())
}
fn selected_is(dir: &Path) -> bool {
    selected_raw().and_then(|v| v["install_root"].as_str().map(PathBuf::from)).is_some_and(|root| same_path(&root, &dir.join("runtime")))
}

fn verify_payload(dir: &Path) -> anyhow::Result<()> {
    check_layout(dir)?;
    let receipt = super::read_receipt(dir).context("Kimi component has no receipt")?;
    for file in REQUIRED_FILES {
        ensure!(receipt.files.contains_key(*file), "Kimi receipt lacks {file}");
    }
    for (file, expected) in &receipt.files {
        let actual = crate::upgrade::manifest::file_sha256_hex(&dir.join(file))?;
        ensure!(&actual == expected, "Kimi payload {file} does not match its receipt");
    }
    Ok(())
}

fn deployment_for(dir: &Path, node: PathBuf) -> KimiDeployment {
    KimiDeployment {
        node,
        adapter: dir.join("src/server.mjs"),
        kimi_cli: dir.join(KIMI_MAIN),
        install_root: dir.join("runtime"),
        version: PINNED_KIMI_VERSION.to_string(),
    }
}

impl Handler for KimiRuntime {
    fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    fn required_files(&self) -> &'static [&'static str] {
        REQUIRED_FILES
    }
    /// Explicit input only: `<source>/components/kimi-runtime` (or the
    /// component directory itself) with an explicitly staged `runtime/`
    /// (`npm install @moonshot-ai/kimi-code@2.3.3 ws@8.22.0` run elsewhere,
    /// for the target platform). A prebuilt payload is accepted unchanged.
    fn from_source(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        ensure!(source.is_absolute(), "source tree must be absolute");
        let adapter = if source.join("components").join(NAME).join("component.json").is_file() { source.join("components").join(NAME) } else { source.to_path_buf() };
        super::dsh_runtime::same_component_version(&adapter, &self.metadata)?;
        let runtime = if adapter.join("runtime").is_dir() { adapter.join("runtime") } else { source.join(".kimi-install") };
        ensure!(runtime.join("node_modules").is_dir(), "explicit source needs runtime/node_modules or checkout .kimi-install/node_modules; no packages are downloaded");
        ensure!(!dest.exists() || (dest.is_dir() && std::fs::read_dir(dest)?.next().is_none()), "source output must be empty");
        ensure!(!dest.starts_with(source) && !source.starts_with(dest), "source output must be separate from input");
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
        copy_tree(&runtime.join("node_modules/ws"), &dest.join("node_modules/ws"))?;
        check_layout(dest)
    }
    fn external(&self, _layout: &Layout) -> bool {
        false
    }
    fn configured_by_hand(&self) -> bool {
        KimiDeployment::selected().is_ok()
    }
    fn prepare(&self, ctx: &Context) -> anyhow::Result<Value> {
        if ctx.dry_run {
            return Ok(json!({"dryRun": true, "unpacked": check_layout(&ctx.dir).is_ok()}));
        }
        verify_payload(&ctx.dir)?;
        let path = node_path(&ctx.dir, &self.options());
        let node = node_detail(path.as_deref(), ctx.exec);
        if node["ready"] != true {
            return Ok(json!({"prepared": false, "needsUser": user_step(), "kimi": {"version": PINNED_KIMI_VERSION, "node": node}}));
        }
        Ok(json!({"prepared": true, "kimi": {"version": PINNED_KIMI_VERSION, "node": node}}))
    }
    fn select(&self, ctx: &Context) -> anyhow::Result<Value> {
        if ctx.dry_run {
            return Ok(json!({"selected": false, "dryRun": true}));
        }
        verify_payload(&ctx.dir)?;
        let path = node_path(&ctx.dir, &self.options());
        let node = node_detail(path.as_deref(), ctx.exec);
        if node["ready"] != true {
            return Ok(json!({"selected": false, "needsUser": user_step(), "kimi": {"version": PINNED_KIMI_VERSION, "node": node}}));
        }
        let deployment = deployment_for(&ctx.dir, path.context("Node path missing")?);
        deployment.install()?;
        Ok(json!({"selected": true, "deployment": deployment, "kimi": {"version": PINNED_KIMI_VERSION, "node": node}}))
    }
    fn deselect(&self, ctx: &Context, _force: bool) -> anyhow::Result<Vec<String>> {
        if !selected_is(&ctx.dir) {
            return Ok(vec![]);
        }
        if !ctx.dry_run {
            std::fs::remove_file(selected_path()?)?;
        }
        Ok(vec!["unselect Kimi Code for new agents; existing agents keep their deployment and data".into()])
    }
    fn probe(&self, layout: &Layout, entry: &Entry) -> Value {
        probe_dir(&super::version_dir(layout, NAME, &entry.selected), &self.options(), &RealExec)
    }
    fn pins(&self, layout: &Layout) -> BTreeMap<String, usize> {
        self.pins_checked(layout).unwrap_or_default()
    }
    fn pins_checked(&self, layout: &Layout) -> anyhow::Result<BTreeMap<String, usize>> {
        let store = crate::session::store::load_cutex_session_store_shared().context("cannot read durable Kimi agent pins; refusing to prune")?;
        let root = super::components_dir(layout).join(NAME);
        let mut saved = Vec::new();
        for session in store.sessions.values().filter_map(|s| s.dsh_session.as_ref()) {
            saved.push(serde_json::to_value(session).context("cannot decode durable Kimi agent pin")?);
        }
        Ok(super::dsh_runtime::pins_for(&root, saved))
    }
    fn restart_gate(&self, _layout: &Layout) -> Result<(), String> {
        Ok(())
    }
}

pub fn probe_dir(dir: &Path, options: &Options, exec: &dyn Exec) -> Value {
    if let Err(error) = check_layout(dir) {
        return json!({"dir": dir, "kimi": requirements(), "state": states::DEGRADED, "problem": format!("{error:#}; install the component again")});
    }
    let selected_node = if selected_is(dir) { selected_raw().and_then(|v| v["node"].as_str().map(PathBuf::from)) } else { None };
    let path = selected_node.or_else(|| node_path(dir, options));
    let node = node_detail(path.as_deref(), exec);
    let mut value = json!({"dir": dir, "kimi": {"version": PINNED_KIMI_VERSION, "node": node}});
    if node["ready"] != true {
        value["state"] = json!(states::NEEDS_USER);
        value["needsUser"] = user_step();
        value["steps"] = user_step()["steps"].clone();
    } else if !selected_is(dir) {
        value["state"] = json!(states::DEGRADED);
        value["problem"] = json!("not selected for new Kimi agents; install again");
    } else if let Err(error) = KimiDeployment::selected() {
        value["state"] = json!(states::DEGRADED);
        value["problem"] = json!(format!("{error:#}"));
    } else {
        value["state"] = json!(states::READY);
    }
    value
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
        ensure!(!kind.is_symlink(), "source payload must not contain links: {}", entry.path().display());
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
    ensure!(std::fs::symlink_metadata(source)?.is_file(), "ordinary source file required (no links): {}", source.display());
    std::fs::copy(source, dest)?;
    Ok(())
}

#[cfg(test)]
#[path = "kimi_runtime_tests.rs"]
mod tests;
