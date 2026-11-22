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
    assert!(f.versions(&store, vec![])[0]
        .blocked_reasons
        .contains(&"pinned".into()));
    write(
        &f.home.join("light/deployment.json"),
        json!({"bundle_manifest":manifest}),
    );
    assert!(f.versions(&CutexSessionStore::default(), vec![])[0]
        .blocked_reasons
        .contains(&"selected".into()));
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
    assert!(f.versions(&CutexSessionStore::default(), vec![])[0]
        .blocked_reasons
        .contains(&"inspection_failed".into()));
    assert!(f.target.exists());
}

#[test]
fn unresolved_claude_plan_has_bounded_explanation_without_changing_other_runtime_items() {
    let f = Fixture::new();
    let adapter = f.layout.runtimes_dir().join("claude-r50");
    std::fs::create_dir_all(adapter.join("src/cute_claude")).unwrap();
    std::fs::write(adapter.join("src/cute_claude/__main__.py"), b"").unwrap();
    write(&adapter.join("runtime-receipt.json"), json!({"runtime":"claude-r50"}));
    let unresolved = PathBuf::from(super::super::runtime_prune_processes::UNKNOWN_CLAUDE);
    let versions = f.versions(&CutexSessionStore::default(), vec![unresolved; 56]);
    let value = serde_json::to_value(&versions).unwrap();
    let claude = value.as_array().unwrap().iter().find(|item| item["kind"] == "claude").unwrap();
    assert_eq!(claude["canPrune"], false);
    assert!(claude["blockedReasons"].as_array().unwrap().contains(&json!("claude_process_unresolved")));
    assert_eq!(claude["claudeProcessInspection"]["unresolvedProcessCount"], 56);
    assert_eq!(claude["claudeProcessInspection"]["processes"].as_array().unwrap().len(), 20);
    assert!(claude["claudeProcessInspection"]["message"].as_str().unwrap().contains("cleanup waits"));
    for item in value.as_array().unwrap().iter().filter(|item| item["kind"] != "claude") {
        assert!(item.get("claudeProcessInspection").is_none());
    }
    assert!(f.versions(&CutexSessionStore::default(), vec![]).iter()
        .all(|item| item.claude_process_inspection.is_none()));
}

#[test]
fn inspection_diagnostics_distinguish_rollback_proof_from_process_failures() {
    let f = Fixture::new();
    let list = inventory::inspect_with_processes(
        &f.home,
        Some(&f.layout),
        &CutexSessionStore::default(),
        Some(Err(anyhow::anyhow!("synthetic process failure"))),
    )
    .unwrap()
    .versions;
    assert!(list[0]
        .blocked_reasons
        .contains(&"process_inspection_failed".into()));
    assert!(!list[0]
        .blocked_reasons
        .contains(&"rollback_protection_unavailable".into()));
    std::fs::remove_file(f.home.join("prune-protection.json")).unwrap();
    let list = f.versions(&CutexSessionStore::default(), vec![]);
    assert!(list[0]
        .blocked_reasons
        .contains(&"inspection_failed".into()));
    assert!(list[0]
        .blocked_reasons
        .contains(&"rollback_protection_unavailable".into()));
    assert!(!list[0]
        .blocked_reasons
        .contains(&"process_inspection_failed".into()));
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
    assert!(f.versions(&CutexSessionStore::default(), vec![])[0]
        .blocked_reasons
        .contains(&"rollback".into()));
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
    assert!(f.versions(&store, vec![])[0]
        .blocked_reasons
        .contains(&"inspection_failed".into()));
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
    let _processes = super::super::runtime_prune_processes::test_inspection(Ok(vec![]));
    let f = Fixture::new();
    let sentinel = f.target.parent().unwrap().join("cutex.exe");
    std::fs::write(&sentinel, b"keep").unwrap();
    let version = f.versions(&CutexSessionStore::default(), vec![]).remove(0);
    let result = delete(&version);
    assert_eq!(result["status"], "deleted", "{result}");
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
    let _processes = super::super::runtime_prune_processes::test_inspection(Ok(vec![]));
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
    assert_eq!(result["results"][0]["reason"], "selected", "{result}");
    assert!(f.target.exists());
}

#[test]
fn owner_action_deletes_and_a_repeated_id_is_not_found() {
    let _processes = super::super::runtime_prune_processes::test_inspection(Ok(vec![]));
    let f = Fixture::new();
    let _scope = ConfigScope::at(f.home.parent().unwrap(), &f.layout.base);
    let version = f.versions(&CutexSessionStore::default(), vec![]).remove(0);
    let result = execute(&[version.id.clone()]).unwrap();
    assert_eq!(result["results"][0]["status"], "deleted", "{result}");
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
    assert!(list
        .iter()
        .any(|v| v.kind == "claude" && v.release == "claude-r12" && v.can_prune));
    write(
        &f.layout.base.join("components/state.json"),
        json!({"kimi-runtime":{"selected":"kimi-r1"}}),
    );
    assert!(f
        .versions(&CutexSessionStore::default(), vec![])
        .iter()
        .find(|v| v.kind == "kimi")
        .unwrap()
        .blocked_reasons
        .contains(&"selected".into()));
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

fn scale_fixture_600_30() -> (Fixture, CutexSessionStore) {
    let f = Fixture::new();
    let registry_path = f.home.join("native-version-registry-v1.json");
    let mut registry = inventory::read_json(&registry_path).unwrap();
    let mut manifests = Vec::new();
    for n in 0..30 {
        let target = f
            .layout
            .release_dir(&format!("r{}", n + 1))
            .join(if cfg!(windows) {
                "codex-app-server.exe"
            } else {
                "codex-app-server"
            });
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"runtime payload").unwrap();
        registry["runtime_paths"][target.to_string_lossy().as_ref()] =
            json!({"version":format!("0.160.0-r{}",n+1),"recorded_at":"2026-11-04T00:00:00Z"});
        manifests.push(f.manifest(&format!("scale-{n}"), &target));
    }
    write(&registry_path, registry);
    let mut store = CutexSessionStore::default();
    for n in 0..600 {
        let mut record = f.pin(manifests[n % 30].clone());
        record.cutex_session_id = format!("cutex.{}", uuid::Uuid::new_v4());
        record.explicit_launch.as_mut().unwrap().native_id = uuid::Uuid::new_v4().to_string();
        record.explicit_launch.as_mut().unwrap().native_home =
            f.home.join(format!("session-home-{n}"));
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
    }
    (f, store)
}
#[test]
fn runtime_prune_scale_inventory_600_30() {
    let (f, store) = scale_fixture_600_30();
    let start = std::time::Instant::now();
    let versions = f.versions(&store, vec![]);
    let seconds = start.elapsed().as_secs_f64();
    assert_eq!(versions.len(), 30);
    println!("PRUNE_SCALE_INVENTORY {{\"sessions\":600,\"runtimeDirs\":30,\"seconds\":{seconds}}}");
}
#[test]
fn runtime_prune_scale_http_600_30() {
    use std::io::{Read, Write};
    let (f, store) = scale_fixture_600_30();
    // Real installations include dependency trees, not just 30 standalone
    // executables. Keep the same version count, but put r1 in a managed tree.
    let sdk = f.home.join("native/sdk-r1");
    std::fs::create_dir_all(&sdk).unwrap();
    let executable = sdk.join(f.target.file_name().unwrap());
    std::fs::write(&executable, b"runtime payload").unwrap();
    let registry_path = f.home.join("native-version-registry-v1.json");
    let mut registry = inventory::read_json(&registry_path).unwrap();
    let paths = registry["runtime_paths"].as_object_mut().unwrap();
    let record = paths.remove(f.target.to_string_lossy().as_ref()).unwrap();
    paths.insert(executable.to_string_lossy().into_owned(), record);
    write(&registry_path, registry);
    for directory in 0..64 {
        let path = sdk.join(format!("dependency-{directory}"));
        std::fs::create_dir(&path).unwrap();
        for file in 0..512 {
            std::fs::write(path.join(format!("library-{file}.py")), b"").unwrap();
        }
    }
    let _config = ConfigScope::at(f.home.parent().unwrap(), &f.layout.base);
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(120)))
            .unwrap();
        stream
            .write_all(
                b"GET /v2/runtimes/prune HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (mut stream, _) = listener.accept().unwrap();
    let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
    let permit = crate::management::v2::control_capacity::admit(
        crate::management::v2::control_capacity::WorkClass::HeavyRead,
    )
    .unwrap();
    let _capacity = crate::management::v2::control_capacity::response_permit_scope(permit);
    let start = std::time::Instant::now();
    handle(&mut stream, &request).unwrap();
    let seconds = start.elapsed().as_secs_f64();
    drop(stream);
    let response = client.join().unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let value: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(value["versions"].as_array().unwrap().len(), 30);
    println!("PRUNE_SCALE_HTTP {{\"method\":\"GET\",\"path\":\"/v2/runtimes/prune\",\"sessions\":600,\"runtimeDirs\":30,\"dependencyFiles\":32768,\"seconds\":{seconds}}}");
}

#[test]
fn runtime_prune_snapshot_inspection_releases_writer_lock_and_rejects_changed_authority() {
    let root = TestDirectory::new();
    let path = root.path().join("sessions.json");
    crate::session::store::save_cutex_session_store_to_path(&path, &CutexSessionStore::default())
        .unwrap();
    let worker_path = path.clone();
    let result = inspect_snapshot(&path, |_| {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            crate::session::store::with_locked_session_store(&worker_path, |store| {
                let record = CutexSessionRecord::new_at(
                    "cutex.writer".into(),
                    None,
                    "host".into(),
                    "/fixture".into(),
                    None,
                    "2026-11-04T00:00:00Z".into(),
                )
                .unwrap();
                store
                    .sessions
                    .insert(record.cutex_session_id.clone(), record);
                crate::session::store::save_locked_session_store(&worker_path, store)
            })
            .unwrap();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .expect("inspection must not hold session writer lock");
        worker.join().unwrap();
        Ok(())
    });
    assert!(
        result.is_err(),
        "changed session authority must refuse before any rename"
    );
}
#[test]
fn runtime_prune_scale_normalizes_each_reference_once() {
    let (f, store) = scale_fixture_600_30();
    let versions = f.versions(&store, vec![]);
    assert_eq!(versions.len(), 30);
    let count = inventory::budget::last_normalizations();
    println!("PRUNE_NORMALIZATIONS {count}");
    assert!(
        count < 4000,
        "must not canonicalize per target/reference pair: {count}"
    );
}

#[test]
fn runtime_prune_reference_projection_preserves_pins_and_ignores_activity_churn() {
    let (f, mut store) = scale_fixture_600_30();
    let id = store.sessions.keys().next().unwrap().clone();
    let mut record = store.sessions.get(&id).unwrap().clone();
    let adapter = f.layout.runtimes_dir().join("claude-r12");
    record.claude_session = Some(serde_json::from_value(json!({
        "deployment":{"python":adapter.join("python"),"adapter_source":adapter.join("adapter"),
            "claude_cli":adapter.join("cli"),"tui_binary":adapter.join("tui"),"claude_home":f.home.join("claude-login")},
        "thread_id":"fixture-claude","state_dir":f.home.join("claude-state"),"model":"test"
    })).unwrap());
    record.dsh_session = Some(serde_json::from_value(json!({
        "thread_id":"fixture-dsh","state_dir":f.home.join("dsh-state"),
        "node":f.home.join("node"),"adapter":f.home.join("dsh/adapter"),"dsh_cli":f.home.join("dsh/cli"),
        "patch":f.home.join("dsh/patch"),"install_root":f.home.join("dsh/install")
    })).unwrap());
    record.profile_binding = Some(serde_json::from_value(json!({
        "profileId":"fixture-profile","variantId":"fixture-variant","runtime":"dsh",
        "credential":{"source":"env","envName":"FIXTURE_KEY"},
        "runtimePackageVersion":"dsh-r3","boundAt":"2026-11-15T00:00:00Z"
    })).unwrap());
    store.sessions.insert(id.clone(), record);
    let path = f.home.join("reference-fixture.json");
    crate::session::store::save_cutex_session_store_to_path(&path, &store).unwrap();
    let projected = crate::session::store::runtime_references_at(&path).unwrap();
    assert_eq!(projected, crate::session::store::runtime_references_from_store(&store));
    let expected = inventory::inspect_with_processes(&f.home, Some(&f.layout), &store, Some(Ok(vec![]))).unwrap();
    let actual = inventory::inspect_reference_snapshot(&f.home, Some(&f.layout), &projected, Some(Ok(vec![]))).unwrap();
    assert_eq!(serde_json::to_value(actual.versions).unwrap(), serde_json::to_value(expected.versions).unwrap());
    inspect_snapshot(&path, |_| {
        crate::session::store::update_session_row(&path, &id, |row| {
            row.note = Some("activity does not change references".into());
            Ok(())
        })?;
        Ok(())
    }).expect("unrelated metadata churn must not invalidate authority");
}

#[test]
fn post_stage_running_process_evidence_restores_the_target() {
    let f = Fixture::new();
    let version = f.versions(&CutexSessionStore::default(), vec![]).remove(0);
    let _processes = super::super::runtime_prune_processes::test_inspection(Ok(vec![f.target.clone()]));
    let result = delete(&version);
    assert_eq!(result["status"], "refused", "{result}");
    assert_eq!(result["reason"], "running", "{result}");
    assert_eq!(result["freedBytes"], 0);
    assert!(f.target.exists());
}

#[test]
fn post_stage_incomplete_process_inventory_restores_the_target() {
    let f = Fixture::new();
    let version = f.versions(&CutexSessionStore::default(), vec![]).remove(0);
    let _processes = super::super::runtime_prune_processes::test_inspection(Err("fixture inventory unavailable".into()));
    let result = delete(&version);
    assert_eq!(result["status"], "refused", "{result}");
    assert_eq!(result["reason"], "inspection_failed", "{result}");
    assert_eq!(result["freedBytes"], 0);
    assert!(f.target.exists());
}
