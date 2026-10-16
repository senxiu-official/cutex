use cutex_job_service::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::Command;
use std::time::{Duration, Instant};

const API: &[u8] = b"api-token-for-js1-tests-32-bytes-minimum";
const GRANT: &[u8] = b"grant-key-for-js1-tests-32-bytes-minimum";

fn launcher() -> String {
    std::fs::canonicalize(test_launcher())
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
fn launcher_sha() -> String {
    static DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIGEST
        .get_or_init(|| cutex_job_service::file_sha256(std::path::Path::new(&launcher())).unwrap())
        .clone()
}
fn runner() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_cutex-job-service"))
}

fn sandbox(cwd: &std::path::Path) -> serde_json::Value {
    json!({
        "permissionProfile": {"type":"managed", "file_system":{"type":"restricted","entries":[{"path":{"type":"special","value":{"kind":"root"}},"access":"read"}]}, "network":"restricted"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": format!("file://{}", cwd.display()),
        "useLegacyLandlock": false
    })
}

fn config(root: &std::path::Path) -> ServiceConfig {
    ServiceConfig {
        external_sources: Vec::new(),
        external_stale_after_secs: 86400,
        external_notify_file: None,
        completion_enabled: false,
        state_root: root.into(),
        grant_key: GRANT.into(),
        api_token: API.into(),
        runner_executable: runner(),
        per_stream_output_bytes: 4096,
        max_read_bytes: 1024,
        cancel_grace: Duration::from_millis(150),
        max_jobs: 16,
        max_active_jobs: 4,
        allowed_launchers: [(launcher(), launcher_sha())].into_iter().collect(),
        completion_wire_version: CompletionWireVersion::V1,
    }
}

fn job_request(cwd: &std::path::Path, action: &str, script: &str) -> JobRequest {
    JobRequest {
        action_id: action.into(),
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: cwd.display().to_string(),
        environment: BTreeMap::new(),
        subscriber_cutex_session_id: "cutex.test.durable-session".into(),
        origin: ExecutionOrigin {
            runtime_agent_id: "runtime-test".into(),
            native_thread_id: "thread-test".into(),
            permission_profile_type: "managed".into(),
        },
    }
}

fn issue(request: &JobRequest) -> ExecutionGrant {
    GrantIssuer::new(GRANT)
        .unwrap()
        .issue(
            request,
            TrustedSandboxContext {
                subject_cutex_session_id: request.subscriber_cutex_session_id.clone(),
                cwd: request.cwd.clone(),
                sandbox_state: sandbox(std::path::Path::new(&request.cwd)),
                launcher_path: launcher(),
                launcher_sha256: launcher_sha(),
                operating_system_uid: unsafe { libc::geteuid() },
            },
            now(),
            60,
        )
        .unwrap()
}

fn caller(subject: &str, operation: CallerOperation, job_id: &str) -> CallerGrant {
    CallerGrantIssuer::new(GRANT)
        .unwrap()
        .issue(subject.into(), operation, job_id.into(), now(), 60)
        .unwrap()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn await_terminal(service: &JobService, id: &str) -> JobRecord {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let job = service.query(API, id).unwrap();
        if job.state.terminal() {
            return job;
        }
        assert!(Instant::now() < until, "job did not become terminal");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn configured_completion_waits_for_terminal_and_normalizes_legacy_queue() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.completion_enabled = true;
    let service = JobService::open(cfg.clone()).unwrap();
    let request = job_request(cwd.path(), "awaiting-completion", "sleep 0.3; printf done");
    let receipt = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    assert!(receipt.job.completion_delivery.enabled);
    assert_eq!(
        receipt.job.completion_delivery.state,
        CompletionDeliveryState::AwaitingTerminal
    );
    let terminal = await_terminal(&service, &receipt.job.job_id);
    assert_eq!(
        terminal.completion_delivery.state,
        CompletionDeliveryState::Ready
    );
    service
        .read_output(API, &terminal.job_id, "stdout", 0, 1024)
        .unwrap();
    assert_eq!(
        service
            .query(API, &terminal.job_id)
            .unwrap()
            .completion_delivery
            .state,
        CompletionDeliveryState::Ready
    );
    drop(service);
    let path = root.path().join("state.json");
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    old["jobs"][&terminal.job_id]["completionDelivery"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    for item in old["outbox"].as_object_mut().unwrap().values_mut() {
        item["deliveryState"] = "disabled".into();
    }
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let restored = JobService::open(cfg).unwrap();
    let actual = restored.query(API, &terminal.job_id).unwrap();
    assert!(actual.completion_delivery.enabled);
    assert_eq!(
        actual.completion_delivery.state,
        CompletionDeliveryState::Ready
    );
    assert_eq!(actual.revision, terminal.revision);
    assert_eq!(
        actual.completion_delivery.event_id,
        terminal.completion_delivery.event_id
    );
}

#[test]
fn real_subprocess_replay_output_and_outbox() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let request = job_request(cwd.path(), "action-one", "printf hello; printf error >&2");
    let grant = issue(&request);
    assert!(matches!(
        service.submit(b"wrong", request.clone(), grant.clone()),
        Err(JobError::Unauthorized(_))
    ));
    let receipt = service.submit(API, request.clone(), grant.clone()).unwrap();
    assert_eq!(receipt.job.state, JobState::Running);
    assert_eq!(
        receipt.job.request.command_summary.as_deref(),
        Some("/bin/sh -c printf hello; printf error >&2")
    );
    let job = await_terminal(&service, &receipt.job.job_id);
    assert_eq!(
        job.state,
        JobState::Exited,
        "job={job:?} stderr={:?}",
        service.read_output(API, &job.job_id, "stderr", 0, 1024)
    );
    assert_eq!(
        job.completion_delivery.state,
        CompletionDeliveryState::Disabled,
        "terminal result must remain queryable when Cutex delivery is not configured"
    );
    let observation = job.execution.as_ref().expect("gate release was observed");
    assert_eq!(observation.basis, "runner_release_to_wait_v1");
    assert!(observation.start_observed_at_epoch_millis.is_some());
    assert!(observation.exit_observed_at_epoch_millis.is_some());
    assert!(observation.observed_run_duration_millis.is_some());
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(stored["version"], 2);
    assert!(job.completion_delivery.event_id.is_some());
    assert_eq!(
        hex::decode(
            service
                .read_output(API, &job.job_id, "stdout", 0, 100)
                .unwrap()
                .bytes_hex
        )
        .unwrap(),
        b"hello"
    );
    let pending = service.pending_outbox(API).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        service.acknowledge_outbox(API, &pending[0].event_id, &pending[0].result_sha256),
        Err(JobError::Conflict(_))
    ));
    let own = caller(
        "cutex.test.durable-session",
        CallerOperation::Query,
        &job.job_id,
    );
    assert_eq!(service.query_for(API, &own, &job.job_id).unwrap(), job);
    let foreign = caller("cutex.test.other", CallerOperation::Query, &job.job_id);
    assert!(matches!(
        service.query_for(API, &foreign, &job.job_id),
        Err(JobError::Unauthorized(_))
    ));
    let wrong_operation = caller(
        "cutex.test.durable-session",
        CallerOperation::Cancel,
        &job.job_id,
    );
    assert!(matches!(
        service.query_for(API, &wrong_operation, &job.job_id),
        Err(JobError::Unauthorized(_))
    ));
    let stale = CallerGrantIssuer::new(GRANT)
        .unwrap()
        .issue(
            "cutex.test.durable-session".into(),
            CallerOperation::Query,
            job.job_id.clone(),
            now().saturating_sub(120),
            60,
        )
        .unwrap();
    assert!(matches!(
        service.query_for(API, &stale, &job.job_id),
        Err(JobError::Unauthorized(_))
    ));
    let replay = service.submit(API, request.clone(), grant).unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.job.job_id, job.job_id);
    let mut rotated_replay = request.clone();
    rotated_replay.origin.runtime_agent_id = "runtime-test-after-rotation".into();
    rotated_replay.origin.native_thread_id = "thread-test-after-rotation".into();
    let replay = service
        .submit(API, rotated_replay.clone(), issue(&rotated_replay))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.job.job_id, job.job_id);
    assert_eq!(
        replay.job.request.origin.unwrap().runtime_agent_id,
        "runtime-test",
        "replay must retain original launch provenance"
    );
    let changed = job_request(cwd.path(), "action-one", "printf changed");
    assert!(matches!(
        service.submit(API, changed.clone(), issue(&changed)),
        Err(JobError::Conflict(_))
    ));
}

#[test]
fn actual_sandbox_denies_write_and_output_is_bounded() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let outside = root.path().join("must-not-exist");
    let service = JobService::open(config(root.path())).unwrap();
    let command = format!(
        "printf '%05000d' 0; printf forbidden > {}",
        outside.display()
    );
    let request = job_request(cwd.path(), "sandbox-denial", &command);
    let receipt = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    let job = await_terminal(&service, &receipt.job.job_id);
    assert_eq!(job.state, JobState::Failed);
    assert!(!outside.exists());
    assert_eq!(job.stdout.retained_bytes, 4096);
    assert!(job.stdout.truncated);
}

#[test]
fn actual_sandbox_denies_network() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let script =
        format!("import socket; s=socket.create_connection(('127.0.0.1',{port}),1); s.close()");
    let request = JobRequest {
        action_id: "network-denial".into(),
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script],
        cwd: cwd.path().display().to_string(),
        environment: BTreeMap::new(),
        subscriber_cutex_session_id: "cutex.test.durable-session".into(),
        origin: ExecutionOrigin {
            runtime_agent_id: "runtime-test".into(),
            native_thread_id: "thread-test".into(),
            permission_profile_type: "managed".into(),
        },
    };
    let receipt = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    let job = await_terminal(&service, &receipt.job.job_id);
    assert_eq!(job.state, JobState::Failed);
    let stderr = hex::decode(
        service
            .read_output(API, &job.job_id, "stderr", 0, 1024)
            .unwrap()
            .bytes_hex,
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&stderr).contains("Operation not permitted"));
}

#[test]
fn actual_workspace_policy_allows_scoped_write_and_denies_outside() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let outside_root = tempfile::tempdir().unwrap();
    let inside = cwd.path().join("allowed");
    let outside = outside_root.path().join("denied");
    let service = JobService::open(config(root.path())).unwrap();
    let request = job_request(
        cwd.path(),
        "workspace-policy",
        &format!(
            "printf allowed > '{}'; printf denied > '{}'",
            inside.display(),
            outside.display()
        ),
    );
    let workspace_state = json!({
        "permissionProfile": {
            "type":"managed",
            "file_system": {
                "type":"restricted",
                "entries":[
                    {"path":{"type":"special","value":{"kind":"root"}},"access":"read"},
                    {"path":{"type":"path","path":cwd.path().display().to_string()},"access":"write"}
                ]
            },
            "network":"restricted"
        },
        "codexLinuxSandboxExe":null,
        "sandboxCwd":format!("file://{}", cwd.path().display()),
        "useLegacyLandlock":false
    });
    let grant = GrantIssuer::new(GRANT)
        .unwrap()
        .issue(
            &request,
            TrustedSandboxContext {
                subject_cutex_session_id: request.subscriber_cutex_session_id.clone(),
                cwd: request.cwd.clone(),
                sandbox_state: workspace_state,
                launcher_path: launcher(),
                launcher_sha256: launcher_sha(),
                operating_system_uid: unsafe { libc::geteuid() },
            },
            now(),
            60,
        )
        .unwrap();
    let receipt = service.submit(API, request, grant).unwrap();
    let job = await_terminal(&service, &receipt.job.job_id);
    assert_eq!(std::fs::read(&inside).unwrap(), b"allowed");
    assert!(!outside.exists());
    assert_ne!(job.exit_code, Some(0));
}

#[test]
fn cancel_is_revision_guarded_and_unrelated_process_survives() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let mut unrelated = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    let request = job_request(cwd.path(), "cancel", "/bin/sleep 30");
    let receipt = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    assert!(matches!(
        service.cancel(API, &receipt.job.job_id, receipt.job.revision + 1),
        Err(JobError::Conflict(_))
    ));
    service
        .cancel(API, &receipt.job.job_id, receipt.job.revision)
        .unwrap();
    let job = await_terminal(&service, &receipt.job.job_id);
    assert_eq!(job.state, JobState::Cancelled);
    let observation = job.execution.expect("cancel still observes child wait");
    assert!(observation.observed_run_duration_millis.is_some());
    assert!(unrelated.try_wait().unwrap().is_none());
    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

#[test]
fn owner_death_kills_job_and_recovery_marks_interrupted() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let envelope = root.path().join("owner-envelope.json");
    let pids_path = root.path().join("owned-pids.json");
    std::fs::write(
        &envelope,
        serde_json::to_vec(&json!({
            "stateRoot": root.path(), "cwd": cwd.path(), "launcher": launcher(),
            "grantKeyHex": hex::encode(GRANT), "apiTokenHex": hex::encode(API), "pidsPath": pids_path
        }))
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_js1-test-owner"))
        .arg("own-and-abort")
        .arg(&envelope)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("state.json")).unwrap()).unwrap();
    let job = state["jobs"].as_object().unwrap().values().next().unwrap();
    let pids: Vec<u32> = serde_json::from_slice(&std::fs::read(&pids_path).unwrap()).unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while pids
        .iter()
        .any(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        pids.iter()
            .all(|pid| !std::path::Path::new(&format!("/proc/{pid}")).exists()),
        "owned process tree survived owner death: {pids:?}"
    );
    let recovered = JobService::open(config(root.path())).unwrap();
    let id = job["jobId"].as_str().unwrap();
    assert_eq!(
        recovered.query(API, id).unwrap().state,
        JobState::Interrupted
    );
    let observation = recovered
        .query(API, id)
        .unwrap()
        .execution
        .expect("proved gate release survives restart projection");
    assert!(observation.start_observed_at_epoch_millis.is_some());
    assert_eq!(observation.exit_observed_at_epoch_millis, None);
    assert_eq!(observation.observed_run_duration_millis, None);
    assert_eq!(recovered.pending_outbox(API).unwrap().len(), 1);
}

#[test]
fn observed_runner_duration_excludes_sentinel_and_output_drain_collection() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let request = job_request(
        cwd.path(),
        "timing-excludes-drain",
        "(sleep 30) & printf parent-exited",
    );
    let before = Instant::now();
    let receipt = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    let job = await_terminal(&service, &receipt.job.job_id);
    let total = before.elapsed();
    let observed = job.execution.unwrap().observed_run_duration_millis.unwrap();
    assert!(
        observed < 1_000,
        "runner interval incorrectly included sentinel/drain: {observed}ms"
    );
    assert!(
        total >= Duration::from_millis(1_800),
        "fixture did not exercise the post-wait sentinel/drain delay: {total:?}"
    );
}

#[test]
fn private_unix_protocol_requires_peer_token() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let socket = temp.path().join("run/job.sock");
    let api_file = temp.path().join("api.key");
    let grant_file = temp.path().join("grant.key");
    std::fs::write(&api_file, API).unwrap();
    std::fs::set_permissions(&api_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&grant_file, GRANT).unwrap();
    std::fs::set_permissions(&grant_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut daemon = Command::new(runner())
        .arg("serve")
        .arg(&state)
        .arg(&socket)
        .arg(&api_file)
        .arg(&grant_file)
        .arg(launcher())
        .spawn()
        .unwrap();
    // Daemon startup verifies the 1.2 GiB installed sandbox launcher once.
    let until = Instant::now() + Duration::from_secs(120);
    while !socket.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists());
    let missing_grant = caller(
        "cutex.test.durable-session",
        CallerOperation::Query,
        "missing",
    );
    let call = |token: &[u8], include_grant: bool| {
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut params = json!({"jobId":"missing"});
        if include_grant {
            params["callerGrant"] = serde_json::to_value(&missing_grant).unwrap();
        }
        writeln!(
            stream,
            "{}",
            json!({"token":hex::encode(token),"method":"query","params":params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    assert_eq!(call(b"wrong", false)["code"], "unauthorized");
    assert_eq!(call(API, true)["code"], "not_found");
    let raw = |token: &[u8], method: &str, params: serde_json::Value| {
        let mut stream = UnixStream::connect(&socket).unwrap();
        writeln!(
            stream,
            "{}",
            json!({"token":hex::encode(token),"method":method,"params":params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    let caps = raw(API, "capabilities", json!({}));
    assert_eq!(
        caps["result"]["humanOperations"],
        json!(["list", "cancel", "readOutput", "markExternal"])
    );
    // Human operations take the root token only: no caller grant.
    let cancel = json!({"jobId":"missing","expectedRevision":1});
    assert_eq!(
        raw(b"wrong", "humanCancel", cancel.clone())["code"],
        "unauthorized"
    );
    assert_eq!(raw(API, "humanCancel", cancel)["code"], "not_found");
    assert_eq!(
        raw(API, "humanCancel", json!({"jobId":"missing"}))["code"],
        "invalid"
    );
    let read = json!({"jobId":"missing","stream":"stdout","maxBytes":100000});
    assert_eq!(
        raw(b"wrong", "humanReadOutput", read.clone())["code"],
        "unauthorized"
    );
    assert_eq!(raw(API, "humanReadOutput", read)["code"], "not_found");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn authenticated_disabled_is_allowed_but_forged_grants_fail_before_launch() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let request = job_request(cwd.path(), "denied", "exit 0");
    let mut grant = issue(&request);
    grant.payload.sandbox_state["permissionProfile"] = json!({"type":"disabled"});
    assert!(matches!(
        service.submit(API, request.clone(), grant),
        Err(JobError::Unauthorized(_))
    ));

    let mut external_request = request.clone();
    external_request.origin.permission_profile_type = "external".into();
    let external = GrantIssuer::new(GRANT).unwrap().issue(
        &external_request,
        TrustedSandboxContext {
            subject_cutex_session_id: external_request.subscriber_cutex_session_id.clone(),
            cwd: external_request.cwd.clone(),
            sandbox_state: json!({
                "permissionProfile":{"type":"external","name":"unreproducible"},
                "codexLinuxSandboxExe":null,
                "sandboxCwd":format!("file://{}", cwd.path().display()),
                "useLegacyLandlock":false
            }),
            launcher_path: launcher(),
            launcher_sha256: launcher_sha(),
            operating_system_uid: unsafe { libc::geteuid() },
        },
        now(),
        60,
    );
    assert!(matches!(external, Err(JobError::Unauthorized(_))));

    let full_access_request = job_request(cwd.path(), "full-access", "printf full-access");
    let mut full_access_request = full_access_request;
    full_access_request.origin.permission_profile_type = "disabled".into();
    let full_access_grant = GrantIssuer::new(GRANT)
        .unwrap()
        .issue(
            &full_access_request,
            TrustedSandboxContext {
                subject_cutex_session_id: full_access_request.subscriber_cutex_session_id.clone(),
                cwd: full_access_request.cwd.clone(),
                sandbox_state: json!({
                    "permissionProfile":{"type":"disabled"},
                    "codexLinuxSandboxExe":null,
                    "sandboxCwd":format!("file://{}", cwd.path().display()),
                    "useLegacyLandlock":false
                }),
                launcher_path: launcher(),
                launcher_sha256: launcher_sha(),
                operating_system_uid: unsafe { libc::geteuid() },
            },
            now(),
            60,
        )
        .unwrap();
    let receipt = service
        .submit(API, full_access_request, full_access_grant)
        .unwrap();
    assert_eq!(
        await_terminal(&service, &receipt.job.job_id).state,
        JobState::Exited
    );

    let mut environment_request = request;
    environment_request
        .environment
        .insert("SECRET".into(), "must-not-be-injected".into());
    let environment_grant = issue(&environment_request);
    assert!(matches!(
        service.submit(API, environment_request, environment_grant),
        Err(JobError::Unauthorized(_))
    ));
    assert_eq!(service.pending_outbox(API).unwrap().len(), 1);
}

#[test]
fn persisted_launch_pending_recovers_as_unknown_without_launch() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let request = job_request(cwd.path(), "crash-window", "/bin/sleep 30");
    let record = JobRecord {
        external: None,
        schema: CONTRACT.into(),
        job_id: "job_crash_window".into(),
        revision: 1,
        request_sha256: "0".repeat(64),
        request: PersistedJobRequest::from(&request),
        state: JobState::LaunchPending,
        created_at_epoch_secs: now(),
        updated_at_epoch_secs: now(),
        process_id: None,
        process_start_ticks: None,
        exit_code: None,
        terminal_reason: None,
        execution: None,
        stdout: StreamSummary {
            retained_bytes: 0,
            observed_bytes: 0,
            truncated: false,
        },
        stderr: StreamSummary {
            retained_bytes: 0,
            observed_bytes: 0,
            truncated: false,
        },
        output_reference: "job-output:job_crash_window".into(),
        completion_delivery: CompletionDeliverySummary::default(),
    };
    std::fs::write(root.path().join("state.json"), serde_json::to_vec(&json!({"version":1,"jobs":{"job_crash_window":record},"actionIndex":{"crash-window":"job_crash_window"},"outbox":{}})).unwrap()).unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let recovered = service.query(API, "job_crash_window").unwrap();
    assert_eq!(recovered.state, JobState::LaunchUnknown);
    assert_eq!(service.pending_outbox(API).unwrap().len(), 1);
    assert_eq!(recovered.execution, None);
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(stored["version"], 1, "legacy unknowns are not backfilled");
}

#[test]
fn child_worktree_cwd_preserves_arguments_and_original_read_only_policy() {
    let root = tempfile::tempdir().unwrap();
    let anchor = tempfile::tempdir().unwrap();
    let child = anchor.path().join("worktree with spaces ' 私有");
    std::fs::create_dir(&child).unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let request = job_request(
        &child,
        "child-cwd",
        "pwd; printf '%s\\n' \"$1\"; touch forbidden",
    );
    let mut request = request;
    request
        .argv
        .extend(["arg0".into(), "literal $(touch injected) ' 私有".into()]);
    let grant = GrantIssuer::new(GRANT)
        .unwrap()
        .issue(
            &request,
            TrustedSandboxContext {
                subject_cutex_session_id: request.subscriber_cutex_session_id.clone(),
                cwd: request.cwd.clone(),
                sandbox_state: sandbox(anchor.path()),
                launcher_path: launcher(),
                launcher_sha256: launcher_sha(),
                operating_system_uid: unsafe { libc::geteuid() },
            },
            now(),
            60,
        )
        .unwrap();
    let receipt = service.submit(API, request, grant).unwrap();
    let job = await_terminal(&service, &receipt.job.job_id);
    assert_ne!(job.exit_code, Some(0));
    let output = service
        .read_output(API, &job.job_id, "stdout", 0, 1024)
        .unwrap();
    let encoded = serde_json::to_value(output).unwrap();
    let text =
        String::from_utf8(hex::decode(encoded["bytesHex"].as_str().unwrap()).unwrap()).unwrap();
    assert!(text.contains(child.to_str().unwrap()), "{text}");
    assert!(text.contains("literal $(touch injected) ' 私有"), "{text}");
    assert!(!child.join("forbidden").exists());
    assert!(!child.join("injected").exists());
}

#[test]
fn failed_launch_submit_query_and_replay_share_completion_projection() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let missing_runner = root.path().join("removed-runner");
    std::fs::copy(runner(), &missing_runner).unwrap();
    let mut cfg = config(root.path());
    cfg.runner_executable = missing_runner.clone();
    cfg.completion_enabled = true;
    let service = JobService::open(cfg).unwrap();
    std::fs::remove_file(missing_runner).unwrap();
    let request = job_request(cwd.path(), "failed-launch-projection", "true");
    let first = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    assert_eq!(first.job.state, JobState::Failed);
    let queried = service.query(API, &first.job.job_id).unwrap();
    let replay = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(
        serde_json::to_value(&first.job).unwrap(),
        serde_json::to_value(queried).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&first.job).unwrap(),
        serde_json::to_value(replay.job).unwrap()
    );
}

#[test]
fn concurrent_submit_and_cancel_finish_without_lock_inversion() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.cancel_grace = Duration::from_millis(500);
    let service = JobService::open(cfg).unwrap();
    let request = job_request(cwd.path(), "lock-cancel", "trap '' TERM; sleep 30");
    let first = service
        .submit(API, request.clone(), issue(&request))
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let cancelling = service.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::thread::spawn(move || {
        let result = cancelling.cancel(API, &first.job.job_id, first.job.revision);
        tx.send(result).unwrap();
    });
    std::thread::sleep(Duration::from_millis(40));
    let submitting = service.clone();
    let request = job_request(cwd.path(), "lock-submit", "true");
    let submit =
        std::thread::spawn(move || submitting.submit(API, request.clone(), issue(&request)));
    rx.recv_timeout(Duration::from_secs(5))
        .expect("cancel deadlocked with submit")
        .unwrap();
    cancel.join().unwrap();
    let second = submit.join().unwrap().unwrap();
    await_terminal(&service, &second.job.job_id);
}

#[test]
fn human_job_pages_are_stable_bounded_authenticated_and_metadata_only() {
    let root = tempfile::tempdir().unwrap();
    let mut jobs = serde_json::Map::new();
    for i in 0..55 {
        let id = format!("job_{i:03}");
        jobs.insert(id.clone(),json!({"schema":CONTRACT,"jobId":id,"revision":1,"requestSha256":"0".repeat(64),
            "request":{"actionId":format!("action-{i:03}"),"argumentCount":1,"cwd":"/tmp",
            "environmentNames":["SECRET_SENTINEL"],"subscriberCutexSessionId":"cutex.owner"},
            "state":"exited","createdAtEpochSecs":100,"updatedAtEpochSecs":101,"exitCode":0,
            "terminalReason":"SECRET_SENTINEL","stdout":{"observedBytes":999,"retainedBytes":10,"truncated":true},
            "stderr":{"observedBytes":0,"retainedBytes":0,"truncated":false},"outputReference":format!("job-output:{id}")}));
    }
    // A newer record carries a persisted command summary (over-long on disk to
    // prove the projection re-bounds it); older records have none.
    jobs["job_054"]["request"]["commandSummary"] = json!(format!("bash -c {}", "é".repeat(400)));
    std::fs::write(
        root.path().join("state.json"),
        serde_json::to_vec(&json!({"version":1,"jobs":jobs,"actionIndex":{},"outbox":{}})).unwrap(),
    )
    .unwrap();
    let mut cfg = config(root.path());
    cfg.max_jobs = 100;
    let service = JobService::open(cfg).unwrap();
    assert!(service.human_list(b"wrong", json!({})).is_err());
    assert!(service.human_list(API, json!({"cursor":"broken"})).is_err());
    assert!(
        service
            .human_list(API, json!({"query":"x".repeat(257)}))
            .is_err()
    );
    let page = service.human_list(API, json!({"limit":10000})).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 50);
    assert_eq!(page["data"][0]["jobId"], "job_054");
    assert!(!page.to_string().contains("SECRET_SENTINEL"));
    assert!(!page.to_string().contains("environmentNames"));
    let command = page["data"][0]["command"].as_str().unwrap();
    assert_eq!(command.chars().count(), 300);
    assert!(command.starts_with("bash -c é"));
    assert!(page["data"][1]["command"].is_null());
    let second = service
        .human_list(API, json!({"cursor":page["nextCursor"]}))
        .unwrap();
    assert_eq!(second["data"].as_array().unwrap().len(), 5);
    assert_eq!(second["data"][0]["jobId"], "job_004");
    assert!(second["nextCursor"].is_null());
    let filtered = service
        .human_list(API, json!({"query":"ACTION-003"}))
        .unwrap();
    assert_eq!(filtered["data"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["data"][0]["stdout"]["truncated"], true);
}

/// The installed cute-codex CLI the tests run jobs through:
/// `CUTEX_JOB_TEST_LAUNCHER`, else `cute-codex` on PATH.
fn test_launcher() -> String {
    if let Some(path) = std::env::var_os("CUTEX_JOB_TEST_LAUNCHER").filter(|v| !v.is_empty()) {
        return path.to_string_lossy().into_owned();
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("cute-codex"))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .expect("set CUTEX_JOB_TEST_LAUNCHER to an installed cute-codex CLI (or put cute-codex on PATH)")
}
