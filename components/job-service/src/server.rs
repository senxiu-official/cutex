use crate::{CallerGrant, ExecutionGrant, JobError, JobRequest, JobService};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_shutdown(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
}

#[derive(Clone)]
pub struct ServerConfig {
    pub socket_path: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestEnvelope {
    token: String,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResponseEnvelope {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

pub fn serve_local(service: JobService, config: ServerConfig) -> Result<(), JobError> {
    install_shutdown_handlers()?;
    SHUTDOWN_REQUESTED.store(false, Ordering::Release);
    if !config.socket_path.is_absolute() {
        return Err(JobError::Invalid("socket path must be absolute".into()));
    }
    let parent = config
        .socket_path
        .parent()
        .ok_or_else(|| JobError::Invalid("socket has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(JobError::Unauthorized(
            "socket directory must be a non-symlink owned by the service UID".into(),
        ));
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let lock_path = parent.join("job-service.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    if lock.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err(JobError::Unauthorized(
            "socket lock must be owned by the service UID".into(),
        ));
    }
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(JobError::Conflict(
            "another Job Service owns this socket directory".into(),
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(&config.socket_path) {
        if !metadata.file_type().is_socket() {
            return Err(JobError::Invalid(
                "socket path collision is not a socket".into(),
            ));
        }
        fs::remove_file(&config.socket_path)?;
    }
    let listener = UnixListener::bind(&config.socket_path)?;
    fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    while !SHUTDOWN_REQUESTED.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let service = service.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, service);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(JobError::Io(error)),
        }
    }
    Ok(())
}

fn install_shutdown_handlers() -> Result<(), JobError> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = request_shutdown as *const () as usize;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0
            || libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0
        {
            return Err(JobError::Io(std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn handle(mut stream: UnixStream, service: JobService) -> Result<(), JobError> {
    let uid = peer_uid(&stream)?;
    if uid != unsafe { libc::geteuid() } {
        return Err(JobError::Unauthorized("local peer UID mismatch".into()));
    }
    let mut line = String::new();
    BufReader::new(stream.try_clone()?)
        .take(1024 * 1024 + 1)
        .read_line(&mut line)?;
    if line.len() > 1024 * 1024 {
        return Err(JobError::Invalid("request exceeds 1 MiB".into()));
    }
    let request: RequestEnvelope = serde_json::from_str(&line)?;
    let token = hex::decode(&request.token)
        .map_err(|_| JobError::Unauthorized("malformed API credential".into()));
    let response =
        match token.and_then(|token| dispatch(&service, &token, &request.method, request.params)) {
            Ok(result) => ResponseEnvelope {
                ok: true,
                result: Some(result),
                code: None,
                message: None,
            },
            Err(error) => ResponseEnvelope {
                ok: false,
                result: None,
                code: Some(error_code(&error)),
                message: Some(error.to_string()),
            },
        };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn dispatch(
    service: &JobService,
    token: &[u8],
    method: &str,
    params: Value,
) -> Result<Value, JobError> {
    match method {
        "capabilities" => service.capabilities(token),
        "humanList" => service.human_list(token, params),
        // Root-token human operations (no caller grant), bridged by Cutex
        // Management v2 for the owner's desktop only.
        "humanCancel" => {
            let id = string_param(&params, "jobId")?;
            let revision = params
                .get("expectedRevision")
                .and_then(Value::as_u64)
                .ok_or_else(|| JobError::Invalid("expectedRevision is required".into()))?;
            Ok(serde_json::to_value(service.cancel(token, id, revision)?)?)
        }
        "listExternalVerificationTargets" => {
            let request = serde_json::from_value(params)?;
            Ok(serde_json::to_value(
                service.list_external_verification_targets(token, request)?,
            )?)
        }
        "lookupExternalEvent" => {
            let request = serde_json::from_value(params)?;
            Ok(serde_json::to_value(
                service.lookup_external_event(token, request)?,
            )?)
        }
        "ingestExternalEvent" => {
            let event = serde_json::from_value(params)?;
            Ok(serde_json::to_value(
                service.ingest_external_event(token, event)?,
            )?)
        }
        "humanMarkExternal" => {
            let p: HumanMarkExternalParams = serde_json::from_value(params)?;
            Ok(serde_json::to_value(service.human_mark_external(
                token,
                &p.job_id,
                p.expected_revision,
                p.state,
                p.exit_code,
                p.note,
            )?)?)
        }
        "prepareRemote" => {
            let grant = caller_grant(&params)?;
            let source = string_param(&params, "source")?;
            Ok(serde_json::to_value(
                service.prepare_remote(token, &grant, source)?,
            )?)
        }
        "registerRemote" => {
            let grant = caller_grant(&params)?;
            let request = serde_json::from_value(
                params
                    .get("request")
                    .cloned()
                    .ok_or_else(|| JobError::Invalid("request is required".into()))?,
            )?;
            Ok(serde_json::to_value(
                service.register_remote(token, &grant, request)?,
            )?)
        }
        "humanReadOutput" => {
            let id = string_param(&params, "jobId")?;
            let stream = string_param(&params, "stream")?;
            let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
            let max = params
                .get("maxBytes")
                .and_then(Value::as_u64)
                .unwrap_or(8192)
                .clamp(1, 8192) as usize;
            crate::mcp_adapter::readable_output(serde_json::to_value(
                service.read_output(token, id, stream, offset, max)?,
            )?)
        }
        "submit" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct P {
                request: JobRequest,
                grant: ExecutionGrant,
            }
            let p: P = serde_json::from_value(params)?;
            Ok(serde_json::to_value(
                service.submit(token, p.request, p.grant)?,
            )?)
        }
        "query" => {
            let grant = caller_grant(&params)?;
            let id = string_param(&params, "jobId")?;
            Ok(serde_json::to_value(service.query_for(token, &grant, id)?)?)
        }
        "cancel" => {
            let grant = caller_grant(&params)?;
            let id = string_param(&params, "jobId")?;
            let revision = params
                .get("expectedRevision")
                .and_then(Value::as_u64)
                .ok_or_else(|| JobError::Invalid("expectedRevision is required".into()))?;
            Ok(serde_json::to_value(
                service.cancel_for(token, &grant, id, revision)?,
            )?)
        }
        "readOutput" => {
            let grant = caller_grant(&params)?;
            let id = string_param(&params, "jobId")?;
            let stream = string_param(&params, "stream")?;
            let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
            let max = params
                .get("maxBytes")
                .and_then(Value::as_u64)
                .unwrap_or(8192)
                .clamp(1, 8192) as usize;
            crate::mcp_adapter::readable_output(serde_json::to_value(
                service.read_output_for(token, &grant, id, stream, offset, max)?,
            )?)
        }
        "pendingOutbox" => Ok(serde_json::to_value(service.pending_outbox(token)?)?),
        "acknowledgeOutbox" => {
            let id = string_param(&params, "eventId")?;
            let digest = string_param(&params, "resultSha256")?;
            service.acknowledge_outbox(token, id, digest)?;
            Ok(json!({"status":"committed"}))
        }
        _ => Err(JobError::Invalid("unknown method".into())),
    }
}

fn caller_grant(params: &Value) -> Result<CallerGrant, JobError> {
    serde_json::from_value(
        params
            .get("callerGrant")
            .cloned()
            .ok_or_else(|| JobError::Unauthorized("caller grant is required".into()))?,
    )
    .map_err(JobError::from)
}

fn string_param<'a>(value: &'a Value, name: &str) -> Result<&'a str, JobError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| JobError::Invalid(format!("{name} is required")))
}
fn error_code(error: &JobError) -> &'static str {
    match error {
        JobError::Invalid(_) | JobError::Serde(_) => "invalid",
        JobError::Unauthorized(_) => "unauthorized",
        JobError::Conflict(_) => "conflict",
        JobError::NotFound(_) => "not_found",
        JobError::Io(_) => "io_failure",
    }
}

fn peer_uid(stream: &UnixStream) -> Result<u32, JobError> {
    let mut credential = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credential as *mut _ as *mut _,
            &mut len,
        )
    };
    if result != 0 {
        return Err(JobError::Io(std::io::Error::last_os_error()));
    }
    Ok(credential.uid)
}

use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HumanMarkExternalParams {
    job_id: String,
    expected_revision: u64,
    state: crate::JobState,
    exit_code: Option<i32>,
    note: Option<String>,
}

#[cfg(test)]
mod external_actor_tests {
    #[test]
    fn socket_caller_cannot_claim_another_credential_identity() {
        let request = serde_json::json!({"jobId":"job_1","expectedRevision":1,"state":"exited","exitCode":0,
            "actor":{"credentialKind":"management_root","deviceId":"forged"}});
        assert!(serde_json::from_value::<super::HumanMarkExternalParams>(request).is_err());
    }
}
