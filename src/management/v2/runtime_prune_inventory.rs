//! Read-only discovery and reference authority for owner-triggered runtime pruning.
use crate::session::model::CutexSessionStore;
use crate::upgrade::layout::Layout;
use anyhow::{Context, ensure};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub id: String,
    pub kind: String,
    pub release: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub bytes: u64,
    pub can_prune: bool,
    pub blocked_reasons: Vec<String>,
    #[serde(skip)]
    pub targets: Vec<PathBuf>,
}

pub struct Inventory {
    pub versions: Vec<Version>,
}

pub(super) fn executable_sha() -> anyhow::Result<String> {
    static SHA: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    SHA.get_or_init(|| {
        std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|path| crate::agent_management::file_sha256(&path))
            .map(|sha| sha.as_str().to_owned())
            .map_err(|_| "active executable unreadable".into())
    })
    .clone()
    .map_err(anyhow::Error::msg)
}

pub fn read_json(path: &Path) -> anyhow::Result<Value> {
    let bytes = std::fs::read(path)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "runtime reference metadata too large"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn optional_json(path: &Path) -> anyhow::Result<Option<Value>> {
    match std::fs::metadata(path) {
        Ok(_) => read_json(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Compare existing paths by canonical identity, absent references lexically. Windows aliases
/// and case are normalized without accepting parent traversal in deletion targets.
pub fn normalized(path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        PathBuf::from(
            crate::agent_management::strip_verbatim_prefix(&path.to_string_lossy()).to_lowercase(),
        )
    }
    #[cfg(not(windows))]
    {
        path
    }
}

pub fn overlaps(target: &Path, reference: &Path) -> bool {
    let target = normalized(target);
    let reference = normalized(reference);
    target == reference || reference.starts_with(&target)
}

/// Path references only, never credential contents or user prose. Declared homes/state paths
/// also protect a version directory if a custom deployment placed user data inside it.
fn references(value: &Value, out: &mut Vec<PathBuf>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "python"
                        | "adapter_source"
                        | "claude_cli"
                        | "tui_binary"
                        | "adapter"
                        | "node"
                        | "dsh_cli"
                        | "kimi_cli"
                        | "rg"
                        | "patch"
                        | "install_root"
                        | "runtime_dir"
                        | "native_path"
                        | "facade_path"
                ) {
                    if let Some(path) = value
                        .as_str()
                        .map(PathBuf::from)
                        .filter(|p| p.is_absolute())
                    {
                        out.push(path);
                    }
                }
                references(value, out);
            }
        }
        Value::Array(values) => {
            for value in values {
                references(value, out);
            }
        }
        _ => {}
    }
}

fn data_references(value: &Value, out: &mut Vec<PathBuf>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "native_home" | "claude_home" | "state_dir") {
                    if let Some(path) = value
                        .as_str()
                        .map(PathBuf::from)
                        .filter(|p| p.is_absolute())
                    {
                        out.push(path);
                    }
                }
                data_references(value, out);
            }
        }
        Value::Array(values) => {
            for value in values {
                data_references(value, out);
            }
        }
        _ => {}
    }
}

fn bundle(path: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let value = read_json(path).context("read pinned bundle")?;
    out.push(path.to_path_buf());
    for role in [
        "executable",
        "cli",
        "code_mode_host",
        "facade",
        "schema",
        "shared_config",
        "shared_auth",
    ] {
        if let Some(path) = value
            .get(role)
            .and_then(|v| v.get("path"))
            .and_then(Value::as_str)
        {
            ensure!(Path::new(path).is_absolute(), "relative bundle artifact");
            out.push(PathBuf::from(path));
        }
    }
    ensure!(
        value
            .pointer("/executable/path")
            .and_then(Value::as_str)
            .is_some(),
        "bundle executable missing"
    );
    Ok(())
}

fn contracts(value: &Value, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if let Value::Object(map) = value {
        for (key, value) in map {
            if key == "bundle_manifest" {
                if let Some(path) = value.as_str() {
                    bundle(Path::new(path), out)?;
                }
            } else {
                contracts(value, out)?;
            }
        }
    } else if let Value::Array(values) = value {
        for value in values {
            contracts(value, out)?;
        }
    }
    Ok(())
}

fn session_references(
    store: &CutexSessionStore,
    pins: &mut Vec<PathBuf>,
    data: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    // All durable rows, including hidden, paused, offline and restorable retired rows.
    for record in store.sessions.values() {
        if let Some(contract) = &record.explicit_launch {
            bundle(&contract.bundle_manifest, pins)?;
            data.push(contract.native_home.clone());
        }
        if let Some(session) = &record.claude_session {
            data.push(session.state_dir.clone());
            data.push(session.deployment.claude_home.clone());
            references(&serde_json::to_value(&session.deployment)?, pins);
            // A malformed retained pin cannot be treated as no pin.
            for name in [
                crate::runtime::claude_pin::PENDING_FILE,
                crate::runtime::claude_pin::PREVIOUS_FILE,
            ] {
                if let Some(value) = optional_json(&session.state_dir.join(name))? {
                    references(&value, pins);
                    data_references(&value, data);
                }
            }
        }
        if let Some(session) = &record.dsh_session {
            data.push(session.state_dir.clone());
            references(&serde_json::to_value(session)?, pins);
        }
        data.push(PathBuf::from(&record.cwd));
        if let Some(binding) = &record.profile_binding {
            references(&serde_json::to_value(binding)?, pins);
        }
    }
    Ok(())
}

fn selected(
    home: &Path,
    layout: Option<&Layout>,
    out: &mut Vec<PathBuf>,
    data: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for kind in ["light", "claude", "dsh", "kimi"] {
        if let Some(value) = optional_json(&home.join(kind).join("deployment.json"))? {
            references(&value, out);
            data_references(&value, data);
            contracts(&value, out)?;
        }
    }
    if let Some(layout) = layout {
        if let Some(value) = optional_json(&layout.base.join("components/state.json"))? {
            let state = value
                .as_object()
                .context("invalid component selection state")?;
            for name in ["claude-runtime", "dsh-runtime", "kimi-runtime"] {
                if let Some(entry) = state.get(name) {
                    let version = entry
                        .get("selected")
                        .and_then(Value::as_str)
                        .context("component selected version missing")?;
                    ensure!(
                        crate::upgrade::layout::valid_label(version),
                        "invalid selected version"
                    );
                    out.push(crate::components::version_dir(layout, name, version));
                }
            }
        }
    }
    Ok(())
}

fn rollback(home: &Path, layout: Option<&Layout>, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let value = read_json(&home.join("prune-protection.json"))
        .context("current kit rollback protection unavailable")?;
    ensure!(
        value["schema"] == "cutex/runtime-prune-protection/v1",
        "unknown rollback protection schema"
    );
    ensure!(
        value["kitOperation"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "rollback operation missing"
    );
    ensure!(
        value["activeExecutableSha256"].as_str() == Some(executable_sha()?.as_str()),
        "rollback protection belongs to another active binary"
    );
    let operation = PathBuf::from(value["kitOperation"].as_str().unwrap());
    ensure!(operation.is_absolute(), "relative kit operation");
    for (file, key) in [
        ("operation.json", "operationSha256"),
        ("backup-manifest.json", "backupManifestSha256"),
        ("originals-manifest.json", "originalsManifestSha256"),
    ] {
        ensure!(
            value["provenance"][key].as_str()
                == Some(crate::agent_management::file_sha256(&operation.join(file))?.as_str()),
            "rollback evidence changed"
        );
    }
    let paths = value["protectedPaths"]
        .as_array()
        .context("rollback paths missing")?;
    ensure!(!paths.is_empty(), "empty rollback proof");
    for path in paths {
        let path = PathBuf::from(path.as_str().context("invalid rollback path")?);
        ensure!(path.is_absolute(), "relative rollback path");
        out.push(path);
    }
    if let Some(layout) = layout.filter(|l| l.install_record_path().exists()) {
        let install = layout.read_install()?;
        if let Some(previous) = install.previous_release {
            crate::upgrade::layout::ensure_label(&previous, "previous release")?;
            out.push(layout.release_dir(&previous));
        }
    }
    let runs = home.join("package-upgrade-runs");
    if runs.exists() {
        let mut all = Vec::new();
        for entry in std::fs::read_dir(runs)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                all.push(serde_json::from_value::<super::runtime_packages::Run>(
                    read_json(&path)?,
                )?);
            }
        }
        for run in &all {
            if run.state == super::runtime_packages::RunState::Running
                || super::runtime_packages::revertable(run, &all, chrono::Utc::now())
            {
                contracts(&serde_json::to_value(run)?, out)?;
            }
        }
    }
    Ok(())
}

fn direct_children(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

/// A version root must be an ordinary owned object. Contained links are removable directory
/// entries, not permission to inspect/delete their destinations (bundled Python uses symlinks).
pub fn target_bytes(path: &Path) -> anyhow::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(!metadata.file_type().is_symlink(), "linked runtime root");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "reparse runtime root"
        );
    }
    tree_bytes(path)
}

/// Logical bytes of entries removed. Never follow a contained link/junction out of the version.
pub fn tree_bytes(path: &Path) -> anyhow::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Ok(metadata.len());
        }
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    ensure!(metadata.is_dir(), "special runtime artifact");
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(path)? {
        bytes = bytes
            .checked_add(tree_bytes(&entry?.path())?)
            .context("runtime size overflow")?;
    }
    Ok(bytes)
}

fn candidate(
    kind: &str,
    release: String,
    version: Option<String>,
    targets: Vec<PathBuf>,
) -> Version {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    for path in &targets {
        digest.update(normalized(path).to_string_lossy().as_bytes());
        digest.update([0]);
    }
    Version {
        id: format!("{:x}", digest.finalize()),
        kind: kind.into(),
        release,
        version,
        bytes: 0,
        can_prune: false,
        blocked_reasons: Vec::new(),
        targets,
    }
}

pub fn inspect(
    home: &Path,
    layout: Option<&Layout>,
    store: &CutexSessionStore,
) -> anyhow::Result<Inventory> {
    inspect_with_processes(home, layout, store, None)
}

pub(super) fn inspect_with_processes(
    home: &Path,
    layout: Option<&Layout>,
    store: &CutexSessionStore,
    known_processes: Option<anyhow::Result<Vec<PathBuf>>>,
) -> anyhow::Result<Inventory> {
    let registry =
        crate::launch::version_registry::load(&home.join("native-version-registry-v1.json"))?;
    let mut versions = Vec::new();
    let mut groups = BTreeMap::<String, BTreeSet<PathBuf>>::new();
    if let Some(registry) = registry {
        for (path, record) in registry.runtime_paths {
            if Path::new(&path).is_file() {
                groups
                    .entry(record.version)
                    .or_default()
                    .insert(PathBuf::from(path));
            }
        }
    }
    for (version, paths) in groups {
        let mut unique = BTreeMap::new();
        for path in paths {
            unique.entry(normalized(&path)).or_insert(path);
        }
        let release = super::runtime_packages::native_release(Some(&version), None)
            .unwrap_or_else(|| version.clone());
        versions.push(candidate(
            "native",
            release,
            Some(version),
            unique.into_values().collect(),
        ));
    }
    // Dedicated managed native trees also include helpers/schema; never remove the bundles index.
    for path in direct_children(&home.join("native"))? {
        let label = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if label == "bundles" || label.starts_with('.') {
            continue;
        }
        let owners = versions
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.targets
                    .iter()
                    .any(|p| normalized(p).starts_with(normalized(&path)))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if owners.len() == 1 {
            let version = &mut versions[owners[0]];
            version
                .targets
                .retain(|p| !normalized(p).starts_with(normalized(&path)));
            version.targets.push(path);
        }
    }
    if let Some(layout) = layout {
        for (kind, component) in [
            ("claude", "claude-runtime"),
            ("dsh", "dsh-runtime"),
            ("kimi", "kimi-runtime"),
        ] {
            for path in direct_children(&crate::components::components_dir(layout).join(component))?
            {
                let label = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_owned();
                if !label.starts_with('.') && crate::components::read_receipt(&path).is_some() {
                    versions.push(candidate(kind, label, None, vec![path]));
                }
            }
        }
        for path in direct_children(&layout.runtimes_dir())? {
            let label = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            if label
                .strip_prefix("claude-r")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                && [
                    path.join("src/cute_claude/__main__.py"),
                    path.join("adapter/src/cute_claude/__main__.py"),
                    path.join("cute-claude/src/cute_claude/__main__.py"),
                ]
                .iter()
                .any(|p| p.is_file())
                && optional_json(&path.join("runtime-receipt.json"))
                    .ok()
                    .flatten()
                    .is_some_and(|receipt| receipt["runtime"].as_str() == Some(label.as_str()))
            {
                versions.push(candidate("claude", label, None, vec![path]));
            }
        }
    }
    let mut chosen = Vec::new();
    let mut pins = Vec::new();
    let mut recovery = Vec::new();
    let mut data = Vec::new();
    // Keep generic inspection_failed for existing clients, and report which
    // authority was unavailable without exposing paths or credential metadata.
    let selected_complete = selected(home, layout, &mut chosen, &mut data).is_ok();
    let pins_complete = session_references(store, &mut pins, &mut data).is_ok();
    let rollback_complete = rollback(home, layout, &mut recovery).is_ok();
    let all_targets = versions
        .iter()
        .flat_map(|v| v.targets.iter().cloned())
        .collect::<Vec<_>>();
    let processes = known_processes
        .unwrap_or_else(|| super::runtime_prune_processes::running_paths(&all_targets));
    for version in &mut versions {
        version.id = candidate(
            &version.kind,
            version.release.clone(),
            version.version.clone(),
            version.targets.clone(),
        )
        .id;
        let mut reasons = BTreeSet::new();
        if !selected_complete || !pins_complete || !rollback_complete {
            reasons.insert("inspection_failed");
        }
        if !selected_complete {
            reasons.insert("selected_reference_inspection_failed");
        }
        if !pins_complete {
            reasons.insert("session_reference_inspection_failed");
        }
        if !rollback_complete {
            reasons.insert("rollback_protection_unavailable");
        }
        for target in &version.targets {
            let native_root = home.join("native");
            let managed = normalized(target).starts_with(normalized(&native_root))
                || layout.is_some_and(|l| {
                    [
                        l.releases_dir(),
                        l.runtimes_dir(),
                        l.natives_dir(),
                        crate::components::components_dir(l),
                    ]
                    .iter()
                    .any(|root| normalized(target).starts_with(normalized(root)))
                });
            if !managed {
                reasons.insert("unmanaged");
            }
            if version.kind == "native"
                && layout.is_some_and(|l| {
                    let relative = normalized(target)
                        .strip_prefix(normalized(&l.runtimes_dir()))
                        .ok()
                        .map(Path::to_path_buf);
                    normalized(target)
                        .starts_with(normalized(&crate::components::components_dir(l)))
                        || relative.is_some_and(|r| {
                            r.components().next().is_some_and(|c| {
                                c.as_os_str().to_string_lossy().starts_with("claude-r")
                            })
                        })
                })
            {
                reasons.insert("unmanaged");
            }
            if version.kind == "native" && target.is_file() {
                let name = target
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let name = name.strip_suffix(".exe").unwrap_or(&name);
                if !matches!(
                    name,
                    "codex" | "cute-codex" | "codex-app-server" | "codex-code-mode-host"
                ) {
                    reasons.insert("unmanaged");
                }
            }
            if chosen
                .iter()
                .any(|p| overlaps(target, p) || overlaps(p, target))
            {
                reasons.insert("selected");
            }
            if pins
                .iter()
                .any(|p| overlaps(target, p) || overlaps(p, target))
            {
                reasons.insert("pinned");
            }
            if data.iter().any(|p| overlaps(target, p)) {
                reasons.insert("pinned");
            }
            if recovery
                .iter()
                .any(|p| overlaps(target, p) || overlaps(p, target))
            {
                reasons.insert("rollback");
            }
            match &processes {
                Ok(paths)
                    if version.kind == "claude"
                        && paths.iter().any(|p| {
                            p == Path::new(super::runtime_prune_processes::UNKNOWN_CLAUDE)
                        }) =>
                {
                    reasons.insert("inspection_failed");
                    reasons.insert("claude_process_unresolved");
                }
                Ok(paths) if paths.iter().any(|p| overlaps(target, p)) => {
                    reasons.insert("running");
                }
                Err(_) => {
                    reasons.insert("inspection_failed");
                    reasons.insert("process_inspection_failed");
                }
                _ => {}
            }
            match target_bytes(target) {
                Ok(bytes) => version.bytes = version.bytes.saturating_add(bytes),
                Err(_) => {
                    reasons.insert("inspection_failed");
                    reasons.insert("runtime_size_inspection_failed");
                }
            }
        }
        version.blocked_reasons = reasons.into_iter().map(str::to_owned).collect();
        version.can_prune = version.blocked_reasons.is_empty();
    }
    versions.sort_by(|a, b| (&a.kind, &a.release, &a.id).cmp(&(&b.kind, &b.release, &b.id)));
    Ok(Inventory { versions })
}
