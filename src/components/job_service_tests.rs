//! The Job Service handler against a private test home: plan, credentials,
//! unit, restart gate, deselect. systemctl is faked; a stand-in daemon
//! answers on the socket where a test needs one.
use super::*;
use crate::upgrade::layout::{InstallRecord, CUTEX_EXE, FACADE_EXE, INSTALL_SCHEMA};
use crate::upgrade::service::fake::FakeExec;
use std::os::unix::fs::PermissionsExt;

const V1: &str = "0.1.0";
const V2: &str = "0.1.1";

/// A private test home (`HOME`) plus a managed install with a selected v4
/// runtime and job-service versions V1 / V2 unpacked. Holds the shared HOME
/// lock for its lifetime.
struct Fixture {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous_home: Option<std::ffi::OsString>,
    root: PathBuf,
    layout: Layout,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.previous_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_exe(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A version dir as `fetch` leaves it (binary + receipt).
fn unpack(layout: &Layout, version: &str) -> PathBuf {
    let dir = super::super::version_dir(layout, NAME, version);
    write_exe(&dir.join(JOB_EXE), format!("job-service-{version}").as_bytes());
    let receipt = super::super::Receipt {
        schema: super::super::RECEIPT_SCHEMA.into(),
        name: NAME.into(),
        version: version.into(),
        origin: super::super::ORIGIN_SIGNED.into(),
        archive_sha256: Some("0".repeat(64)),
        files: super::super::file_hashes(&dir).unwrap(),
        unpacked_at: "2026-10-16T00:00:00Z".into(),
    };
    std::fs::write(dir.join(super::super::RECEIPT_FILE), serde_json::to_vec(&receipt).unwrap()).unwrap();
    dir
}

fn fixture() -> Fixture {
    let guard = crate::config::paths::test_home_env_lock();
    let root = std::env::temp_dir().join(format!("cutex-jobsvc-{}", uuid::Uuid::new_v4().simple()));
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".cutex/codex-home")).unwrap();
    std::fs::write(home.join(".cutex/codex-home/config.toml"), b"").unwrap();
    let previous_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &home) };
    let layout = Layout::at(root.join("b"));
    let release = "0.1.1";
    let dir = layout.release_dir(release);
    write_exe(&dir.join(CUTEX_EXE), b"#!/bin/sh\n");
    write_exe(&dir.join(FACADE_EXE), b"facade");
    crate::upgrade::layout::point_current(&layout, release).unwrap();
    for (version, cli) in [("0.160.0-r52", "old-cli"), ("0.162.1-r53", "new-cli")] {
        let n = layout.native_dir(version);
        write_exe(&n.join("codex-app-server"), format!("srv-{version}").as_bytes());
        write_exe(&n.join("codex"), cli.as_bytes());
        write_exe(&n.join("codex-code-mode-host"), b"host");
        std::fs::write(n.join("codex_app_server_protocol.schemas.json"), b"{}").unwrap();
    }
    layout
        .write_install(&InstallRecord {
            schema: INSTALL_SCHEMA.into(),
            os: "linux".into(),
            service_manager: "systemd".into(),
            channel: "release".into(),
            created_at: "2026-10-16T00:00:00Z".into(),
            active_release: release.into(),
            previous_release: None,
            unit_dir: Some(root.join("cutex-units").display().to_string()),
            task_name: None,
            extra: Default::default(),
        })
        .unwrap();
    // A selected v4 runtime on the newer native (as `cutex human install-runtime`).
    let n = layout.native_dir("0.162.1-r53").canonicalize().unwrap();
    let pin = |p: PathBuf| {
        let p = p.canonicalize().unwrap();
        json!({"path": p, "sha256": crate::agent_management::file_sha256(&p).unwrap().as_str()})
    };
    let codex_home = home.join(".cutex/codex-home").canonicalize().unwrap();
    let bundle = json!({
        "version": 4,
        "upstream_commit": "0000000000000000000000000000000000000000",
        "native_patch_commit": "0000000000000000000000000000000000000000",
        "executable": pin(n.join("codex-app-server")),
        "cli": pin(n.join("codex")),
        "code_mode_host": pin(n.join("codex-code-mode-host")),
        "facade": pin(dir.join(FACADE_EXE)),
        "schema": pin(n.join("codex_app_server_protocol.schemas.json")),
        "shared_config": pin(codex_home.join("config.toml")),
    });
    let manifest = dir.join("manifests/default.json");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(&manifest, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    crate::launch::local_deployment::LocalDeployment { native_home: codex_home, bundle_manifest: manifest.canonicalize().unwrap(), job_mcp: None }
        .install()
        .unwrap();
    unpack(&layout, V1);
    unpack(&layout, V2);
    Fixture { _guard: guard, previous_home, root, layout }
}

fn code(error: &anyhow::Error) -> &'static str {
    error.downcast_ref::<UpgradeError>().map(|e| e.code).unwrap_or("<other>")
}

fn fake_descriptor(plan: &Plan, _: Duration) -> anyhow::Result<JobMcpDescriptor> {
    Ok(JobMcpDescriptor {
        version: 1,
        adapter: verified(&plan.binary).unwrap(),
        launcher: plan.launcher.clone(),
        endpoint: plan.paths.socket.clone(),
        api_token_file: plan.paths.api_token.clone(),
        grant_key_file: plan.paths.grant_key.clone(),
        daemon_pid: 4242,
        daemon_start_ticks: 7,
    })
}

fn handler() -> JobService {
    JobService { ready: fake_descriptor, ..Default::default() }
}

fn ctx<'a>(f: &'a Fixture, exec: &'a FakeExec, version: &str, dry_run: bool) -> Context<'a> {
    Context {
        layout: &f.layout,
        dir: super::super::version_dir(&f.layout, NAME, version),
        version: version.into(),
        exec,
        previous: None,
        dry_run,
    }
}

/// A stand-in daemon on the job socket answering `capabilities` (this test
/// process: its executable is not under the components dir).
fn fake_daemon(paths: &Paths) -> std::thread::JoinHandle<()> {
    use std::io::{BufRead, Write};
    std::fs::create_dir_all(&paths.root).unwrap();
    let _ = std::fs::remove_file(&paths.socket);
    let listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
    std::thread::spawn(move || {
        for _ in 0..64 {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut line = String::new();
            let _ = std::io::BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
            if line.contains("\"capabilities\"") {
                let _ = writeln!(stream, "{}", json!({"ok": true, "result": {"sources": ["nci"], "externalNotifyFile": true}}));
            }
        }
    })
}

fn stop_daemon(paths: &Paths, daemon: std::thread::JoinHandle<()>) {
    let _ = std::fs::remove_file(&paths.socket);
    drop(daemon);
}

fn jobs(paths: &Paths, value: Value) {
    std::fs::create_dir_all(&paths.root).unwrap();
    std::fs::write(&paths.state, serde_json::to_vec(&json!({"version": 3, "jobs": value})).unwrap()).unwrap();
}

#[test]
fn plan_pins_the_version_binary_and_always_offers_nci() {
    let f = fixture();
    let plan = handler().plan(&f.layout, &super::super::version_dir(&f.layout, NAME, V1)).unwrap();
    assert!(plan.binary.ends_with(format!("components/job-service/{V1}/cutex-job-service")), "{}", plan.binary.display());
    assert!(!plan.unit_text.contains("/current/"), "{}", plan.unit_text);
    assert!(plan.unit_text.starts_with(UNIT_MARKER));
    assert!(plan.unit_text.contains("After=cutex-agent-bus.service"));
    assert!(plan.unit_text.contains("Restart=on-failure") && plan.unit_text.contains("UMask=0077"));
    assert_eq!(plan.argv[1], "serve");
    assert_eq!(plan.argv[2], plan.paths.root.display().to_string());
    assert!(plan.argv[6].ends_with("natives/0.162.1-r53/codex"), "launcher = selected runtime's codex");
    let joined = plan.argv.join(" ");
    assert!(joined.contains("--allow-launcher") && joined.contains("natives/0.160.0-r52/codex"), "{joined}");
    assert!(joined.contains(&format!("--completion-v2 {} {}", plan.bus_url, plan.paths.completion_token.display())));
    assert!(joined.ends_with(&format!("--external-source nci --external-notify-file {}", plan.paths.notify_file.display())));
    assert!(plan.paths.notify_file.ends_with(".cutex/runtime/inbound-mail/notify-address-v1.json"));
    assert!(plan.paths.root.ends_with(".cutex/runtime/job-service/v1"));
}

#[test]
fn refusals_name_what_is_missing() {
    let f = fixture();
    let h = handler();
    // A binary that differs from its receipt.
    let dir = super::super::version_dir(&f.layout, NAME, V1);
    std::fs::write(dir.join(JOB_EXE), b"tampered").unwrap();
    assert_eq!(code(&h.plan(&f.layout, &dir).unwrap_err()), "component_unverified");
    let unmanaged = Layout::at(f.root.join("elsewhere"));
    assert_eq!(code(&h.plan(&unmanaged, &dir).unwrap_err()), "unmanaged_install");
    std::fs::remove_file(crate::config::paths::runtime_dir().unwrap().join("light/deployment.json")).unwrap();
    let dir2 = super::super::version_dir(&f.layout, NAME, V2);
    assert_eq!(code(&h.plan(&f.layout, &dir2).unwrap_err()), "no_runtime_selected");
    let entry = super::super::Entry { selected: V2.into(), origin: "signed".into(), ..Default::default() };
    let probe = h.probe(&f.layout, &entry);
    assert_eq!(probe["state"], states::NEEDS_USER, "{probe}");
    assert!(probe["steps"][0].as_str().unwrap().contains("install-runtime"));
}

#[test]
fn prepare_creates_private_credentials_once_and_refuses_foreign_units() {
    let f = fixture();
    let h = handler();
    let exec = FakeExec::default();
    let paths = Paths::current().unwrap();
    h.prepare(&ctx(&f, &exec, V1, true)).unwrap();
    assert!(!paths.api_token.exists(), "a dry run writes nothing");
    // A dry run plans a version that is not downloaded yet.
    let mut later = ctx(&f, &exec, "0.2.0", true);
    later.dir = super::super::version_dir(&f.layout, NAME, "0.2.0");
    h.prepare(&later).unwrap();
    h.select(&later).unwrap();
    later.dry_run = false;
    assert!(h.prepare(&later).is_err(), "a real run needs the receipt");
    h.prepare(&ctx(&f, &exec, V1, false)).unwrap();
    for file in [&paths.api_token, &paths.grant_key, &paths.completion_token] {
        let meta = std::fs::metadata(file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{}", file.display());
        assert!(meta.len() >= 32);
    }
    assert_eq!(std::fs::metadata(&paths.root).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(std::fs::read_to_string(&paths.completion_token).unwrap().starts_with("cutex-job-service-"));
    let token = std::fs::read(&paths.api_token).unwrap();
    let again = h.prepare(&ctx(&f, &exec, V1, false)).unwrap();
    assert_eq!(std::fs::read(&paths.api_token).unwrap(), token, "credentials are kept");
    assert_eq!(again["actions"], json!([]));
    assert!(exec.calls.lock().unwrap().is_empty());
    // A unit cutex did not write.
    let unit = default_unit_dir().unwrap().join(JOB_UNIT);
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "[Service]\nExecStart=/owner/job serve\n").unwrap();
    assert_eq!(code(&h.prepare(&ctx(&f, &exec, V1, false)).unwrap_err()), "component_external");
    assert!(h.external(&f.layout));
    assert_eq!(code(&h.deselect(&ctx(&f, &exec, V1, false), true).unwrap_err()), "component_external");
    assert!(std::fs::read_to_string(&unit).unwrap().contains("/owner/job"));
}

#[test]
fn select_restarts_selects_and_defers_while_jobs_run() {
    let f = fixture();
    let h = handler();
    let exec = FakeExec::default();
    let paths = Paths::current().unwrap();
    h.prepare(&ctx(&f, &exec, V1, false)).unwrap();
    let report = h.select(&ctx(&f, &exec, V1, false)).unwrap();
    assert_eq!((report["restarted"].clone(), report["restartPending"].clone()), (json!(true), json!(false)), "{report}");
    assert_eq!(
        exec.calls.lock().unwrap().clone(),
        vec![
            "systemctl --user daemon-reload".to_string(),
            format!("systemctl --user enable {JOB_UNIT}"),
            format!("systemctl --user restart {JOB_UNIT}"),
        ]
    );
    let unit = std::fs::read_to_string(default_unit_dir().unwrap().join(JOB_UNIT)).unwrap();
    assert!(unit.contains(&format!("components/job-service/{V1}/cutex-job-service serve")), "{unit}");
    let job = crate::launch::local_deployment::LocalDeployment::selected().unwrap().unwrap().job_mcp.expect("descriptor selected");
    assert!(job.adapter.path.ends_with(format!("{V1}/cutex-job-service")));
    assert!(job.launcher.path.ends_with("natives/0.162.1-r53/codex"));

    // A daemon answers (not V2's binary, so selecting V2 needs a restart),
    // and a local job runs: the restart is deferred.
    let daemon = fake_daemon(&paths);
    jobs(&paths, json!({
        "job-local": {"state": "running"},
        "job-nci": {"state": "running", "external": {"source": "nci"}},
        "job-done": {"state": "exited"},
    }));
    assert_eq!(active_jobs(&paths).unwrap(), vec!["job-local".to_string()]);
    assert!(h.restart_gate(&f.layout).is_err());
    exec.calls.lock().unwrap().clear();
    let report = h.select(&ctx(&f, &exec, V2, false)).unwrap();
    assert_eq!(report["restartPending"], true, "{report}");
    assert!(exec.calls.lock().unwrap().is_empty(), "no systemctl while deferred");
    assert!(unit == std::fs::read_to_string(default_unit_dir().unwrap().join(JOB_UNIT)).unwrap(), "the unit still runs V1");
    assert!(pending_path(&f.layout).is_file());
    let entry = super::super::Entry { selected: V2.into(), origin: "signed".into(), ..Default::default() };
    let probe = h.probe(&f.layout, &entry);
    assert_eq!(probe["state"], states::RESTART_PENDING, "{probe}");
    assert_eq!(probe["nci"], true);
    assert_eq!(probe["activeJobs"], 1);
    // Removing is refused while a local job runs (unless forced).
    assert_eq!(code(&h.deselect(&ctx(&f, &exec, V2, false), false).unwrap_err()), "jobs_running");

    // Only the external job left: it survives restarts and does not block.
    jobs(&paths, json!({"job-nci": {"state": "running", "external": {"source": "nci"}}}));
    assert!(h.restart_gate(&f.layout).is_ok());
    let report = h.select(&ctx(&f, &exec, V2, false)).unwrap();
    assert_eq!(report["restarted"], true, "{report}");
    assert!(!pending_path(&f.layout).exists());
    assert!(std::fs::read_to_string(default_unit_dir().unwrap().join(JOB_UNIT)).unwrap().contains(&format!("{V2}/cutex-job-service serve")));
    let job = crate::launch::local_deployment::LocalDeployment::selected().unwrap().unwrap().job_mcp.unwrap();
    assert!(job.adapter.path.ends_with(format!("{V2}/cutex-job-service")));
    // The selected descriptor pins V2.
    assert_eq!(h.pins(&f.layout).get(V2).copied(), Some(1));
    stop_daemon(&paths, daemon);

    // Deselect: unit gone, descriptor unselected, credentials and state kept.
    exec.calls.lock().unwrap().clear();
    let dry = h.deselect(&ctx(&f, &exec, V2, true), false).unwrap();
    assert!(!dry.is_empty() && exec.calls.lock().unwrap().is_empty());
    h.deselect(&ctx(&f, &exec, V2, false), false).unwrap();
    assert!(!default_unit_dir().unwrap().join(JOB_UNIT).exists());
    assert!(crate::launch::local_deployment::LocalDeployment::selected().unwrap().unwrap().job_mcp.is_none());
    assert!(paths.api_token.is_file() && paths.state.is_file());
    assert!(exec.calls.lock().unwrap().iter().any(|c| c == &format!("systemctl --user disable --now {JOB_UNIT}")));
    assert!(uninstall_unit(&exec, false).unwrap().is_empty(), "nothing left for service uninstall");
}

#[test]
fn a_service_cutex_did_not_install_is_external() {
    let f = fixture();
    let h = handler();
    let paths = Paths::current().unwrap();
    assert!(!h.external(&f.layout));
    let daemon = fake_daemon(&paths);
    assert!(h.external(&f.layout), "a daemon outside the components dir");
    stop_daemon(&paths, daemon);
    assert!(!h.external(&f.layout));
}

#[test]
fn service_uninstall_removes_our_unit_only() {
    let f = fixture();
    let h = handler();
    let exec = FakeExec::default();
    h.prepare(&ctx(&f, &exec, V1, false)).unwrap();
    h.select(&ctx(&f, &exec, V1, false)).unwrap();
    let unit = default_unit_dir().unwrap().join(JOB_UNIT);
    assert!(unit.is_file());
    assert_eq!(uninstall_unit(&exec, true).unwrap().len(), 2);
    assert!(unit.is_file(), "dry run");
    uninstall_unit(&exec, false).unwrap();
    assert!(!unit.exists());
}

#[test]
fn unit_paths_with_spaces_and_percent() {
    let argv = vec!["/tmp/a b/cutex-job-service".to_string(), "serve".into(), "/x/50%".into()];
    let text = unit_text(&argv, Path::new("/tmp/a b/v1"), "cutex-agent-bus.service");
    assert!(text.contains("\nWorkingDirectory=/tmp/a b/v1\n"), "{text}");
    assert!(text.contains("ExecStart=\"/tmp/a b/cutex-job-service\" serve /x/50%%\n"), "{text}");
}

#[test]
fn drill_label_renames_the_unit() {
    let name = |label: Option<&str>| {
        let _guard = crate::config::paths::test_home_env_lock();
        let env = crate::upgrade::service::TEST_SERVICE_NAME_ENV;
        let previous = std::env::var_os(env);
        match label {
            Some(l) => unsafe { std::env::set_var(env, l) },
            None => unsafe { std::env::remove_var(env) },
        }
        let name = unit_name().unwrap();
        match previous {
            Some(v) => unsafe { std::env::set_var(env, v) },
            None => unsafe { std::env::remove_var(env) },
        }
        name
    };
    assert_eq!(name(None), JOB_UNIT);
    assert_eq!(name(Some("jsdrill")), "cutex-jsdrill-job-service.service");
}
