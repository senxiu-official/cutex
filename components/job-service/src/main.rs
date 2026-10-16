use cutex_job_service::{
    CompletionDeliveryConfig, CompletionDeliveryWorker, JobError, JobService, ServerConfig,
    ServiceConfig,
};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("__runner") => cutex_job_service::private_runner_main(&args[1..]),
        Some("__sentinel") => cutex_job_service::private_sentinel_main(&args[1..]),
        Some("serve") => serve(&args[1..]),
        Some("mcp-stdio") => mcp_stdio(&args[1..]),
        _ => Err(JobError::Invalid(
            "usage: cutex-job-service serve STATE_ROOT SOCKET API_TOKEN_FILE GRANT_KEY_FILE SANDBOX_LAUNCHER [--allow-launcher PATH] [--external-source nci] [--external-stale-after-secs SECONDS] [--external-notify-file PATH] [--completion ENDPOINT TOKEN_FILE | --completion-v2 ENDPOINT TOKEN_FILE] | mcp-stdio SOCKET API_TOKEN_FILE GRANT_KEY_FILE SANDBOX_LAUNCHER".into(),
        )),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

fn mcp_stdio(args: &[String]) -> Result<(), JobError> {
    if args.len() != 4 {
        return Err(JobError::Invalid(
            "mcp-stdio requires SOCKET API_TOKEN_FILE GRANT_KEY_FILE SANDBOX_LAUNCHER".into(),
        ));
    }
    let api_token = read_secret(&args[1])?;
    let grant_key = read_secret(&args[2])?;
    let launcher = std::fs::canonicalize(&args[3])?;
    let launcher_sha256 = cutex_job_service::file_sha256(&launcher)?;
    cutex_job_service::serve_mcp_stdio(cutex_job_service::McpAdapterConfig {
        socket_path: args[0].clone().into(),
        api_token,
        grant_key,
        launcher_path: launcher.to_string_lossy().into_owned(),
        launcher_sha256,
    })
}

fn serve(args: &[String]) -> Result<(), JobError> {
    if args.len() < 5 {
        return Err(JobError::Invalid(
            "serve requires STATE_ROOT SOCKET API_TOKEN_FILE GRANT_KEY_FILE SANDBOX_LAUNCHER"
                .into(),
        ));
    }
    let mut launchers = vec![args[4].as_str()];
    let mut completion = None;
    let mut external_sources = Vec::new();
    let mut external_stale_after_secs = 86400;
    let mut external_notify_file: Option<std::path::PathBuf> = None;
    let mut index = 5;
    while index < args.len() {
        match args[index].as_str() {
            "--external-source" if index + 1 < args.len() => {
                external_sources.push(args[index + 1].clone());
                index += 2;
            }
            "--external-stale-after-secs" if index + 1 < args.len() => {
                external_stale_after_secs = args[index + 1].parse().map_err(|_| {
                    JobError::Invalid("external stale interval must be an integer".into())
                })?;
                index += 2;
            }
            "--external-notify-file" if index + 1 < args.len() && external_notify_file.is_none() => {
                let path = std::path::PathBuf::from(&args[index + 1]);
                if !path.is_absolute() {
                    return Err(JobError::Invalid("external notify file must be an absolute path".into()));
                }
                external_notify_file = Some(path);
                index += 2;
            }
            "--allow-launcher" if index + 1 < args.len() => {
                launchers.push(args[index + 1].as_str());
                index += 2;
            }
            "--completion" | "--completion-v2" if index + 2 < args.len() && completion.is_none() => {
                completion = Some((args[index].as_str(), args[index + 1].clone(), args[index + 2].clone()));
                index += 3;
            }
            _ => return Err(JobError::Invalid("invalid serve option; expected --allow-launcher PATH, --external-source nci, --external-stale-after-secs SECONDS, --external-notify-file PATH or --completion[-v2] ENDPOINT TOKEN_FILE".into())),
        }
    }
    let api = read_secret(&args[2])?;
    let grant = read_secret(&args[3])?;
    let mut allowed_launchers = std::collections::BTreeMap::new();
    for path in launchers {
        let path = std::fs::canonicalize(path)?;
        let digest = cutex_job_service::file_sha256(&path)?;
        allowed_launchers.insert(path.to_string_lossy().into_owned(), digest);
    }
    let service = JobService::open(ServiceConfig {
        completion_enabled: completion.is_some(),
        state_root: args[0].clone().into(),
        grant_key: grant,
        api_token: api,
        runner_executable: std::env::current_exe()?,
        per_stream_output_bytes: 1024 * 1024,
        max_read_bytes: 64 * 1024,
        cancel_grace: Duration::from_secs(2),
        max_jobs: 1024,
        max_active_jobs: 16,
        allowed_launchers,
        external_sources,
        external_stale_after_secs,
        external_notify_file,
        completion_wire_version: if completion
            .as_ref()
            .is_some_and(|(kind, _, _)| *kind == "--completion-v2")
        {
            cutex_job_service::CompletionWireVersion::V2
        } else {
            cutex_job_service::CompletionWireVersion::V1
        },
    })?;
    let _completion_worker = if let Some((_, endpoint, token_file)) = completion {
        Some(CompletionDeliveryWorker::start(
            service.clone(),
            CompletionDeliveryConfig {
                endpoint,
                token_file: token_file.into(),
                request_timeout: Duration::from_secs(5),
                minimum_backoff: Duration::from_secs(2),
                maximum_backoff: Duration::from_secs(5 * 60),
                idle_interval: Duration::from_millis(250),
            },
        )?)
    } else {
        None
    };
    cutex_job_service::serve_local(
        service,
        ServerConfig {
            socket_path: args[1].clone().into(),
        },
    )
}

fn read_secret(path: &str) -> Result<Vec<u8>, JobError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(JobError::Invalid(
            "secret file must be regular, current-user owned, and mode 0600 or stricter".into(),
        ));
    }
    let value = std::fs::read(path)?;
    if value.len() < 32 || value.len() > 4096 {
        return Err(JobError::Invalid(
            "secret file length must be 32..=4096 bytes".into(),
        ));
    }
    Ok(value)
}
