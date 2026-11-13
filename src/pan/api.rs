//! Owner Management v2 and narrow, authenticated CLI dispatch.
use super::*;
use crate::http::server::{write_json_response, SimpleHttpRequest};
use std::{collections::HashMap, io::Write, net::TcpStream, time::Duration};
pub const BUS_PATH: &str = "/api/pan/v1";
pub const PREFIX: &str = "/v2/pan";

fn authorize_agent_download(caller: &Caller, target: &str) -> Result<()> {
    // Shape errors are independent of caller permissions, including before a
    // session lookup. Share the service's errors instead of calling them denial.
    super::download::target_directory(target)?;
    if caller.kind == "owner" {
        return Ok(());
    }
    let denied = || {
        Error::new(
            "pan_permission_denied",
            "The download directory is outside this agent's writable permissions",
            false,
        )
    };
    let id = caller.id.as_deref().ok_or_else(denied)?;
    let record = crate::session::store::shared_session_row(id)
        .map_err(|_| denied())?
        .ok_or_else(denied)?;
    if !agent_may_download_to(&record, target) {
        return Err(denied());
    }
    Ok(())
}

fn agent_may_download_to(record: &crate::session::model::CutexSessionRecord, target: &str) -> bool {
    use crate::agent_management::envelope::{path_within, PermissionEnvelope, SandboxLevel};
    if crate::agent_management::agent_execution_blocked(record) {
        return false;
    }
    if let Some(session) = &record.claude_session {
        let Ok((write, exec)) = crate::runtime::claude::effective_tools(record, session) else {
            return false;
        };
        if !write && !exec {
            return false;
        }
    }
    let (sandbox, writable_roots) = match PermissionEnvelope::from_record(record) {
        Ok(envelope) => (envelope.sandbox, envelope.writable_roots),
        // Claude creation persists tool switches, not Codex sandbox defaults.
        // Its own workspace remains writable when its effective tools permit
        // writing; this does not grant unbounded host filesystem access.
        Err(_)
            if record.claude_session.is_some()
                && record.sandbox_mode.is_none()
                && record.permission_defaults.is_none() =>
        {
            (
                SandboxLevel::WorkspaceWrite,
                std::iter::once(
                    crate::session::service::cutex_session_launch_cwd(record).to_string(),
                )
                .chain(record.sandbox_writable_roots.iter().cloned())
                .collect(),
            )
        }
        Err(_) => return false,
    };
    let target = std::path::Path::new(target);
    if crate::artifacts::paths::validate_local_path(target).is_err() {
        return false;
    }
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    if !target.is_dir() {
        return false;
    }
    match sandbox {
        SandboxLevel::ReadOnly => false,
        SandboxLevel::FullAccess => true,
        SandboxLevel::WorkspaceWrite => writable_roots.iter().any(|root| {
            std::path::Path::new(root)
                .canonicalize()
                .ok()
                .and_then(|root| {
                    root.to_str()
                        .zip(target.to_str())
                        .map(|(root, target)| path_within(target, root))
                })
                .unwrap_or(false)
        }),
    }
}

fn agent_task_view(task: Download, caller: &Caller) -> Value {
    let own = task.caller == *caller || caller.kind == "owner";
    let same_domain = !own
        && caller
            .id
            .as_deref()
            .zip(task.caller.id.as_deref())
            .is_some_and(|(caller, producer)| {
                crate::agent_bus::boundary::AgentBoundary::load()
                    .is_ok_and(|boundary| boundary.domain(caller) == boundary.domain(producer))
            });
    let mut value = json!(task);
    if !own && !same_domain {
        if let Some(object) = value.as_object_mut() {
            object.remove("targetDir");
            object.remove("targetPath");
        }
    }
    value
}
pub fn is_path(path: &str) -> bool {
    path == PREFIX || path.starts_with("/v2/pan/")
}
pub fn error_response(stream: &mut TcpStream, e: &Error) -> anyhow::Result<()> {
    let status = e.http_status();
    write_json_response(
        stream,
        status,
        "Pan Error",
        &json!({"error":{"code":e.code,"message":e.message,"retryable":e.retryable,"details":{}}}),
    )
}
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let service = match Service::host() {
        Ok(s) => s,
        Err(e) => return error_response(stream, &e),
    };
    handle_with(stream, request, &service)
}
pub(super) fn handle_with(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    service: &Arc<Service>,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or("");
    if path == "/v2/pan/events" && request.method == "GET" {
        return events(stream, service);
    }
    let query = |key: &str, default: &str| {
        url::form_urlencoded::parse(
            request
                .path
                .split_once('?')
                .map(|(_, q)| q)
                .unwrap_or("")
                .as_bytes(),
        )
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| default.into())
    };
    let result = (|| -> Result<(u16, Value)> {
        let body = || -> Result<Value> {
            serde_json::from_slice(&request.body)
                .map_err(|_| Error::invalid("Expected a JSON request body"))
        };
        let n = |k: &str, d: &str| {
            query(k, d)
                .parse::<u64>()
                .map_err(|_| Error::invalid("Invalid numeric query parameter"))
        };
        let value = match (request.method.as_str(), path) {
            ("GET", "/v2/pan/connection") => service.connection()?,
            ("PUT", "/v2/pan/connection") => service.configure(&body()?)?,
            ("DELETE", "/v2/pan/connection") => service.disconnect()?,
            ("POST", "/v2/pan/auth") => service.begin_auth()?,
            ("POST", "/v2/pan/auth/code") => service.authorize_code(&body()?)?,
            ("GET", "/v2/pan/files") => {
                service.files(&query("dir", "/"), n("start", "0")?, n("limit", "100")?)?
            }
            ("GET", "/v2/pan/search") => service.search(
                &query("dir", "/"),
                &query("key", ""),
                query("recursive", "true") == "true",
            )?,
            ("GET", "/v2/pan/meta") => json!({"file":service.meta(&query("path",""))?}),
            ("GET", "/v2/pan/downloads") => {
                json!({"downloads":service.downloads(n("limit","100")? as usize)?})
            }
            ("POST", "/v2/pan/downloads") => {
                let b: Start = serde_json::from_value(body()?)
                    .map_err(|_| Error::invalid("requestId, remote and targetDir are required"))?;
                return Ok((
                    202,
                    json!({"download":service.start_download(&b.request_id,&b.remote,&b.target_dir,Caller::owner())?}),
                ));
            }
            ("GET", p) if p.starts_with("/v2/pan/auth/") => service.poll_auth(&p[13..])?,
            (method, p) if p.starts_with("/v2/pan/downloads/") => {
                let rest = &p[18..];
                if let Some((id, action)) = rest.split_once('/') {
                    if method != "POST" || !["pause", "resume", "cancel"].contains(&action) {
                        return Err(Error::new("pan_not_found", "Unknown Pan route", false));
                    }
                    let result = service.control(id, action)?;
                    service.ensure_worker();
                    json!({"download":result})
                } else if method == "GET" {
                    json!({"download":service.download(rest)?})
                } else {
                    return Err(Error::new("pan_not_found", "Unknown Pan route", false));
                }
            }
            _ => {
                return Err(Error::new(
                    "pan_not_found",
                    "Unknown Pan route or method",
                    false,
                ))
            }
        };
        Ok((200, value))
    })();
    match result {
        Ok((status, v)) => write_json_response(stream, status, "OK", &v),
        Err(e) => error_response(stream, &e),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Start {
    request_id: String,
    remote: String,
    target_dir: String,
}

fn events(stream: &mut TcpStream, service: &Arc<Service>) -> anyhow::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")?;
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut ticks = 0;
    loop {
        service.ensure_worker();
        let tasks = service.changed_tasks()?;
        for task in tasks {
            let body = serde_json::to_string(&json!({"download":task}))?;
            if seen.get(&task.id) != Some(&body) {
                writeln!(stream, "event: pan.download\ndata: {body}\n")?;
                seen.insert(task.id.clone(), body);
            }
        }
        ticks += 1;
        if ticks % 15 == 0 {
            stream.write_all(b": keepalive\n\n")?;
        }
        stream.flush()?;
        if crate::http::server::stream_disconnected(stream).unwrap_or(true) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Only read/query/download operations; account configuration is deliberately
/// absent. Caller comes from verified bus credentials and the live roster.
pub fn cli(service: &Arc<Service>, caller: Caller, body: &[u8]) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Command {
        operation: String,
        #[serde(default)]
        dir: Option<String>,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        target_dir: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        id: Option<String>,
    }
    let c: Command =
        serde_json::from_slice(body).map_err(|_| Error::invalid("Invalid Pan command"))?;
    match c.operation.as_str() {
        "ls" => service.files(c.dir.as_deref().unwrap_or("/"), 0, 1000),
        "search" => service.search(
            c.dir.as_deref().unwrap_or("/"),
            c.key.as_deref().unwrap_or(""),
            true,
        ),
        "meta" => Ok(json!({"file":service.meta(c.remote.as_deref().unwrap_or(""))?})),
        "download" => {
            let target = c.target_dir.as_deref().unwrap_or("");
            authorize_agent_download(&caller, target)?;
            Ok(
                json!({"download":service.start_download(c.request_id.as_deref().unwrap_or(""),c.remote.as_deref().unwrap_or(""),target,caller)?}),
            )
        }
        "status" => {
            if let Some(id) = c.id {
                Ok(json!({"download":agent_task_view(service.download(&id)?, &caller)}))
            } else {
                Ok(json!({"downloads":service.downloads(100)?.into_iter()
                    .map(|task| agent_task_view(task, &caller)).collect::<Vec<_>>()}))
            }
        }
        "pause" | "resume" | "cancel" => {
            let task = service.download(c.id.as_deref().unwrap_or(""))?;
            if caller.kind != "owner" && task.caller != caller {
                return Err(Error::new(
                    "pan_permission_denied",
                    "Only the download's creator or the owner may control it",
                    false,
                ));
            }
            if c.operation == "resume" {
                authorize_agent_download(&caller, &task.target_dir)?;
            }
            let t = service.control(c.id.as_deref().unwrap_or(""), &c.operation)?;
            service.ensure_worker();
            Ok(json!({"download":t}))
        }
        _ => Err(Error::invalid("Unsupported Pan operation")),
    }
}
