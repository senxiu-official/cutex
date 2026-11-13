//! Real portless-process regression tests. The fixture is this test executable,
//! never a text file disguised as a system executable.
use super::*;
use crate::management::v2::services::{parse_path, route_json, ServicesContext};
use crate::services::store::{Caller, RegisterRequest, ServiceCommand, UpdateRequest};
use std::path::{Path, PathBuf};

fn request(slug: &str) -> RegisterRequest {
    serde_json::from_value(json!({
        "slug":slug,"runtime":"worker","command":["unused-worker-command"],
        "lifetime":"persistent"
    }))
    .unwrap()
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("cutex-workers-{}", uuid::Uuid::new_v4())))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn store(root: &Fixture) -> ServiceStore {
    // A worker must register even when every port is busy; never call this probe.
    ServiceStore::new(root.0.join("services"), "fixture")
        .with_port_probe(|_| panic!("a worker must not allocate a port"))
}

#[test]
fn worker_definition_has_no_endpoint_and_rejects_web_fields() {
    let root = Fixture::new();
    let store = store(&root);
    let (worker, _) = store.register(request("worker"), &Caller::Owner).unwrap();
    assert_eq!(worker.port, 0);
    assert!(!worker.port_allocated);
    assert_eq!(
        worker.idle_stop_preset,
        crate::services::idle::IdleStopPreset::Forever
    );
    assert!(store.has_previous_build_rollback_blockers().unwrap());
    let before_registry = std::fs::read(store.registry_path()).unwrap();
    let rollback = crate::upgrade::older_stores::preflight(&root.0, "0.2.2").unwrap_err();
    assert!(rollback.to_string().contains("worker services"), "{rollback}");
    assert_eq!(std::fs::read(store.registry_path()).unwrap(), before_registry);
    let view = service_view(&worker, None);
    for field in ["port", "url", "localUrl", "proxyHost", "link"] {
        assert!(view[field].is_null(), "{field}");
    }
    assert_eq!(view["runtime"], "worker");
    assert_eq!(worker.ceiling.as_deref(), Some("unrestricted"));
    for patch in [
        json!({"port":31481}),
        json!({"path":"/"}),
        json!({"root":root.0}),
        json!({"tilePath":"/tile"}),
        json!({"reveal":true}),
        json!({"loginLinkCommand":"login"}),
    ] {
        let mut value =
            serde_json::to_value(json!({"slug":"bad","runtime":"worker","command":"worker"}))
                .unwrap();
        for (key, value_in) in patch.as_object().unwrap() {
            value[key] = value_in.clone();
        }
        assert!(
            store
                .validate_register(serde_json::from_value(value).unwrap(), &Caller::Owner)
                .is_err(),
            "{patch}"
        );
    }
    assert!(store
        .update(
            "worker",
            UpdateRequest {
                runtime: Some("managed".into()),
                ..Default::default()
            },
            &Caller::Owner
        )
        .is_err());
    let tile = crate::services::tile::TileCache::default().answer(
        "fixture",
        &worker,
        false,
        Instant::now(),
        |_| panic!("no tile fetch"),
    );
    assert!(tile["tile"].is_null());
    assert!(tile["error"].is_null());
    assert_eq!(
        crate::services::proxy::auth::issue(&store, "worker", "/")
            .unwrap_err()
            .code,
        "service_has_no_view"
    );
    let command = crate::services::process::build_command(&worker, &root.0).unwrap();
    let env: std::collections::HashMap<_, _> = command
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().to_string(),
                v.map(|s| s.to_string_lossy().to_string()),
            )
        })
        .collect();
    assert_eq!(env.get("PORT"), Some(&None));
    assert_eq!(env.get("HOST"), Some(&None));
    let mut literal = worker.clone();
    literal.command = Some(ServiceCommand::Argv(vec!["worker".into(), "{port}".into()]));
    let literal = crate::services::process::build_command(&literal, &root.0).unwrap();
    assert_eq!(literal.get_args().next().unwrap(), "{port}");
}

#[test]
fn worker_agent_execution_and_update_are_gated_like_managed() {
    use crate::services::agent_api::{handle_with_policy, AgentServicePolicy};
    let root = Fixture::new();
    let store = store(&root);
    let caller = Caller::Agent {
        cutex_session_id: "cutex.00000000-0000-0000-0000-000000000001".into(),
        project_id: None,
    };
    let mut policy = AgentServicePolicy::permissive();
    policy.managed_within_envelope = false;
    let register =
        json!({"op":"register","args":{"slug":"worker","runtime":"worker","command":"work"}});
    let denied = handle_with_policy(&store, &caller, register.to_string().as_bytes(), &policy);
    assert_eq!(denied["ok"], false, "{denied}");
    assert!(store.load().unwrap().services.is_empty());
    policy.managed_within_envelope = true;
    let created = handle_with_policy(&store, &caller, register.to_string().as_bytes(), &policy);
    assert_eq!(created["ok"], true, "{created}");
    store.set_stopped("worker", true).unwrap();
    let repeated = handle_with_policy(&store, &caller, register.to_string().as_bytes(), &policy);
    assert_eq!(repeated["ok"], true, "{repeated}");
    assert!(store.get("worker").unwrap().stopped, "re-register must preserve owner stop");
    policy.managed_within_envelope = false;
    let update = json!({"op":"update","args":{"slug":"worker","command":"other"}});
    let denied = handle_with_policy(&store, &caller, update.to_string().as_bytes(), &policy);
    assert_eq!(denied["ok"], false, "{denied}");
    assert_eq!(
        store.get("worker").unwrap().command,
        Some(ServiceCommand::Shell("work".into()))
    );
}

fn wait_for(timeout: u64, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while !check() {
        assert!(Instant::now() < deadline, "worker fixture timed out");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn command(root: &Path, crash_once: bool) -> ServiceCommand {
    let _ = (root, crash_once);
    ServiceCommand::Argv(vec![
        std::env::current_exe().unwrap().display().to_string(),
        "--exact".into(),
        "services::supervisor::worker_tests::worker_process_fixture".into(),
        "--nocapture".into(),
    ])
}

#[test]
fn worker_process_fixture() {
    let Ok(root) = std::env::var("WORKER_FIXTURE_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    std::fs::create_dir_all(&root).unwrap();
    if std::env::var_os("WORKER_FIXTURE_CHILD").is_none() {
        if std::env::var_os("WORKER_FIXTURE_CRASH_ONCE").is_some()
            && std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(root.join("crashed"))
                .is_ok()
        {
            println!("worker-crash-once");
            std::process::exit(31);
        }
        let _child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::supervisor::worker_tests::worker_process_fixture",
                "--nocapture",
            ])
            .env("WORKER_FIXTURE_CHILD", "1")
            .spawn()
            .unwrap();
        println!("worker-ready");
    }
    let file = root.join(format!("heartbeat-{}", std::process::id()));
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        std::fs::write(&file, chrono::Utc::now().to_rfc3339()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Running(Arc<Supervisor>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

#[test]
fn worker_real_process_routes_restart_idle_handover_and_stop() {
    let root = Fixture::new();
    let store = store(&root);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let supervisor = Supervisor::new(
        store.clone(),
        Box::new(move |event| captured.lock().unwrap().push(event)),
    );
    let _running = Running(supervisor.clone());
    supervisor.start().unwrap();
    let context = ServicesContext {
        store: store.clone(),
        supervisor: Some(supervisor.clone()),
        owner_check: |_| Ok(()),
        caller: "fixture".into(),
        load_runtime_status: |_| Ok(None),
    };
    let call = |method: &str, path: &str, body: Value| {
        route_json(
            &context,
            method,
            path,
            &parse_path(path).unwrap(),
            body.to_string().as_bytes(),
        )
    };
    let heartbeats = root.0.join("heartbeats");
    let registered=call("POST","/v2/services",json!({"slug":"worker","runtime":"worker","command":command(&root.0,true),"lifetime":"persistent","idleStopPreset":"1h","env":{"WORKER_FIXTURE_ROOT":heartbeats,"WORKER_FIXTURE_CRASH_ONCE":"1"}})).unwrap();
    assert_eq!(registered.1["service"]["runtime"], "worker");
    wait_for(15, || {
        supervisor.logs("worker", 40).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("worker-ready")
    });
    assert!(supervisor.snapshots()["worker"].restarts >= 1);
    supervisor.probe_all();
    assert_eq!(
        supervisor.snapshots()["worker"].health,
        ServiceHealth::Reachable
    );
    assert!(supervisor.snapshots()["worker"].last_probe_at.is_none());
    wait_for(5, || {
        std::fs::read_dir(&heartbeats)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|f| f.file_name().to_string_lossy().starts_with("heartbeat-"))
            .count()
            >= 2
    });
    for path in [
        "/v2/services/worker/open",
        "/v2/services/worker/view-ticket",
    ] {
        assert_eq!(
            call("POST", path, json!({"start":true})).unwrap_err().code,
            "service_has_no_view"
        );
    }
    let old_pid = supervisor.snapshots()["worker"].pid;
    #[cfg(windows)]
    assert!(supervisor.inner.lock().unwrap().entries["worker"].running.as_ref().unwrap().tree.has_job_object(), "Windows worker must use its service Job object, not the taskkill fallback");
    #[cfg(unix)]
    assert_eq!(unsafe { libc::getpgid(old_pid.unwrap() as i32) }, old_pid.unwrap() as i32, "worker must lead a private process group");
    let handover = call(
        "PATCH",
        "/v2/services/worker",
        json!({"ownerScope":"project","projectId":"fixture-project","ownerCutexSessionId":null}),
    )
    .unwrap();
    assert_eq!(handover.1["service"]["ownerScope"], "project");
    assert_eq!(supervisor.snapshots()["worker"].pid, old_pid);
    let future = chrono::Utc::now() + chrono::Duration::hours(2);
    let lease = supervisor
        .lifecycle
        .lease("worker", None, None, future)
        .unwrap();
    supervisor.stop_idle_services_at(future);
    assert!(
        store.get("worker").unwrap().stopped_idle_at.is_none(),
        "active lease prevents idle stop"
    );
    supervisor
        .lifecycle
        .release("worker", &lease.lease_id, future);
    wait_for(5, || {
        supervisor.stop_idle_services_at(future + chrono::Duration::hours(2));
        store.get("worker").unwrap().stopped_idle_at.is_some()
    });
    call("POST", "/v2/services/worker/start", json!({})).unwrap();
    assert!(store.get("worker").unwrap().stopped_idle_at.is_none());
    assert_eq!(supervisor.view("worker").unwrap()["health"], "reachable");
    call("POST", "/v2/services/worker/restart", json!({})).unwrap();
    call("POST", "/v2/services/worker/stop", json!({})).unwrap();
    assert!(store.get("worker").unwrap().stopped);
    assert!(supervisor.snapshots()["worker"].pid.is_none());
    assert!(store.request_restart("worker", &Caller::Owner).is_err());
    // Wait beyond the Unix TERM/KILL grace, then prove parent AND child writes stop.
    std::thread::sleep(Duration::from_secs(6));
    let files = std::fs::read_dir(&heartbeats)
        .unwrap()
        .map(|f| f.unwrap().path())
        .collect::<Vec<_>>();
    let before = files
        .iter()
        .map(|f| std::fs::read(f).unwrap())
        .collect::<Vec<_>>();
    std::thread::sleep(Duration::from_millis(200));
    let after = files
        .iter()
        .map(|f| std::fs::read(f).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        before, after,
        "stopping worker must terminate the entire fixture tree"
    );
    supervisor.shutdown();
    let restarted = Supervisor::new(store.clone(), Box::new(|_| {}));
    let _second = Running(restarted.clone());
    restarted.start().unwrap();
    assert!(
        restarted.snapshots()["worker"].pid.is_none(),
        "manual stop survives daemon restart"
    );
    let events = events.lock().unwrap();
    for reason in ["registered", "started", "exited", "updated", "stopped"] {
        assert!(
            events.iter().any(|e| e.reason == reason),
            "missing {reason}"
        );
    }
}
