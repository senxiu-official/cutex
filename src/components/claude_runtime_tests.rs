use super::*;

#[test]
fn bundled_source_install_uses_the_published_release_label() {
    let handler = ClaudeRuntime::default();
    let features: serde_json::Value = serde_json::from_str(include_str!("../../components/claude-runtime/src/cute_claude/features.json")).unwrap();
    assert_eq!(handler.metadata().version, "claude-r32");
    assert_eq!(features["release"], handler.metadata().version);
}
use crate::profiles::lifecycle::tests::TestHome;

#[test]
fn metadata_is_the_component_json_and_matches_the_adapter_release() {
    let runtime = ClaudeRuntime::default();
    let meta = runtime.metadata();
    assert_eq!(meta.name, NAME);
    assert_eq!(meta.platforms, vec!["linux-x64".to_string(), "windows-x64".to_string(), "darwin-arm64".to_string()]);
    let features: Value =
        serde_json::from_str(include_str!("../../components/claude-runtime/src/cute_claude/features.json")).unwrap();
    assert_eq!(features["release"], meta.version.as_str(), "component version = features.json release (contract §0.5)");
    assert!(meta.version.starts_with("claude-r"));
    assert!(runtime.restart_gate(&Layout::at("/nonexistent")).is_ok());
    assert!(!runtime.external(&Layout::at("/nonexistent")));
}

#[test]
fn candidates_follow_path_then_the_installers_default_places() {
    let home = Path::new(if cfg!(windows) { "C:\\Users\\u" } else { "/home/u" });
    let path_var = std::env::join_paths([home.join("bin"), home.join("bin")]).unwrap();
    let list = claude_cli_candidates(Some(home), Some(&path_var), Some(&home.join("AppData")));
    let exe = if cfg!(windows) { "claude.exe" } else { "claude" };
    assert_eq!(list[0], home.join("bin").join(exe), "PATH first");
    assert_eq!(list.iter().filter(|p| **p == home.join("bin").join(exe)).count(), 1, "deduplicated");
    if cfg!(windows) {
        assert!(list.contains(&home.join(".local").join("bin").join("claude.exe")));
        assert!(list.contains(&home.join("AppData").join("npm").join("claude.cmd")));
    } else {
        assert!(list.contains(&home.join(".local/bin/claude")));
        assert!(list.contains(&home.join(".claude/local/claude")));
        assert!(list.contains(&PathBuf::from("/usr/local/bin/claude")));
    }
}

#[test]
fn layout_needs_the_files_cutex_runs() {
    let root = temp_root("layout");
    std::fs::create_dir_all(root.join("src/cute_claude")).unwrap();
    assert!(check_layout(Path::new("relative")).is_err());
    assert!(check_layout(&root).unwrap_err().to_string().contains("component.json"));
    fake_runtime(&root);
    check_layout(&root).unwrap();
    std::fs::write(root.join("component.json"), r#"{"version":"claude-r28"}"#).unwrap();
    assert!(check_layout(&root).unwrap_err().to_string().contains(BUNDLED_PYTHON_METADATA));
    std::fs::write(root.join(BUNDLED_PYTHON_METADATA), "{}").unwrap();
    assert!(check_layout(&root).unwrap_err().to_string().contains(BUNDLED_PYTHON_PAYLOAD));
    std::fs::write(root.join(BUNDLED_PYTHON_PAYLOAD), "fixture").unwrap();
    std::fs::write(root.join(BUNDLED_PYTHON_LICENSE), "PSF license fixture").unwrap();
    check_layout(&root).unwrap();
    std::fs::remove_file(root.join(LOCK_FILE)).unwrap();
    assert!(check_layout(&root).unwrap_err().to_string().contains(LOCK_FILE));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_claude_cli_is_a_user_step() {
    let options = Options { claude_cli: Some(std::env::temp_dir().join("no-such-claude-cli")), ..Options::default() };
    let error = find_claude_cli(&options).unwrap_err();
    let step = error.downcast_ref::<NeedsUser>().expect("NeedsUser");
    assert_eq!(step.code, "claude_cli_missing");
    assert!(step.steps.iter().any(|s| s.contains("claude.ai/install")), "{:?}", step.steps);
}

fn fake_claude_cli(dir: &Path, name: &str, version: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    #[cfg(windows)]
    let path = dir.join(format!("{name}.cmd"));
    #[cfg(not(windows))]
    let path = dir.join(name);
    #[cfg(windows)]
    std::fs::write(&path, format!("@echo {version}\r\n")).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[test]
fn bundled_cli_is_default_and_environment_override_wins() {
    let root = temp_root("cli-choice");
    fake_runtime(&root);
    std::fs::write(
        root.join("component.json"),
        r#"{"schema":"cutex/component/v1","name":"claude-runtime","version":"claude-r28","platforms":[]}"#,
    )
    .unwrap();
    let bundled = fake_claude_cli(&root.join("prepared/venv/Lib/site-packages/claude_agent_sdk/_bundled"), "claude", "1.2.3 (Claude Code)");
    let system = fake_claude_cli(&root.join("system"), "claude", "9.9.9 (Claude Code)");
    std::fs::write(
        prepared_dir(&root).join(PREPARE_RECEIPT),
        serde_json::to_vec(&json!({"bundledClaudeCli": bundled})).unwrap(),
    )
    .unwrap();

    let options = Options { claude_cli_choice: Some(crate::config::claude::ClaudeCliChoice::Bundled), ..Options::default() };
    let selected = resolve_claude_cli(&root, &options).unwrap();
    assert_eq!(selected.0, bundled);
    assert_eq!(selected.1, "1.2.3 (Claude Code)");
    assert_eq!(selected.2["selected"], "bundled");
    assert_eq!(selected.2["override"], false);

    let options = Options {
        claude_cli: Some(system.clone()),
        claude_cli_choice: Some(crate::config::claude::ClaudeCliChoice::Bundled),
        ..Options::default()
    };
    let selected = resolve_claude_cli(&root, &options).unwrap();
    assert_eq!(selected.0, system);
    assert_eq!(selected.1, "9.9.9 (Claude Code)");
    assert_eq!(selected.2["selected"], "system");
    assert_eq!(selected.2["override"], true);
    assert_eq!(selected.2["bundled"]["version"], "1.2.3 (Claude Code)");
    assert_eq!(selected.2["system"]["version"], "9.9.9 (Claude Code)");

    // A damaged non-selected bundled binary is diagnostic only: the explicit
    // environment/system override must still win.
    #[cfg(windows)]
    std::fs::write(&bundled, "@exit /b 1\r\n").unwrap();
    #[cfg(not(windows))]
    std::fs::write(&bundled, "#!/bin/sh\nexit 1\n").unwrap();
    let selected = resolve_claude_cli(&root, &options).unwrap();
    assert_eq!(selected.0, system);
    assert_eq!(selected.2["selected"], "system");
    assert!(selected.2["bundled"].is_null());
    assert!(selected.2["bundledError"].as_str().is_some());
    let bundled_only = Options {
        claude_cli_choice: Some(crate::config::claude::ClaudeCliChoice::Bundled),
        ..Options::default()
    };
    assert!(resolve_claude_cli(&root, &bundled_only).is_err(), "a selected damaged bundle fails closed");

    // An invalid explicit override wins and fails, but leaves a complete
    // status record instead of stale/null diagnostics.
    let invalid_override = Options {
        claude_cli: Some(root.join("missing-system-cli")),
        ..Options::default()
    };
    assert!(resolve_claude_cli(&root, &invalid_override).is_err());
    let status = read_cli_status(&root);
    assert_eq!(status["selected"], "system");
    assert!(status["systemError"].as_str().is_some());
    assert!(status["bundledError"].as_str().is_some(), "the damaged bundle is still diagnosed");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn component_cli_choice_wire_is_bounded_and_does_not_expose_paths_or_diagnostics() {
    let raw = json!({
        "selected": "system",
        "override": true,
        "bundled": {"path": "/private/bundled", "version": " 2.1.301 "},
        "bundledError": null,
        "system": {"path": "/private/system", "version": "2.1.299"},
        "systemError": null,
    });
    assert_eq!(
        cli_choice_wire(&raw),
        json!({
            "selected": "system",
            "bundled": {"version": "2.1.301"},
            "system": {"version": "2.1.299"},
        })
    );
    assert_eq!(
        cli_choice_wire(&json!({"bundled": {"version": null}, "system": null})),
        json!({"selected": "bundled", "bundled": {"version": null}, "system": null})
    );
    assert!(
        cli_choice_wire(&json!({"selected": "system", "bundled": {}, "system": null})).is_null()
    );
    assert!(
        cli_choice_wire(&json!({"selected": "bundled", "bundled": null, "system": null})).is_null()
    );
    assert_eq!(
        cli_choice_wire(&json!({"selected": "bundled", "bundled": {"version": "bad\nheader"}}))["bundled"]["version"],
        Value::Null
    );
}

#[test]
fn r29_release_policy_bundles_an_unmodified_cli_by_default_and_keeps_the_off_switch_and_notices() {
    let build = include_str!("../../components/claude-runtime/build.sh");
    assert!(build.contains(r#"${CUTEX_CLAUDE_BUNDLE_CLI:-1}"#));
    assert!(build.contains("0) ;;"));
    assert!(build.contains("CUTEX_CLAUDE_BUNDLE_CLI must be 0 or 1"));

    let vendor = include_str!("../../components/claude-runtime/scripts/vendor_wheels.py");
    assert!(vendor.contains("actual != expected_sha256"));
    assert!(vendor.contains("if bundled != [expected_cli]"));
    assert!(vendor.contains("shutil.copyfile(wheel, target)"));

    let notices = include_str!("../../components/claude-runtime/scripts/wheel_notices.py");
    assert!(notices.contains("https://www.anthropic.com/legal/commercial-terms"));
    assert!(notices.contains("https://code.claude.com/docs/en/legal-and-compliance"));
}

#[test]
fn venv_lives_in_prepared() {
    let python = venv_python(Path::new("/x"));
    if cfg!(windows) {
        assert!(python.ends_with("prepared/venv/Scripts/python.exe"));
    } else {
        assert!(python.ends_with("prepared/venv/bin/python"));
    }
}

fn temp_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("cutex-claude-runtime-{tag}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]))
}

/// The files `check_layout` wants, a fake SDK in `src/` (the import probe
/// needs no network), an empty `wheels/` and a lock without requirements.
fn fake_runtime(dir: &Path) {
    std::fs::create_dir_all(dir.join("src/cute_claude")).unwrap();
    std::fs::create_dir_all(dir.join("src/claude_agent_sdk")).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::create_dir_all(dir.join(WHEELS_DIR)).unwrap();
    std::fs::write(dir.join("component.json"), r#"{"schema":"cutex/component/v1","name":"claude-runtime","version":"claude-r27","platforms":[]}"#).unwrap();
    std::fs::write(dir.join("src/cute_claude/__init__.py"), "__version__ = '9.9.9'\n").unwrap();
    std::fs::write(dir.join("src/cute_claude/__main__.py"), "def main():\n    pass\n").unwrap();
    std::fs::write(dir.join("src/cute_claude/features.json"), r#"{"version":1,"release":"claude-r27","features":{}}"#).unwrap();
    std::fs::write(dir.join("src/claude_agent_sdk/__init__.py"), "__version__ = '0.0.0-fake'\n").unwrap();
    std::fs::write(dir.join("scripts/connect_tui.py"), "").unwrap();
    std::fs::write(dir.join(LOCK_FILE), "# python-versions: 3.11 3.12 3.13 3.14\n").unwrap();
}

/// What the generic unpack writes: every file's sha256.
fn write_receipt(dir: &Path, version: &str) {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                let rel = entry.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
                files.insert(rel, crate::upgrade::manifest::file_sha256_hex(&entry.path()).unwrap());
            }
        }
    }
    let receipt = super::super::Receipt {
        schema: super::super::RECEIPT_SCHEMA.into(),
        name: NAME.into(),
        version: version.into(),
        origin: "signed".into(),
        archive_sha256: None,
        files,
        unpacked_at: String::new(),
    };
    std::fs::write(dir.join(super::super::RECEIPT_FILE), serde_json::to_vec(&receipt).unwrap()).unwrap();
}

#[cfg(unix)]
fn fake_session(id: &str, deployment: ClaudeDeployment) {
    use crate::session::model::CutexSessionRecord;
    let mut record = CutexSessionRecord::new(id.into(), None, "host".into(), "/tmp".into(), None).unwrap();
    record.claude_session = Some(crate::runtime::claude::ClaudeSession {
        deployment,
        thread_id: format!("thread-{id}"),
        state_dir: std::env::temp_dir().join(id),
        model: "sonnet".into(),
        effort: None,
        instructions: String::new(),
        allow_write: false,
        allow_exec: false,
        write_explicit: false,
        exec_explicit: false,
        native_session_id: None,
        connection_id: None,
        initial_permission_mode: None,
    });
    let mut store = crate::session::store::load_cutex_session_store().unwrap();
    store.sessions.insert(id.to_string(), record);
    crate::session::store::save_cutex_session_store(&store).unwrap();
}

/// Real Python (skipped without 3.11+), real venv, pip `--no-index
/// --require-hashes` on an empty lock, fake `claude` and TUI.
#[cfg(unix)]
#[test]
fn prepare_select_upgrade_pins_and_deselect() {
    use std::os::unix::fs::PermissionsExt;
    let home = TestHome::new();
    let Ok(_python) = find_python(&Options::default()) else {
        eprintln!("skipped: no Python 3.11+ on PATH");
        return;
    };
    let root = temp_root("handler");
    let layout = Layout::at(root.join("base"));
    let r1 = super::super::version_dir(&layout, NAME, "claude-r98");
    let r2 = super::super::version_dir(&layout, NAME, "claude-r99");
    fake_runtime(&r1);
    fake_runtime(&r2);
    write_receipt(&r1, "claude-r98");
    write_receipt(&r2, "claude-r99");
    let claude = root.join("bin/claude");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(
        &claude,
        "#!/bin/sh\nif [ \"$1\" = auth ]; then echo '{\"loggedIn\": false}'; exit 1; fi\necho '9.9.9 (Claude Code)'\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tui = root.join("bin/codex");
    std::fs::write(&tui, "").unwrap();
    let exec = crate::upgrade::service::RealExec;
    let ctx = |dir: &Path, version: &str| Context {
        layout: &layout,
        dir: dir.to_path_buf(),
        version: version.into(),
        exec: &exec,
        previous: None,
        dry_run: false,
    };
    let entry = |version: &str| Entry { selected: version.into(), origin: "signed".into(), installed_at: String::new(), last_error: None };

    // No CLI: prepared, not selected, needsUser.
    let missing = ClaudeRuntime {
        options: Some(Options { claude_cli: Some(root.join("bin/none")), tui_binary: Some(tui.clone()), ..Options::default() }),
        ..ClaudeRuntime::default()
    };
    let prepared = missing.prepare(&ctx(&r1, "claude-r98")).unwrap();
    assert_eq!(prepared["venvCreated"], true, "{prepared}");
    assert_eq!(prepared["receipt"]["sdk"], "0.0.0-fake");
    let selected = missing.select(&ctx(&r1, "claude-r98")).unwrap();
    assert_eq!(selected["selected"], false);
    assert_eq!(selected["needsUser"]["code"], "claude_cli_missing");
    assert!(ClaudeDeployment::selected().is_err());
    let probe = missing.probe(&layout, &entry("claude-r98"));
    assert_eq!(probe["state"], states::NEEDS_USER);
    assert!(probe["steps"][0].as_str().unwrap().contains("claude.ai/install"), "{probe}");

    // With the CLI: the venv is reused, the deployment selected; the default
    // account is not logged in, so the user still has a step.
    let runtime = ClaudeRuntime {
        options: Some(Options { claude_cli: Some(claude.clone()), tui_binary: Some(tui.clone()), ..Options::default() }),
        ..ClaudeRuntime::default()
    };
    assert_eq!(runtime.prepare(&ctx(&r1, "claude-r98")).unwrap()["venvCreated"], false);
    let selected = runtime.select(&ctx(&r1, "claude-r98")).unwrap();
    assert!(!prepared_dir(&r1).join(NEEDS_USER_FILE).exists(), "a resolved step is cleared");
    assert_eq!(selected["selected"], true, "{selected}");
    assert_eq!(selected["claudeCliVersion"], "9.9.9 (Claude Code)");
    assert_eq!(selected["needsUser"]["code"], "claude_login_required");
    let first = ClaudeDeployment::selected().unwrap();
    assert_eq!(first.adapter_source, r1);
    assert_eq!(first.python, venv_python(&r1));
    assert_eq!(first.claude_home, crate::config::paths::config_dir().unwrap().join("claude-home"));
    let written: ClaudeDeployment =
        serde_json::from_slice(&std::fs::read(prepared_dir(&r1).join(DEPLOYMENT_FILE)).unwrap()).unwrap();
    assert_eq!(written, first);
    let probe = runtime.probe(&layout, &entry("claude-r98"));
    assert_eq!(probe["state"], states::NEEDS_USER, "cached: not logged in");
    assert!(probe["steps"][0].as_str().unwrap().contains("account login default"), "{probe}");
    crate::runtime::claude_accounts::update_registry(|registry| {
        registry.default_status.as_mut().unwrap().logged_in = true;
        Ok(())
    })
    .unwrap();
    let health = runtime.probe(&layout, &entry("claude-r98"));
    assert_eq!(health["state"], states::READY, "{health}");

    // A session created on r98 pins it; an update to r99 selects r99 for new
    // sessions only, keeps the login home, and never touches the session.
    let login = root.join("existing-login");
    std::fs::create_dir_all(&login).unwrap();
    ClaudeDeployment { claude_home: login.clone(), ..first.clone() }.install().unwrap();
    let pinned = ClaudeDeployment::selected().unwrap();
    fake_session("cutex.pinned-on-r98", pinned.clone());
    assert_eq!(runtime.pins(&layout), BTreeMap::from([("claude-r98".to_string(), 1)]));
    runtime.prepare(&ctx(&r2, "claude-r99")).unwrap();
    assert_eq!(runtime.select(&ctx(&r2, "claude-r99")).unwrap()["selected"], true);
    let second = ClaudeDeployment::selected().unwrap();
    assert_eq!(second.adapter_source, r2);
    assert_eq!(second.claude_home, login, "an update keeps the login home");
    let store = crate::session::store::load_cutex_session_store().unwrap();
    assert_eq!(store.sessions["cutex.pinned-on-r98"].claude_session.as_ref().unwrap().deployment, pinned, "never repinned");
    assert!(r1.join("prepared/venv").is_dir(), "the old version stays");
    assert_eq!(runtime.pins(&layout), BTreeMap::from([("claude-r98".to_string(), 1)]));
    assert_eq!(runtime.probe(&layout, &entry("claude-r98"))["state"], states::DEGRADED, "r98 is no longer selected");

    // Remove = deselect for new sessions only; the selected version alone.
    assert!(runtime.deselect(&ctx(&r1, "claude-r98"), false).unwrap().is_empty());
    assert_eq!(runtime.deselect(&ctx(&r2, "claude-r99"), false).unwrap().len(), 1);
    assert!(ClaudeDeployment::selected().is_err());
    assert!(r2.join("prepared/venv").is_dir(), "files stay");
    let store = crate::session::store::load_cutex_session_store().unwrap();
    assert_eq!(store.sessions["cutex.pinned-on-r98"].claude_session.as_ref().unwrap().deployment, pinned);

    // A changed archive file is refused before anything runs.
    std::fs::write(r1.join("src/cute_claude/__main__.py"), "evil = 1\n").unwrap();
    let error = runtime.prepare(&ctx(&r1, "claude-r98")).unwrap_err();
    assert!(format!("{error:#}").contains("does not match its receipt"), "{error:#}");
    std::fs::write(r1.join("src/cute_claude/__main__.py"), "def main():\n    pass\n").unwrap();

    // An interrupted venv is rebuilt.
    std::fs::remove_file(prepared_dir(&r1).join("venv").join(VENV_READY)).unwrap();
    assert_eq!(runtime.prepare(&ctx(&r1, "claude-r98")).unwrap()["venvCreated"], true);
    drop(home);
    let _ = std::fs::remove_dir_all(&root);
}

/// End to end in a private home (not part of the normal run):
///
///   scripts/e2e-claude-runtime.sh <work-dir> [<official claude CLI>]
///
/// (builds both archives, sets up the private home and the environment).
///
/// A signed test release names the old version; the generic install downloads it from a
/// local source, verifies and unpacks it and runs this handler (venv from the
/// archive's wheels only). The debug `cutex` creates and starts a Claude agent
/// on it. Release 0.1.2 names the new one: installing it selects that for new
/// agents while the old agent stays pinned and still starts; remove deselects only.
/// The official CLI runs only `--version` and `auth status` against the
/// private, empty login home: no login, no model request.
#[cfg(unix)]
#[test]
#[ignore]
fn e2e_signed_install_upgrade_with_a_pinned_session_and_remove() {
    use crate::components::{install, prune, remove, InstallRequest, Lock};
    use crate::upgrade::manifest::{self as m, Artifact, ComponentPart};
    use crate::upgrade::minisign::test_key::TestKey;
    use crate::upgrade::service::RealExec;
    use crate::upgrade::stage::fixtures::{installed_layout, temp};
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let archives = PathBuf::from(var("CUTEX_E2E_CLAUDE_ARCHIVES"));
    let cutex = PathBuf::from(var("CUTEX_E2E_CUTEX_BIN"));
    let home = PathBuf::from(var("CUTEX_TEST_PRIVATE_HOME"));
    assert_eq!(std::env::var_os("HOME").map(PathBuf::from), Some(home.clone()), "HOME must be the private home");
    assert!(home.join(".cutex-test-private-home").is_file());
    let platform = m::platform_key();
    let versions = std::env::var("CUTEX_E2E_VERSIONS").unwrap_or_else(|_| "claude-r13 claude-r14".into());
    let (v1, v2) = versions.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string())).expect("CUTEX_E2E_VERSIONS = \"<old> <new>\"");
    let tui = temp("e2e-tui").join("codex");
    std::fs::write(&tui, "").unwrap();
    unsafe {
        std::env::set_var(CLAUDE_CLI_ENV, var("CUTEX_E2E_CLAUDE_CLI"));
        std::env::set_var(TUI_ENV, &tui);
    }

    let key = TestKey::new(11);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("e2e-source");
    crate::upgrade::state::Settings { source: Some(source.display().to_string()), ..Default::default() }.save(&layout).unwrap();
    let ship = |release: &str, sequence: u64, version: &str| {
        let name = format!("component-claude-runtime-{version}-{platform}.tar.gz");
        let bytes = std::fs::read(archives.join(&name)).unwrap();
        let tag = format!("component-claude-runtime-{version}");
        std::fs::create_dir_all(source.join(&tag)).unwrap();
        std::fs::write(source.join(&tag).join(&name), &bytes).unwrap();
        let mut manifest = m::fixtures::manifest(release, sequence);
        manifest.components.insert(
            NAME.into(),
            ComponentPart {
                version: version.into(),
                artifacts: [(platform.clone(), Artifact { tag: Some(tag), name, sha256: m::sha256_hex(&bytes), size: bytes.len() as u64 })].into(),
            },
        );
        let doc = serde_json::to_vec_pretty(&manifest).unwrap();
        let dir = layout.release_dir(release);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), &doc).unwrap();
        std::fs::write(dir.join("manifest.json.minisig"), key.sign(&doc, true, "file:manifest.json")).unwrap();
    };
    let run = |args: &[&str]| -> Value {
        let out = std::process::Command::new(&cutex).args(args).env("HOME", &home).env("CUTEX_TEST_PRIVATE_HOME", &home).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "cutex {args:?}: {}{}", stdout, String::from_utf8_lossy(&out.stderr));
        serde_json::from_str(&stdout).unwrap_or_else(|_| json!({"text": stdout}))
    };
    let workspace = temp("e2e-workspace");

    // 1. Install r13 from the signed release.
    ship("0.1.1", 50, v1.as_str());
    let installed = install(Some(&layout), &keys, NAME, &InstallRequest::default(), &RealExec, Lock::Acquire).unwrap();
    eprintln!("install r13: {installed:#}");
    let r13 = super::super::version_dir(&layout, NAME, v1.as_str());
    assert_eq!(installed["component"]["selected"], v1.as_str());
    assert_eq!(installed["result"]["select"]["selected"], true, "{installed:#}");
    assert_eq!(installed["result"]["select"]["needsUser"]["code"], "claude_login_required");
    assert_eq!(installed["component"]["state"], states::NEEDS_USER);
    assert_eq!(ClaudeDeployment::selected().unwrap().adapter_source, r13);
    assert!(r13.join("prepared/venv").is_dir() && r13.join("receipt.json").is_file());

    // 2. A Claude agent on r13: created, its runtime starts (no login, no prompt).
    let created = run(&["human", "claude", "new", "e2e-on-r13", "--model", "sonnet", "--cwd", workspace.to_str().unwrap()]);
    let id = created["cutex_session_id"].as_str().or_else(|| created["cutexSessionId"].as_str()).expect("session id").to_string();
    let started = run(&["human", "claude", "start", &id]);
    eprintln!("start on r13: {started}");
    let status = run(&["human", "claude", "status", &id]);
    eprintln!("status on r13: {status}");
    assert!(matches!(status["state"].as_str(), Some("waiting" | "idle" | "ready")), "{status}");
    let accounts = run(&["human", "claude", "account", "list"]);
    eprintln!("accounts: {accounts}");
    run(&["human", "claude", "close", &id]);

    // 3. Update: release 0.1.2 names r14; r14 is selected for new agents only.
    ship("0.1.2", 51, v2.as_str());
    std::fs::copy(layout.release_executable("0.1.1"), layout.release_executable("0.1.2")).unwrap();
    crate::upgrade::layout::point_current(&layout, "0.1.2").unwrap();
    let updated = install(Some(&layout), &keys, NAME, &InstallRequest::default(), &RealExec, Lock::Acquire).unwrap();
    eprintln!("install r14: {}", updated["component"]);
    let r14 = super::super::version_dir(&layout, NAME, v2.as_str());
    assert_eq!(updated["component"]["selected"], v2.as_str());
    assert_eq!(ClaudeDeployment::selected().unwrap().adapter_source, r14);
    assert!(r13.join("prepared/venv/bin/python").is_file(), "r13 stays");
    let versions = updated["component"]["versions"].as_array().unwrap().clone();
    let pinned = |v: &str| versions.iter().find(|x| x["version"] == v).map(|x| x["pinnedBy"].clone());
    assert_eq!(pinned(v1.as_str()), Some(json!(1)), "{versions:?}");
    let store = crate::session::store::load_cutex_session_store().unwrap();
    assert_eq!(store.sessions[&id].claude_session.as_ref().unwrap().deployment.adapter_source, r13, "never repinned");
    run(&["human", "claude", "start", &id]);
    let status = run(&["human", "claude", "status", &id]);
    eprintln!("status on r13 after the update: {status}");
    assert!(matches!(status["state"].as_str(), Some("waiting" | "idle" | "ready")), "{status}");
    run(&["human", "claude", "close", &id]);
    let created14 = run(&["human", "claude", "new", "e2e-on-r14", "--model", "sonnet", "--cwd", workspace.to_str().unwrap()]);
    let id14 = created14["cutex_session_id"].as_str().or_else(|| created14["cutexSessionId"].as_str()).unwrap().to_string();
    let store = crate::session::store::load_cutex_session_store().unwrap();
    assert_eq!(store.sessions[&id14].claude_session.as_ref().unwrap().deployment.adapter_source, r14);

    // 4. Remove = deselect for new sessions; the pinned agent still starts; prune keeps pinned r13.
    let removed = remove(Some(&layout), &keys, NAME, false, false, &RealExec, Lock::Acquire).unwrap();
    eprintln!("remove: {removed}");
    assert!(ClaudeDeployment::selected().is_err());
    run(&["human", "claude", "start", &id]);
    run(&["human", "claude", "close", &id]);
    let pruned = prune(Some(&layout), &keys, NAME, true).unwrap();
    eprintln!("prune (dry run): {pruned:?}");
    assert!(!pruned.iter().any(|d| d.ends_with(&v1)), "a pinned version is never pruned");
    eprintln!("e2e layout: {}", layout.base.display());
}

#[test]
fn a_missing_python_is_a_user_step_with_steps() {
    let home = TestHome::new();
    let root = temp_root("python");
    let layout = Layout::at(root.join("base"));
    let dir = super::super::version_dir(&layout, NAME, "claude-r98");
    fake_runtime(&dir);
    write_receipt(&dir, "claude-r98");
    let exec = crate::upgrade::service::RealExec;
    let ctx = Context { layout: &layout, dir: dir.clone(), version: "claude-r98".into(), exec: &exec, previous: None, dry_run: false };
    let runtime = ClaudeRuntime { options: Some(Options { python: Some(root.join("no-python")), ..Options::default() }), ..ClaudeRuntime::default() };
    let prepared = runtime.prepare(&ctx).unwrap();
    assert_eq!(prepared["prepared"], false);
    assert_eq!(prepared["needsUser"]["code"], "python_missing");
    let selected = runtime.select(&ctx).unwrap();
    assert_eq!(selected["selected"], false);
    assert_eq!(selected["needsUser"]["code"], "python_missing");
    let entry = Entry { selected: "claude-r98".into(), origin: "signed".into(), installed_at: String::new(), last_error: None };
    let probe = runtime.probe(&layout, &entry);
    assert_eq!(probe["state"], states::NEEDS_USER);
    let steps: Vec<&str> = probe["steps"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(steps[0].contains("Python 3.11"), "{steps:?}");
    assert!(steps.iter().any(|s| s.contains(PYTHON_ENV)), "{steps:?}");
    drop(home);
    let _ = std::fs::remove_dir_all(&root);
}

/// Report from a colleague's install: a `<version>/prepared/needs-user.json` written into a version
/// dir that was never unpacked would leave a half dir that every later fetch refuses. A user step is
/// only recorded into an unpacked version (receipt.json); probe / select / prepare never create the
/// version dir, and a dry run of a version not downloaded yet is not refused.
#[test]
fn a_user_step_never_creates_a_version_dir_that_was_not_unpacked() {
    let home = TestHome::new();
    let root = temp_root("not-unpacked");
    let layout = Layout::at(root.join("base"));
    let dir = super::super::version_dir(&layout, NAME, "claude-r98");
    let step = || needs_user("python_missing", "no Python", python_steps(false));
    let out = user_step_or(&dir, step(), |step| json!({"needsUser": step})).unwrap();
    assert_eq!(out["needsUser"]["code"], "python_missing");
    assert!(!dir.exists(), "no marker, no prepared/ in a dir that was not unpacked");
    // Same for a dir that exists but has no receipt (an unpack that never finished).
    std::fs::create_dir_all(&dir).unwrap();
    user_step_or(&dir, step(), |step| step).unwrap();
    assert!(!prepared_dir(&dir).exists());
    std::fs::remove_dir_all(&dir).unwrap();

    let exec = crate::upgrade::service::RealExec;
    let runtime = ClaudeRuntime { options: Some(Options { python: Some(root.join("no-python")), ..Options::default() }), ..ClaudeRuntime::default() };
    let ctx = |dry_run| Context { layout: &layout, dir: dir.clone(), version: "claude-r98".into(), exec: &exec, previous: None, dry_run };
    assert!(format!("{:#}", runtime.prepare(&ctx(false)).unwrap_err()).contains("component.json"));
    assert!(runtime.select(&ctx(false)).is_err());
    let entry = Entry { selected: "claude-r98".into(), origin: "signed".into(), installed_at: String::new(), last_error: None };
    let probe = runtime.probe(&layout, &entry);
    assert_eq!(probe["state"], states::DEGRADED, "{probe}");
    assert!(probe["problem"].as_str().unwrap().contains("cutex component install claude-runtime"), "{probe}");
    let dry = runtime.prepare(&ctx(true)).unwrap();
    assert_eq!(dry["unpacked"], false, "{dry}");
    assert!(!dir.exists(), "nothing above created the version dir");

    // An unpacked version still records the step (probe reads it).
    fake_runtime(&dir);
    write_receipt(&dir, "claude-r98");
    user_step_or(&dir, step(), |step| step).unwrap();
    assert_eq!(recorded_needs_user(&dir).unwrap()["code"], "python_missing");
    // Files missing from an unpacked version: degraded with the repair hint, not the stale step.
    std::fs::remove_file(dir.join("src/cute_claude/__main__.py")).unwrap();
    let probe = runtime.probe(&layout, &entry);
    assert_eq!(probe["state"], states::DEGRADED, "{probe}");
    assert!(probe["problem"].as_str().unwrap().contains("__main__.py"), "{probe}");
    drop(home);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn needs_user_marker_is_written_atomically() {
    use crate::config::atomic::test_hook::{self, Stage};
    let dir = temp_root("needs-user-atomic");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(crate::components::RECEIPT_FILE), "{}").unwrap();
    let marker = prepared_dir(&dir).join(NEEDS_USER_FILE);
    let stages = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = stages.clone();
    let expected = marker.clone();
    let guard = test_hook::observe(move |stage, tmp| {
        assert_eq!(tmp.parent(), expected.parent(), "temp marker sits next to the marker");
        assert!(!expected.exists(), "the marker appears only by the rename");
        seen.borrow_mut().push(stage);
    });
    let step = NeedsUser { code: "claude_cli_missing", message: "install".into(), steps: vec!["a".into()] };
    record_needs_user(&dir, Some(&step));
    drop(guard);
    assert_eq!(*stages.borrow(), vec![Stage::Created, Stage::Written]);
    assert_eq!(recorded_needs_user(&dir).unwrap()["code"], "claude_cli_missing");
    let names: Vec<_> = std::fs::read_dir(prepared_dir(&dir)).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec![std::ffi::OsString::from(NEEDS_USER_FILE)], "no temp file left");
    record_needs_user(&dir, None);
    assert!(!marker.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_python_override_precedes_the_bundled_interpreter() {
    let Ok(host) = find_python(&Options::default()) else { return };
    let root = temp_root("python-override");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(BUNDLED_PYTHON_METADATA), "{}").unwrap();
    let selected = find_base_python(&root, &Options { python: Some(host.path.clone()), ..Options::default() }).unwrap();
    assert_eq!(selected.source, "override");
    assert_eq!(selected.path, host.path);
    let _ = std::fs::remove_dir_all(root);
}

/// Real release archive, real standalone Python and real offline wheel install. Run alone with a
/// platform-matching archive; PATH is emptied so this cannot silently fall back to host Python.
#[test]
#[ignore]
fn e2e_release_archive_prepares_with_bundled_python_and_cli_offline() {
    let archive = PathBuf::from(std::env::var("CUTEX_E2E_CLAUDE_ARCHIVE").expect("set CUTEX_E2E_CLAUDE_ARCHIVE"));
    let home = PathBuf::from(std::env::var("CUTEX_TEST_PRIVATE_HOME").expect("set CUTEX_TEST_PRIVATE_HOME"));
    assert_eq!(std::env::var_os("HOME").map(PathBuf::from), Some(home.clone()));
    assert!(home.join(".cutex-test-private-home").is_file());
    let version = ClaudeRuntime::default().metadata().version.clone();
    let root = home.join(format!("{version}-offline-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    let layout = Layout::at(root.join("install-base"));
    let dir = super::super::version_dir(&layout, NAME, &version);
    crate::upgrade::archive::unpack_tar_gz_file(&archive, &dir).unwrap();
    write_receipt(&dir, &version);

    struct RestoreEnv(&'static str, Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            unsafe {
                match self.1.take() {
                    Some(value) => std::env::set_var(self.0, value),
                    None => std::env::remove_var(self.0),
                }
            }
        }
    }
    let empty_path = root.join("empty-path");
    std::fs::create_dir_all(&empty_path).unwrap();
    let restore_path = RestoreEnv("PATH", std::env::var_os("PATH"));
    let restore_appdata = RestoreEnv("APPDATA", std::env::var_os("APPDATA"));
    unsafe {
        std::env::set_var("PATH", &empty_path);
        std::env::set_var("APPDATA", &empty_path);
    }

    let exec = crate::upgrade::service::RealExec;
    let ctx = Context { layout: &layout, dir: dir.clone(), version: version.into(), exec: &exec, previous: None, dry_run: false };
    let tui = root.join(if cfg!(windows) { "codex.exe" } else { "codex" });
    std::fs::write(&tui, b"").unwrap();
    let runtime = ClaudeRuntime {
        options: Some(Options { tui_binary: Some(tui), ..Options::default() }),
        ..ClaudeRuntime::default()
    };
    let prepared = runtime.prepare(&ctx).unwrap();
    assert_eq!(prepared["venvCreated"], true, "{prepared}");
    assert_eq!(prepared["receipt"]["basePythonSource"], "bundled", "{prepared}");
    assert_eq!(prepared["receipt"]["basePythonVersion"], "3.13.17", "{prepared}");
    assert!(venv_python(&dir).is_file());
    assert!(bundled_python_path(&dir).is_file());
    let selected = runtime.select(&ctx).unwrap();
    assert_eq!(selected["selected"], true, "{selected}");
    assert_eq!(selected["claudeCli"]["selected"], "bundled", "{selected}");
    assert_eq!(selected["claudeCli"]["override"], false, "{selected}");
    assert_eq!(selected["claudeCli"]["system"], Value::Null, "{selected}");
    assert_eq!(selected["claudeCliVersion"], "2.1.299 (Claude Code)", "{selected}");
    assert!(ClaudeDeployment::selected().unwrap().claude_cli.to_string_lossy().contains("_bundled"));
    drop(restore_appdata);
    drop(restore_path);
    let _ = std::fs::remove_dir_all(root);
}
