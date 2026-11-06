use super::*;
use crate::agent_management::{file_sha256, ExplicitLaunchContract};
use crate::role_revision::Sha256 as FileHash;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};
use serde_json::json;

struct Fixture {
    _home: crate::profiles::lifecycle::tests::TestHome,
    deployment: LocalDeployment,
    bundle: StockBundle,
    original: Vec<u8>,
}

fn fixture() -> Fixture {
    let home = crate::profiles::lifecycle::tests::TestHome::new();
    let scratch = home.root().join("agent-tmp/Kittiwake/native-r58/linux-x64");
    fs::create_dir_all(&scratch).unwrap();
    let native_home = home.root().join("native-home");
    fs::create_dir_all(&native_home).unwrap();
    let make = |name: &str| {
        let path = scratch.join(name);
        fs::write(&path, format!("public native fixture {name}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        VerifiedFile {
            path: path.canonicalize().unwrap(),
            sha256: file_sha256(&path).unwrap(),
        }
    };
    let bundle = StockBundle {
        version: 4,
        upstream_commit: "upstream".into(),
        native_patch_commit: Some("r58-fixture".into()),
        executable: make("codex-app-server"),
        cli: Some(make("codex")),
        code_mode_host: make("codex-code-mode-host"),
        facade: make("cutex-mcp"),
        schema: make("schema.json"),
        shared_config: VerifiedFile {
            path: native_home.join("config-not-readable-or-copied.toml"),
            sha256: FileHash::new("a".repeat(64)).unwrap(),
        },
        launch_config_sha256: None,
    };
    let manifest = home.root().join("selected-bundle.json");
    let original = serde_json::to_vec_pretty(&bundle).unwrap();
    fs::write(&manifest, &original).unwrap();
    let deployment = LocalDeployment {
        native_home: native_home.canonicalize().unwrap(),
        bundle_manifest: manifest.canonicalize().unwrap(),
        job_mcp: None,
    };
    let version = json!({"version":"0.164.0","recorded_at":"2026-10-29T00:00:00Z"});
    let registry = json!({"schema":super::super::version_registry::REGISTRY_SCHEMA,
        "latest_cli":{"version":"99.0.0","recorded_at":"2026-10-29T00:00:00Z"},
        "latest_runtime":{"version":"99.0.0","recorded_at":"2026-10-29T00:00:00Z"},
        "runtime_artifacts":{bundle.executable.sha256.as_str():version.clone(),bundle.cli.as_ref().unwrap().sha256.as_str():version.clone()},
        "runtime_paths":{bundle.executable.path.to_str().unwrap():version}});
    crate::config::atomic::write_private_pretty_json_atomic(
        &super::super::version_registry::registry_path().unwrap(),
        &registry,
        "test registry",
    )
    .unwrap();
    Fixture {
        _home: home,
        deployment,
        bundle,
        original,
    }
}

#[test]
fn scratch_registry_copy_keeps_old_paths_and_adds_managed_aliases() {
    let f = fixture();
    let before = super::super::version_registry::load(
        &super::super::version_registry::registry_path().unwrap(),
    )
    .unwrap()
    .unwrap();
    let managed = copy(&f.deployment).unwrap();
    let next: StockBundle =
        serde_json::from_slice(&fs::read(&managed.bundle_manifest).unwrap()).unwrap();
    assert!(status(&managed).unwrap().managed);
    assert!(!status(&f.deployment).unwrap().managed);
    assert_eq!(fs::read(&f.deployment.bundle_manifest).unwrap(), f.original);
    for ((role, old), (new_role, new)) in artifacts(&f.bundle).into_iter().zip(artifacts(&next)) {
        assert_eq!(role, new_role);
        assert_eq!(old.sha256, new.sha256);
        assert_ne!(old.path, new.path);
        assert_eq!(fs::read(&old.path).unwrap(), fs::read(&new.path).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&old.path).unwrap().permissions().mode(),
                fs::metadata(&new.path).unwrap().permissions().mode()
            );
        }
    }
    assert_eq!(next.shared_config, f.bundle.shared_config);
    let after = super::super::version_registry::load(
        &super::super::version_registry::registry_path().unwrap(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(after.latest_runtime, before.latest_runtime);
    assert_eq!(after.latest_cli, before.latest_cli);
    for path in [
        &f.bundle.executable.path,
        &next.executable.path,
        &f.bundle.cli.as_ref().unwrap().path,
        &next.cli.as_ref().unwrap().path,
    ] {
        assert_eq!(
            after.runtime_paths[path.to_str().unwrap()].version,
            "0.164.0"
        );
    }
}

#[test]
fn copying_is_idempotent_and_concurrent_publishers_share_verified_bytes() {
    let f = fixture();
    let a = f.deployment.clone();
    let b = a.clone();
    let x = std::thread::spawn(move || copy(&a).unwrap());
    let y = std::thread::spawn(move || copy(&b).unwrap());
    let a = x.join().unwrap();
    let b = y.join().unwrap();
    assert_eq!(a.bundle_manifest, b.bundle_manifest);
    assert_eq!(copy(&a).unwrap().bundle_manifest, a.bundle_manifest);
    assert_eq!(fs::read(&f.deployment.bundle_manifest).unwrap(), f.original);
}

#[test]
fn corrupt_source_never_publishes_a_copy() {
    let f = fixture();
    fs::write(&f.bundle.executable.path, b"changed").unwrap();
    assert!(copy(&f.deployment).is_err());
    assert!(!root().unwrap().exists());
    assert_eq!(fs::read(&f.deployment.bundle_manifest).unwrap(), f.original);
}

#[test]
fn corrupt_existing_managed_copy_is_refused_not_overwritten() {
    let f = fixture();
    let managed = copy(&f.deployment).unwrap();
    let b: StockBundle =
        serde_json::from_slice(&fs::read(&managed.bundle_manifest).unwrap()).unwrap();
    fs::write(&b.executable.path, b"corrupt cache").unwrap();
    assert!(copy(&f.deployment).is_err());
    assert_eq!(fs::read(&b.executable.path).unwrap(), b"corrupt cache");
    f.bundle.executable.validate().unwrap();
}

#[test]
fn readonly_status_does_not_copy_create_directories_or_read_shared_config() {
    let f = fixture();
    let registry = fs::read(super::super::version_registry::registry_path().unwrap()).unwrap();
    assert!(!status(&f.deployment).unwrap().managed);
    assert!(!root().unwrap().exists());
    assert_eq!(
        fs::read(super::super::version_registry::registry_path().unwrap()).unwrap(),
        registry
    );
}

#[test]
fn unknown_artifact_never_inherits_registry_latest_version() {
    let f = fixture();
    let path = super::super::version_registry::registry_path().unwrap();
    let mut registry = super::super::version_registry::load(&path)
        .unwrap()
        .unwrap();
    registry.runtime_artifacts.clear();
    crate::config::atomic::write_private_pretty_json_atomic(&path, &registry, "unknown fixture")
        .unwrap();
    let before = fs::read(&path).unwrap();
    let managed = copy(&f.deployment).unwrap();
    assert!(status(&managed).unwrap().managed);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(managed
        .bundle_manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .any(|p| p
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("unknown-")));
}

#[test]
fn selecting_managed_defaults_preserves_old_bundle_and_session_pin_bytes() {
    let f = fixture();
    let path = crate::config::paths::runtime_dir()
        .unwrap()
        .join("light/deployment.json");
    crate::config::atomic::write_private_pretty_json_atomic(&path, &f.deployment, "old selection")
        .unwrap();
    let contract = ExplicitLaunchContract {
        version: 4,
        migration_action_id: None,
        native_id: uuid::Uuid::new_v4().to_string(),
        native_home: f.deployment.native_home.clone(),
        bundle_manifest: f.deployment.bundle_manifest.clone(),
        bundle_sha256: file_sha256(&f.deployment.bundle_manifest).unwrap(),
    };
    let pinned = serde_json::to_vec(&contract).unwrap();
    let pin_file = crate::config::paths::runtime_dir()
        .unwrap()
        .join("old-session-pin.json");
    fs::write(&pin_file, &pinned).unwrap();
    let mut record = CutexSessionRecord::new_at(
        "old".into(),
        None,
        crate::platform::host::current_host_name(),
        "/work".into(),
        None,
        "2026-10-29T00:00:00Z".into(),
    )
    .unwrap();
    record.runtime_backend = CutexSessionRuntimeBackend::Host;
    record.explicit_launch = Some(contract);
    assert!(manage_selected().unwrap().managed);
    assert_eq!(fs::read(&pin_file).unwrap(), pinned);
    assert_eq!(fs::read(&f.deployment.bundle_manifest).unwrap(), f.original);
    assert!(
        !LocalDeployment::selected_package_differs(&record),
        "a byte-identical relocation is not a runtime upgrade"
    );
    let selected = LocalDeployment::selected().unwrap().unwrap();
    assert_ne!(selected.bundle_manifest, f.deployment.bundle_manifest);
    assert!(status(&selected).unwrap().managed);
}

#[test]
fn frontend_only_version_is_recorded_from_its_own_sha() {
    let f = fixture();
    let path = super::super::version_registry::registry_path().unwrap();
    let mut registry = super::super::version_registry::load(&path)
        .unwrap()
        .unwrap();
    registry
        .runtime_artifacts
        .get_mut(f.bundle.cli.as_ref().unwrap().sha256.as_str())
        .unwrap()
        .version = "0.165.0-cli".into();
    crate::config::atomic::write_private_pretty_json_atomic(&path, &registry, "frontend-only")
        .unwrap();
    let managed = copy(&f.deployment).unwrap();
    let bundle: StockBundle =
        serde_json::from_slice(&fs::read(managed.bundle_manifest).unwrap()).unwrap();
    let after = super::super::version_registry::load(&path)
        .unwrap()
        .unwrap();
    assert_eq!(
        after.runtime_paths[bundle.executable.path.to_str().unwrap()].version,
        "0.164.0"
    );
    assert_eq!(
        after.runtime_paths[bundle.cli.unwrap().path.to_str().unwrap()].version,
        "0.165.0-cli"
    );
    assert_eq!(after.latest_cli, registry.latest_cli);
}

#[test]
fn unknown_frontend_never_receives_the_backend_version() {
    let f = fixture();
    let path = super::super::version_registry::registry_path().unwrap();
    let mut registry = super::super::version_registry::load(&path)
        .unwrap()
        .unwrap();
    registry
        .runtime_artifacts
        .remove(f.bundle.cli.as_ref().unwrap().sha256.as_str());
    crate::config::atomic::write_private_pretty_json_atomic(&path, &registry, "unknown CLI")
        .unwrap();
    let managed = copy(&f.deployment).unwrap();
    let bundle: StockBundle =
        serde_json::from_slice(&fs::read(managed.bundle_manifest).unwrap()).unwrap();
    let after = super::super::version_registry::load(&path)
        .unwrap()
        .unwrap();
    assert!(!after
        .runtime_artifacts
        .contains_key(bundle.cli.as_ref().unwrap().sha256.as_str()));
    assert!(!after
        .runtime_paths
        .contains_key(bundle.cli.unwrap().path.to_str().unwrap()));
}

#[test]
fn malformed_display_registry_preserved_without_blocking_runtime_copy_or_selection() {
    let f = fixture();
    let path = super::super::version_registry::registry_path().unwrap();
    fs::write(&path, b"broken display registry").unwrap();
    select_default(&f.deployment).unwrap();
    let result = manage_selected().unwrap();
    assert!(result.managed);
    assert!(!result.registry_metadata_available);
    assert_eq!(fs::read(&path).unwrap(), b"broken display registry");
    assert_eq!(fs::read(f.deployment.bundle_manifest).unwrap(), f.original);
}

#[test]
fn near_limit_registry_never_becomes_unreadable_due_to_pretty_serialization() {
    let f = fixture();
    let path = super::super::version_registry::registry_path().unwrap();
    let mut registry = super::super::version_registry::load(&path)
        .unwrap()
        .unwrap();
    let record = registry.latest_runtime.clone();
    while serde_json::to_vec(&registry).unwrap().len() < 62 * 1024 {
        let key = f
            ._home
            .root()
            .join(format!("old-display-path-{}", registry.runtime_paths.len()))
            .to_string_lossy()
            .into_owned();
        registry.runtime_paths.insert(key, record.clone());
    }
    let before = serde_json::to_vec(&registry).unwrap();
    assert!(before.len() < 64 * 1024);
    fs::write(&path, &before).unwrap();
    let managed = copy(&f.deployment).unwrap();
    assert!(status(&managed).unwrap().managed);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(super::super::version_registry::load(&path).is_ok());
    assert_eq!(
        status(&managed).unwrap().known_version_paths_recorded,
        Some(false)
    );
}

#[test]
fn stale_managed_default_cannot_overwrite_a_newer_install_selection() {
    let f = fixture();
    select_default(&f.deployment).unwrap();
    let path = crate::config::paths::runtime_dir()
        .unwrap()
        .join("light/deployment.json");
    let before = fs::read(&path).unwrap();
    let managed = copy(&f.deployment).unwrap();
    let mut newer = f.deployment.clone();
    let new_manifest = f._home.root().join("newer.json");
    fs::write(&new_manifest, &f.original).unwrap();
    newer.bundle_manifest = new_manifest.canonicalize().unwrap();
    select_default(&newer).unwrap();
    let current = fs::read(&path).unwrap();
    assert!(select_if_unchanged(&managed, &before).is_err());
    assert_eq!(fs::read(&path).unwrap(), current);
}

#[test]
fn selection_lock_excludes_other_default_writers() {
    let _f = fixture();
    let guard = selection_lock().unwrap();
    let other = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root().unwrap().join(".selection.lock"))
        .unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&other).is_err());
    drop(guard);
    fs2::FileExt::try_lock_exclusive(&other).unwrap();
}
