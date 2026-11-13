//! Authenticated loopback host routing on the existing management listener.
pub mod auth;
mod forward;
pub(crate) mod head;
use crate::management::listeners::{ConnectionInfo, ConnectionSlot};
use crate::services::store::{ServiceRuntime, ServiceStore};
use head::{read_head, Head};
use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct Slot;
impl Slot {
    fn acquire(max: usize) -> Option<Self> {
        ACTIVE
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n < max {
                    Some(n + 1)
                } else {
                    None
                }
            })
            .ok()
            .map(|_| Self)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `first` freezes the management/proxy decision for the TCP connection.
/// Returns true after taking over or closing a malformed request. Non-proxy
/// bytes, including any pre-read body/pipeline, are returned unchanged.
pub fn intercept(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    info: &ConnectionInfo,
    slot: &mut Option<ConnectionSlot>,
    first: bool,
) -> bool {
    if info.remote {
        return false;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let raw = match read_head(stream, buffer) {
        Ok(Some(raw)) => raw,
        Ok(None) => return true,
        // An idle keep-alive connection closes silently, as it did before the
        // hook: an unsolicited 400 could be read as the reply to a new request.
        Err(error) if idle_close(&error, buffer) => return true,
        Err(_) => {
            let _ = error(stream, 400, "invalid_request");
            return true;
        }
    };
    let parsed = Head::parse(&raw);
    let service_host = String::from_utf8_lossy(&raw).split("\r\n").any(|line| {
        line.split_once(':').is_some_and(|(key, value)| {
            key.trim().eq_ignore_ascii_case("host") && value.trim()
                .split(':').next().unwrap_or("").trim_end_matches('.')
                .to_ascii_lowercase().ends_with(".svc.localhost")
        })
    });
    // Restore the byte buffer before deciding whether to handle it.
    buffer.splice(..0, raw);
    if !service_host { return false; }
    let head = match parsed {
        Ok(head) => head,
        Err(_) => {
            let _ = error(stream, 400, "invalid_request");
            return true;
        }
    };
    let Some(slug) = head.get("host").and_then(head::service_slug) else {
        let _ = error(stream, 400, "invalid_service_host");
        return true;
    };
    if !first {
        let _ = error(stream, 421, "host_changed");
        return true;
    }
    drop(slot.take()); // Proxy streams never consume management slots.
    let store = match ServiceStore::open_default() {
        Ok(store) => store,
        Err(_) => {
            let _ = error(stream, 503, "services_store_unavailable");
            return true;
        }
    };
    serve(stream, buffer, info, &store, &slug, None);
    true
}
pub(crate) fn serve_relay(
    stream: &mut TcpStream,
    info: &ConnectionInfo,
    store: &ServiceStore,
    slug: &str,
    policy: &crate::management::relay::service::Policy,
) {
    crate::management::relay::service::with_response_policy(policy.origin(), ||
        serve(stream, &mut Vec::new(), info, store, slug, Some(policy)));
}
fn serve(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    info: &ConnectionInfo,
    store: &ServiceStore,
    slug: &str,
    relay: Option<&crate::management::relay::service::Policy>,
) {
    let supervisor = crate::services::supervisor::global();
    serve_with_supervisor(stream, buffer, info, store, slug, supervisor.as_ref(), relay);
}

fn serve_with_supervisor(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    info: &ConnectionInfo,
    store: &ServiceStore,
    slug: &str,
    supervisor: Option<&Arc<crate::services::supervisor::Supervisor>>,
    relay: Option<&crate::management::relay::service::Policy>,
) {
    let max = match store.load() {
        Ok(registry) => registry.settings.proxy_max_connections as usize,
        Err(_) => {
            let _ = error(stream, 503, "services_store_unavailable");
            return;
        }
    };
    let Some(_slot) = Slot::acquire(max) else {
        let _ = error(stream, 503, "proxy_busy");
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);
    let mut host = None;
    let mut upstream_port = None;
    let mut traffic_guard = None;
    loop {
        let raw = match read_head(stream, buffer) {
            Ok(Some(raw)) => raw,
            Ok(None) => break,
            Err(error) if idle_close(&error, buffer) => break,
            Err(_) => {
                let _ = error(stream, 400, "invalid_request");
                break;
            }
        };
        let head = match Head::parse(&raw) {
            Ok(h) => h,
            Err(_) => {
                let _ = error(stream, 400, "invalid_request");
                break;
            }
        };
        let (method, target) = match head.request() {
            Ok(r) => r,
            Err(_) => {
                let _ = error(stream, 400, "invalid_request");
                break;
            }
        };
        let this_host = head.get("host").unwrap().to_ascii_lowercase();
        if host.as_ref().is_some_and(|host| host != &this_host)
            || (relay.is_none() && head::service_slug(&this_host).as_deref() != Some(slug))
        {
            let _ = error(stream, 421, "host_changed");
            break;
        }
        host = Some(this_host);
        if relay.is_some_and(|policy| !policy.allows(&head))
            || (relay.is_none() && (!auth::cross_site_allowed(&head)
            || (head.websocket() && !auth::websocket_origin_allowed(&head))))
        {
            if target.split('?').next() == Some("/__cutex/auth") {
                for (name, value) in url::form_urlencoded::parse(target.split_once('?').map_or("", |(_, q)| q).as_bytes()) {
                    if name == "ticket" { auth::discard(&value); }
                }
            }
            let _ = error(stream, 403, "cross_site_blocked");
            break;
        }
        let path = target.split('?').next().unwrap_or(target);
        if relay.is_none() && path == "/__cutex/auth" {
            let args: Vec<_> = url::form_urlencoded::parse(
                target
                    .split_once('?')
                    .map(|(_, q)| q)
                    .unwrap_or("")
                    .as_bytes(),
            )
            .collect();
            let tickets: Vec<_> = args.iter().filter(|(k, _)| k == "ticket").collect();
            let cookie = if tickets.len() == 1 && method == "GET" {
                auth::exchange(store, slug, &tickets[0].1)
            } else {
                for (_, ticket) in &tickets {
                    auth::discard(ticket);
                }
                None
            };
            if method != "GET" {
                let _ = error(stream, 405, "method_not_allowed");
                break;
            }
            if let Some(cookie) = cookie {
                let next = args
                    .iter()
                    .find(|(k, _)| k == "next")
                    .map(|(_, v)| auth::safe_next(v))
                    .unwrap_or("/");
                let _=write!(stream,"HTTP/1.1 302 Found\r\nLocation: {next}\r\nSet-Cookie: {}={cookie}; HttpOnly; SameSite=Lax; Path=/\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",auth::cookie_name(store));
            } else {
                let _ = error(stream, 401, "ticket_required");
            }
            break;
        }
        if path == "/__cutex" || path.starts_with("/__cutex/") {
            let _ = error(stream, 404, "reserved_path");
            break;
        }
        if relay.is_none() && !auth::authorized(store, slug, &head) {
            let _ = error(stream, 401, "ticket_required");
            break;
        }
        if traffic_guard.is_none() {
            traffic_guard = Some(super::traffic::begin(slug));
        }
        let mut record = match store.get(slug) {
            Ok(record) => record,
            Err(e) => {
                let _ = error_with_detail(
                    stream,
                    e.status,
                    e.code,
                    (e.status == 410).then_some(e.message.as_str()),
                );
                break;
            }
        };
        if let Err(refusal) = record.require_view() {
            let _ = error(stream, refusal.status, refusal.code);
            break;
        }
        if record.archived {
            let _ = error(stream, 409, "service_archived");
            break;
        }
        if record.runtime == ServiceRuntime::Static {
            let headers = relay.map(|p| crate::management::relay::service::response_headers(p.origin())).unwrap_or_default();
            let _ = crate::services::static_files::serve_with_headers(&record, method, target, stream, &headers);
            break;
        }
        if record.runtime == ServiceRuntime::Managed {
            if let Some(supervisor) = supervisor {
                record = match supervisor.ready_for_proxy(slug) {
                    Ok(record) => record,
                    Err(error) => {
                        let _ = error_with_detail(stream, error.status, error.code, Some(&error.message));
                        break;
                    }
                };
            }
            let snapshot = supervisor
                .and_then(|s| s.snapshots().remove(slug))
                .or_else(|| store.load_state().remove(slug));
            if record.stopped || record.stopped_idle_at.is_some()
                || snapshot
                    .as_ref()
                    .is_none_or(|s| s.pid.is_none() || s.next_restart_at.is_some())
            {
                let _ = error(stream, 503, "service_stopped");
                break;
            }
        }
        if crate::services::control_ports::check(record.port).is_err() {
            let _ = error(stream,403,"service_control_port_forbidden");break;
        }
        let bytes = if relay.is_some(){forward::request_bytes_relay(&head,&record,&info.peer.ip().to_string())}else{forward::request_bytes(&head,&record,&info.peer.ip().to_string())};
        let request = match bytes {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = error(stream, 400, "invalid_request");
                break;
            }
        };
        if upstream_port.is_some_and(|p| p != record.port) {
            let _ = error(stream, 503, "service_changed");
            break;
        }
        // HTTP keep-alive on the client does not promise that the backend's
        // idle socket survives until its next request. Use one upstream per
        // request; a completed response never leaves a stale socket to reuse.
        // exchange/tunnel retain it until a streaming response/upgrade ends.
        let started = Instant::now();
        let mut upstream = match TcpStream::connect_timeout(
                &([127, 0, 0, 1], record.port).into(),
                Duration::from_secs(3),
            ) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
                    upstream_port = Some(record.port);
                    s
                }
                Err(cause) => {
                    log_failure(slug, method, "connect", &cause, Some(false), started.elapsed());
                    let _ = error(stream, 502, "upstream_unreachable");
                    break;
                }
            };
        let mut upstream_buffer = Vec::new();
        if let Some(expect) = head.get("expect") {
            if !expect.eq_ignore_ascii_case("100-continue") {
                let _ = error(stream, 417, "expectation_failed");
                break;
            }
            if stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                break;
            }
        }
        if let Err(cause) = upstream.write_all(&request) {
            log_failure(slug, method, "request_head", &cause, Some(false), started.elapsed());
            let _ = error(stream, 502, "upstream_unreachable");
            break;
        }
        super::traffic::touch(slug);
        match forward::exchange_with_policy(stream, &mut upstream, buffer, &mut upstream_buffer, &head, slug, relay.map(|p|p.origin())) {
            Ok(forward::Response::Upgrade) => {
                if let Err(cause) = forward::tunnel(stream, &mut upstream, buffer, slug) {
                    log_failure(slug, method, "upgrade_tunnel", &cause, Some(true), started.elapsed());
                }
                break;
            }
            Ok(forward::Response::KeepAlive) if !head.closes() => {}
            _ => break,
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Deliberately omit URLs, headers, bodies and even error Display text: all
/// may contain service credentials. Unknown/custom methods are content too.
fn failure_event(slug: &str, method: &str, stage: &str, cause: &io::Error,
    response_started: Option<bool>, elapsed: Duration) -> serde_json::Value {
    let method = match method {
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" | "CONNECT" | "TRACE" => method,
        _ => "OTHER",
    };
    serde_json::json!({"schema":1,"slug":slug,"method":method,"stage":stage,
        "errorKind":format!("{:?}",cause.kind()),"osError":cause.raw_os_error(),
        "responseStarted":response_started,"elapsedMs":elapsed.as_millis(),"upstreamReused":false})
}

fn log_failure(slug: &str, method: &str, stage: &str, cause: &io::Error,
    response_started: Option<bool>, elapsed: Duration) {
    eprintln!("cutex-service-proxy-error {}", failure_event(slug, method, stage, cause, response_started, elapsed));
}
/// No byte of a new request arrived before the idle timeout or the peer reset.
fn idle_close(error: &io::Error, buffer: &[u8]) -> bool {
    buffer.is_empty()
        && matches!(
            error.kind(),
            io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
        )
}
fn error(stream: &mut TcpStream, status: u16, code: &str) -> io::Result<()> {
    error_with_detail(stream, status, code, None)
}
fn error_with_detail(
    stream: &mut TcpStream,
    status: u16,
    code: &str,
    detail: Option<&str>,
) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    let body = match detail {
        Some(detail) => format!("{code}\n{detail}\n"),
        None => format!("{code}\n"),
    };
    let auth = if status == 401 {
        "X-Cutex-Service-Auth: ticket-required\r\n"
    } else {
        ""
    };
    let policy_headers = crate::management::relay::service::error_headers();
    write!(stream,"HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nX-Cutex-Service-Error: {code}\r\n{auth}Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n{policy_headers}Connection: close\r\n\r\n{body}",body.len())
}

#[cfg(test)]
mod tests;
