//! One-use, certificate-bound service channels. No browser-side control is an
//! authorization boundary here: every channel is checked again by the daemon.
use super::*;
use crate::http::server::SimpleHttpRequest;
use crate::management::client_tokens::DeviceScope;
use crate::services::{
    proxy::head::Head,
    store::{ServiceRecord, ServiceStore},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::io::Write;
use std::net::Shutdown;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

const TICKET_TTL: Duration = Duration::from_secs(60);
const CHANNEL_TTL: Duration = Duration::from_secs(12 * 60 * 60);
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Binding {
    host_id: String,
    slug: String,
    viewer_origin: String,
    device_id: String,
    certificate_id: String,
    principal_stamp: String,
    scope: DeviceScope,
    service_stamp: String,
}
struct Grant {
    binding: Binding,
    expires: Instant,
}
static TICKETS: OnceLock<Mutex<HashMap<String, Grant>>> = OnceLock::new();
fn tickets() -> &'static Mutex<HashMap<String, Grant>> {
    TICKETS.get_or_init(Mutex::default)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    host_id: String,
    slug: String,
    viewer_origin: String,
    #[serde(default)]
    ticket: Option<String>,
}
fn fault(status: u16, code: &'static str, message: &'static str) -> ApiError {
    ApiError(
        status,
        match status {
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Service Unavailable",
        },
        code,
        message,
    )
}
fn unavailable() -> ApiError {
    fault(
        503,
        "relay_view_unavailable",
        "service channel state unavailable",
    )
}
fn invalid() -> ApiError {
    fault(400, "relay_view_invalid", "invalid service channel request")
}
fn forbidden() -> ApiError {
    fault(
        403,
        "relay_view_forbidden",
        "a current mTLS owner device with full scope is required",
    )
}
fn record_stamp(record: &ServiceRecord) -> String {
    fingerprint(&serde_json::to_vec(record).unwrap_or_default())
}
fn viewer_host(host_id: &str, slug: &str) -> String {
    let digest = fingerprint(format!("{host_id}\0{slug}").as_bytes());
    format!("v-{}.localhost", &digest[..48])
}
fn origin_authority(slug: &str, origin: &str) -> Option<String> {
    origin_authority_for(&crate::management::instance::instance_id(), slug, origin)
}
fn origin_authority_for(host_id: &str, slug: &str, origin: &str) -> Option<String> {
    crate::services::store::validate_slug(slug).ok()?;
    // Require canonical explicit port, one exact localhost label, no URL tricks.
    let label = viewer_host(host_id, slug);
    let prefix = format!("http://{label}:");
    let port = origin.strip_prefix(&prefix)?.parse::<u16>().ok()?;
    (port > 0 && origin == format!("{prefix}{port}")).then(|| format!("{label}:{port}"))
}
fn safe_target(target: &str) -> bool {
    if target.len() > 8192 {
        return false;
    }
    let mut path = target
        .split('?')
        .next()
        .unwrap_or(target)
        .as_bytes()
        .to_vec();
    for _ in 0..4 {
        if path.starts_with(b"//")
            || path.contains(&b'\\')
            || path.iter().any(|b| *b < 32 || *b == 127)
            || path
                .split(|b| *b == b'/')
                .any(|part| part == b"." || part == b"..")
            || path == b"/__cutex"
            || path.starts_with(b"/__cutex/")
        {
            return false;
        }
        if !path.contains(&b'%') {
            return true;
        }
        let mut decoded = Vec::new();
        let mut i = 0;
        while i < path.len() {
            if path[i] == b'%' {
                if i + 2 >= path.len() {
                    return false;
                }
                let Ok(text) = std::str::from_utf8(&path[i + 1..i + 3]) else {
                    return false;
                };
                let Ok(byte) = u8::from_str_radix(text, 16) else {
                    return false;
                };
                decoded.push(byte);
                i += 3;
            } else {
                decoded.push(path[i]);
                i += 1;
            }
        }
        path = decoded;
    }
    false
}
#[derive(Clone)]
pub(crate) struct Policy {
    binding: Binding,
    token: VerifiedClientToken,
    relay: Store,
    services: ServiceStore,
}
impl Policy {
    fn current(input: &Input) -> anyhow::Result<Self> {
        if input.host_id != crate::management::instance::instance_id()
            || origin_authority(&input.slug, &input.viewer_origin).is_none()
        {
            bail!(invalid());
        }
        let token = crate::management::listeners::current_client_token().ok_or_else(forbidden)?;
        let peer = current_peer().ok_or_else(forbidden)?;
        if !token.scope.is_full() || !token.principal.is_owner() || token.token_id != peer.device_id
        {
            bail!(forbidden());
        }
        let relay = Store::open().map_err(|_| unavailable())?;
        let services = ServiceStore::open_default().map_err(|_| unavailable())?;
        let record = services.get(&input.slug).map_err(|_| {
            fault(
                404,
                "relay_view_service_not_found",
                "registered service unavailable",
            )
        })?;
        let binding = Binding {
            host_id: input.host_id.clone(),
            slug: input.slug.clone(),
            viewer_origin: input.viewer_origin.clone(),
            device_id: token.token_id.clone(),
            certificate_id: peer.certificate_id,
            principal_stamp: token.principal.access_stamp(),
            scope: token.scope,
            service_stamp: record_stamp(&record),
        };
        let policy = Self {
            binding,
            token,
            relay,
            services,
        };
        if !policy.live() {
            bail!(forbidden());
        }
        Ok(policy)
    }
    pub(crate) fn origin(&self) -> &str {
        &self.binding.viewer_origin
    }
    pub(crate) fn allows(&self, head: &Head) -> bool {
        let Some(authority) = origin_authority(&self.binding.slug, self.origin()) else {
            return false;
        };
        let Ok((method, target)) = head.request() else {
            return false;
        };
        if head.get("host") != Some(authority.as_str())
            || method == "CONNECT"
            || head.get("service-worker").is_some()
            || head.get("sec-fetch-dest") == Some("serviceworker")
            || matches!(head.get("sec-fetch-site"), Some("cross-site" | "same-site"))
        {
            return false;
        }
        let path = target.split('?').next().unwrap_or(target);
        if path == "/__cutex" || path.starts_with("/__cutex/") || !safe_target(target) {
            return false;
        }
        let origin = head.get("origin");
        if origin.is_some_and(|o| o != self.origin())
            || ((head.websocket() || !matches!(method, "GET" | "HEAD" | "OPTIONS"))
                && origin != Some(self.origin()))
        {
            return false;
        }
        // These are native-only credentials. Never let them reach a service.
        if head.headers.iter().any(|(k, v)| {
            k == "authorization"
                && v.split_once(' ').is_some_and(|(scheme, token)| {
                    scheme.eq_ignore_ascii_case("bearer")
                        && crate::management::client_tokens::looks_like_client_token(token)
                })
        }) {
            return false;
        }
        self.live()
    }
    pub(crate) fn live(&self) -> bool {
        self.token.scope.is_full()
            && self.token.principal.is_owner()
            && self.token.still_valid_at(&self.relay.tokens)
            && self
                .relay
                .active(&self.binding.certificate_id)
                .ok()
                .flatten()
                .is_some_and(|r| r.device_id == self.binding.device_id)
            && self.services.get(&self.binding.slug).is_ok_and(|r| {
                !r.stopped
                    && !crate::services::control_ports::forbidden(r.port)
                    && record_stamp(&r) == self.binding.service_stamp
            })
    }
    pub(crate) fn watch(&self, stream: &TcpStream) -> std::io::Result<Watch> {
        let socket = stream.try_clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        let quit = stop.clone();
        let policy = self.clone();
        let worker = std::thread::spawn(move || {
            let start = Instant::now();
            while !quit.load(Ordering::Acquire) {
                if start.elapsed() >= CHANNEL_TTL || !policy.live() {
                    let _ = socket.shutdown(Shutdown::Both);
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        Ok(Watch {
            stop,
            worker: Some(worker),
        })
    }
}
pub(crate) struct Watch {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn issue(binding: Binding) -> anyhow::Result<serde_json::Value> {
    let mut vault = tickets().lock().map_err(|_| unavailable())?;
    let now = Instant::now();
    vault.retain(|_, g| g.expires > now);
    if vault.len() >= 256 {
        bail!(fault(
            503,
            "relay_view_ticket_limit",
            "too many outstanding service grants"
        ));
    }
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| unavailable())?;
    let ticket = URL_SAFE_NO_PAD.encode(bytes);
    let result = json!({"ticket":ticket,"expiresAt":(chrono::Utc::now()+chrono::Duration::seconds(60)).to_rfc3339(),"ttlSeconds":60,"channelPath":"/v2/service-relay/channel","protocol":"cutex-service-v1","hostId":binding.host_id,"slug":binding.slug,"viewerOrigin":binding.viewer_origin});
    vault.insert(
        fingerprint(ticket.as_bytes()),
        Grant {
            binding,
            expires: now + TICKET_TTL,
        },
    );
    Ok(result)
}
#[cfg(test)]
fn consume(ticket: &str, binding: &Binding) -> bool {
    let Ok(mut vault) = tickets().lock() else {
        return false;
    };
    vault
        .remove(&fingerprint(ticket.as_bytes()))
        .is_some_and(|g| g.expires > Instant::now() && g.binding == *binding)
}
pub(crate) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let result = (|| -> anyhow::Result<()> {
        if request.method != "POST" || request.body.len() > 4096 {
            bail!(invalid());
        }
        let input: Input = serde_json::from_slice(&request.body).map_err(|_| invalid())?;
        // Consume before binding validation: a guessed binding cannot probe/reuse
        // a stolen grant on another host, device or slug.
        let presented = if path == "/v2/service-relay/channel" {
            let ticket = input.ticket.as_deref().ok_or_else(invalid)?;
            tickets()
                .lock()
                .map_err(|_| unavailable())?
                .remove(&fingerprint(ticket.as_bytes()))
        } else {
            None
        };
        let policy = Policy::current(&input)?;
        match path {
            "/v2/service-relay/tickets" if input.ticket.is_none() => {
                crate::http::server::write_json_response(
                    stream,
                    201,
                    "Created",
                    &issue(policy.binding.clone())?,
                )?
            }
            "/v2/service-relay/channel" => {
                if !request.headers.get("connection").is_some_and(|v| {
                    v.split(',')
                        .any(|s| s.trim().eq_ignore_ascii_case("upgrade"))
                }) || request.headers.get("upgrade").map(String::as_str)
                    != Some("cutex-service-v1")
                {
                    bail!(invalid());
                }
                if !presented
                    .is_some_and(|g| g.expires > Instant::now() && g.binding == policy.binding)
                {
                    bail!(fault(
                        403,
                        "relay_view_ticket_invalid",
                        "invalid, expired or already used service grant"
                    ));
                }
                let info =
                    crate::management::listeners::current_connection().ok_or_else(forbidden)?;
                let _watch = policy.watch(stream)?;
                stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: cutex-service-v1\r\nCache-Control: no-store\r\n\r\n")?;
                stream.flush()?;
                // After 101, failures belong to this service channel, never a
                // second Management error response inside the byte stream.
                crate::services::proxy::serve_relay(
                    stream,
                    &info,
                    &policy.services,
                    &input.slug,
                    &policy,
                );
            }
            _ => bail!(invalid()),
        }
        Ok(())
    })();
    if let Err(error) = result {
        super::refusal("service_channel");
        let fallback = ApiError(
            503,
            "Service Unavailable",
            "relay_view_unavailable",
            "service channel unavailable",
        );
        let e = error.downcast_ref::<ApiError>().unwrap_or(&fallback);
        return crate::management::v2::server::write_v2_error(
            stream,
            e.0,
            e.1,
            e.2,
            e.3,
            e.0 == 503,
            json!({}),
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests;

thread_local! { static RESPONSE_ORIGIN: RefCell<Option<String>> = const {RefCell::new(None)}; }
pub(crate) fn with_response_policy(origin: &str, run: impl FnOnce()) {
    struct Reset(Option<String>);
    impl Drop for Reset {
        fn drop(&mut self) {
            RESPONSE_ORIGIN.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(RESPONSE_ORIGIN.with(|s| s.replace(Some(origin.to_string()))));
    run();
}
pub(crate) fn error_headers() -> String {
    RESPONSE_ORIGIN.with(|s| {
        s.borrow()
            .as_deref()
            .map(response_headers)
            .unwrap_or_default()
    })
}
/// Added alongside any upstream CSP; multiple policies intersect. Native must
/// enforce the same policy for its own responses and navigation paths too.
pub(crate) fn response_headers(origin: &str) -> String {
    let ws = origin.replacen("http://", "ws://", 1);
    format!("Content-Security-Policy: default-src 'self' data: blob:; connect-src 'self' {ws}; script-src 'self' 'unsafe-inline' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; worker-src 'none'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-src 'self'; frame-ancestors 'self'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n")
}
fn sanitized_csp(value: &str) -> std::io::Result<String> {
    let mut policies = Vec::new();
    let mut reporting = false;
    for policy in value.split(',') {
        if policy.trim().is_empty() {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut kept = Vec::new();
        for directive in policy.split(';').map(str::trim).filter(|d| !d.is_empty()) {
            let name = directive.split_ascii_whitespace().next().unwrap();
            if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            if name.eq_ignore_ascii_case("report-uri") || name.eq_ignore_ascii_case("report-to") {
                reporting = true;
            } else {
                kept.push(directive);
            }
        }
        policies.push(kept.join("; "));
    }
    if reporting && policies.len() > 1 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(policies.join(", "))
}
fn sanitized_cookie(value: &str) -> Option<String> {
    let mut parts = value.split(';');
    let pair = parts.next()?.trim();
    let (name, value) = pair.split_once('=')?;
    let name = name.trim();
    let value = value.trim();
    let octets = if value.starts_with('"') {
        value.strip_prefix('"')?.strip_suffix('"')?
    } else {
        value
    };
    // RFC6265 cookie-octet. Refuse combined/malformed pairs rather than letting
    // a browser reinterpret a comma or quote as a second reserved cookie.
    if !octets
        .bytes()
        .all(|b| matches!(b,0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e))
    {
        return None;
    }
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    };
    if !token(name)
        || name.to_ascii_lowercase().starts_with("cutex_view_")
        || name.to_ascii_lowercase().starts_with("cutex_svc_")
    {
        return None;
    }
    let mut kept = vec![pair];
    for part in parts.map(str::trim).filter(|p| !p.is_empty()) {
        let attribute = part.split_once('=').map_or(part, |(name, _)| name).trim();
        if !token(attribute) {
            return None;
        }
        if !attribute.eq_ignore_ascii_case("domain") {
            kept.push(part);
        }
    }
    Some(kept.join("; "))
}
pub(crate) fn response_head(head: &Head, origin: &str) -> std::io::Result<Vec<u8>> {
    if head.headers.iter().any(|(key, value)| {
        key == "connection"
            && value.split(',').any(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "content-security-policy"
                        | "set-cookie"
                        | "referrer-policy"
                        | "x-content-type-options"
                )
            })
    }) {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut raw = format!("{}\r\n", head.first);
    for (key, value) in &head.headers {
        if matches!(
            key.as_str(),
            "alt-svc"
                | "link"
                | "refresh"
                | "report-to"
                | "reporting-endpoints"
                | "nel"
                | "content-security-policy-report-only"
        ) {
            continue;
        }
        if key == "content-security-policy" {
            raw.push_str(&format!("{key}: {}\r\n", sanitized_csp(value)?));
            continue;
        }
        if key == "location" {
            if value.bytes().any(|b| b < 0x20) || value.contains('\\') || value.starts_with("//") {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            let base = url::Url::parse(origin).map_err(|_| std::io::ErrorKind::InvalidData)?;
            let target = base
                .join(value)
                .map_err(|_| std::io::ErrorKind::InvalidData)?;
            if target.origin() != base.origin() {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
        }
        if key == "set-cookie" {
            if let Some(value) = sanitized_cookie(value) {
                raw.push_str(&format!("{key}: {value}\r\n"));
            }
        } else {
            raw.push_str(&format!("{key}: {value}\r\n"));
        }
    }
    raw.push_str(&response_headers(origin));
    raw.push_str("\r\n");
    Ok(raw.into_bytes())
}
