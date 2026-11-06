use super::*;
use std::sync::Mutex;

/// Hold the shared HOME lock and override inherited instance/install paths too.
/// No test may discover the owner's selection or default component directory.
struct PrivateHome {
    _home: crate::profiles::lifecycle::tests::TestHome,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl PrivateHome {
    fn new() -> Self {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let root = home.root();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), "test").unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        let mut previous = Vec::new();
        for (key, value) in [
            ("CUTEX_HOME", root.join(".cutex")),
            ("CUTEX_TEST_PRIVATE_HOME", root.to_path_buf()),
            ("TEMP", root.join("tmp")),
            ("TMP", root.join("tmp")),
            ("TMPDIR", root.join("tmp")),
            ("CUTEX_INSTALL_BASE", root.join("unmanaged-base")),
        ] {
            previous.push((key, std::env::var_os(key)));
            unsafe { std::env::set_var(key, value) };
        }
        Self { _home: home, previous }
    }
}
impl Drop for PrivateHome {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..).rev() {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

struct Fixture {
    root: PathBuf,
    // TEMP is process-wide. Keep the shared HOME/environment guard until all
    // fixture files are removed, including for tests that only probe payloads.
    _home: PrivateHome,
}
impl Fixture {
    fn new() -> Self {
        let home = PrivateHome::new();
        let root = std::env::temp_dir().join(format!("dsh-component-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self { root, _home: home }
    }
    fn payload(&self) -> PathBuf {
        let dir = self.root.join("payload");
        for file in REQUIRED_FILES {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "// fixture\n").unwrap();
        }
        std::fs::write(
            dir.join("component.json"),
            include_str!("../../components/dsh-runtime/component.json"),
        )
        .unwrap();
        for (file, name, version) in [
            (
                "runtime/node_modules/@deepseek-ai/dsh/package.json",
                "@deepseek-ai/dsh",
                DSH_VERSION,
            ),
            ("runtime/node_modules/ws/package.json", "ws", "8.22.0"),
            ("node_modules/ws/package.json", "ws", "8.22.0"),
            (
                "runtime/node_modules/@deepseek-ai/dsh-session-format-catalog/package.json",
                "@deepseek-ai/dsh-session-format-catalog",
                DSH_VERSION,
            ),
        ] {
            std::fs::write(dir.join(file), json!({"name": name, "version": version}).to_string()).unwrap();
        }
        dir
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn metadata_version(version: &str) -> String {
    let mut metadata = Metadata::parse(include_str!("../../components/dsh-runtime/component.json"));
    metadata.version = version.into();
    serde_json::to_string(&metadata).unwrap()
}

struct MockExec {
    version: &'static str,
    calls: Mutex<Vec<String>>,
}
impl MockExec {
    fn new(version: &'static str) -> Self {
        Self {
            version,
            calls: Mutex::new(vec![]),
        }
    }
}
impl Exec for MockExec {
    fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        assert_eq!(args, &["--version"]);
        assert!(Path::new(program).is_absolute());
        self.calls.lock().unwrap().push(program.into());
        Ok(self.version.into())
    }
}

#[test]
fn versions_are_numeric_and_stable() {
    for version in ["v24.9.0", "24.10.0", "v24.21.0\r\n", "25.0.0", "100.0.0", "24.9.0+build.1"] {
        assert!(node_version_supported(version), "{version}");
    }
    for version in [
        "v24.8.99",
        "v22.99.0",
        "9.99.99",
        "24.9",
        "24.9.0-rc.1",
        "24.09.0",
        "24.9.0\nnoise",
        "24.9.0+",
        "24.9.0+x+y",
        "garbage",
        "99999999999999999999999.0.0",
    ] {
        assert!(!node_version_supported(version), "{version}");
    }
}

#[test]
fn metadata_and_requirement_contract() {
    let handler = DshRuntime::default();
    assert_eq!(handler.metadata().name, NAME);
    assert_eq!(handler.metadata().platforms, ["linux-x64", "windows-x64"]);
    assert_eq!(requirements()["node"]["minimum"], "24.9.0");
    assert_eq!(requirements()["version"], DSH_VERSION);
}

#[test]
fn uninstalled_status_card_surfaces_node_minimum() {
    let _home = PrivateHome::new();
    let card = super::super::status_of(None, &[], &DshRuntime::default(), None);
    assert_eq!(card["name"], NAME);
    assert_eq!(card["selected"], Value::Null);
    assert_eq!(card["dsh"]["version"], DSH_VERSION);
    assert_eq!(card["dsh"]["node"]["minimum"], NODE_MINIMUM);
    assert_eq!(card["dsh"]["node"]["version"], Value::Null);
    assert_eq!(card["dsh"]["node"]["ready"], false);
}

#[test]
fn manual_status_card_reports_the_selected_node_without_an_entry() {
    let fixture = Fixture::new();
    let node = fixture.root.join("node");
    std::fs::write(&node, "fixture").unwrap();
    crate::config::atomic::write_private_pretty_json_atomic(&selected_path().unwrap(), &json!({"node":node}), "fixture").unwrap();
    let ready = status_requirements_with(&MockExec::new("v24.21.0"));
    assert_eq!(ready["node"]["version"], "v24.21.0");
    assert_eq!(ready["node"]["ready"], true);
    let old = status_requirements_with(&MockExec::new("v22.0.0"));
    assert_eq!(old["node"]["ready"], false);
    assert_eq!(old["node"]["minimum"], NODE_MINIMUM);
}

#[test]
fn layout_rejects_wrong_package_version_and_missing_adapter() {
    let f = Fixture::new();
    let dir = f.payload();
    check_layout(&dir).unwrap();
    std::fs::write(
        dir.join("runtime/node_modules/@deepseek-ai/dsh/package.json"),
        r#"{"name":"@deepseek-ai/dsh","version":"0.2.1"}"#,
    )
    .unwrap();
    assert!(check_layout(&dir).unwrap_err().to_string().contains(DSH_VERSION));
    std::fs::remove_file(dir.join("src/store.mjs")).unwrap();
    assert!(check_layout(&dir).unwrap_err().to_string().contains("store.mjs"));
    assert!(check_layout(Path::new("relative")).is_err());
}

#[test]
fn source_install_refuses_a_tree_of_another_component_version() {
    // Source builds must match this binary's compiled component tag, while
    // already-installed legacy versions remain independently supported.
    let f = Fixture::new();
    let source = f.payload();
    let old = metadata_version("dsh-r1");
    assert_ne!(old, include_str!("../../components/dsh-runtime/component.json"));
    std::fs::write(source.join("component.json"), old).unwrap();
    let out = f.root.join("out");
    let error = DshRuntime::default().from_source(&source, &out).unwrap_err().to_string();
    assert!(error.contains("dsh-r1") && error.contains(&DshRuntime::default().metadata().version), "{error}");
    assert!(!out.exists());
}

#[test]
fn retained_r1_install_stays_advertised_under_a_newer_binary() {
    // A host upgraded from r266 keeps its registered dsh-r1 (source) install
    // until the r2 component is installed; its own metadata identifies it.
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    let label = "dsh-r1-source-0123456789ab";
    let dir = super::super::version_dir(&layout, NAME, label);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::rename(f.payload(), &dir).unwrap();
    std::fs::write(dir.join("component.json"), metadata_version("dsh-r1")).unwrap();
    register(&layout, label);
    select_fixture(&dir);
    assert!(advertised_with(Some(&layout)));
}

#[test]
fn node_probe_uses_only_explicit_binary_and_version_argument() {
    let f = Fixture::new();
    let dir = f.payload();
    let exe = f.root.join("mock-node");
    std::fs::write(&exe, "fixture only").unwrap();
    let exec = MockExec::new("v24.8.9\n");
    let value = node_detail(Some(&exe), &exec);
    assert_eq!(value["version"], "v24.8.9");
    assert_eq!(value["ready"], false);
    assert_eq!(exec.calls.lock().unwrap().len(), 1);
    let value = probe_dir(&dir, &Options { node: Some(exe) }, &exec);
    assert_eq!(value["state"], states::NEEDS_USER, "{value}");
    assert_eq!(value["needsUser"]["code"], "dsh_node_required");
    assert_eq!(value["dsh"]["node"]["minimum"], NODE_MINIMUM);
    let missing = node_detail(Some(&f.root.join("absent")), &exec);
    assert_eq!(missing["version"], Value::Null);
}

#[test]
fn source_install_is_offline_and_does_not_copy_staging_user_data() {
    let f = Fixture::new();
    let source = f.payload();
    std::fs::write(source.join("runtime/secret-config.json"), "must stay behind").unwrap();
    let out = f.root.join("out");
    std::fs::create_dir(&out).unwrap();
    DshRuntime::default().from_source(&source, &out).unwrap();
    check_layout(&out).unwrap();
    assert!(!out.join("runtime/secret-config.json").exists());
    assert!(DshRuntime::default().from_source(&source, &out).is_err());
}

#[test]
fn pins_count_saved_deployments_and_exclude_outside_runtime() {
    let f = Fixture::new();
    let root = f.root.join("components/dsh-runtime");
    let one = root.join("dsh-r1/runtime");
    let two = root.join("dsh-r2/runtime");
    let pins = pins_for(
        &root,
        [
            json!({"install_root": one}),
            json!({"deployment": {"install_root": one}}),
            json!({"install_root": two}),
            json!({"install_root": f.root.join("external/runtime")}),
        ],
    );
    assert_eq!(pins.get("dsh-r1"), Some(&2));
    assert_eq!(pins.get("dsh-r2"), Some(&1));
    assert_eq!(pins.len(), 2);
}

#[test]
fn external_prefix_sessions_pin_adapter_component_without_double_counting() {
    let f = Fixture::new();
    let root = f.root.join("components/dsh-runtime");
    let adapter = root.join("dsh-r7/src/server.mjs");
    let pins = pins_for(&root, [
        json!({"adapter": adapter, "install_root": f.root.join("owner-prefix")}),
        json!({"deployment": {"adapter": adapter, "install_root": f.root.join("owner-prefix")}}),
        json!({"adapter": root.join("dsh-r2/src/server.mjs"), "install_root": root.join("dsh-r2/runtime")}),
        json!({"adapter": f.root.join("other/src/server.mjs"), "install_root": f.root.join("owner-prefix")}),
    ]);
    assert_eq!(pins.get("dsh-r7"), Some(&2));
    assert_eq!(pins.get("dsh-r2"), Some(&1));
    assert_eq!(pins.len(), 2);
}

fn adapter_fixture(f: &Fixture, name: &str, version: &str) -> PathBuf {
    let dir = f.root.join("base/components").join(name).join(version);
    let files = if name == NAME { ADAPTER_FILES } else { crate::components::kimi_runtime::ADAPTER_FILES };
    for file in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "// adapter fixture\n").unwrap();
    }
    std::fs::write(dir.join("component.json"), json!({"schema": super::super::COMPONENT_SCHEMA,
        "name": name, "version": version, "platforms": ["windows-x64", "linux-x64"]}).to_string()).unwrap();
    let dsh = name == NAME;
    let contract = json!({"schema": "cutex/external-runtime/v1", "name": name, "version": version,
        "payload": "adapter-v1", "externalRoot": {
            "kind": "npm-prefix", "packages": if dsh {
                json!({"@deepseek-ai/dsh": "0.2.1-rc.3", "@deepseek-ai/dsh-session-format-catalog": "0.2.1-rc.3", "ws": "8.22.0"})
            } else { json!({"@moonshot-ai/kimi-code": "2.3.3", "ws": "8.22.0"}) },
            "cliEntry": if dsh { "node_modules/@deepseek-ai/dsh/lib/bin.js" } else { "node_modules/@moonshot-ai/kimi-code/dist/main.mjs" }
        }, "node": {"range": ">=24.9.0 <25", "recommended": "24.21.0", "features": ["node:sqlite"]},
        "checks": {"rg": !dsh, "nodePty": !dsh}, "environment": {"installRoot": if dsh { "DSH_INSTALL_ROOT" } else { "KIMI_INSTALL_ROOT" }} });
    let mut contract = contract;
    if dsh { contract["externalRoot"]["catalogEntry"] = json!("node_modules/@deepseek-ai/dsh-session-format-catalog/lib/index.js"); }
    std::fs::write(dir.join("external-runtime.json"), contract.to_string()).unwrap();
    let hashes: BTreeMap<_, _> = files.iter().map(|file| (file.to_string(), crate::upgrade::manifest::file_sha256_hex(&dir.join(file)).unwrap())).collect();
    std::fs::write(dir.join("receipt.json"), json!({"schema": super::super::RECEIPT_SCHEMA, "name": name,
        "version": version, "origin": "signed", "archiveSha256": null, "files": hashes, "unpackedAt": "2026-10-29T00:00:00Z"}).to_string()).unwrap();
    dir
}

#[test]
fn future_adapter_layouts_accept_without_bundled_dependencies_or_readme() {
    let f = Fixture::new();
    for (name, version) in [(NAME, "dsh-r7"), (crate::components::kimi_runtime::NAME, "kimi-r8")] {
        let dir = adapter_fixture(&f, name, version);
        if name == NAME { check_layout(&dir).unwrap(); verify_payload(&dir).unwrap(); }
        else { crate::components::kimi_runtime::check_layout(&dir).unwrap(); crate::components::kimi_runtime::verify_payload(&dir).unwrap(); }
        assert!(!dir.join("README.md").exists());
        assert!(!dir.join("node_modules").exists());
        assert!(!dir.join("runtime").exists());
        let dest = f.root.join(format!("copy-{version}"));
        let handler: Box<dyn Handler> = if name == NAME { Box::new(DshRuntime::default()) } else { Box::new(crate::components::kimi_runtime::KimiRuntime::default()) };
        handler.from_source(&dir, &dest).unwrap();
        assert!(dest.join("external-runtime.json").is_file());
        assert!(!dest.join("runtime").exists());
    }
}

#[test]
fn adapter_get_and_unverified_select_do_not_probe_or_replace_default() {
    let f = Fixture::new();
    let dir = adapter_fixture(&f, NAME, "dsh-r7");
    let before = b"{\"existing\":\"r2-default\"}";
    std::fs::create_dir_all(selected_path().unwrap().parent().unwrap()).unwrap();
    std::fs::write(selected_path().unwrap(), before).unwrap();
    let external = cached_external_probe(&dir, NAME, false);
    assert_eq!(external["external"]["checkedAt"], Value::Null);
    assert_eq!(external["state"], states::NEEDS_USER);
    let exec = MockExec::new("v24.21.0");
    let ctx = Context { layout: &Layout { base: f.root.join("base") }, dir, version: "dsh-r7".into(), exec: &exec, previous: None, dry_run: false };
    let result = DshRuntime::default().select(&ctx).unwrap();
    assert_eq!(result["selected"], false);
    assert!(exec.calls.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(selected_path().unwrap()).unwrap(), before);
}

#[test]
fn real_node_adapter_restore_uses_pinned_contract_and_external_prefix() {
    // Explicit opt-in executable in private CI/drills; no install or model call.
    let Some(node) = std::env::var_os("CUTEX_TEST_EXTERNAL_NODE").map(PathBuf::from) else { return; };
    let f = Fixture::new();
    let dir = adapter_fixture(&f, NAME, "dsh-r7");
    let prefix = f.root.join("owner-npm-prefix");
    let mut contract = external_contract(&dir, NAME).unwrap();
    for package in ["@deepseek-ai/dsh", "@deepseek-ai/dsh-session-format-catalog"] {
        contract.external_root.packages.insert(package.into(), "99.2.0".into());
    }
    std::fs::write(dir.join("external-runtime.json"), serde_json::to_vec(&contract).unwrap()).unwrap();
    for (package, version) in &contract.external_root.packages {
        let path = prefix.join("node_modules").join(package).join("package.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut metadata = json!({"name": package, "version": version});
        if package == "ws" { metadata["main"] = json!("index.js"); }
        if package == "@deepseek-ai/dsh-session-format-catalog" { metadata["main"] = json!("lib/index.js"); }
        std::fs::write(path, metadata.to_string()).unwrap();
    }
    std::fs::write(prefix.join("node_modules/ws/index.js"), "throw Error('entry probe must not load ws');").unwrap();
    for entry in std::iter::once(&contract.external_root.cli_entry).chain(contract.external_root.catalog_entry.iter()) {
        let path = prefix.join(entry);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "throw Error('vendor CLI must never execute in readiness probes');").unwrap();
    }
    let mut receipt = super::super::read_receipt(&dir).unwrap();
    receipt.files.insert("external-runtime.json".into(), crate::upgrade::manifest::file_sha256_hex(&dir.join("external-runtime.json")).unwrap());
    std::fs::write(dir.join("receipt.json"), serde_json::to_vec(&receipt).unwrap()).unwrap();
    let deployment = DshDeployment { node, adapter: dir.join("src/server.mjs"),
        dsh_cli: prefix.join(&contract.external_root.cli_entry), install_root: prefix,
        version: "99.2.0".into() };
    deployment.validate().unwrap();
    // A later broken default configuration cannot retarget this saved binding.
    let layout = Layout { base: f.root.join("base") };
    let request = serde_json::from_value(json!({"externalRoot": f.root.join("broken-new-default"), "node": deployment.node})).unwrap();
    let status = super::super::external_runtime::configure(&layout, NAME, &contract, request, &super::super::external_runtime::RealExec).unwrap();
    assert!(!status.ready());
    deployment.validate().unwrap();
    std::fs::write(deployment.install_root.join("node_modules/ws/package.json"), json!({"name": "ws", "version": "0.0.0"}).to_string()).unwrap();
    assert!(deployment.validate().unwrap_err().to_string().contains("upstream_version_mismatch"));
}

#[test]
fn full_component_get_and_capability_polling_never_start_adapter_dependencies() {
    let Some(sentinel) = std::env::var_os("CUTEX_TEST_PROBE_SENTINEL").map(PathBuf::from) else { return; };
    let f = Fixture::new();
    let layout = Layout { base: f.root.join("base") };
    std::fs::create_dir_all(&layout.base).unwrap();
    std::fs::write(layout.install_record_path(), "{}").unwrap();
    let node = f.root.join(if cfg!(windows) { "node.exe" } else { "node" });
    let rg = f.root.join(if cfg!(windows) { "rg.exe" } else { "rg" });
    for binary in [&node, &rg] { std::fs::copy(&sentinel, binary).unwrap(); }
    struct ProbeOk;
    impl Exec for ProbeOk {
        fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
            if program.ends_with(if cfg!(windows) { "rg.exe" } else { "/rg" }) { return Ok("ripgrep 15.0.0".into()); }
            if args == ["--version"] { return Ok("v24.21.0".into()); }
            if args.iter().any(|s| s.contains("cutex-entries-v1")) { return Ok(json!({"marker":"cutex-entries-v1", "issues":[]}).to_string()); }
            if args.iter().any(|s| s.contains("cutex-sqlite-ok")) { return Ok("cutex-sqlite-ok".into()); }
            Ok("cutex-pty-ok".into())
        }
    }
    let mut state = super::super::State::new();
    let mut selected_paths = vec![];
    for (name, version) in [(NAME, "dsh-r7"), (crate::components::kimi_runtime::NAME, "kimi-r8")] {
        let dir = adapter_fixture(&f, name, version);
        let contract = external_contract(&dir, name).unwrap();
        let root = f.root.join(format!("{name}-prefix"));
        for (package, version) in &contract.external_root.packages {
            let path = root.join("node_modules").join(package).join("package.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, json!({"name": package, "version": version}).to_string()).unwrap();
        }
        for entry in std::iter::once(&contract.external_root.cli_entry).chain(contract.external_root.catalog_entry.iter()) {
            let path = root.join(entry);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "// not executed").unwrap();
        }
        let request = serde_json::from_value(json!({"externalRoot": root, "node": node, "rg": rg})).unwrap();
        assert!(super::super::external_runtime::configure(&layout, name, &contract, request, &ProbeOk).unwrap().ready());
        let package = if name == NAME { "@deepseek-ai/dsh" } else { crate::runtime::kimi::KIMI_PACKAGE };
        let path = if name == NAME { crate::runtime::dsh::deployment_path().unwrap() } else { crate::runtime::kimi::deployment_path().unwrap() };
        let mut deployment = json!({"node": node, "adapter": dir.join("src/server.mjs"), "install_root": root, "version": contract.external_root.packages[package]});
        deployment[if name == NAME { "dsh_cli" } else { "kimi_cli" }] = json!(root.join(&contract.external_root.cli_entry));
        if name != NAME { deployment["rg"] = json!(rg); }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&deployment).unwrap()).unwrap();
        selected_paths.push((path, serde_json::to_vec(&deployment).unwrap()));
        state.insert(name.into(), Entry { selected: version.into(), origin: "signed".into(), ..Entry::default() });
    }
    super::super::save_state(&layout, &state).unwrap();
    unsafe { std::env::set_var("CUTEX_INSTALL_BASE", &layout.base); }
    for handler in [Box::new(DshRuntime::default()) as Box<dyn Handler>, Box::new(crate::components::kimi_runtime::KimiRuntime::default())] {
        let view = super::super::status_of(Some(&layout), &[], handler.as_ref(), None);
        assert_eq!(view["state"], states::READY);
        assert!(handler.configured_by_hand());
    }
    assert!(advertised_with(Some(&layout)));
    assert!(advertised_with(None));
    assert!(crate::components::kimi_runtime::advertised_with(Some(&layout)));
    assert!(crate::components::kimi_runtime::advertised_with(None));
    crate::components::kimi_runtime::register_profile_hooks();
    assert_eq!(crate::profiles::kimi::component_status().0, crate::profiles::runtime_variants::Status::Supported);
    assert!(!node.with_extension("log").exists(), "GET/capability polling started Node");
    assert!(!rg.with_extension("log").exists(), "GET/capability polling started ripgrep");
    for (path, bytes) in selected_paths { assert_eq!(std::fs::read(path).unwrap(), bytes); }
}

#[test]
fn dry_run_does_not_require_payload_or_run_node() {
    let f = Fixture::new();
    let exec = MockExec::new("v24.9.0");
    let layout = Layout::at(&f.root);
    let ctx = Context {
        layout: &layout,
        dir: f.root.join("absent"),
        version: "dsh-r2".into(),
        exec: &exec,
        previous: None,
        dry_run: true,
    };
    let handler = DshRuntime::default();
    assert_eq!(handler.prepare(&ctx).unwrap()["dryRun"], true);
    assert_eq!(handler.select(&ctx).unwrap()["dryRun"], true);
    assert!(exec.calls.lock().unwrap().is_empty());
}

#[test]
fn prepare_refuses_an_unreceipted_tree_before_running_node() {
    let f = Fixture::new();
    let dir = f.payload();
    let exe = f.root.join("node-fixture");
    std::fs::write(&exe, "fixture").unwrap();
    let exec = MockExec::new("v24.9.0");
    let layout = Layout::at(&f.root);
    let ctx = Context {
        layout: &layout,
        dir,
        version: "dsh-r2".into(),
        exec: &exec,
        previous: None,
        dry_run: false,
    };
    let handler = DshRuntime {
        options: Some(Options { node: Some(exe) }),
        ..DshRuntime::default()
    };
    assert!(handler.prepare(&ctx).unwrap_err().to_string().contains("receipt"));
    assert!(exec.calls.lock().unwrap().is_empty());
}

#[test]
fn payload_verification_checks_required_coverage_and_detects_tampering() {
    let f = Fixture::new();
    let dir = f.payload();
    let mut files = super::super::file_hashes(&dir).unwrap();
    let write_receipt = |files: &BTreeMap<String, String>| {
        std::fs::write(
            dir.join(super::super::RECEIPT_FILE),
            json!({
                "schema": super::super::RECEIPT_SCHEMA, "name": NAME, "version": "dsh-r2", "origin": "source",
                "files": files, "unpackedAt": "2026-10-25T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
    };
    write_receipt(&files);
    verify_payload(&dir).unwrap();
    let saved = files.remove("src/history-reader.mjs").unwrap();
    write_receipt(&files);
    assert!(verify_payload(&dir)
        .unwrap_err()
        .to_string()
        .contains("receipt lacks src/history-reader.mjs"));
    files.insert("src/history-reader.mjs".into(), saved);
    write_receipt(&files);
    std::fs::write(dir.join("src/rpc.mjs"), "// altered after unpack\n").unwrap();
    assert!(verify_payload(&dir).unwrap_err().to_string().contains("does not match its receipt"));
}

#[test]
fn broken_payload_is_degraded_without_executing_a_binary() {
    let f = Fixture::new();
    let dir = f.payload();
    let exe = f.root.join("node-fixture");
    std::fs::write(&exe, "fixture").unwrap();
    std::fs::remove_file(dir.join("src/history-reader.mjs")).unwrap();
    let exec = MockExec::new("v24.9.0");
    let probe = probe_dir(&dir, &Options { node: Some(exe) }, &exec);
    assert_eq!(probe["state"], states::DEGRADED);
    assert_eq!(probe["dsh"]["node"]["minimum"], NODE_MINIMUM);
    assert!(exec.calls.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn source_rejects_links() {
    let f = Fixture::new();
    let source = f.payload();
    std::os::unix::fs::symlink(&f.root, source.join("runtime/node_modules/escape")).unwrap();
    assert!(DshRuntime::default().from_source(&source, &f.root.join("out")).is_err());
}

fn register(layout: &Layout, version: &str) {
    let state = BTreeMap::from([(NAME.into(), Entry {
        selected: version.into(), origin: "source".into(), installed_at: "2026-10-25T00:00:00Z".into(), last_error: None,
    })]);
    super::super::save_state(layout, &state).unwrap();
}

fn select_fixture(dir: &Path) -> DshDeployment {
    let deployment = DshDeployment {
        // Intentionally absent: capability presence must not require Node health.
        node: dir.join("missing-node"), adapter: dir.join("src/server.mjs"),
        dsh_cli: dir.join("runtime/node_modules/@deepseek-ai/dsh/lib/bin.js"),
        install_root: dir.join("runtime"), version: DSH_VERSION.into(),
    };
    crate::config::atomic::write_private_pretty_json_atomic(&selected_path().unwrap(), &deployment, "fixture").unwrap();
    deployment
}

#[test]
fn advertised_absent_and_retained_unregistered_files_are_hidden() {
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    assert!(!advertised_with(None));
    assert!(!advertised_with(Some(&layout)));
    let payload = f.payload();
    let dir = super::super::version_dir(&layout, NAME, "dsh-r2");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::rename(payload, &dir).unwrap();
    assert!(!advertised_with(Some(&layout)));
    // r260 legacy selectors are valid even at the managed component path.
    select_fixture(&dir);
    assert!(advertised_with(Some(&layout)));
    std::fs::remove_file(selected_path().unwrap()).unwrap();
    assert!(!advertised_with(Some(&layout)));
}

#[test]
fn advertised_source_install_accepts_base_metadata_with_matching_source_identity() {
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    for (version, origin, expected) in [
        ("dsh-r2-source-012345abcdef", super::super::ORIGIN_SOURCE, true),
        ("dsh-r2-source-012345abcdef", super::super::ORIGIN_SIGNED, false),
        ("dsh-r1-source-012345abcdef", super::super::ORIGIN_SOURCE, false),
        ("dsh-r2-source-012345abcde", super::super::ORIGIN_SOURCE, false),
        ("dsh-r2-source-012345abcdef0", super::super::ORIGIN_SOURCE, false),
        ("dsh-r2-source-012345abcdeg", super::super::ORIGIN_SOURCE, false),
    ] {
        let dir = super::super::version_dir(&layout, NAME, version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("component.json"), metadata_version("dsh-r2")).unwrap();
        super::super::save_state(&layout, &BTreeMap::from([(NAME.into(), Entry {
            selected: version.into(), origin: origin.into(), installed_at: "2026-10-25T00:00:00Z".into(), last_error: None,
        })])).unwrap();
        assert_eq!(advertised_with(Some(&layout)), expected, "{version} ({origin})");
    }
}

#[test]
fn advertised_registered_install_keeps_repair_card_without_node_or_internal_files() {
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    let payload = f.payload();
    let dir = super::super::version_dir(&layout, NAME, "dsh-r2");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::rename(payload, &dir).unwrap();
    std::fs::write(dir.join("component.json"), metadata_version("dsh-r2")).unwrap();
    register(&layout, "dsh-r2");
    assert!(advertised_with(Some(&layout)));
    let deployment = select_fixture(&dir);
    // A non-executable mock binary proves presence does not execute Node.
    std::fs::write(&deployment.node, "not a Node executable").unwrap();
    assert!(advertised_with(Some(&layout)));
    std::fs::remove_file(&deployment.node).unwrap();
    std::fs::remove_file(dir.join("src/runtime.mjs")).unwrap();
    std::fs::remove_file(dir.join("runtime/node_modules/@deepseek-ai/dsh/package.json")).unwrap();
    assert!(advertised_with(Some(&layout)));
    assert!(check_layout(&dir).is_err());
    // Metadata alone cannot validate a runnable deployment.
    assert!(DshDeployment::selected().is_err());
}

#[test]
fn advertised_registered_install_disappears_when_root_is_moved_or_entry_removed() {
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    let dir = super::super::version_dir(&layout, NAME, "dsh-r2");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::rename(f.payload(), &dir).unwrap();
    std::fs::write(dir.join("component.json"), metadata_version("dsh-r2")).unwrap();
    register(&layout, "dsh-r2");
    select_fixture(&dir);
    assert!(advertised_with(Some(&layout)));
    let moved = f.root.join("moved-payload");
    std::fs::rename(&dir, &moved).unwrap();
    assert!(!advertised_with(Some(&layout)));
    std::fs::rename(moved, &dir).unwrap();
    // Complete remove deselects before deleting the registration and retains
    // the version files for pinned sessions.
    let exec = MockExec::new("v24.21.0");
    let ctx = Context {
        layout: &layout, dir: dir.clone(), version: "dsh-r2".into(), exec: &exec,
        previous: super::super::installed(&layout, NAME), dry_run: false,
    };
    DshRuntime::default().deselect(&ctx, false).unwrap();
    super::super::save_state(&layout, &BTreeMap::new()).unwrap();
    assert!(!selected_path().unwrap().exists());
    assert!(!advertised_with(Some(&layout)));
    assert!(dir.is_dir());
    assert!(exec.calls.lock().unwrap().is_empty());
}

#[test]
fn advertised_rejects_invalid_component_identity_and_version_path() {
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    let dir = super::super::version_dir(&layout, NAME, "dsh-r2");
    std::fs::create_dir_all(&dir).unwrap();
    register(&layout, "dsh-r2");
    for metadata in ["{", r#"{"name":"dsh-runtime"}"#, include_str!("../../components/claude-runtime/component.json")] {
        std::fs::write(dir.join("component.json"), metadata).unwrap();
        assert!(!advertised_with(Some(&layout)));
    }
    register(&layout, "../payload");
    f.payload();
    assert!(!advertised_with(Some(&layout)));
}

#[test]
fn advertised_manual_external_selection_works_without_managed_layout() {
    let f = Fixture::new();
    let payload = f.payload();
    select_fixture(&payload);
    assert!(advertised_with(None));
    assert!(advertised());
    // A hand-deployed adapter need not have a component.json or receipt.
    std::fs::remove_file(payload.join("component.json")).unwrap();
    assert!(advertised_with(None));
    assert!(advertised_with(Some(&Layout::at(f.root.join("other-base")))));
    std::fs::remove_file(selected_path().unwrap()).unwrap();
    assert!(!advertised_with(None));
}

#[test]
fn advertised_manual_selection_rejects_missing_core_payload_and_untrusted_records() {
    let f = Fixture::new();
    let payload = f.payload();
    let original = select_fixture(&payload);
    let write = |value: &Value| {
        crate::config::atomic::write_private_pretty_json_atomic(&selected_path().unwrap(), value, "fixture").unwrap();
    };
    for (key, value) in [
        ("adapter", json!("relative/server.mjs")),
        ("dsh_cli", json!(payload.join("src/server.mjs"))),
        ("install_root", json!("runtime")),
        ("version", json!("0.2.1")),
        ("unexpected", json!(true)),
    ] {
        let mut record = serde_json::to_value(&original).unwrap();
        record[key] = value;
        write(&record);
        assert!(!advertised_with(None), "{key}");
    }
    write(&json!({"adapter": original.adapter}));
    assert!(!advertised_with(None));
    write(&serde_json::to_value(&original).unwrap());
    std::fs::write(payload.join("runtime/node_modules/@deepseek-ai/dsh/package.json"), r#"{"name":"@deepseek-ai/dsh","version":"0.2.1"}"#).unwrap();
    assert!(!advertised_with(None));
    f.payload();
    assert!(advertised_with(None));
    std::fs::remove_file(&original.dsh_cli).unwrap();
    assert!(!advertised_with(None));
    f.payload();
    std::fs::remove_file(&original.adapter).unwrap();
    assert!(!advertised_with(None));
    f.payload();
    std::fs::rename(&original.install_root, payload.join("moved-runtime")).unwrap();
    assert!(!advertised_with(None));
}

#[test]
fn capability_resource_refreshes_manual_presence_on_each_read() {
    let f = Fixture::new();
    let check = |expected| {
        let resource = crate::management::v2::capabilities::capabilities_resource();
        assert_eq!(resource["featureList"].as_array().unwrap().iter().any(|value| value == NAME), expected);
        assert_eq!(resource["features"]["dshRuntime"].as_bool().unwrap_or(false), expected);
    };
    check(false);
    select_fixture(&f.payload());
    check(true);
    std::fs::remove_file(selected_path().unwrap()).unwrap();
    check(false);
}

#[test]
fn archived_durable_sessions_pin_versions_during_prune() {
    use crate::session::model::{CutexSessionArchiveState, CutexSessionRecord, CutexSessionRuntimeBackend};
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    std::fs::write(layout.install_record_path(), "{}").unwrap();
    let pinned = super::super::version_dir(&layout, NAME, "dsh-r1");
    let active = super::super::version_dir(&layout, NAME, "dsh-r2");
    let unpinned = super::super::version_dir(&layout, NAME, "dsh-r3");
    for dir in [&pinned, &active, &unpinned] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(super::super::RECEIPT_FILE), "{}").unwrap();
    }
    let mut store = crate::session::model::CutexSessionStore::default();
    for archived in [false, true] {
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut record = CutexSessionRecord::new(id.clone(), None, "fixture-host".into(), f.root.to_string_lossy().into_owned(), None).unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::Dsh;
        if archived {
            record.archive_state = CutexSessionArchiveState::Retired;
            record.retired_at = Some("2026-10-25T00:00:00Z".into());
        }
        let deployment = if archived { &pinned } else { &active };
        record.dsh_session = Some(crate::runtime::dsh::DshSession {
            flavor: Default::default(), kimi: None,
            thread_id: uuid::Uuid::new_v4().to_string(), state_dir: f.root.join(&id),
            node: deployment.join("missing-node"), adapter: deployment.join("src/server.mjs"),
            dsh_cli: deployment.join("runtime/node_modules/@deepseek-ai/dsh/lib/bin.js"),
            patch: f.root.join(&id).join("profile.patch.yml"), install_root: deployment.join("runtime"),
            native_session_id: None, deployment_version: DSH_VERSION.into(), model: "deepseek-v4-pro".into(),
            reasoning_effort: None, api_key_env: "DSH_API_KEY".into(), spike: false,
        });
        store.sessions.insert(id, record);
    }
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let pins = DshRuntime::default().pins_checked(&layout).unwrap();
    assert_eq!(pins.get("dsh-r1"), Some(&1));
    assert_eq!(pins.get("dsh-r2"), Some(&1));
    assert_eq!(DshRuntime::default().pins(&layout), pins);
    assert_eq!(super::super::prune(Some(&layout), &[], NAME, true).unwrap(), vec![unpinned.display().to_string()]);
    assert!(pinned.is_dir());
}

#[test]
fn pin_store_load_failure_refuses_prune_and_status_is_best_effort() {
    let f = Fixture::new();
    let layout = Layout::at(&f.root);
    std::fs::write(layout.install_record_path(), "{}").unwrap();
    let candidate = super::super::version_dir(&layout, NAME, "dsh-r1");
    std::fs::create_dir_all(&candidate).unwrap();
    std::fs::write(candidate.join(super::super::RECEIPT_FILE), "{}").unwrap();
    let path = crate::session::store::cutex_sessions_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{invalid session store").unwrap();
    let handler = DshRuntime::default();
    assert!(handler.pins_checked(&layout).unwrap_err().to_string().contains("refusing to prune"));
    assert!(handler.pins(&layout).is_empty());
    for dry_run in [true, false] {
        let error = super::super::prune(Some(&layout), &[], NAME, dry_run).unwrap_err();
        assert!(error.to_string().contains("refusing to prune"));
    }
    assert!(candidate.is_dir());
}
