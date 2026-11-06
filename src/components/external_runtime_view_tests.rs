use super::*;

fn save(fixture: &Fixture, config: ExternalConfig, env: &BTreeMap<OsString, OsString>) -> State {
    let resolved = resolve_defaults(&config, &fixture.contract, env).unwrap();
    let mut status = ExternalStatus::unchecked(&fixture.contract, &resolved);
    status.checked_at = Some("2026-10-30T00:00:00Z".into());
    let state = State {
        schema: STATE_SCHEMA.into(),
        name: fixture.contract.name.clone(),
        config,
        contract: fixture.contract.clone(),
        resolved,
        status,
    };
    put(
        &state_path(&fixture.layout, &fixture.contract.name).unwrap(),
        &serde_json::to_vec(&state).unwrap(),
    );
    state
}

fn env_for(fixture: &Fixture) -> BTreeMap<OsString, OsString> {
    let family = family(&fixture.contract.name).unwrap();
    let mut env = BTreeMap::new();
    env.insert(
        format!("CUTEX_{family}_INSTALL_ROOT").into(),
        fixture
            .config
            .external_root
            .as_ref()
            .unwrap()
            .as_os_str()
            .to_owned(),
    );
    env.insert(
        format!("CUTEX_{family}_NODE").into(),
        fixture.config.node.as_ref().unwrap().as_os_str().to_owned(),
    );
    if fixture.contract.checks.rg {
        env.insert(
            "CUTEX_KIMI_RG".into(),
            fixture.config.rg.as_ref().unwrap().as_os_str().to_owned(),
        );
    }
    env
}

#[test]
fn raw_overrides_are_distinct_from_effective_paths_and_get_is_read_only() {
    for kimi in [false, true] {
        let fixture = Fixture::new(kimi);
        let mut config = fixture.config.clone();
        // Preserve an explicit lexical spelling even though the effective root
        // is canonicalized. On Windows the fixture also exercises verbatim paths.
        config.external_root = Some(config.external_root.unwrap().join("..").join("prefix"));
        let state = save(&fixture, config.clone(), &BTreeMap::new());
        let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
        let bytes = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let value = view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap();
        assert_eq!(
            value["storedConfig"],
            serde_json::to_value(&config).unwrap()
        );
        assert_ne!(
            value["storedConfig"]["externalRoot"],
            value["resolved"]["externalRoot"]
        );
        assert_eq!(value["root"], value["resolved"]["externalRoot"]);
        assert_eq!(value["node"], value["resolved"]["node"]);
        assert_eq!(value["rg"], value["resolved"]["rg"]);
        assert_eq!(value["checkedAt"], "2026-10-30T00:00:00Z");
        let mut changed_env = env_for(&fixture);
        changed_env.insert(
            "CUTEX_KIMI_NODE".into(),
            fixture.layout.base.join("other-node").into_os_string(),
        );
        changed_env.insert(
            "CUTEX_DSH_NODE".into(),
            fixture.layout.base.join("other-node").into_os_string(),
        );
        assert_eq!(
            view_with_env(
                &fixture.layout,
                &fixture.contract.name,
                &fixture.contract,
                &changed_env
            )
            .unwrap(),
            value
        );
        // Fixture executables are deliberately invalid sentinel bytes, never
        // runnable Node/rg. A GET must only reuse the saved result.
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_eq!(
            cached(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap(),
            state.status
        );
    }
}

#[test]
fn auto_paths_null_clear_and_environment_invalidation_preserve_overrides() {
    let fixture = Fixture::new(true);
    let env = env_for(&fixture);
    let mut config = fixture.config.clone();
    let unchanged: ConfigureRequest = serde_json::from_value(json!({})).unwrap();
    unchanged.apply(&mut config).unwrap();
    assert_eq!(config, fixture.config);
    let clear: ConfigureRequest =
        serde_json::from_value(json!({"externalRoot":null,"node":null,"rg":null})).unwrap();
    clear.apply(&mut config).unwrap();
    assert_eq!(config, ExternalConfig::default());
    let state = save(&fixture, config, &env);
    let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
    let bytes = fs::read(&path).unwrap();
    let value = view_with_env(
        &fixture.layout,
        &fixture.contract.name,
        &fixture.contract,
        &env,
    )
    .unwrap();
    assert_eq!(
        value["storedConfig"],
        json!({"externalRoot":null,"node":null,"rg":null})
    );
    assert_eq!(value["checkedAt"], "2026-10-30T00:00:00Z");
    assert!(value["resolved"]["externalRoot"].is_string());
    let mut changed = env.clone();
    changed.insert(
        "CUTEX_KIMI_INSTALL_ROOT".into(),
        fixture
            .layout
            .base
            .join("different-prefix")
            .into_os_string(),
    );
    let value = view_with_env(
        &fixture.layout,
        &fixture.contract.name,
        &fixture.contract,
        &changed,
    )
    .unwrap();
    assert!(value["checkedAt"].is_null());
    assert_ne!(
        value["resolved"]["externalRoot"],
        serde_json::to_value(state.resolved.external_root).unwrap()
    );
    assert!(value["storedConfig"]["externalRoot"].is_null());
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn only_drive_and_unc_verbatim_paths_are_presented_differently() {
    for (raw, expected) in [
        (r"\\?\C:\tools\node.exe", r"C:\tools\node.exe"),
        (r"\\?\UNC\server\share\node.exe", r"\\server\share\node.exe"),
        (r"\\?\Volume{abc}\node.exe", r"\\?\Volume{abc}\node.exe"),
        (
            r"\\?\GLOBALROOT\Device\node.exe",
            r"\\?\GLOBALROOT\Device\node.exe",
        ),
        (r"\\.\pipe\node", r"\\.\pipe\node"),
        (r"\\?\UNC\server", r"\\?\UNC\server"),
        (r"\\?\UNC\.\device", r"\\?\UNC\.\device"),
        (r"\\?\C:relative", r"\\?\C:relative"),
        ("/usr/bin/node", "/usr/bin/node"),
    ] {
        assert_eq!(display_path(raw), expected);
        let value = diagnostics_view(
            json!({"issues":[{"actual":"missing", "path":raw}], "storedConfig":{"node":raw}}),
        );
        assert_eq!(value["issues"][0]["path"], expected);
        assert_eq!(value["storedConfig"]["node"], raw);
    }
}

#[test]
fn exception_summary_prefers_real_error_and_retains_bounded_original() {
    let raw = "node:internal/modules/cjs/loader:1411\n  throw err;\n  ^\n\nError: Cannot find module 'ws'\n    at Module._resolveFilename (node:internal/a:10:2)\nNode.js v24.21.0\n";
    let value = diagnostics_view(json!({"issues":[{"actual":raw}]}));
    assert_eq!(
        value["issues"][0]["actual"],
        "Error: Cannot find module 'ws'"
    );
    assert_eq!(value["issues"][0]["diagnostics"], raw);
    assert_eq!(diagnostics_view(value.clone()), value);
    let retained =
        diagnostics_view(json!({"issues":[{"actual":" Error: short ","diagnostics":raw}]}));
    assert_eq!(retained["issues"][0]["diagnostics"], raw);
    let long = format!(
        "MODULE_NOT_FOUND: {}\n    at ignored (file:1:2)",
        "界🙂".repeat(18000)
    );
    let value = diagnostics_view(json!({"issues":[{"actual":long}]}));
    let issue = &value["issues"][0];
    assert_eq!(issue["actual"].as_str().unwrap().chars().count(), 240);
    let diagnostics = issue["diagnostics"].as_str().unwrap();
    assert!(diagnostics.len() <= 64 * 1024);
    assert!(diagnostics.ends_with("… [truncated]"));
    assert!(long.starts_with(diagnostics.strip_suffix("\n… [truncated]").unwrap()));
    assert!(!issue["actual"].as_str().unwrap().contains('\n'));
    assert_eq!(diagnostics_view(value.clone()), value);
}

#[test]
fn legacy_typed_issues_stay_unchanged_and_corrupt_state_is_not_reset() {
    let fixture = Fixture::new(false);
    let mut state = save(&fixture, fixture.config.clone(), &BTreeMap::new());
    let original = "node:internal/loader:1\nError: missing ws\n    at resolve (loader:1:1)";
    state.status.issues.push(ExternalIssue {
        code: "external_root_invalid".into(),
        expected: None,
        actual: Some(original.into()),
        path: fixture.config.external_root.clone(),
        steps: vec![],
    });
    let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
    let bytes = serde_json::to_vec(&state).unwrap();
    put(&path, &bytes);
    let value = view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap();
    assert_eq!(value["issues"][0]["actual"], "Error: missing ws");
    assert_eq!(value["issues"][0]["diagnostics"], original);
    assert!(value["nodeVersion"].is_null());
    assert!(!node_version_path(&fixture.layout, &fixture.contract.name)
        .unwrap()
        .exists());
    assert_eq!(
        read_state(&fixture.layout, &fixture.contract.name)
            .unwrap()
            .unwrap()
            .status,
        state.status
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    // Retain all old strictness: unknown state keys, mismatched path binding,
    // malformed timestamps, and issues on unchecked state are not repairable GETs.
    for corruption in 0..4 {
        let mut corrupt = serde_json::to_value(&state).unwrap();
        match corruption {
            0 => corrupt["unexpected"] = json!(true),
            1 => corrupt["status"]["root"] = json!(null),
            2 => corrupt["status"]["checkedAt"] = json!("not-a-time"),
            _ => corrupt["status"]["checkedAt"] = json!(null),
        }
        let bytes = serde_json::to_vec(&corrupt).unwrap();
        put(&path, &bytes);
        assert!(view(&fixture.layout, &fixture.contract.name, &fixture.contract).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn absent_state_view_does_not_create_cache() {
    let fixture = Fixture::new(true);
    let value = view_with_env(
        &fixture.layout,
        &fixture.contract.name,
        &fixture.contract,
        &env_for(&fixture),
    )
    .unwrap();
    assert!(value["checkedAt"].is_null());
    assert_eq!(
        value["storedConfig"],
        json!({"externalRoot":null,"node":null,"rg":null})
    );
    assert!(!fixture.layout.base.exists());
}

#[test]
fn error_text_displays_drive_unc_paths_but_retains_raw_diagnostics() {
    for (raw, expected) in [
        (
            r"MODULE_NOT_FOUND: Cannot find module '\\?\C:\a folder\entry.js'",
            r"MODULE_NOT_FOUND: Cannot find module 'C:\a folder\entry.js'",
        ),
        (
            r"Error: missing \\?\D:\prefix\entry.js (required)",
            r"Error: missing D:\prefix\entry.js (required)",
        ),
        (
            r#"Error: missing "\\?\UNC\server\a share\entry.js""#,
            r#"Error: missing "\\server\a share\entry.js""#,
        ),
        (
            r"MODULE_NOT_FOUND: \\?\UNC\server\share\entry.js",
            r"MODULE_NOT_FOUND: \\server\share\entry.js",
        ),
        (
            r"Error: '\\?\Volume{abc}\entry.js'",
            r"Error: '\\?\Volume{abc}\entry.js'",
        ),
        (
            r"Error: \\?\GLOBALROOT\Device\entry.js",
            r"Error: \\?\GLOBALROOT\Device\entry.js",
        ),
        (r"Error: \\?\UNC\.\device", r"Error: \\?\UNC\.\device"),
    ] {
        let value = diagnostics_view(json!({"issues":[{"actual":raw}]}));
        assert_eq!(value["issues"][0]["actual"], expected);
        if raw != expected {
            assert_eq!(value["issues"][0]["diagnostics"], raw);
        }
        assert_eq!(diagnostics_view(value.clone()), value);
    }
}

#[test]
fn package_metadata_invalidity_is_distinct_from_valid_version_mismatch() {
    for kimi in [false, true] {
        let fixture = Fixture::new(kimi);
        let package = fixture
            .config
            .external_root
            .as_ref()
            .unwrap()
            .join("node_modules/ws/package.json");
        for bytes in [
            None,
            Some(b"{".as_slice()),
            Some(br#"{"version":"8.22.0"}"#.as_slice()),
            Some(br#"{"name":"other","version":"8.22.0"}"#.as_slice()),
            Some(br#"{"name":"ws"}"#.as_slice()),
            Some(br#"{"name":"ws","version":"latest"}"#.as_slice()),
            Some(br#"{"name":"ws","version":8220}"#.as_slice()),
        ] {
            if let Some(bytes) = bytes {
                put(&package, bytes);
            } else {
                fs::remove_file(&package).unwrap();
            }
            let status = probe(&fixture.contract, &fixture.config, &FakeExec::default()).unwrap();
            let issue = status
                .issues
                .iter()
                .find(|issue| issue.path.as_ref() == Some(&package))
                .unwrap();
            assert_eq!(issue.code, "external_root_invalid");
            assert!(!issue.steps.is_empty());
            assert!(!status
                .issues
                .iter()
                .any(|issue| issue.code == "upstream_version_mismatch"));
        }
        // A directory in place of package.json produces a read failure on both
        // supported platforms, without depending on permission bypass behavior.
        fs::remove_file(&package).unwrap();
        fs::create_dir(&package).unwrap();
        let status = probe(&fixture.contract, &fixture.config, &FakeExec::default()).unwrap();
        assert!(status
            .issues
            .iter()
            .any(|issue| issue.code == "external_root_invalid"
                && issue.path.as_ref() == Some(&package)));
        fs::remove_dir(&package).unwrap();
        put(&package, br#"{"name":"ws","version":"8.21.0"}"#);
        let status = probe(&fixture.contract, &fixture.config, &FakeExec::default()).unwrap();
        let issue = status
            .issues
            .iter()
            .find(|issue| issue.path.as_ref() == Some(&package))
            .unwrap();
        assert_eq!(issue.code, "upstream_version_mismatch");
        assert_eq!(issue.actual.as_deref(), Some("8.21.0"));
    }
}

fn checked_fixture(kimi: bool) -> (Fixture, FakeExec) {
    let fixture = Fixture::new(kimi);
    let exec = FakeExec::default();
    assert!(configure(
        &fixture.layout,
        &fixture.contract.name,
        &fixture.contract,
        fixture.request(),
        &exec
    )
    .unwrap()
    .ready());
    (fixture, exec)
}

#[test]
fn observed_node_version_reuses_the_existing_probe_without_get_exec_or_state_changes() {
    for kimi in [false, true] {
        let (fixture, mut exec) = checked_fixture(kimi);
        let versions = || {
            exec.calls
                .borrow()
                .iter()
                .filter(|(_, args)| args.as_slice() == ["--version"] && !args.is_empty())
                .count()
        };
        // Kimi additionally runs rg --version. Neither command is added by the
        // observation wrapper, and repeated GETs never append any probe calls.
        assert_eq!(versions(), if kimi { 2 } else { 1 });
        let calls = exec.calls.borrow().len();
        let path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
        let observation_path = node_version_path(&fixture.layout, &fixture.contract.name).unwrap();
        let state_bytes = fs::read(&path).unwrap();
        let observation_bytes = fs::read(&observation_path).unwrap();
        let state: State = serde_json::from_slice(&state_bytes).unwrap();
        let status: ExternalStatus = serde_json::from_value(
            serde_json::from_slice::<serde_json::Value>(&state_bytes).unwrap()["status"].clone(),
        )
        .unwrap();
        assert_eq!(status, state.status);
        for _ in 0..2 {
            let value = view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap();
            assert_eq!(value["nodeVersion"], "v24.21.0");
            assert_eq!(diagnostics_view(value.clone()), value);
        }
        assert_eq!(exec.calls.borrow().len(), calls);
        assert_eq!(fs::read(&path).unwrap(), state_bytes);
        assert_eq!(fs::read(&observation_path).unwrap(), observation_bytes);
        // An unsupported but real observed version is displayed accurately,
        // rather than being replaced with the recommended version.
        exec.version = "v23.1.0\n".into();
        assert!(!check(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            &exec
        )
        .unwrap()
        .ready());
        assert_eq!(
            view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap()
                ["nodeVersion"],
            "v23.1.0"
        );
    }
}

#[test]
fn missing_corrupt_and_stale_observations_are_ignored_without_get_repairs() {
    let (fixture, _) = checked_fixture(false);
    let path = node_version_path(&fixture.layout, &fixture.contract.name).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for corruption in 0..7 {
        let mut corrupt = original.clone();
        match corruption {
            0 => corrupt["schema"] = json!("unknown"),
            1 => corrupt["stateSha256"] = json!("wrong"),
            2 => corrupt["node"] = json!(fixture.layout.base.join("different-node")),
            3 => corrupt["fingerprint"]["len"] = json!(0),
            4 => corrupt["fingerprint"]["modifiedNanos"] = json!(1_000_000_000u64),
            5 => corrupt["observedNodeVersion"] = json!("recommended"),
            _ => corrupt["unknown"] = json!(true),
        }
        let bytes = serde_json::to_vec(&corrupt).unwrap();
        put(&path, &bytes);
        assert!(
            view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap()
                ["nodeVersion"]
                .is_null()
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    put(&path, b"{");
    assert!(
        view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap()["nodeVersion"]
            .is_null()
    );
    assert_eq!(fs::read(&path).unwrap(), b"{");
    fs::remove_file(&path).unwrap();
    assert!(
        view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap()["nodeVersion"]
            .is_null()
    );
    assert!(!path.exists());
    put(&path, &serde_json::to_vec(&original).unwrap());
    // Even a semantically identical rewritten state changes the exact-byte
    // binding. Strict legacy state parsing itself still succeeds.
    let state_path = state_path(&fixture.layout, &fixture.contract.name).unwrap();
    let mut state_bytes = fs::read(&state_path).unwrap();
    state_bytes.push(b'\n');
    put(&state_path, &state_bytes);
    assert!(read_state(&fixture.layout, &fixture.contract.name)
        .unwrap()
        .is_some());
    assert!(
        view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap()["nodeVersion"]
            .is_null()
    );
}

#[test]
fn changed_node_fingerprint_invalidates_observation_and_sidecar_failure_is_nonfatal() {
    let (fixture, exec) = checked_fixture(true);
    let path = node_version_path(&fixture.layout, &fixture.contract.name).unwrap();
    let bytes = fs::read(&path).unwrap();
    put(
        fixture.config.node.as_ref().unwrap(),
        b"changed sentinel executable bytes and length",
    );
    assert!(
        view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap()["nodeVersion"]
            .is_null()
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(check(
        &fixture.layout,
        &fixture.contract.name,
        &fixture.contract,
        &exec
    )
    .unwrap()
    .ready());
    assert!(
        view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap()["nodeVersion"]
            .is_null()
    );
    assert!(path.is_dir());
}

#[test]
fn environment_or_contract_changes_cannot_reuse_an_old_node_observation() {
    let fixture = Fixture::new(true);
    let env = env_for(&fixture);
    let state = save(&fixture, ExternalConfig::default(), &env);
    let node = state.resolved.node.as_ref().unwrap();
    let observation = NodeVersionObservation {
        schema: NODE_VERSION_SCHEMA.into(),
        node: node.clone(),
        fingerprint: NodeFingerprint::read(node).unwrap(),
        state_sha256: crate::upgrade::manifest::sha256_hex(
            &fs::read(state_path(&fixture.layout, &fixture.contract.name).unwrap()).unwrap(),
        ),
        observed_node_version: "v24.10.0".into(),
    };
    let path = node_version_path(&fixture.layout, &fixture.contract.name).unwrap();
    let bytes = serde_json::to_vec(&observation).unwrap();
    put(&path, &bytes);
    assert_eq!(
        view_with_env(
            &fixture.layout,
            &fixture.contract.name,
            &fixture.contract,
            &env
        )
        .unwrap()["nodeVersion"],
        "v24.10.0"
    );
    let mut changed_env = env.clone();
    changed_env.insert(
        "CUTEX_KIMI_RG".into(),
        fixture.layout.base.join("other-rg").into_os_string(),
    );
    let value = view_with_env(
        &fixture.layout,
        &fixture.contract.name,
        &fixture.contract,
        &changed_env,
    )
    .unwrap();
    assert!(value["checkedAt"].is_null());
    assert!(value["nodeVersion"].is_null());
    let mut changed_contract = fixture.contract.clone();
    changed_contract.node.recommended = "24.22.0".into();
    let value = view_with_env(
        &fixture.layout,
        &fixture.contract.name,
        &changed_contract,
        &env,
    )
    .unwrap();
    assert!(value["checkedAt"].is_null());
    assert!(value["nodeVersion"].is_null());
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn node_replaced_during_probe_does_not_acquire_the_old_observed_version() {
    struct ReplacingExec<'a> {
        inner: &'a FakeExec,
        node: &'a Path,
    }
    impl Exec for ReplacingExec<'_> {
        fn run(&self, program: &str, args: &[String]) -> Result<String> {
            let output = self.inner.run(program, args);
            if Path::new(program) == self.node && args == ["--version"] {
                // Replace only fixture bytes; never execute a real binary.
                fs::write(
                    self.node,
                    b"different fixture executable after version observation",
                )?;
            }
            output
        }
    }
    let fixture = Fixture::new(false);
    let exec = FakeExec::default();
    let replacing = ReplacingExec {
        inner: &exec,
        node: fixture.config.node.as_deref().unwrap(),
    };
    let status = configure(
        &fixture.layout,
        &fixture.contract.name,
        &fixture.contract,
        fixture.request(),
        &replacing,
    )
    .unwrap();
    // Fingerprint evidence is display-only: the existing probe result still
    // completes, but no observation is recorded for the replacement binary.
    assert!(status.ready());
    assert!(!node_version_path(&fixture.layout, &fixture.contract.name)
        .unwrap()
        .exists());
    let value = view(&fixture.layout, &fixture.contract.name, &fixture.contract).unwrap();
    assert!(value["nodeVersion"].is_null());
}
