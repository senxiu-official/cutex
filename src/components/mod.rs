//! Optional components (components/README.md, contract v2): backend pieces
//! cutex installs, updates and removes on demand (`job-service`, …).
//!
//! Generic part (this module): `component.json` metadata (compiled in), the
//! archive named by the running release's signed manifest
//! (`components.<name>`), sha256 + size check, the release unpacker,
//! immutable version dirs `<install base>/components/<name>/<version>/` with
//! `receipt.json`, `<install base>/components/state.json`, the CLI
//! (`cutex component …`), `/v2/components` and the backend-update hooks.
//! Component specifics are behind [`Handler`] (`src/components/<name>.rs`).
//!
//! Rules (contract §0): versions are kept side by side and never repinned or
//! deleted while pinned; install = unpack + select for new agents; a component
//! failure never removes a capability or fails a backend update (the previous
//! version stays, `degraded`); unmanaged hosts are refused; a service cutex
//! did not install is `external`. Install / remove / upgrade runs exclude each
//! other through `upgrade.lock`.

pub mod claude_runtime;
pub mod job_service;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::upgrade::layout::{valid_label, Layout};
use crate::upgrade::manifest::{platform_key, Artifact, ReleaseManifest};
use crate::upgrade::minisign::PublicKey;
use crate::upgrade::service::Exec;
use crate::upgrade::stage::{upgrade_error, UpgradeError};

pub const COMPONENT_SCHEMA: &str = "cutex/component/v1";
pub const RECEIPT_SCHEMA: &str = "cutex/component-receipt/v1";
pub const RECEIPT_FILE: &str = "receipt.json";
/// Static Management capability for `/v2/components`.
pub const CAPABILITY: &str = "components";
/// Host event published on every change (`/v2/components` state).
pub const UPDATED_EVENT: &str = "cutex/components/updated";
pub const ORIGIN_SIGNED: &str = "signed";
pub const ORIGIN_SOURCE: &str = "source";

/// `component.json`: identity only (card text is desktop i18n keyed by name).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub schema: String,
    pub name: String,
    pub version: String,
    pub platforms: Vec<String>,
}

impl Metadata {
    pub fn parse(text: &str) -> Self {
        let metadata: Self = serde_json::from_str(text).expect("component.json");
        assert_eq!(metadata.schema, COMPONENT_SCHEMA);
        assert!(valid_label(&metadata.name) && valid_label(&metadata.version));
        metadata
    }

    pub fn supported_here(&self) -> bool {
        self.platforms.iter().any(|p| *p == platform_key())
    }
}

/// What a handler step gets.
pub struct Context<'a> {
    pub layout: &'a Layout,
    /// `<base>/components/<name>/<version>/`.
    pub dir: PathBuf,
    pub version: String,
    pub exec: &'a dyn Exec,
    /// The version selected before this operation, if any.
    pub previous: Option<Entry>,
    pub dry_run: bool,
}

/// `state` values of `GET /v2/components`.
pub mod states {
    pub const MISSING: &str = "missing";
    pub const INSTALLING: &str = "installing";
    pub const READY: &str = "ready";
    pub const DEGRADED: &str = "degraded";
    pub const NEEDS_USER: &str = "needsUser";
    pub const EXTERNAL: &str = "external";
    pub const RESTART_PENDING: &str = "restartPending";
    pub const NOT_IN_MANIFEST: &str = "notInManifest";
}

/// A component's cutex side (contract §4). Errors meant for the owner are
/// `UpgradeError`s (status + code).
pub trait Handler: Send + Sync {
    fn metadata(&self) -> &Metadata;
    /// Files the unpacked archive must contain (the executables checked
    /// against `receipt.json` before any start).
    fn required_files(&self) -> &'static [&'static str];
    /// CLI-only source installs: build or take the files from a checkout into `dest`.
    fn from_source(&self, source: &Path, dest: &Path) -> anyhow::Result<()>;
    /// Whether a service cutex did not install occupies this component's place.
    fn external(&self, layout: &Layout) -> bool;
    /// Unmanaged (hand-deployed) hosts: the component is set up by hand and
    /// usable (`GET /v2/components` then reports `external`, not `missing`).
    fn configured_by_hand(&self) -> bool {
        false
    }
    /// One-time preparation of `ctx.dir` (credentials, units, checks). Idempotent.
    fn prepare(&self, ctx: &Context) -> anyhow::Result<Value>;
    /// Select `ctx.dir` for new agents. Returns `{restartPending: bool, …}`.
    fn select(&self, ctx: &Context) -> anyhow::Result<Value>;
    /// Undo `select` (stop, unregister, unselect). Files and data stay.
    /// `force` overrides the in-use refusal.
    fn deselect(&self, ctx: &Context, force: bool) -> anyhow::Result<Vec<String>>;
    /// Cheap read-only health: `{"state": states::…, …}` for an installed component.
    fn probe(&self, layout: &Layout, entry: &Entry) -> Value;
    /// Version -> how many sessions / selections / processes pin it.
    fn pins(&self, layout: &Layout) -> BTreeMap<String, usize>;
    /// `Ok` when a restart / replacement is harmless now, else the reason.
    fn restart_gate(&self, layout: &Layout) -> Result<(), String>;
}

/// Every component this cutex knows.
pub fn handlers() -> Vec<Box<dyn Handler>> {
    #[allow(unused_mut)]
    let mut list: Vec<Box<dyn Handler>> =
        vec![Box::new(job_service::JobService::default()), Box::new(claude_runtime::ClaudeRuntime::default())];
    #[cfg(test)]
    TEST_HANDLER.with(|extra| list.extend(extra.borrow().map(|make| make())));
    list
}

#[cfg(test)]
thread_local! {
    /// Unit tests: an extra handler (a fake) on this test's thread.
    pub(crate) static TEST_HANDLER: std::cell::RefCell<Option<fn() -> Box<dyn Handler>>> = const { std::cell::RefCell::new(None) };
}

pub fn handler(name: &str) -> anyhow::Result<Box<dyn Handler>> {
    handlers()
        .into_iter()
        .find(|h| h.metadata().name == name)
        .ok_or_else(|| UpgradeError::boxed(404, "component_unknown", format!("no component named `{name}`"), json!({ "name": name })))
}

// ------------------------------------------------------------------ state

/// `<base>/components/state.json` entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub selected: String,
    /// `signed` | `source`.
    pub origin: String,
    pub installed_at: String,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// `{<name>: Entry}`.
pub type State = BTreeMap<String, Entry>;

pub fn components_dir(layout: &Layout) -> PathBuf {
    layout.base.join("components")
}

pub fn version_dir(layout: &Layout, name: &str, version: &str) -> PathBuf {
    components_dir(layout).join(name).join(version)
}

fn state_path(layout: &Layout) -> PathBuf {
    components_dir(layout).join("state.json")
}

pub fn load_state(layout: &Layout) -> State {
    std::fs::read(state_path(layout)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub fn save_state(layout: &Layout, state: &State) -> anyhow::Result<()> {
    crate::config::atomic::write_private_pretty_json_atomic(&state_path(layout), state, "components state")
}

fn update_entry(layout: &Layout, name: &str, change: impl FnOnce(&mut Option<Entry>)) -> anyhow::Result<()> {
    let mut state = load_state(layout);
    let mut entry = state.remove(name);
    change(&mut entry);
    if let Some(entry) = entry {
        state.insert(name.to_string(), entry);
    }
    save_state(layout, &state)
}

pub fn installed(layout: &Layout, name: &str) -> Option<Entry> {
    load_state(layout).get(name).cloned()
}

/// `receipt.json` of a version dir.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub schema: String,
    pub name: String,
    pub version: String,
    pub origin: String,
    #[serde(default)]
    pub archive_sha256: Option<String>,
    /// Relative path -> sha256 of every file.
    pub files: BTreeMap<String, String>,
    pub unpacked_at: String,
}

pub fn read_receipt(dir: &Path) -> Option<Receipt> {
    serde_json::from_slice::<Receipt>(&std::fs::read(dir.join(RECEIPT_FILE)).ok()?).ok().filter(|r| r.schema == RECEIPT_SCHEMA)
}

/// Before any start: `file` in `dir` must still have the receipt's sha256.
pub fn verify_file(dir: &Path, file: &str) -> anyhow::Result<PathBuf> {
    let receipt = read_receipt(dir).with_context(|| format!("{} has no receipt", dir.display()))?;
    let path = dir.join(file);
    let actual = crate::upgrade::manifest::file_sha256_hex(&path)?;
    if receipt.files.get(file) != Some(&actual) {
        return Err(upgrade_error(409, "component_unverified", format!("{} does not match its receipt", path.display())));
    }
    Ok(path)
}

fn file_hashes(dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "component files must not contain links");
            if kind.is_dir() {
                stack.push(entry.path());
            } else {
                let rel = entry.path().strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
                if rel != RECEIPT_FILE {
                    files.insert(rel, crate::upgrade::manifest::file_sha256_hex(&entry.path())?);
                }
            }
        }
    }
    Ok(files)
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).into_iter().flatten().flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(_) => total += entry.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    total
}

fn installed_versions(layout: &Layout, name: &str) -> Vec<String> {
    let mut versions: Vec<String> = std::fs::read_dir(components_dir(layout).join(name))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join(RECEIPT_FILE).is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|v| valid_label(v))
        .collect();
    versions.sort_by(|a, b| crate::upgrade::manifest::compare_releases(a, b));
    versions
}

// ------------------------------------------------------- locks and checks

/// `upgrade.lock`: install / remove / upgrade runs exclude each other.
pub enum Lock {
    /// Take `upgrade.lock` (refuse when held).
    Acquire,
    /// The caller (an upgrade run) already holds it.
    Held,
}

fn acquire(layout: &Layout, lock: &Lock) -> anyhow::Result<Option<crate::upgrade::state::OperationLock>> {
    match lock {
        Lock::Held => Ok(None),
        Lock::Acquire => crate::upgrade::state::OperationLock::try_acquire(layout)?
            .map(Some)
            .ok_or_else(|| crate::upgrade::stage::blocked("run_in_progress", "a backend upgrade or another component operation is running")),
    }
}

fn managed(layout: Option<&Layout>) -> anyhow::Result<&Layout> {
    layout
        .filter(|l| l.is_managed())
        .ok_or_else(|| upgrade_error(409, "unmanaged_install", "not a managed cutex install (no install.json); components are set up by hand on this host"))
}

fn supported(handler: &dyn Handler) -> anyhow::Result<()> {
    if !handler.metadata().supported_here() {
        return Err(UpgradeError::boxed(
            409,
            "component_unsupported_platform",
            format!("{} runs on {} only", handler.metadata().name, handler.metadata().platforms.join(", ")),
            json!({ "name": handler.metadata().name }),
        ));
    }
    Ok(())
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// -------------------------------------------------------------- resolution

/// `<base>/components/manifests/<release>.json` (+ `.minisig`): the signed
/// manifest of a release installed without one (a first `cutex service
/// install` from an archive), fetched once by [`fetch_release_manifest`].
/// Kept outside the release dir, whose file record must not change.
fn cached_manifest_path(layout: &Layout, release: &str) -> PathBuf {
    components_dir(layout).join("manifests").join(format!("{release}.json"))
}

/// The running release's verified manifest (its release dir, else the
/// cached copy); `None` without one.
pub fn release_manifest(layout: &Layout, keys: &[PublicKey]) -> anyhow::Result<Option<ReleaseManifest>> {
    let release = layout.active_release()?;
    let unverified = |e: anyhow::Error| upgrade_error(409, "release_unverified", format!("the manifest of release {release} does not verify: {e:#}"));
    if layout.release_dir(&release).join(crate::upgrade::stage::MANIFEST_FILE).is_file() {
        return crate::upgrade::stage::staged_manifest(layout, keys, &release).map(Some).map_err(unverified);
    }
    let cached = cached_manifest_path(layout, &release);
    let (Ok(bytes), Ok(signature)) = (std::fs::read(&cached), std::fs::read_to_string(cached.with_extension("json.minisig"))) else {
        return Ok(None);
    };
    crate::upgrade::minisign::verify_text(keys, &bytes, &signature).map_err(unverified)?;
    let manifest = ReleaseManifest::parse(&bytes).map_err(unverified)?;
    ensure!(manifest.cutex.release == release, "cached manifest names release {}", manifest.cutex.release);
    Ok(Some(manifest))
}

/// Whether the running build is a source / dev build (no signed release).
fn dev_build(release: &str) -> bool {
    release.starts_with("dev-") || crate::build_info::RELEASE.is_none()
}

/// [`release_manifest`], else (a signed build installed without its
/// manifest) the manifest of the same release fetched from the configured
/// source, verified, checked against this build's commit and cached.
pub fn fetch_release_manifest(layout: &Layout, keys: &[PublicKey]) -> anyhow::Result<Option<ReleaseManifest>> {
    if let Some(manifest) = release_manifest(layout, keys)? {
        return Ok(Some(manifest));
    }
    let release = layout.active_release()?;
    if dev_build(&release) || keys.is_empty() {
        return Ok(None);
    }
    let settings = crate::upgrade::state::Settings::load(layout);
    let channel = settings.channel(layout);
    let tag = crate::upgrade::manifest::release_tag(&release);
    let (manifest, bytes, signature) = crate::upgrade::stage::fetch_manifest(&settings.source()?, keys, channel, &tag, None)
        .map_err(|e| upgrade_error(409, "release_unverified", format!("could not get the signed manifest of release {release}: {e:#}")))?;
    if let Some(commit) = crate::build_info::COMMIT {
        let n = commit.len().min(manifest.cutex.commit.len());
        ensure!(n >= 7 && commit[..n] == manifest.cutex.commit[..n], "the manifest of {release} names commit {}, this build is {commit}", manifest.cutex.commit);
    }
    let cached = cached_manifest_path(layout, &release);
    std::fs::create_dir_all(cached.parent().context("manifest cache dir")?)?;
    crate::config::atomic::write_bytes_atomic(&cached.with_extension("json.minisig"), signature.as_bytes())?;
    crate::config::atomic::write_bytes_atomic(&cached, &bytes)?;
    Ok(Some(manifest))
}

/// One component version a manifest names for this platform.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub version: String,
    pub tag: String,
    pub artifact: Artifact,
}

pub fn offer(manifest: &ReleaseManifest, name: &str) -> Option<Offer> {
    manifest.component_artifact(name).map(|(component, artifact)| Offer {
        version: component.version.clone(),
        tag: artifact.tag.clone().unwrap_or_else(|| format!("component-{name}-{}", component.version)),
        artifact: artifact.clone(),
    })
}

fn staging_dir(layout: &Layout, name: &str) -> PathBuf {
    layout.staging_dir().join(format!("component-{name}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]))
}

fn place(handler: &dyn Handler, files: &Path, target: &Path, origin: &str, version: &str, archive_sha256: Option<String>) -> anyhow::Result<()> {
    let name = &handler.metadata().name;
    for file in handler.required_files() {
        ensure!(files.join(file).is_file(), "component {name} lacks {file}");
    }
    let receipt = Receipt {
        schema: RECEIPT_SCHEMA.into(),
        name: name.clone(),
        version: version.into(),
        origin: origin.into(),
        archive_sha256,
        files: file_hashes(files)?,
        unpacked_at: now(),
    };
    std::fs::write(files.join(RECEIPT_FILE), serde_json::to_vec_pretty(&receipt)?)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(files, target).with_context(|| format!("move {} to {}", files.display(), target.display()))
}

/// `<base>/components/<name>/<version>/` for a signed offer: present (same
/// archive) or downloaded from the configured source and unpacked.
pub fn fetch(layout: &Layout, handler: &dyn Handler, offer: &Offer) -> anyhow::Result<PathBuf> {
    fetch_with(layout, handler, offer, None)
}

/// [`fetch`] from `source` (default: the configured release source).
pub fn fetch_with(layout: &Layout, handler: &dyn Handler, offer: &Offer, source: Option<&crate::upgrade::source::Source>) -> anyhow::Result<PathBuf> {
    let name = &handler.metadata().name;
    let target = version_dir(layout, name, &offer.version);
    if target.exists() {
        let receipt = read_receipt(&target);
        ensure!(
            receipt.as_ref().and_then(|r| r.archive_sha256.as_deref()) == Some(offer.artifact.sha256.as_str()),
            "{} exists but was not unpacked from the manifest's archive; move it away",
            target.display()
        );
        return Ok(target);
    }
    let staging = staging_dir(layout, name);
    std::fs::create_dir_all(&staging)?;
    let result = (|| -> anyhow::Result<()> {
        let archive = staging.join(&offer.artifact.name);
        progress("download");
        let configured;
        let source = match source {
            Some(source) => source,
            None => {
                configured = crate::upgrade::state::Settings::load(layout).source()?;
                &configured
            }
        };
        source.download(&offer.tag, &offer.artifact.name, &archive, offer.artifact.size, &offer.artifact.sha256)?;
        let files = staging.join("files");
        progress("unpack");
        crate::upgrade::archive::unpack_tar_gz_file(&archive, &files)?;
        place(handler, &files, &target, ORIGIN_SIGNED, &offer.version, Some(offer.artifact.sha256.clone()))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result?;
    Ok(target)
}

fn fetch_source(layout: &Layout, handler: &dyn Handler, checkout: &Path) -> anyhow::Result<(String, PathBuf)> {
    let name = handler.metadata().name.clone();
    let staging = staging_dir(layout, &name);
    std::fs::create_dir_all(&staging)?;
    let result = (|| -> anyhow::Result<(String, PathBuf)> {
        let files = staging.join("files");
        std::fs::create_dir_all(&files)?;
        handler.from_source(checkout, &files)?;
        let mut digest = String::new();
        for (file, sha) in file_hashes(&files)? {
            digest.push_str(&format!("{file}\0{sha}\n"));
        }
        let version = format!("{}-source-{}", handler.metadata().version, &crate::upgrade::manifest::sha256_hex(digest.as_bytes())[..12]);
        let target = version_dir(layout, &name, &version);
        if !target.exists() {
            place(handler, &files, &target, ORIGIN_SOURCE, &version, None)?;
        }
        Ok((version, target))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

// --------------------------------------------------------------- operations

#[derive(Clone, Debug, Default)]
pub struct InstallRequest {
    /// CLI only: a checkout to build / take the files from (`origin: source`).
    pub from_source: Option<PathBuf>,
    pub dry_run: bool,
}

/// Install / update `name` to the version the running release names (or a
/// source build). Idempotent. Returns `{component, result}`.
pub fn install(layout: Option<&Layout>, keys: &[PublicKey], name: &str, request: &InstallRequest, exec: &dyn Exec, lock: Lock) -> anyhow::Result<Value> {
    let handler = handler(name)?;
    install_with(layout, keys, handler.as_ref(), request, exec, lock)
}

pub fn install_with(
    layout: Option<&Layout>,
    keys: &[PublicKey],
    handler: &dyn Handler,
    request: &InstallRequest,
    exec: &dyn Exec,
    lock: Lock,
) -> anyhow::Result<Value> {
    supported(handler)?;
    let layout = managed(layout)?;
    let name = handler.metadata().name.clone();
    let _lock = if request.dry_run { None } else { acquire(layout, &lock)? };
    let previous = installed(layout, &name);
    if previous.is_none() && handler.external(layout) {
        return Err(UpgradeError::boxed(
            409,
            "component_external",
            format!("a {name} that cutex did not install is already running on this host; it is left alone"),
            json!({ "name": name }),
        ));
    }
    let (version, dir, origin) = match &request.from_source {
        Some(checkout) => {
            ensure!(!request.dry_run, "--from-source has no dry run (it builds)");
            let (version, dir) = fetch_source(layout, handler, checkout)?;
            (version, dir, ORIGIN_SOURCE)
        }
        None => {
            let manifest = fetch_release_manifest(layout, keys)?.ok_or_else(|| {
                UpgradeError::boxed(409, "component_source_only", format!(
                    "this cutex release has no signed manifest (source install): use `cutex component install {name} --from-source <checkout>`"
                ), json!({ "name": name }))
            })?;
            let offer = offer(&manifest, &name).ok_or_else(|| {
                UpgradeError::boxed(409, "component_not_in_release", format!(
                    "cutex release {} does not ship {name} for {}; update cutex", manifest.cutex.release, platform_key()
                ), json!({ "name": name }))
            })?;
            let dir = if request.dry_run { version_dir(layout, &name, &offer.version) } else { fetch(layout, handler, &offer)? };
            (offer.version, dir, ORIGIN_SIGNED)
        }
    };
    let ctx = Context { layout, dir, version: version.clone(), exec, previous: previous.clone(), dry_run: request.dry_run };
    let result = (|| -> anyhow::Result<Value> {
        if !request.dry_run {
            progress("prepare");
        }
        let prepared = handler.prepare(&ctx)?;
        if !request.dry_run {
            progress("select");
        }
        let selected = handler.select(&ctx)?;
        Ok(json!({ "version": version, "origin": origin, "prepare": prepared, "select": selected }))
    })();
    if !request.dry_run {
        match &result {
            Ok(_) => update_entry(layout, &name, |entry| {
                *entry = Some(Entry { selected: version.clone(), origin: origin.into(), installed_at: now(), last_error: None });
            })?,
            Err(error) => {
                let message = format!("{error:#}");
                update_entry(layout, &name, |entry| {
                    if let Some(entry) = entry {
                        entry.last_error = Some(message);
                    }
                })?;
            }
        }
        publish_state(layout, keys, handler);
    }
    let result = result?;
    Ok(json!({ "component": status_of(Some(layout), keys, handler, None), "result": result }))
}

/// Remove `name`: deselect (stop, unregister, unselect for new agents).
/// Version dirs and data stay (`cutex component prune`).
pub fn remove(layout: Option<&Layout>, keys: &[PublicKey], name: &str, force: bool, dry_run: bool, exec: &dyn Exec, lock: Lock) -> anyhow::Result<Value> {
    let handler = handler(name)?;
    remove_with(layout, keys, handler.as_ref(), force, dry_run, exec, lock)
}

pub fn remove_with(
    layout: Option<&Layout>,
    keys: &[PublicKey],
    handler: &dyn Handler,
    force: bool,
    dry_run: bool,
    exec: &dyn Exec,
    lock: Lock,
) -> anyhow::Result<Value> {
    supported(handler)?;
    let layout = managed(layout)?;
    let name = handler.metadata().name.clone();
    let _lock = if dry_run { None } else { acquire(layout, &lock)? };
    let entry = installed(layout, &name)
        .ok_or_else(|| UpgradeError::boxed(409, "component_not_installed", format!("{name} is not installed by cutex"), json!({ "name": name })))?;
    let ctx = Context {
        layout,
        dir: version_dir(layout, &name, &entry.selected),
        version: entry.selected.clone(),
        exec,
        previous: Some(entry),
        dry_run,
    };
    if !dry_run {
        progress("stop");
    }
    let actions = handler.deselect(&ctx, force)?;
    if !dry_run {
        update_entry(layout, &name, |entry| *entry = None)?;
        publish_state(layout, keys, handler);
    }
    Ok(json!({ "component": status_of(Some(layout), keys, handler, None), "actions": actions }))
}

/// Delete version dirs nothing needs: not selected, not pinned, not named by
/// the running or the previous release's manifest.
pub fn prune(layout: Option<&Layout>, keys: &[PublicKey], name: &str, dry_run: bool) -> anyhow::Result<Vec<String>> {
    let handler = handler(name)?;
    let layout = managed(layout)?;
    let _lock = if dry_run { None } else { acquire(layout, &Lock::Acquire)? };
    let selected = installed(layout, name).map(|e| e.selected);
    let pins = handler.pins(layout);
    let mut named = std::collections::BTreeSet::new();
    let record = layout.read_install().ok();
    for release in [layout.active_release().ok(), record.and_then(|r| r.previous_release)].into_iter().flatten() {
        if let Ok(manifest) = crate::upgrade::stage::staged_manifest(layout, keys, &release) {
            if let Some(offer) = offer(&manifest, name) {
                named.insert(offer.version);
            }
        }
    }
    let mut removed = Vec::new();
    for version in installed_versions(layout, name) {
        if Some(&version) == selected.as_ref() || pins.get(&version).copied().unwrap_or(0) > 0 || named.contains(&version) {
            continue;
        }
        let dir = version_dir(layout, name, &version);
        if !dry_run {
            std::fs::remove_dir_all(&dir)?;
        }
        removed.push(dir.display().to_string());
    }
    Ok(removed)
}

/// Where installs come from on this host.
pub fn install_from(layout: Option<&Layout>, keys: &[PublicKey]) -> &'static str {
    match layout.filter(|l| l.is_managed()) {
        None => "none",
        Some(layout) => match release_manifest(layout, keys) {
            Ok(Some(_)) => "release",
            // A signed build without its manifest fetches it on install.
            _ if !layout.active_release().is_ok_and(|r| dev_build(&r)) && !keys.is_empty() => "release",
            _ => "source-only",
        },
    }
}

/// One component's view in `GET /v2/components`.
pub fn status_of(layout: Option<&Layout>, keys: &[PublicKey], handler: &dyn Handler, operation: Option<&Value>) -> Value {
    let meta = handler.metadata();
    let supported = meta.supported_here();
    let layout = layout.filter(|l| l.is_managed());
    let entry = layout.and_then(|l| installed(l, &meta.name));
    let manifest = layout.map(|l| release_manifest(l, keys));
    let available = match &manifest {
        Some(Ok(Some(m))) => offer(m, &meta.name).map(|o| o.version),
        _ => None,
    };
    let in_manifest = matches!(&manifest, Some(Ok(Some(m))) if offer(m, &meta.name).is_some());
    let probe = match (layout, &entry) {
        (Some(l), Some(e)) if supported => handler.probe(l, e),
        _ => Value::Null,
    };
    let running = operation.is_some_and(|op| op["name"] == meta.name.as_str() && op["state"] == "running");
    let state = if running {
        states::INSTALLING
    } else if !supported {
        states::MISSING
    } else if layout.is_none() {
        if handler.configured_by_hand() { states::EXTERNAL } else { states::MISSING }
    } else if let Some(entry) = &entry {
        match probe["state"].as_str() {
            Some(state) => state,
            None if entry.last_error.is_some() => states::DEGRADED,
            None => states::READY,
        }
    } else if layout.is_some_and(|l| handler.external(l)) {
        states::EXTERNAL
    } else if !in_manifest && matches!(&manifest, Some(Ok(Some(_)))) {
        states::NOT_IN_MANIFEST
    } else {
        states::MISSING
    };
    let pins = layout.map(|l| handler.pins(l)).unwrap_or_default();
    let versions: Vec<Value> = layout
        .map(|l| installed_versions(l, &meta.name))
        .unwrap_or_default()
        .into_iter()
        .map(|version| {
            let size = layout.map(|l| dir_size(&version_dir(l, &meta.name, &version))).unwrap_or(0);
            json!({ "version": version, "pinnedBy": pins.get(&version).copied().unwrap_or(0), "sizeBytes": size })
        })
        .collect();
    let mut view = json!({
        "name": meta.name,
        "supported": supported,
        "platforms": meta.platforms,
        "state": state,
        "selected": entry.as_ref().map(|e| e.selected.clone()),
        "available": available,
        "origin": entry.as_ref().map(|e| e.origin.clone()),
        "versions": versions,
        "detail": probe,
        "error": entry.as_ref().and_then(|e| e.last_error.clone()),
    });
    if state == states::NEEDS_USER {
        view["steps"] = probe.get("steps").cloned().unwrap_or_else(|| json!([]));
    }
    view
}

/// `GET /v2/components`.
pub fn list(layout: Option<&Layout>, keys: &[PublicKey], operation: Option<&Value>) -> Value {
    json!({
        "managed": layout.is_some_and(|l| l.is_managed()),
        "installFrom": install_from(layout, keys),
        "components": handlers().iter().map(|h| status_of(layout, keys, h.as_ref(), operation)).collect::<Vec<_>>(),
        "operation": operation,
    })
}

// --------------------------------------------------------- backend updates

/// `cutex upgrade stage`: download + verify (+ prepare later) the new
/// versions of installed signed components. Never fetches components that
/// are not installed. Best effort: failures are returned as notes.
pub fn stage_installed(layout: &Layout, manifest: &ReleaseManifest, source: Option<&crate::upgrade::source::Source>) -> Vec<String> {
    let mut notes = Vec::new();
    for (name, entry) in load_state(layout) {
        let Ok(handler) = handler(&name) else { continue };
        if entry.origin != ORIGIN_SIGNED || !handler.metadata().supported_here() {
            continue;
        }
        let Some(offer) = offer(manifest, &name) else { continue };
        if offer.version == entry.selected {
            continue;
        }
        if let Err(error) = fetch_with(layout, handler.as_ref(), &offer, source) {
            notes.push(format!("{name} {}: {error:#}", offer.version));
        }
    }
    notes
}

/// Bytes the next stage would download for installed components (disk check).
pub fn stage_bytes(layout: &Layout, manifest: &ReleaseManifest) -> u64 {
    load_state(layout)
        .iter()
        .filter(|(_, e)| e.origin == ORIGIN_SIGNED)
        .filter_map(|(name, e)| offer(manifest, name).filter(|o| o.version != e.selected).map(|o| o.artifact.size))
        .sum()
}

/// During an upgrade run's swap (upgrade.lock held): select the versions the
/// new release names for installed signed components. Writes the previous
/// state to `snapshot` for [`restore_after_rollback`]. Never fails the run:
/// a failure leaves the previous version selected (`lastError`, degraded).
pub fn select_for_release(layout: &Layout, keys: &[PublicKey], exec: &dyn Exec, snapshot: &Path) -> Vec<String> {
    let state = load_state(layout);
    if state.is_empty() {
        return Vec::new();
    }
    if let Some(parent) = snapshot.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(snapshot, serde_json::to_vec_pretty(&state).unwrap_or_default());
    let manifest = release_manifest(layout, keys);
    let mut notes = Vec::new();
    for (name, entry) in state {
        let target = match &manifest {
            Ok(Some(m)) if entry.origin == ORIGIN_SIGNED => match offer(m, &name) {
                Some(o) => Some(o.version),
                None => {
                    notes.push(format!("{name} stays on {} (notInManifest)", entry.selected));
                    None
                }
            },
            Err(error) if entry.origin == ORIGIN_SIGNED => {
                notes.push(format!("{name} stays on {}: {error:#}", entry.selected));
                None
            }
            _ => None,
        };
        let outcome = match &target {
            // A new version: fetch (normally staged already), prepare, select.
            Some(version) if *version != entry.selected => {
                install(Some(layout), keys, &name, &InstallRequest::default(), exec, Lock::Held).map(|out| out["result"]["select"].clone())
            }
            // Same version: select again (the native / launcher set may have changed).
            _ => reselect(layout, &name, &entry, exec),
        };
        let shown = target.unwrap_or_else(|| entry.selected.clone());
        match outcome {
            Ok(select) if select["restartPending"] == true => notes.push(format!("{name} {shown} selected; restarts when no job is running")),
            Ok(select) if select["restarted"] == true => notes.push(format!("{name} {shown} restarted")),
            Ok(_) => {}
            Err(error) => {
                let message = format!("{error:#}");
                let _ = update_entry(layout, &name, |e| {
                    if let Some(e) = e {
                        e.last_error = Some(message.clone());
                    }
                });
                notes.push(format!("{name} stays on {}: {message}", entry.selected));
            }
        }
    }
    notes
}

/// `select` again for the selected version (no fetch).
fn reselect(layout: &Layout, name: &str, entry: &Entry, exec: &dyn Exec) -> anyhow::Result<Value> {
    let handler = handler(name)?;
    let ctx = Context {
        layout,
        dir: version_dir(layout, name, &entry.selected),
        version: entry.selected.clone(),
        exec,
        previous: Some(entry.clone()),
        dry_run: false,
    };
    handler.select(&ctx)
}

/// After a rolled-back run: reselect the versions selected before the swap.
pub fn restore_after_rollback(layout: &Layout, exec: &dyn Exec, snapshot: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(snapshot) else { return Vec::new() };
    let Ok(before) = serde_json::from_slice::<State>(&bytes) else { return Vec::new() };
    let now_state = load_state(layout);
    let mut notes = Vec::new();
    for (name, old) in before {
        if now_state.get(&name).map(|e| &e.selected) == Some(&old.selected) {
            continue;
        }
        let Ok(handler) = handler(&name) else { continue };
        let ctx = Context {
            layout,
            dir: version_dir(layout, &name, &old.selected),
            version: old.selected.clone(),
            exec,
            previous: now_state.get(&name).cloned(),
            dry_run: false,
        };
        match handler.prepare(&ctx).and_then(|_| handler.select(&ctx)) {
            Ok(_) => {
                let _ = update_entry(layout, &name, |e| *e = Some(Entry { last_error: None, ..old.clone() }));
                notes.push(format!("{name} back on {}", old.selected));
            }
            Err(error) => notes.push(format!("{name} not restored to {}: {error:#}", old.selected)),
        }
    }
    notes
}

/// Management's watcher: apply a pending restart once its gate opens.
pub fn settle_pending(layout: &Layout, exec: &dyn Exec) {
    for (name, entry) in load_state(layout) {
        let Ok(handler) = handler(&name) else { continue };
        if handler.probe(layout, &entry)["state"] != states::RESTART_PENDING || handler.restart_gate(layout).is_err() {
            continue;
        }
        let Ok(Some(_lock)) = crate::upgrade::state::OperationLock::try_acquire(layout) else { continue };
        let ctx = Context { layout, dir: version_dir(layout, &name, &entry.selected), version: entry.selected.clone(), exec, previous: Some(entry), dry_run: false };
        let result = handler.select(&ctx);
        let _ = update_entry(layout, &name, |e| {
            if let Some(e) = e {
                e.last_error = result.as_ref().err().map(|error| format!("{error:#}"));
            }
        });
        publish_state(layout, &crate::upgrade::keys::trusted_keys(), handler.as_ref());
    }
}

/// Event `cutex/components/updated` (`reason: state | startup`; the HTTP
/// layer sends `operation` ones): `{name, reason, state, operation}`.
pub fn event_params(name: &str, reason: &str, state: &Value, operation: Option<&Value>) -> Value {
    json!({ "name": name, "reason": reason, "state": state, "operation": operation })
}

fn publish_state(layout: &Layout, keys: &[PublicKey], handler: &dyn Handler) {
    if cfg!(test) {
        return;
    }
    let status = status_of(Some(layout), keys, handler, None);
    publish(event_params(&handler.metadata().name, "state", &status["state"], None));
}

/// Publish one event (no-op in unit tests).
pub fn publish(params: Value) {
    if cfg!(test) {
        return;
    }
    crate::management::v2::documents::publish_host_event(UPDATED_EVENT, params);
}

// ------------------------------------------------------------ progress

/// Operation steps for `/v2/components` (`download`, `unpack`, `prepare`,
/// `select`, `stop`, …). One operation runs at a time (upgrade.lock), so a
/// single hook suffices.
static PROGRESS: std::sync::Mutex<Option<fn(&str)>> = std::sync::Mutex::new(None);

pub fn set_progress_hook(hook: Option<fn(&str)>) {
    if let Ok(mut slot) = PROGRESS.lock() {
        *slot = hook;
    }
}

fn progress(step: &str) {
    let hook = PROGRESS.lock().ok().and_then(|slot| *slot);
    if let Some(hook) = hook {
        hook(step);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
