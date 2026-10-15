//! Root-authenticated, bounded Job Service bridge for the owner's clients.
//!
//! Routes (all root-only, see `agent_management_admin_path`):
//! - `POST /v2/job-service/management-query` -> socket `humanList`
//! - `GET  /v2/job-service/capabilities`     -> socket `capabilities`
//! - `POST /v2/job-service/cancel`           -> socket `humanCancel`
//! - `POST /v2/job-service/mark-external`  -> socket `humanMarkExternal`
//! - `POST /v2/job-service/output`           -> socket `humanReadOutput`
//!
//! The Job Service is a separate Linux-only daemon on a private Unix socket;
//! cutex authenticates to it with its root `api.token`. No caller grants are
//! involved: these operations act for the human owner.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use serde_json::{json, Map, Value};
use std::net::TcpStream;

const JOB_ID_MAX_LEN: usize = 160;
const OUTPUT_MAX_BYTES: u64 = 8192;

/// Failure of one Job Service socket call.
#[derive(Debug)]
enum CallError {
    /// The service replied `{ok: false, code, message}`.
    Service { code: String, message: String },
    /// No usable reply (no runner, connect/IO/timeout, malformed reply).
    Unavailable(anyhow::Error),
}

struct HttpError {
    status: u16,
    reason: &'static str,
    code: &'static str,
    retryable: bool,
}

fn map_call_error(error: &CallError) -> (HttpError, String) {
    let unavailable = HttpError {
        status: 503,
        reason: "Service Unavailable",
        code: "job_service_unavailable",
        retryable: true,
    };
    match error {
        CallError::Unavailable(error) => (unavailable, format!("{error:#}")),
        CallError::Service { code, message } => {
            let mapped = match code.as_str() {
                "not_found" => HttpError {
                    status: 404,
                    reason: "Not Found",
                    code: "job_not_found",
                    retryable: false,
                },
                "conflict" => HttpError {
                    status: 409,
                    reason: "Conflict",
                    code: "job_conflict",
                    retryable: false,
                },
                // An older deployed Job Service without the human operations.
                // Its reply message is `invalid request: unknown method` (JobError
                // Display); accept the bare form too.
                "invalid"
                    if message == "unknown method"
                        || message.ends_with(": unknown method") =>
                {
                    HttpError {
                    status: 501,
                    reason: "Not Implemented",
                    code: "job_service_operation_unsupported",
                    retryable: false,
                }
                },
                "unauthorized" => HttpError {
                    status: 403,
                    reason: "Forbidden",
                    code: "job_service_unauthorized",
                    retryable: false,
                },
                _ => unavailable,
            };
            (mapped, message.clone())
        }
    }
}

fn write_call_error(stream: &mut TcpStream, error: &CallError) -> anyhow::Result<()> {
    let (mapped, message) = map_call_error(error);
    super::server::write_v2_error(
        stream,
        mapped.status,
        mapped.reason,
        mapped.code,
        &message,
        mapped.retryable,
        json!({}),
    )
}

fn write_invalid(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        400,
        "Bad Request",
        "invalid_request",
        message,
        false,
        json!({}),
    )
}

/// `POST /v2/job-service/management-query`: bounded Job list (`humanList`).
pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let Ok(params) = serde_json::from_slice::<Value>(&request.body) else {
        return write_invalid(stream, "Invalid Job query");
    };
    if !valid_list_params(&params) {
        return write_invalid(stream, "Expected query, cursor and limit for Jobs");
    }
    match call("humanList", params) {
        Ok(mut result) => {
            add_agent_names(&mut result);
            write_json_response(stream, 200, "OK", &result)
        }
        // Kept as before: every list failure is a retryable 503.
        Err(error) => super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "job_service_unavailable",
            &map_call_error(&error).1,
            true,
            json!({}),
        ),
    }
}

/// `GET /v2/job-service/capabilities`: the service's `capabilities` result verbatim.
pub(super) fn handle_capabilities(stream: &mut TcpStream) -> anyhow::Result<()> {
    match call("capabilities", json!({})) {
        Ok(result) => write_json_response(stream, 200, "OK", &result),
        Err(error) => super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "job_service_unavailable",
            &map_call_error(&error).1,
            true,
            json!({}),
        ),
    }
}

/// `POST /v2/job-service/cancel` {jobId, expectedRevision}: the job record.
pub(super) fn handle_cancel(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
) -> anyhow::Result<()> {
    let Some(params) = cancel_params(&request.body) else {
        return write_invalid(
            stream,
            "Expected {jobId: string (<= 160), expectedRevision: integer}",
        );
    };
    match call("humanCancel", params) {
        Ok(result) => write_json_response(stream, 200, "OK", &result),
        Err(error) => write_call_error(stream, &error),
    }
}

/// Owner-only terminal update; the model-facing adapter has no corresponding tool.
pub(super) fn handle_mark_external(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let Some(params) = mark_external_params(&request.body) else {
        return write_invalid(stream, "Expected {jobId, expectedRevision, state: exited|failed|cancelled, exitCode?, note?}; exited requires a nonnegative exitCode");
    };
    match call("humanMarkExternal", params) {
        Ok(result) => write_json_response(stream, 200, "OK", &result),
        Err(CallError::Service { code, message }) if code == "invalid" && !message.ends_with("unknown method") => write_invalid(stream, &message),
        Err(error) => write_call_error(stream, &error),
    }
}

/// A failed refresh keeps the last known features; request paths never do socket IO.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct JobCapabilities {
    pub external: bool,
    pub nci: bool,
    /// The Job Service was started with `--external-notify-file` (hands agents the owner's address).
    pub notify_file: bool,
}

impl JobCapabilities {
    fn from_reply(value: &Value) -> Option<Self> {
        value.as_object()?;
        for key in ["humanOperations", "sources"] {
            if let Some(items) = value.get(key) {
                if !items.as_array()?.iter().all(Value::is_string) { return None; }
            }
        }
        let contains = |key: &str, name: &str| value.get(key).and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item == name));
        Some(Self {
            external: contains("humanOperations", "markExternal"),
            nci: contains("sources", "nci"),
            notify_file: value.get("externalNotifyFile").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Default)]
struct CapabilityCache {
    value: JobCapabilities,
    refresh_after: Option<std::time::Instant>,
    refreshing: bool,
}

impl CapabilityCache {
    fn snapshot(&mut self, now: std::time::Instant) -> (JobCapabilities, bool) {
        let refresh = !self.refreshing && self.refresh_after.is_none_or(|next| now >= next);
        if refresh { self.refreshing = true; }
        (self.value, refresh)
    }

    fn finish(&mut self, result: Option<JobCapabilities>) {
        if let Some(value) = result { self.value = value; }
        self.refreshing = false;
        self.refresh_after = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
    }
}

#[cfg(target_os = "linux")]
fn capability_cache() -> &'static std::sync::Mutex<CapabilityCache> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<CapabilityCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(CapabilityCache::default()))
}

/// Seed discovery before the listener starts, never during bootstrap handling.
pub fn prime_capabilities() {
    #[cfg(target_os = "linux")]
    {
        let cache = capability_cache();
        let (_, refresh) = cache.lock().unwrap_or_else(|e| e.into_inner())
            .snapshot(std::time::Instant::now());
        if !refresh { return; }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        if std::thread::Builder::new().name("job-capabilities-startup".into()).spawn(move || {
            let result = socket_round_trip_with_timeout("capabilities", json!({}), Some(std::time::Duration::from_secs(1)))
                .map_err(CallError::Unavailable).and_then(interpret_reply).ok()
                .and_then(|v| JobCapabilities::from_reply(&v));
            let mut state = cache.lock().unwrap_or_else(|e| e.into_inner());
            state.finish(result);
            if result.is_none() { state.refresh_after = None; }
            drop(state);
            let _ = sender.send(());
        }).is_ok() {
            // Also bounds filesystem stalls; a late probe still seeds the cache.
            let _ = receiver.recv_timeout(std::time::Duration::from_secs(1));
        } else {
            let mut state = cache.lock().unwrap_or_else(|e| e.into_inner());
            state.finish(None);
            state.refresh_after = None;
        }
        let _ = cached_capabilities();
    }
}

pub(super) fn cached_capabilities() -> JobCapabilities {
    #[cfg(target_os = "linux")]
    {
        let cache = capability_cache();
        let (value, refresh) = cache.lock().unwrap_or_else(|e| e.into_inner())
            .snapshot(std::time::Instant::now());
        if refresh {
            if std::thread::Builder::new().name("job-capabilities".into()).spawn(move || {
                let result = call("capabilities", json!({})).ok().and_then(|v| JobCapabilities::from_reply(&v));
                cache.lock().unwrap_or_else(|e| e.into_inner()).finish(result);
            }).is_err() {
                cache.lock().unwrap_or_else(|e| e.into_inner()).finish(None);
            }
        }
        value
    }
    #[cfg(not(target_os = "linux"))]
    JobCapabilities::default()
}

/// `POST /v2/job-service/output` {jobId, stream, offset?, maxBytes?}: one readable page.
pub(super) fn handle_output(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
) -> anyhow::Result<()> {
    let Some(params) = output_params(&request.body) else {
        return write_invalid(
            stream,
            "Expected {jobId: string (<= 160), stream: \"stdout\"|\"stderr\", offset?: integer, maxBytes?: integer}",
        );
    };
    match call("humanReadOutput", params) {
        Ok(result) => write_json_response(stream, 200, "OK", &result),
        Err(error) => write_call_error(stream, &error),
    }
}

fn valid_list_params(params: &Value) -> bool {
    params.as_object().is_some_and(|object| {
        object.iter().all(|(k, v)| match k.as_str() {
            "query" => v.as_str().is_some_and(|s| s.len() <= 256),
            "cursor" => {
                v.is_null()
                    || v.as_str().is_some_and(|s| {
                        s.len() <= 160
                            && s.split_once(':').is_some_and(|(time, id)| {
                                time.parse::<u64>().is_ok() && !id.is_empty()
                            })
                    })
            }
            "limit" => v.is_null() || v.as_u64().is_some(),
            _ => false,
        })
    })
}

fn valid_job_id(value: Option<&Value>) -> Option<String> {
    let id = value?.as_str()?;
    (!id.is_empty() && id.len() <= JOB_ID_MAX_LEN).then(|| id.to_string())
}

fn body_object(body: &[u8], allowed: &[&str]) -> Option<Map<String, Value>> {
    let Value::Object(object) = serde_json::from_slice::<Value>(body).ok()? else {
        return None;
    };
    object
        .keys()
        .all(|key| allowed.contains(&key.as_str()))
        .then_some(object)
}

/// Strict `{jobId, expectedRevision}`; both required, nothing else.
fn cancel_params(body: &[u8]) -> Option<Value> {
    let object = body_object(body, &["jobId", "expectedRevision"])?;
    let job_id = valid_job_id(object.get("jobId"))?;
    let revision = object.get("expectedRevision")?.as_u64()?;
    Some(json!({"jobId": job_id, "expectedRevision": revision}))
}

fn mark_external_params(body: &[u8]) -> Option<Value> {
    let object = body_object(body, &["jobId", "expectedRevision", "state", "exitCode", "note"])?;
    let job_id = valid_job_id(object.get("jobId"))?;
    let revision = object.get("expectedRevision")?.as_u64()?;
    if revision == 0 { return None; }
    let state = object.get("state")?.as_str()?;
    if !matches!(state, "exited" | "failed" | "cancelled") { return None; }
    let mut params = json!({"jobId":job_id,"expectedRevision":revision,"state":state});
    if let Some(value) = object.get("exitCode") {
        let code = value.as_u64()?;
        if code > i32::MAX as u64 { return None; }
        params["exitCode"] = json!(code);
    } else if state == "exited" { return None; }
    if let Some(value) = object.get("note") {
        let note = value.as_str()?;
        if note.len() > 2048 || note.chars().any(|ch| ch.is_control() && ch != '\n' && ch != '\t') { return None; }
        params["note"] = json!(note);
    }
    Some(params)
}

/// Strict `{jobId, stream, offset?, maxBytes?}`; maxBytes clamps to 1..=8192.
fn output_params(body: &[u8]) -> Option<Value> {
    let object = body_object(body, &["jobId", "stream", "offset", "maxBytes"])?;
    let job_id = valid_job_id(object.get("jobId"))?;
    let stream = object.get("stream")?.as_str()?;
    if stream != "stdout" && stream != "stderr" {
        return None;
    }
    let optional_u64 = |key: &str| -> Option<Option<u64>> {
        match object.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(value) => value.as_u64().map(Some),
        }
    };
    let offset = optional_u64("offset")?.unwrap_or(0);
    let max_bytes = optional_u64("maxBytes")?
        .unwrap_or(OUTPUT_MAX_BYTES)
        .clamp(1, OUTPUT_MAX_BYTES);
    Some(json!({"jobId": job_id, "stream": stream, "offset": offset, "maxBytes": max_bytes}))
}

/// Names are presentation only; ownership remains the service's session ID.
fn add_agent_names(result: &mut Value) {
    let Some(rows) = result["data"].as_array_mut() else {
        return;
    };
    let Ok(store) = crate::session::store::load_cutex_session_store() else {
        return;
    };
    for row in rows {
        if let Some(id) = row["sessionId"].as_str() {
            if let Some(record) = store.sessions.values().find(|r| r.cutex_session_id == id) {
                row["agentName"] = json!(crate::session::metadata::cutex_session_display_name(record));
            }
        }
    }
}

/// Interpret one socket reply `{ok, result, code, message}`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn interpret_reply(mut reply: Value) -> Result<Value, CallError> {
    if reply["ok"] == true {
        return Ok(reply["result"].take());
    }
    match reply["code"].as_str() {
        Some(code) => Err(CallError::Service {
            code: code.to_string(),
            message: reply["message"]
                .as_str()
                .unwrap_or("Job Service request failed")
                .to_string(),
        }),
        None => Err(CallError::Unavailable(anyhow::anyhow!(
            "{}",
            reply["message"].as_str().unwrap_or("Job Service request failed")
        ))),
    }
}

#[cfg(not(target_os = "linux"))]
fn call(_method: &str, _params: Value) -> Result<Value, CallError> {
    Err(CallError::Unavailable(anyhow::anyhow!(
        "No local Job Service runner is available on this platform"
    )))
}

#[cfg(target_os = "linux")]
fn call(method: &str, params: Value) -> Result<Value, CallError> {
    let reply = socket_round_trip(method, params).map_err(CallError::Unavailable)?;
    interpret_reply(reply)
}

#[cfg(target_os = "linux")]
fn socket_round_trip(method: &str, params: Value) -> anyhow::Result<Value> {
    socket_round_trip_with_timeout(method, params, None)
}

#[cfg(target_os = "linux")]
fn socket_round_trip_with_timeout(method: &str, params: Value, timeout: Option<std::time::Duration>) -> anyhow::Result<Value> {
    let root = crate::config::paths::runtime_dir()?.join("job-service/v1");
    socket_round_trip_at(&root, method, params, timeout)
}

#[cfg(target_os = "linux")]
fn socket_round_trip_at(root: &std::path::Path, method: &str, params: Value, timeout: Option<std::time::Duration>) -> anyhow::Result<Value> {
    use anyhow::Context;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let deadline = timeout.map(|budget| std::time::Instant::now() + budget);
    let remaining = || -> anyhow::Result<Duration> {
        match deadline {
            Some(end) => end.checked_duration_since(std::time::Instant::now())
                .filter(|left| !left.is_zero()).map(|left| left.min(Duration::from_secs(2)))
                .ok_or_else(|| anyhow::anyhow!("Job Service request deadline exceeded")),
            None => Ok(Duration::from_secs(2)),
        }
    };
    let path = root.join("job-service.sock");
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    anyhow::ensure!(
        bytes.len() < addr.sun_path.len(),
        "Job socket path is too long"
    );
    addr.sun_family = libc::AF_UNIX as _;
    for (a, b) in addr.sun_path.iter_mut().zip(bytes) {
        *a = *b as _;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut socket = unsafe { UnixStream::from_raw_fd(fd) };
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&addr) as _,
        )
    };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::ensure!(
            err.raw_os_error() == Some(libc::EINPROGRESS),
            "Cannot connect to Job Service: {err}"
        );
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        anyhow::ensure!(
            unsafe { libc::poll(&mut poll, 1, remaining()?.as_millis().max(1) as i32) } > 0,
            "Job Service connection timed out"
        );
        if let Some(err) = socket.take_error()? {
            return Err(err.into());
        }
    }
    socket.set_nonblocking(false)?;
    let token =
        std::fs::read(root.join("api.token")).context("Job Service credential unavailable")?;
    anyhow::ensure!(
        (32..=4096).contains(&token.len()),
        "Invalid Job Service credential size"
    );
    let token = token.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut body = serde_json::to_vec(&json!({"token":token,"method":method,"params":params}))?;
    anyhow::ensure!(body.len() < 16384, "Job request too large");
    body.push(b'\n');
    let mut sent = 0;
    while sent < body.len() {
        socket.set_write_timeout(Some(remaining()?))?;
        let written = socket.write(&body[sent..])?;
        anyhow::ensure!(written > 0, "Job Service write closed");
        sent += written;
    }
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        socket.set_read_timeout(Some(remaining()?))?;
        let count = socket.read(&mut chunk)?;
        if count == 0 { break; }
        reply.extend_from_slice(&chunk[..count]);
        anyhow::ensure!(reply.len() <= 1048576, "Job response exceeds 1 MiB");
    }
    anyhow::ensure!(reply.len() <= 1048576, "Job response exceeds 1 MiB");
    Ok(serde_json::from_slice(&reply)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    struct SocketFixtureRoot(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl SocketFixtureRoot {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let path = std::env::temp_dir().join(format!("job-cap-{}", uuid::Uuid::new_v4()));
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &std::path::Path { &self.0 }
    }

    #[cfg(target_os = "linux")]
    impl Drop for SocketFixtureRoot {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_startup_probe_seeds_first_snapshot() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let root = SocketFixtureRoot::new();
        std::fs::write(root.path().join("api.token"), [7u8; 32]).unwrap();
        let listener = UnixListener::bind(root.path().join("job-service.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(socket.try_clone().unwrap()).read_line(&mut request).unwrap();
            socket.write_all(b"{\"ok\":true,\"result\":{\"humanOperations\":[\"markExternal\"],\"sources\":[\"nci\"]}}").unwrap();
        });
        let reply = socket_round_trip_at(root.path(), "capabilities", json!({}), Some(std::time::Duration::from_secs(1))).unwrap();
        server.join().unwrap();
        let mut cache = CapabilityCache::default();
        cache.finish(JobCapabilities::from_reply(&interpret_reply(reply).unwrap()));
        assert_eq!(cache.snapshot(std::time::Instant::now()), (JobCapabilities { external: true, nci: true, notify_file: false }, false));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_probe_deadline_does_not_reset_on_drip_reads() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let root = SocketFixtureRoot::new();
        std::fs::write(root.path().join("api.token"), [7u8; 32]).unwrap();
        let listener = UnixListener::bind(root.path().join("job-service.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(socket.try_clone().unwrap()).read_line(&mut request).unwrap();
            for _ in 0..40 {
                if socket.write_all(b" ").is_err() { break; }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
        let start = std::time::Instant::now();
        assert!(socket_round_trip_at(root.path(), "capabilities", json!({}), Some(std::time::Duration::from_millis(100))).is_err());
        assert!(start.elapsed() < std::time::Duration::from_millis(600));
        server.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_ingest_errors_keep_misconfiguration_pending_and_discard_only_bad_events() {
        let service = |code: &str, message: &str| classify_ingest_error(CallError::Service { code: code.into(), message: message.into() });
        use ExternalIngestError::{Permanent, Retry};
        assert_eq!(service("unauthorized", "unauthorized: bad token"), Retry("job_service_unauthorized"));
        assert_eq!(service("invalid", "invalid request: unknown method"), Retry("job_service_operation_unsupported"));
        assert_eq!(service("invalid", "unknown method"), Retry("job_service_operation_unsupported"));
        assert_eq!(service("source_disabled", "nci"), Retry("job_service_source_disabled"));
        assert_eq!(service("invalid", "invalid request: invalid external source event"), Permanent);
        assert_eq!(service("invalid", "invalid request: external resource values exceed bounds"), Permanent);
        assert_eq!(service("invalid", "serialization failure: unknown field `x`"), Retry("job_service_rejected"));
        assert_eq!(service("conflict", "conflict: outbox"), Retry("job_service_rejected"));
        assert_eq!(service("not_found", "not found: x"), Retry("job_service_rejected"));
        assert_eq!(service("io_failure", "disk"), Retry("job_service_unavailable"));
        assert_eq!(classify_ingest_error(CallError::Unavailable(anyhow::anyhow!("transport"))), Retry("job_service_unavailable"));
    }

    #[test]
    fn capability_cache_is_nonblocking_and_retains_last_success() {
        let mut cache = CapabilityCache::default();
        let now = std::time::Instant::now();
        assert_eq!(cache.snapshot(now), (JobCapabilities::default(), true));
        // Even while the background runner is hung, readers get the cached value.
        assert_eq!(cache.snapshot(now), (JobCapabilities::default(), false));
        let supported = JobCapabilities::from_reply(&json!({"humanOperations":["markExternal"],"sources":["nci"]})).unwrap();
        assert_eq!(supported, JobCapabilities { external: true, nci: true, notify_file: false });
        assert!(JobCapabilities::from_reply(&json!({"sources":["nci"],"externalNotifyFile":true})).unwrap().notify_file);
        cache.finish(Some(supported));
        assert_eq!(cache.snapshot(now), (supported, false));
        assert_eq!(cache.snapshot(now + std::time::Duration::from_secs(31)), (supported, true));
        cache.finish(None);
        assert_eq!(cache.snapshot(now), (supported, false));
        assert_eq!(cache.snapshot(now + std::time::Duration::from_secs(31)), (supported, true));
        cache.finish(JobCapabilities::from_reply(&Value::Null));
        assert_eq!(cache.snapshot(now), (supported, false));
        cache.finish(JobCapabilities::from_reply(&json!({"humanOperations":["list"]})));
        assert_eq!(cache.snapshot(now), (JobCapabilities::default(), false));
    }

    #[test]
    fn cancel_body_is_strict() {
        assert_eq!(
            cancel_params(br#"{"jobId":"job_1","expectedRevision":3}"#),
            Some(json!({"jobId":"job_1","expectedRevision":3}))
        );
        for body in [
            &br#"{"jobId":"job_1"}"#[..],
            br#"{"expectedRevision":3}"#,
            br#"{"jobId":"","expectedRevision":3}"#,
            br#"{"jobId":7,"expectedRevision":3}"#,
            br#"{"jobId":"job_1","expectedRevision":-1}"#,
            br#"{"jobId":"job_1","expectedRevision":"3"}"#,
            br#"{"jobId":"job_1","expectedRevision":3,"extra":true}"#,
            br#"[]"#,
            b"not json",
        ] {
            assert_eq!(cancel_params(body), None, "{}", String::from_utf8_lossy(body));
        }
        let long = format!(r#"{{"jobId":"{}","expectedRevision":1}}"#, "j".repeat(161));
        assert_eq!(cancel_params(long.as_bytes()), None);
        let max = format!(r#"{{"jobId":"{}","expectedRevision":1}}"#, "j".repeat(160));
        assert!(cancel_params(max.as_bytes()).is_some());
    }

    #[test]
    fn external_mark_accepts_only_bounded_owner_terminal_facts() {
        for state in ["exited", "failed", "cancelled"] {
            let value = json!({"jobId":"job_remote", "expectedRevision":3, "state":state, "exitCode":0, "note":"owner verified"});
            assert_eq!(mark_external_params(&serde_json::to_vec(&value).unwrap()), Some(value));
        }
        for extra in [
            json!({"state":"running"}), json!({"state":"registered"}),
            json!({"state":"exited"}), json!({"state":"failed","exitCode":-1}),
            json!({"state":"failed","exitCode":2147483648_u64}),
            json!({"state":"failed","exitCode":null}),
            json!({"state":"failed","note":"x".repeat(2049)}),
            json!({"state":"failed","note":"\u{1b}[31m"}),
            json!({"state":"failed","expectedRevision":0}),
            json!({"state":"failed","source":"nci"}),
        ] {
            let mut value = json!({"jobId":"job_remote","expectedRevision":3});
            value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(mark_external_params(&serde_json::to_vec(&value).unwrap()).is_none(), "{value}");
        }
        for body in [b"null".as_slice(), b"[]", b"{}", b"not json"] {
            assert!(mark_external_params(body).is_none());
        }
    }

    #[test]
    fn output_body_is_strict_and_clamps_page_size() {
        assert_eq!(
            output_params(br#"{"jobId":"job_1","stream":"stderr"}"#),
            Some(json!({"jobId":"job_1","stream":"stderr","offset":0,"maxBytes":8192}))
        );
        assert_eq!(
            output_params(br#"{"jobId":"job_1","stream":"stdout","offset":10,"maxBytes":100000}"#),
            Some(json!({"jobId":"job_1","stream":"stdout","offset":10,"maxBytes":8192}))
        );
        assert_eq!(
            output_params(br#"{"jobId":"job_1","stream":"stdout","offset":null,"maxBytes":0}"#),
            Some(json!({"jobId":"job_1","stream":"stdout","offset":0,"maxBytes":1}))
        );
        for body in [
            &br#"{"jobId":"job_1"}"#[..],
            br#"{"jobId":"job_1","stream":"both"}"#,
            br#"{"stream":"stdout"}"#,
            br#"{"jobId":"job_1","stream":"stdout","offset":-1}"#,
            br#"{"jobId":"job_1","stream":"stdout","maxBytes":"8"}"#,
            br#"{"jobId":"job_1","stream":"stdout","limit":8}"#,
            b"null",
        ] {
            assert_eq!(output_params(body), None, "{}", String::from_utf8_lossy(body));
        }
    }

    fn mapped(reply: Value) -> (u16, &'static str, bool) {
        let error = interpret_reply(reply).unwrap_err();
        let (http, _) = map_call_error(&error);
        (http.status, http.code, http.retryable)
    }

    #[test]
    fn job_service_errors_map_to_http() {
        assert_eq!(
            interpret_reply(json!({"ok":true,"result":{"jobId":"j"}})).unwrap(),
            json!({"jobId":"j"})
        );
        assert_eq!(
            mapped(json!({"ok":false,"code":"not_found","message":"j"})),
            (404, "job_not_found", false)
        );
        assert_eq!(
            mapped(json!({"ok":false,"code":"conflict","message":"job revision mismatch"})),
            (409, "job_conflict", false)
        );
        assert_eq!(
            mapped(json!({"ok":false,"code":"invalid","message":"unknown method"})),
            (501, "job_service_operation_unsupported", false)
        );
        // What job-list-r1 (and every Job Service before humanCancel) replies.
        assert_eq!(
            mapped(json!({"ok":false,"code":"invalid","message":"invalid request: unknown method"})),
            (501, "job_service_operation_unsupported", false)
        );
        assert_eq!(
            mapped(json!({"ok":false,"code":"unauthorized","message":"bad token"})),
            (403, "job_service_unauthorized", false)
        );
        for reply in [
            json!({"ok":false,"code":"invalid","message":"stream must be stdout or stderr"}),
            json!({"ok":false,"code":"io_failure","message":"disk"}),
            json!({"ok":false,"message":"no code"}),
            json!({}),
        ] {
            assert_eq!(mapped(reply), (503, "job_service_unavailable", true));
        }
        let (http, message) = map_call_error(&CallError::Unavailable(anyhow::anyhow!("gone")));
        assert_eq!((http.status, http.code, http.retryable), (503, "job_service_unavailable", true));
        assert_eq!(message, "gone");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_has_no_runner() {
        assert!(matches!(
            call("capabilities", json!({})),
            Err(CallError::Unavailable(_))
        ));
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalIngestError {
    /// The event itself failed the Job Service's validation; retrying the
    /// same bytes can never succeed. Only the optional NCI verifier calls the
    /// Job Service, so ordinary builds never construct it.
    #[cfg(any(test, feature = "nci-qstat"))]
    Permanent,
    /// Delivery failed for a reason outside the event (misconfiguration,
    /// version skew, outage). The event stays pending until its 24 h expiry;
    /// the code is surfaced by `GET /v2/inbound-mail` as `deliveryErrorCode`.
    Retry(&'static str),
}

#[cfg(all(target_os = "linux", any(test, feature = "nci-qstat")))]
fn classify_ingest_error(error: CallError) -> ExternalIngestError {
    let CallError::Service { code, message } = error else {
        return ExternalIngestError::Retry("job_service_unavailable");
    };
    // `message` is the JobError Display; it is classified, never surfaced.
    match code.as_str() {
        // Rotated / mismatched API token: an owner fix, not a bad event.
        "unauthorized" => ExternalIngestError::Retry("job_service_unauthorized"),
        // A pre-P2 Job Service answers an unknown method with `invalid`; the
        // wire distinguishes it by the `unknown method` message.
        "invalid" if message == "unknown method" || message.ends_with(": unknown method") => {
            ExternalIngestError::Retry("job_service_operation_unsupported")
        }
        // Event validation failures are `JobError::Invalid` ("invalid request:
        // ..."), raised after the source is known to be enabled.
        "invalid" if message.starts_with("invalid request: ") => ExternalIngestError::Permanent,
        // Any other `invalid` (e.g. `serialization failure` from a Job Service
        // whose event schema differs from ours, or an unrecognised message)
        // is ambiguous between a bad event and version skew: keep it pending.
        "invalid" => ExternalIngestError::Retry("job_service_rejected"),
        "source_disabled" => ExternalIngestError::Retry("job_service_source_disabled"),
        "conflict" | "not_found" => ExternalIngestError::Retry("job_service_rejected"),
        _ => ExternalIngestError::Retry("job_service_unavailable"),
    }
}

#[cfg(all(target_os = "linux", feature = "nci-qstat"))]
pub(crate) fn ingest_external_event(event: Value) -> Result<Value, ExternalIngestError> {
    call("ingestExternalEvent", event).map_err(classify_ingest_error)
}

#[cfg(all(target_os = "linux", feature = "nci-qstat"))]
pub(crate) fn lookup_external_event(event: Value) -> Result<Value, ExternalIngestError> {
    call("lookupExternalEvent", event).map_err(classify_ingest_error)
}
#[cfg(all(target_os = "linux", feature = "nci-qstat"))]
pub(crate) fn list_external_verification_targets(params: Value) -> Result<Value, ExternalIngestError> {
    call("listExternalVerificationTargets", params).map_err(classify_ingest_error)
}
