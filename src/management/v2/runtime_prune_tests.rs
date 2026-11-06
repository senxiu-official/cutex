use super::*;
use crate::session::model::{CutexSessionArchiveState, CutexSessionRecord, CutexSessionStore};

struct Fixture {
    _root: TestDirectory,
    home: PathBuf,
    layout: crate::upgrade::layout::Layout,
    target: PathBuf,
}
struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("runtime-prune-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn write(path: &Path, value: Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}
impl Fixture {
    fn new() -> Self {
        let root = TestDirectory::new();
        let home = root.path().join("home/runtime");
        let layout = crate::upgrade::layout::Layout::at(root.path().join("install"));
        let target = layout.release_dir("r1").join(if cfg!(windows) {
            "codex-app-server.exe"
        } else {
            "codex-app-server"
        });
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"runtime payload").unwrap();
        write(
            &home.join("native-version-registry-v1.json"),
            json!({"schema":"cutex/native-version-registry/v1",
            "latest_cli":{"version":"0.160.0-r1","recorded_at":"2026-11-04T00:00:00Z"},
            "latest_runtime":{"version":"0.160.0-r1","recorded_at":"2026-11-04T00:00:00Z"},
            "runtime_artifacts":{},"runtime_paths":{(target.to_string_lossy().into_owned()):{"version":"0.160.0-r1","recorded_at":"2026-11-04T00:00:00Z"}}}),
        );
        let operation = root.path().join("kit/operations/current");
        let mut provenance = serde_json::Map::new();
        for (file, key) in [
            ("operation.json", "operationSha256"),
            ("backup-manifest.json", "backupManifestSha256"),
            ("originals-manifest.json", "originalsManifestSha256"),
        ] {
            write(&operation.join(file), json!({"phase":"complete"}));
            provenance.insert(
                key.into(),
                json!(crate::agent_management::file_sha256(&operation.join(file)).unwrap()),
            );
        }
        write(
            &home.join("prune-protection.json"),
            json!({"schema":"cutex/runtime-prune-protection/v1",
            "kitOperation":operation,"activeExecutableSha256":inventory::executable_sha().unwrap(),
            "protectedPaths":[layout.release_dir("previous")],"provenance":provenance}),
        );
        Self {
            _root: root,
            home,
            layout,
            target,
        }
    }
    fn versions(&self, store: &CutexSessionStore, processes: Vec<PathBuf>) -> Vec<Version> {
        inventory::inspect_with_processes(
            &self.home,
            Some(&self.layout),
            store,
            Some(Ok(processes)),
        )
        .unwrap()
        .versions
    }
    fn manifest(&self, name: &str, artifact: &Path) -> PathBuf {
        let path = self.home.join(format!("{name}.json"));
        write(
            &path,
            json!({"executable":{"path":artifact,"sha256":"a".repeat(64)}}),
        );
        path
    }
    fn pin(&self, manifest: PathBuf) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.test".into(),
            None,
            "host".into(),
            self.home.to_string_lossy().into_owned(),
            None,
            "2026-11-04T00:00:00Z".into(),
        )
        .unwrap();
        record.explicit_launch = Some(
            serde_json::from_value(
                json!({"version":4,"native_id":"0189aaaa-0000-7000-8000-000000000001",
            "native_home":self.home,"bundle_manifest":manifest,"bundle_sha256":"e".repeat(64)}),
            )
            .unwrap(),
        );
        record
    }
}

#[test]
fn inspection_is_read_only_and_legacy_candidates_are_files_not_release_dirs() {
    let f = Fixture::new();
    let sentinel = f.target.parent().unwrap().join("cutex.exe");
    std::fs::write(&sentinel, b"backend").unwrap();
    let list = f.versions(&CutexSessionStore::default(), vec![]);
    assert_eq!(list.len(), 1);
    assert!(list[0].can_prune);
    assert_eq!(list[0].bytes, 15);
    assert_eq!(list[0].targets, vec![f.target.clone()]);
    assert!(sentinel.exists());
    assert!(f.target.exists());
}

#[test]
fn selected_manifest_and_retired_pins_are_both_authoritative() {
    let f = Fixture::new();
    let manifest = f.manifest("bundle", &f.target);
    let mut record = f.pin(manifest.clone());
    record.archive_state = CutexSessionArchiveState::Retired;
    let mut store = CutexSessionStore::default();
    store
        .sessions
        .insert(record.cutex_session_id.clone(), record);
    assert!(
        f.versions(&store, vec![])[0]
            .blocked_reasons
            .contains(&"pinned".into())
    );
    write(
        &f.home.join("light/deployment.json"),
        json!({"bundle_manifest":manifest}),
    );
    assert!(
        f.versions(&CutexSessionStore::default(), vec![])[0]
            .blocked_reasons
            .contains(&"selected".into())
    );
}

#[test]
fn missing_or_stale_kit_proof_refuses_without_deleting() {
    let f = Fixture::new();
    let proof = f.home.join("prune-protection.json");
    let mut value = inventory::read_json(&proof).unwrap();
    value["activeExecutableSha256"] = json!("0".repeat(64));
    write(&proof, value);
    assert!(!f.versions(&CutexSessionStore::default(), vec![])[0].can_prune);
    std::fs::remove_file(proof).unwrap();
    assert!(
        f.versions(&CutexSessionStore::default(), vec![])[0]
            .blocked_reasons
            .contains(&"inspection_failed".into())
    );
    assert!(f.target.exists());
}

#[test]
fn inspection_diagnostics_distinguish_rollback_proof_from_process_failures() {
    let f = Fixture::new();
    let list = inventory::inspect_with_processes(&f.home, Some(&f.layout),
        &CutexSessionStore::default(), Some(Err(anyhow::anyhow!("synthetic process failure"))))
        .unwrap().versions;
    assert!(list[0].blocked_reasons.contains(&"process_inspection_failed".into()));
    assert!(!list[0].blocked_reasons.contains(&"rollback_protection_unavailable".into()));
    std::fs::remove_file(f.home.join("prune-protection.json")).unwrap();
    let list = f.versions(&CutexSessionStore::default(), vec![]);
    assert!(list[0].blocked_reasons.contains(&"inspection_failed".into()));
    assert!(list[0].blocked_reasons.contains(&"rollback_protection_unavailable".into()));
    assert!(!list[0].blocked_reasons.contains(&"process_inspection_failed".into()));
    assert!(!list[0].can_prune);
    assert!(f.target.exists());
}

#[test]
fn running_images_or_interpreter_arguments_protect_targets() {
    let f = Fixture::new();
    let list = f.versions(&CutexSessionStore::default(), vec![f.target.clone()]);
    assert_eq!(list[0].blocked_reasons, vec!["running"]);
    assert!(!list[0].can_prune);
}

#[test]
fn rollback_artifact_and_unknown_process_inventory_are_refusals() {
    let f = Fixture::new();
    let proof = f.home.join("prune-protection.json");
    let mut value = inventory::read_json(&proof).unwrap();
    value["protectedPaths"] = json!([f.target]);
    write(&proof, value);
    assert!(
        f.versions(&CutexSessionStore::default(), vec![])[0]
            .blocked_reasons
            .contains(&"rollback".into())
    );
    let list = inventory::inspect_with_processes(
        &f.home,
        Some(&f.layout),
        &CutexSessionStore::default(),
        Some(Err(anyhow::anyhow!("unknown"))),
    )
    .unwrap();
    assert!(!list.versions[0].can_prune);
}

#[test]
fn dangling_pinned_manifest_is_not_an_empty_pin_set() {
    let f = Fixture::new();
    let record = f.pin(f.home.join("missing.json"));
    let mut store = CutexSessionStore::default();
    store
        .sessions
        .insert(record.cutex_session_id.clone(), record);
    assert!(
        f.versions(&store, vec![])[0]
            .blocked_reasons
            .contains(&"inspection_failed".into())
    );
}

#[test]
fn cleanup_routes_are_owner_only_and_ids_are_not_paths() {
    for method in ["GET", "POST"] {
        assert_eq!(
            super::super::access::classify(method, PATH),
            Some(super::super::access::Class::OwnerOnly)
        );
    }
    assert!(execute(&[]).is_err());
    assert!(execute(&["../runtime".into()]).is_err());
    assert!(execute(&["a".repeat(64), "a".repeat(64)]).is_err());
    assert!(execute(&vec!["a".repeat(64); 101]).is_err());
}

#[test]
fn deletion_removes_only_the_resolved_artifacts_and_reports_removed_bytes() {
    let f = Fixture::new();
    let sentinel = f.target.parent().unwrap().join("cutex.exe");
    std::fs::write(&sentinel, b"keep").unwrap();
    let version = f.versions(&CutexSessionStore::default(), vec![]).remove(0);
    let result = delete(&version);
    assert_eq!(result["status"], "deleted");
    assert_eq!(result["freedBytes"], 15);
    assert!(!f.target.exists());
    assert!(sentinel.exists());
}

struct ConfigScope {
    env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    selection: crate::config::instance::Selection,
}
impl ConfigScope {
    fn at(path: &Path, base: &Path) -> Self {
        let mut previous = Vec::new();
        for (name, value) in [("CUTEX_HOME", path), ("CUTEX_INSTALL_BASE", base)] {
            previous.push((name, std::env::var_os(name)));
            unsafe {
                std::env::set_var(name, value);
            }
        }
        let selection = crate::config::instance::selected().unwrap();
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
            path.to_path_buf(),
        ));
        Self {
            env: previous,
            selection,
        }
    }
}
impl Drop for ConfigScope {
    fn drop(&mut self) {
        unsafe {
            for (name, value) in &self.env {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        crate::config::instance::set_test_selection(self.selection.clone());
    }
}

#[test]
fn post_rechecks_a_selection_changed_after_preview() {
    let f = Fixture::new();
    let _scope = ConfigScope::at(f.home.parent().unwrap(), &f.layout.base);
    let version = f.versions(&CutexSessionStore::default(), vec![]).remove(0);
    assert!(version.can_prune);
    let manifest = f.manifest("selected-after-get", &f.target);
    write(
        &f.home.join("light/deployment.json"),
        json!({"bundle_manifest":manifest}),
    );
    let result = execute(&[version.id]).unwrap();
    assert_eq!(result["freedBytes"], 0);
    assert_eq!(result["results"][0]["status"], "refused");
    assert_eq!(result["results"][0]["reason"], "selected");
    assert!(f.target.exists());
}

#[test]
fn owner_action_deletes_and_a_repeated_id_is_not_found() {
    let f = Fixture::new();
    let _scope = ConfigScope::at(f.home.parent().unwrap(), &f.layout.base);
    let version = f.versions(&CutexSessionStore::default(), vec![]).remove(0);
    let result = execute(&[version.id.clone()]).unwrap();
    assert_eq!(result["results"][0]["status"], "deleted");
    assert_eq!(result["freedBytes"], 15);
    let replay = execute(&[version.id]).unwrap();
    assert_eq!(replay["freedBytes"], 0);
    assert_eq!(replay["results"][0]["reason"], "not_found");
}

#[test]
fn contained_links_do_not_expose_or_remove_external_payloads() {
    let root = TestDirectory::new();
    let version = root.path().join("version");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), b"outside user data").unwrap();
    let link = version.join("python-link");
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&outside, &link).is_err() {
        return;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(not(any(windows, unix)))]
    return;
    assert!(inventory::target_bytes(&link).is_err());
    assert!(inventory::target_bytes(&version).is_ok());
    remove(&version).unwrap();
    assert_eq!(
        std::fs::read(outside.join("keep")).unwrap(),
        b"outside user data"
    );
}

#[test]
fn component_and_legacy_provider_versions_are_discovered_and_selected_pins_win() {
    let f = Fixture::new();
    for (kind, name) in [
        ("claude", "claude-runtime"),
        ("dsh", "dsh-runtime"),
        ("kimi", "kimi-runtime"),
    ] {
        let version = format!("{kind}-r1");
        let path = crate::components::version_dir(&f.layout, name, &version);
        write(
            &path.join("receipt.json"),
            json!({"schema":"cutex/component-receipt/v1","name":name,"version":version,
            "origin":"signed","files":{},"unpackedAt":"2026-11-04T00:00:00Z"}),
        );
    }
    let legacy = f.layout.runtimes_dir().join("claude-r12");
    write(
        &legacy.join("runtime-receipt.json"),
        json!({"runtime":"claude-r12"}),
    );
    let source = legacy.join("adapter/src/cute_claude/__main__.py");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"adapter").unwrap();
    let list = f.versions(&CutexSessionStore::default(), vec![]);
    for kind in ["native", "claude", "dsh", "kimi"] {
        assert!(list.iter().any(|v| v.kind == kind && v.can_prune), "{kind}");
    }
    assert!(
        list.iter()
            .any(|v| v.kind == "claude" && v.release == "claude-r12" && v.can_prune)
    );
    write(
        &f.layout.base.join("components/state.json"),
        json!({"kimi-runtime":{"selected":"kimi-r1"}}),
    );
    assert!(
        f.versions(&CutexSessionStore::default(), vec![])
            .iter()
            .find(|v| v.kind == "kimi")
            .unwrap()
            .blocked_reasons
            .contains(&"selected".into())
    );
}

#[test]
fn a_declared_user_home_inside_an_old_runtime_is_protected_without_reading_credentials() {
    let f = Fixture::new();
    let root = f.layout.runtimes_dir().join("claude-r12");
    write(
        &root.join("runtime-receipt.json"),
        json!({"runtime":"claude-r12"}),
    );
    let source = root.join("adapter/src/cute_claude/__main__.py");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"adapter").unwrap();
    let elsewhere = f.layout.runtimes_dir().join("claude-r99");
    let mut record = CutexSessionRecord::new_at(
        "cutex.claude".into(),
        None,
        "host".into(),
        f.home.to_string_lossy().into_owned(),
        None,
        "2026-11-04T00:00:00Z".into(),
    )
    .unwrap();
    record.claude_session=Some(serde_json::from_value(json!({"deployment":{"python":elsewhere.join("python"),"adapter_source":elsewhere.join("adapter"),
        "claude_cli":elsewhere.join("cli"),"tui_binary":elsewhere.join("tui"),"claude_home":root.join("owner-login")},
        "thread_id":"thread","state_dir":f.home.join("agent-state"),"model":"test"})).unwrap());
    let mut store = CutexSessionStore::default();
    store
        .sessions
        .insert(record.cutex_session_id.clone(), record);
    let list = f.versions(&store, vec![]);
    let old = list.iter().find(|v| v.release == "claude-r12").unwrap();
    assert!(old.blocked_reasons.contains(&"pinned".into()));
    assert!(!root.join("owner-login").exists());
}
