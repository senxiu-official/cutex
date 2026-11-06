//! Immutable native artifact copies for future bundles. Never edits existing pins.
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::local_deployment::LocalDeployment;
use super::stock::{StockBundle, VerifiedFile};

pub fn root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("native"))
}

/// Read-only classification, including Windows ordinary/verbatim aliases.
pub fn is_inside(path: &Path, root: &Path) -> bool {
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => {
            path.is_absolute()
                && !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                && path.starts_with(root)
        }
    }
}

fn artifacts(bundle: &StockBundle) -> Vec<(&'static str, &VerifiedFile)> {
    let mut files = vec![
        ("executable", &bundle.executable),
        ("code_mode_host", &bundle.code_mode_host),
        ("facade", &bundle.facade),
        ("schema", &bundle.schema),
    ];
    if let Some(cli) = &bundle.cli {
        files.push(("cli", cli));
    }
    files
}

fn filename(role: &str, file: &VerifiedFile) -> String {
    if role == "schema" {
        return "codex_app_server_protocol.schemas.json".into();
    }
    let suffix = if file
        .path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        ".exe"
    } else {
        ""
    };
    let stem = match role {
        "executable" => "codex-app-server",
        "cli" => "codex",
        "code_mode_host" => "codex-code-mode-host",
        "facade" => "cutex-mcp",
        _ => unreachable!(),
    };
    format!("{stem}{suffix}")
}

fn replace_paths(bundle: &mut StockBundle, directory: &Path) -> anyhow::Result<()> {
    let path = |role, file: &VerifiedFile| directory.join(filename(role, file)).canonicalize();
    bundle.executable.path = path("executable", &bundle.executable)?;
    bundle.code_mode_host.path = path("code_mode_host", &bundle.code_mode_host)?;
    bundle.facade.path = path("facade", &bundle.facade)?;
    bundle.schema.path = path("schema", &bundle.schema)?;
    if let Some(cli) = &mut bundle.cli {
        cli.path = path("cli", cli)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedStatus {
    pub managed: bool,
    pub bundle_manifest: PathBuf,
    pub managed_root: PathBuf,
    pub artifacts: Vec<ArtifactStatus>,
    pub registry_metadata_available: bool,
    pub known_version_paths_recorded: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactStatus {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
    pub managed: bool,
}

/// Does not create directories, select defaults, read auth/config or spawn anything.
pub fn status(deployment: &LocalDeployment) -> anyhow::Result<ManagedStatus> {
    let bundle: StockBundle = serde_json::from_slice(&fs::read(&deployment.bundle_manifest)?)?;
    let root = root()?;
    let mut entries = Vec::new();
    for (role, file) in artifacts(&bundle) {
        file.validate()?;
        entries.push(ArtifactStatus {
            role: role.into(),
            path: file.path.clone(),
            sha256: file.sha256.as_str().into(),
            managed: is_inside(&file.path, &root),
        });
    }
    let registry = super::version_registry::load_relaxed();
    let recorded: Vec<bool> = registry
        .as_ref()
        .map(|registry| {
            std::iter::once(&bundle.executable)
                .chain(bundle.cli.iter())
                .filter_map(|file| {
                    let version = registry.runtime_version(file.sha256.as_str())?;
                    Some(
                        file.path
                            .to_str()
                            .and_then(|path| registry.runtime_paths.get(path))
                            .is_some_and(|record| record.version == version),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ManagedStatus {
        managed: entries.iter().all(|e| e.managed),
        bundle_manifest: deployment.bundle_manifest.clone(),
        managed_root: root,
        artifacts: entries,
        registry_metadata_available: registry.is_some(),
        known_version_paths_recorded: (!recorded.is_empty())
            .then(|| recorded.iter().all(|recorded| *recorded)),
    })
}

/// Copy public artifacts and produce a new immutable template; no deployment or
/// session file is changed. Shared config/auth/native_home are deliberately not copied.
pub fn copy(deployment: &LocalDeployment) -> anyhow::Result<LocalDeployment> {
    let _runtime_reference = crate::runtime::reference_guard::acquire_shared()?;
    deployment.check_instance_paths()?;
    let original: StockBundle = serde_json::from_slice(&fs::read(&deployment.bundle_manifest)?)?;
    ensure!(
        original.soon_ingress(),
        "managed copy requires a compatible native bundle"
    );
    for (_, file) in artifacts(&original) {
        file.validate()?;
    }
    let registry_path = super::version_registry::registry_path()?;
    let root = root()?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&root)?;
    let root = root.canonicalize()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".copy.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    // Version metadata is not launch authority. Preserve unreadable/malformed
    // registry bytes and keep the runtime usable with an unknown version.
    let before_bytes = fs::read(&registry_path).ok();
    let mut registry = super::version_registry::load(&registry_path).ok().flatten();
    let version = registry
        .as_ref()
        .and_then(|r| r.runtime_version(original.executable.sha256.as_str()))
        .map(str::to_owned);
    let label: String = version
        .as_deref()
        .unwrap_or("unknown")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let identity = serde_json::to_vec(&(
        original.version,
        &original.upstream_commit,
        &original.native_patch_commit,
        artifacts(&original)
            .iter()
            .map(|(role, file)| (*role, file.sha256.as_str()))
            .collect::<Vec<_>>(),
    ))?;
    let directory = root.join(format!("{label}-{:x}", Sha256::digest(&identity)));
    if !directory.try_exists()? {
        let staging = root.join(format!(".copy-{}", uuid::Uuid::new_v4()));
        let mut stage_builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            stage_builder.mode(0o700);
        }
        stage_builder.create(&staging)?;
        for (role, file) in artifacts(&original) {
            let destination = staging.join(filename(role, file));
            let mut source = fs::File::open(&file.path)?;
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            std::io::copy(&mut source, &mut target)?;
            target.sync_all()?;
            drop(target);
            fs::set_permissions(&destination, source.metadata()?.permissions())?;
            ensure!(
                crate::agent_management::file_sha256(&destination)? == file.sha256,
                "native artifact changed during copy: {role}"
            );
        }
        // The lock serializes copy publishers; never overwrite a published directory.
        ensure!(
            !directory.try_exists()?,
            "managed native destination appeared during copy"
        );
        #[cfg(unix)]
        fs::File::open(&staging)?.sync_all()?;
        fs::rename(&staging, &directory)?;
        #[cfg(unix)]
        fs::File::open(&root)?.sync_all()?;
    }
    ensure!(
        !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
        "managed native directory is a symlink"
    );
    let mut managed = original.clone();
    replace_paths(&mut managed, &directory)?;
    for (_, file) in artifacts(&managed) {
        ensure!(
            is_inside(&file.path, &directory),
            "managed artifact escapes its directory"
        );
        file.validate()
            .context("existing managed native copy is corrupt; refusing overwrite")?;
    }
    let bytes = serde_json::to_vec_pretty(&managed)?;
    let manifests = root.join("bundles");
    fs::create_dir_all(&manifests)?;
    let manifest = manifests.join(format!("{:x}.json", Sha256::digest(&bytes)));
    if manifest.try_exists()? {
        ensure!(
            fs::read(&manifest)? == bytes,
            "managed native template changed; refusing overwrite"
        );
    } else {
        crate::config::atomic::write_private_bytes_atomic(&manifest, &bytes)?;
    }
    // Version labels remain evidence-based: never guess an unknown SHA's version
    // from registry.latest. Registered bytes gain both original and managed paths.
    if let Some(registry) = &mut registry {
        for (old, new) in std::iter::once((&original.executable, &managed.executable))
            .chain(original.cli.iter().zip(managed.cli.iter()))
        {
            // Frontend-only updates are supported: the CLI's version belongs
            // to its own SHA, never the backend or registry.latest.
            let Some(record) = registry.runtime_artifacts.get(old.sha256.as_str()).cloned() else {
                continue;
            };
            for path in [&old.path, &new.path] {
                let key = path
                    .to_str()
                    .context("native path is not UTF-8")?
                    .to_owned();
                // Do not repair contradictory historical display metadata as
                // part of runtime creation. Status reports missing aliases.
                registry
                    .runtime_paths
                    .entry(key)
                    .or_insert_with(|| record.clone());
            }
        }
        let bytes = serde_json::to_vec_pretty(registry)?;
        let current = fs::read(&registry_path).ok();
        if bytes.len() <= 64 * 1024
            && current == before_bytes
            && before_bytes.as_ref().is_some_and(|bytes| {
                serde_json::from_slice::<super::version_registry::NativeVersionRegistry>(bytes)
                    .ok()
                    .as_ref()
                    != Some(&*registry)
            })
        {
            if let Err(error) =
                crate::config::atomic::write_private_bytes_atomic(&registry_path, &bytes)
            {
                eprintln!("native version display aliases could not be recorded: {error}");
            }
        }
    }
    Ok(LocalDeployment {
        native_home: deployment.native_home.clone(),
        bundle_manifest: manifest.canonicalize()?,
        job_mcp: deployment.job_mcp.clone(),
    })
}

/// Explicit kit/owner operation. Only the new default is selected; old bundles
/// and all session pins remain byte-identical and their source paths remain valid.
pub fn manage_selected() -> anyhow::Result<ManagedStatus> {
    let _runtime_reference = crate::runtime::reference_guard::acquire_shared()?;
    let path = crate::config::paths::runtime_dir()?.join("light/deployment.json");
    let before = fs::read(&path)?;
    let selected: LocalDeployment = serde_json::from_slice(&before)?;
    let managed = copy(&selected)?;
    select_if_unchanged(&managed, &before)?;
    status(&managed)
}

fn selection_lock() -> anyhow::Result<fs::File> {
    let root = root()?;
    fs::create_dir_all(&root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".selection.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

pub(crate) fn select_default(deployment: &LocalDeployment) -> anyhow::Result<()> {
    let _runtime_reference = crate::runtime::reference_guard::acquire_shared()?;
    let bundle: StockBundle = serde_json::from_slice(&fs::read(&deployment.bundle_manifest)?)?;
    for (_, file) in artifacts(&bundle) { file.validate()?; }
    let _guard = selection_lock()?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &crate::config::paths::runtime_dir()?.join("light/deployment.json"),
        deployment,
        "native runtime selection",
    )
}

fn select_if_unchanged(managed: &LocalDeployment, before: &[u8]) -> anyhow::Result<()> {
    let _runtime_reference = crate::runtime::reference_guard::acquire_shared()?;
    let _guard = selection_lock()?;
    let path = crate::config::paths::runtime_dir()?.join("light/deployment.json");
    ensure!(
        fs::read(&path)? == before,
        "native selection changed during managed copy"
    );
    let selected: LocalDeployment = serde_json::from_slice(before)?;
    if managed.bundle_manifest != selected.bundle_manifest {
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            managed,
            "managed native selection",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
