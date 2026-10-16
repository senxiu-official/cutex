use cutex_job_service::*;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    state_root: String,
    cwd: String,
    launcher: String,
    grant_key_hex: String,
    api_token_hex: String,
    pids_path: String,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("__runner") => cutex_job_service::private_runner_main(&args[1..]),
        Some("__sentinel") => cutex_job_service::private_sentinel_main(&args[1..]),
        Some("own-and-abort") => own_and_abort(&args[1..]),
        _ => Err(JobError::Invalid("test owner invocation invalid".into())),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

fn own_and_abort(args: &[String]) -> Result<(), JobError> {
    let path = args
        .first()
        .ok_or_else(|| JobError::Invalid("envelope path required".into()))?;
    let envelope: Envelope = serde_json::from_slice(&std::fs::read(path)?)?;
    let grant_key = hex::decode(envelope.grant_key_hex)
        .map_err(|_| JobError::Invalid("bad test key".into()))?;
    let api = hex::decode(envelope.api_token_hex)
        .map_err(|_| JobError::Invalid("bad test token".into()))?;
    let runner = std::env::current_exe()?;
    let launcher_path = std::fs::canonicalize(&envelope.launcher)?
        .to_string_lossy()
        .into_owned();
    let launcher_sha256 = cutex_job_service::file_sha256(std::path::Path::new(&launcher_path))?;
    let service = JobService::open(ServiceConfig {
        external_sources: Vec::new(),
        external_stale_after_secs: 86400,
        external_notify_file: None,
        completion_enabled: false,
        state_root: envelope.state_root.into(),
        grant_key: grant_key.clone(),
        api_token: api.clone(),
        runner_executable: runner,
        per_stream_output_bytes: 4096,
        max_read_bytes: 1024,
        cancel_grace: Duration::from_millis(100),
        max_jobs: 16,
        max_active_jobs: 4,
        allowed_launchers: [(launcher_path.clone(), launcher_sha256.clone())]
            .into_iter()
            .collect(),
        completion_wire_version: CompletionWireVersion::V1,
    })?;
    let request = JobRequest {
        action_id: "death-test".into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "/bin/sleep 30 & wait".into()],
        cwd: envelope.cwd.clone(),
        environment: BTreeMap::new(),
        subscriber_cutex_session_id: "cutex.test.owner-death".into(),
        origin: ExecutionOrigin {
            runtime_agent_id: "runtime-owner-test".into(),
            native_thread_id: "thread-owner-test".into(),
            permission_profile_type: "managed".into(),
        },
    };
    let sandbox = serde_json::json!({"permissionProfile":{"type":"managed","file_system":{"type":"restricted","entries":[{"path":{"type":"special","value":{"kind":"root"}},"access":"read"}]},"network":"restricted"},"codexLinuxSandboxExe":null,"sandboxCwd":format!("file://{}", envelope.cwd),"useLegacyLandlock":false});
    let grant = GrantIssuer::new(&grant_key)?.issue(
        &request,
        TrustedSandboxContext {
            subject_cutex_session_id: request.subscriber_cutex_session_id.clone(),
            cwd: request.cwd.clone(),
            sandbox_state: sandbox,
            launcher_path,
            launcher_sha256,
            operating_system_uid: unsafe { libc::geteuid() },
        },
        now(),
        60,
    )?;
    let receipt = service.submit(&api, request, grant)?;
    println!("{}", serde_json::to_string(&receipt)?);
    use std::io::Write;
    std::io::stdout().flush()?;
    std::thread::sleep(Duration::from_millis(500));
    let mut pids = vec![receipt.job.process_id.unwrap_or_default()];
    let mut cursor = 0;
    while cursor < pids.len() {
        let pid = pids[cursor];
        cursor += 1;
        if let Ok(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
            pids.extend(
                children
                    .split_whitespace()
                    .filter_map(|value| value.parse::<u32>().ok()),
            );
        }
    }
    std::fs::write(&envelope.pids_path, serde_json::to_vec(&pids)?)?;
    unsafe { libc::_exit(0) }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
