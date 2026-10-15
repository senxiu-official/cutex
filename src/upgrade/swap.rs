//! Switching what runs (RFC §4.2 swap, §5.1).
//!
//! - [`activate`]: the services' executable: Linux `current` link, Windows
//!   `bin\cutex.exe` / `bin\cutex-mcp.exe` rename swap; the install record
//!   keeps `previousRelease`.
//! - [`select_runtime`] (full mode): a v4 bundle manifest in
//!   `releases/<r>/manifests/default.json` (new native and / or this release's
//!   facade, the host's native home config), selected for NEW agents with the
//!   same `LocalDeployment::install` as `cutex human install-runtime`
//!   (native home and job MCP descriptor kept). Existing agents keep their
//!   pinned bundle until a runtime-package run moves them.
//! - [`register_native`]: native version registry entries (display only).

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};

use super::layout::{Layout, CUTEX_EXE, FACADE_EXE};
use super::manifest::ReleaseManifest;
use super::stage::{NATIVE_APP_SERVER, NATIVE_CLI, NATIVE_CODE_MODE_HOST, NATIVE_SCHEMA};
use crate::launch::stock::{StockBundle, VerifiedFile};

/// Canonical spelling for bundle paths (no verbatim prefix on Windows).
fn canonical(path: &Path) -> anyhow::Result<PathBuf> {
    let resolved = path.canonicalize().with_context(|| format!("resolve {}", path.display()))?;
    Ok(PathBuf::from(crate::agent_management::strip_verbatim_prefix(&resolved.to_string_lossy())))
}

fn verified(path: &Path) -> anyhow::Result<VerifiedFile> {
    let path = canonical(path)?;
    Ok(VerifiedFile { sha256: crate::agent_management::file_sha256(&path)?, path })
}

/// Point the services at `release`. Returns the release that ran before.
pub fn activate(layout: &Layout, release: &str) -> anyhow::Result<String> {
    super::layout::ensure_label(release, "release")?;
    let mut record = layout.read_install()?;
    let previous = layout.active_release().unwrap_or_else(|_| record.active_release.clone());
    ensure!(layout.release_executable(release).is_file(), "release {release} is not installed");
    if cfg!(windows) {
        let tag = super::layout::valid_label(&previous).then_some(previous.as_str()).unwrap_or("previous");
        let mut aside = Vec::new();
        for exe in [CUTEX_EXE, FACADE_EXE] {
            aside.extend(super::layout::rename_swap(&layout.release_dir(release).join(exe), &layout.bin_dir().join(exe), tag)?);
        }
        super::layout::prune_swap_leftovers(&layout.bin_dir(), &aside);
    } else {
        #[cfg(unix)]
        super::layout::point_current(layout, release)?;
    }
    if previous != release {
        record.previous_release = Some(previous.clone());
    }
    record.active_release = release.to_string();
    layout.write_install(&record)?;
    Ok(previous)
}

/// Native files to pin: the release's native (`natives/<version>`) or the
/// currently selected bundle's.
struct NativeFiles {
    executable: VerifiedFile,
    cli: Option<VerifiedFile>,
    code_mode_host: VerifiedFile,
    schema: VerifiedFile,
    upstream_commit: String,
    native_patch_commit: Option<String>,
}

fn native_from_dir(dir: &Path, commit: &str, patch: Option<&str>) -> anyhow::Result<NativeFiles> {
    Ok(NativeFiles {
        executable: verified(&dir.join(NATIVE_APP_SERVER))?,
        cli: dir.join(NATIVE_CLI).is_file().then(|| verified(&dir.join(NATIVE_CLI))).transpose()?,
        code_mode_host: verified(&dir.join(NATIVE_CODE_MODE_HOST))?,
        schema: verified(&dir.join(NATIVE_SCHEMA))?,
        upstream_commit: commit.to_string(),
        native_patch_commit: patch.map(str::to_string),
    })
}

fn native_from_bundle(bundle: &StockBundle) -> NativeFiles {
    NativeFiles {
        executable: bundle.executable.clone(),
        cli: bundle.cli.clone(),
        code_mode_host: bundle.code_mode_host.clone(),
        schema: bundle.schema.clone(),
        upstream_commit: bundle.upstream_commit.clone(),
        native_patch_commit: bundle.native_patch_commit.clone(),
    }
}

/// The native home new agents start from (kept across upgrades); its
/// `config.toml` is pinned as the bundle's shared config (created empty on
/// a fresh install).
fn native_home(selected: Option<&crate::launch::local_deployment::LocalDeployment>) -> anyhow::Result<PathBuf> {
    let home = match selected {
        Some(deployment) => deployment.native_home.clone(),
        None => crate::config::paths::host_codex_home_dir()?,
    };
    if !home.is_dir() {
        std::fs::create_dir_all(&home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let config = home.join("config.toml");
    if !config.is_file() {
        std::fs::write(&config, b"")?;
    }
    canonical(&home)
}

/// Write `releases/<release>/manifests/default.json`; returns its path.
fn write_bundle(layout: &Layout, release: &str, native: NativeFiles, home: &Path) -> anyhow::Result<PathBuf> {
    let bundle = StockBundle {
        version: 4,
        upstream_commit: native.upstream_commit,
        native_patch_commit: native.native_patch_commit,
        executable: native.executable,
        cli: native.cli,
        code_mode_host: native.code_mode_host,
        facade: verified(&layout.release_dir(release).join(FACADE_EXE))?,
        schema: native.schema,
        shared_config: verified(&home.join("config.toml"))?,
        launch_config_sha256: None,
    };
    let path = layout.release_dir(release).join("manifests").join("default.json");
    crate::config::atomic::write_pretty_json_atomic(&path, &bundle, "runtime bundle manifest")?;
    canonical(&path)
}

/// Full mode: select this release's runtime package for new agents.
/// Returns false when there is nothing to select (no native anywhere).
pub fn select_runtime(layout: &Layout, release: &str, manifest: &ReleaseManifest) -> anyhow::Result<bool> {
    use crate::launch::local_deployment::LocalDeployment;
    let selected = LocalDeployment::selected()?;
    let native = match &manifest.native {
        Some(part) => {
            let dir = layout.native_dir(&part.version);
            ensure!(dir.join(NATIVE_APP_SERVER).is_file(), "native {} is not staged", part.version);
            native_from_dir(&dir, &part.commit, part.native_patch_commit.as_deref())?
        }
        None => match &selected {
            Some(deployment) => {
                let bundle: StockBundle = serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)
                    .context("selected runtime bundle unreadable")?;
                native_from_bundle(&bundle)
            }
            None => return Ok(false),
        },
    };
    let home = native_home(selected.as_ref())?;
    let manifest_path = write_bundle(layout, release, native, &home)?;
    LocalDeployment {
        native_home: home,
        bundle_manifest: manifest_path,
        job_mcp: selected.and_then(|deployment| deployment.job_mcp),
    }
    .install()?;
    if let Some(part) = &manifest.native {
        register_native(&part.version, &layout.native_dir(&part.version))?;
    }
    Ok(true)
}

/// Record `version` for the staged native files in the version registry
/// and make it the latest (display metadata; a failure is not fatal for
/// launches, but the caller reports it).
pub fn register_native(version: &str, dir: &Path) -> anyhow::Result<()> {
    use crate::launch::version_registry::{NativeVersionRecord, NativeVersionRegistry, REGISTRY_SCHEMA};
    let path = crate::launch::version_registry::registry_path()?;
    let record = NativeVersionRecord {
        version: version.to_string(),
        recorded_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M%:z").to_string(),
    };
    let mut registry = crate::launch::version_registry::load(&path).ok().flatten().unwrap_or_else(|| NativeVersionRegistry {
        schema: REGISTRY_SCHEMA.into(),
        latest_cli: record.clone(),
        latest_runtime: record.clone(),
        runtime_artifacts: Default::default(),
        runtime_paths: Default::default(),
    });
    let server = canonical(&dir.join(NATIVE_APP_SERVER))?;
    registry.runtime_artifacts.insert(super::manifest::file_sha256_hex(&server)?, record.clone());
    registry.runtime_paths.insert(server.to_string_lossy().into_owned(), record.clone());
    registry.latest_runtime = record.clone();
    if dir.join(NATIVE_CLI).is_file() {
        registry.latest_cli = record;
    }
    crate::config::atomic::write_private_pretty_json_atomic(&path, &registry, "native version registry")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::upgrade::stage::fixtures::installed_layout;

    #[test]
    fn activate_switches_current_and_records_previous() {
        let layout = installed_layout("r239", 40);
        let other = installed_layout("r240", 41);
        // Install r240 next to r239 in the same base.
        std::fs::create_dir_all(layout.release_dir("r240")).unwrap();
        for exe in [CUTEX_EXE, FACADE_EXE] {
            std::fs::copy(other.release_dir("r240").join(exe), layout.release_dir("r240").join(exe)).unwrap();
        }
        assert_eq!(activate(&layout, "r240").unwrap(), "r239");
        assert_eq!(layout.active_release().unwrap(), "r240");
        let record = layout.read_install().unwrap();
        assert_eq!((record.active_release.as_str(), record.previous_release.as_deref()), ("r240", Some("r239")));
        assert_eq!(activate(&layout, "r239").unwrap(), "r240");
        assert_eq!(layout.read_install().unwrap().previous_release.as_deref(), Some("r240"));
        assert!(activate(&layout, "r999").is_err());
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&other.base).unwrap();
    }

    #[test]
    fn generated_bundle_passes_the_stock_reference_checks() {
        let layout = installed_layout("r240", 41);
        let dir = layout.native_dir("0.162.1-r53");
        std::fs::create_dir_all(&dir).unwrap();
        for (file, data) in [(NATIVE_APP_SERVER, "srv"), (NATIVE_CLI, "cli"), (NATIVE_CODE_MODE_HOST, "host"), (NATIVE_SCHEMA, "{}")] {
            std::fs::write(dir.join(file), data).unwrap();
        }
        let home = layout.base.join("native-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "model = \"x\"\n").unwrap();
        let home = canonical(&home).unwrap();
        let native = native_from_dir(&dir, "a956835d020762cb2b570053af06f643a11c0ecc", None).unwrap();
        let path = write_bundle(&layout, "r240", native, &home).unwrap();
        let digest = crate::agent_management::file_sha256(&path).unwrap();
        let bundle = StockBundle::load_references(&home, &path, &digest).unwrap();
        assert_eq!(bundle.version, 4);
        assert!(bundle.soon_ingress());
        assert!(bundle.facade.path.ends_with("releases/r240/cutex-mcp"));
        assert!(bundle.executable.path.ends_with("natives/0.162.1-r53/codex-app-server"));
        std::fs::remove_dir_all(&layout.base).unwrap();
    }
}
