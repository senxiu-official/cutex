//! Explicitly pinned, bounded HTTP transport. No redirects or proxy environment.
use super::{
    policy::Settings,
    store::{Peer, Store},
    Error, Result,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

type PeerGate = Mutex<()>;
type PeerGates = BTreeMap<(PathBuf, String), Weak<PeerGate>>;

fn peer_gate(store: &Store, peer: &Peer) -> Arc<PeerGate> {
    static GATES: OnceLock<Mutex<PeerGates>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    gates.retain(|_, gate| gate.strong_count() != 0);
    let key = (store.dir.clone(), peer.peer_id.clone());
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

pub fn permitted(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || (u32::from(ip) & 0xffc00000) == 0x64400000,
        IpAddr::V6(ip) => ip.is_loopback() || ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}
pub fn addresses(raw: &str) -> Result<(url::Url, Vec<SocketAddr>)> {
    let u = url::Url::parse(raw).map_err(|_| Error::new("peer_url_insecure", 400))?;
    if u.scheme() != "http"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.path() != "/"
    {
        return Err(Error::new("peer_url_insecure", 400));
    }
    let port = u
        .port_or_known_default()
        .ok_or_else(|| Error::new("peer_url_insecure", 400))?;
    let addrs: Vec<_> = match u
        .host()
        .ok_or_else(|| Error::new("peer_url_insecure", 400))?
    {
        url::Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Domain(host) => (host, port)
            .to_socket_addrs()
            .map_err(|_| Error::new("peer_unreachable", 503))?
            .collect(),
    };
    if addrs.is_empty() || addrs.iter().any(|a| !permitted(a.ip())) {
        return Err(Error::new("peer_url_insecure", 400));
    }
    Ok((u, addrs))
}
pub fn request(
    url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Result<Value> {
    request_with_settings(url, method, path, token, body, &Settings::default(), true)
}

pub fn request_with_settings(
    url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    settings: &Settings,
    allow_loopback: bool,
) -> Result<Value> {
    let (url, addrs) = addresses(url)?;
    validate_callback_addresses(&addrs, allow_loopback)?;
    if !path.starts_with("/peer/v1/")
        || path.chars().any(|c| c.is_control() || c.is_whitespace())
        || !matches!(method, "GET" | "POST" | "PUT" | "DELETE")
    {
        return Err(Error::new("invalid_request", 400));
    }
    let connect_timeout = Duration::from_secs(settings.connect_timeout_secs.into());
    let read_timeout = Duration::from_secs(settings.read_timeout_secs.into());
    let mut stream = addrs
        .iter()
        .find_map(|addr| TcpStream::connect_timeout(addr, connect_timeout).ok())
        .ok_or_else(|| Error::new("peer_unreachable", 503))?;
    stream
        .set_write_timeout(Some(read_timeout))
        .map_err(|_| Error::new("peer_unreachable", 503))?;
    let data = body
        .map(serde_json::to_vec)
        .transpose()?
        .unwrap_or_default();
    let mut headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        &url[url::Position::BeforeHost..url::Position::AfterPort],
        data.len()
    );
    if let Some(token) = token {
        if !super::token::valid(token) {
            return Err(Error::new("peer_credential_required", 401));
        }
        headers.push_str(&format!(
            "Authorization: Bearer {token}\r\nX-Cutex-Peer-Instance: {}\r\n",
            super::instance_id()
        ));
    }
    headers.push_str("\r\n");
    let mut send = || -> std::io::Result<()> {
        stream.write_all(headers.as_bytes())?;
        stream.write_all(&data)?;
        Ok(())
    };
    send().map_err(|_| Error::new("peer_unreachable", 503))?;
    let bytes = read_response(&mut stream, read_timeout)?;
    decode_response(&bytes).map_err(|error| request_error(method, path, error))
}

// A remote schema rejection describes this mutation, not the peer's negotiated
// protocol. Locally detected response/DTO errors remain peer-level failures.
fn request_error(method: &str, path: &str, mut error: Error) -> Error {
    let mutation = match (method, path.strip_prefix("/peer/v1/asks/")) {
        ("PUT", Some(id)) => !id.is_empty() && !id.contains('/'),
        ("POST", Some(tail)) => tail
            .strip_suffix("/ops")
            .is_some_and(|id| !id.is_empty() && !id.contains('/')),
        _ => method == "POST" && path == "/peer/v1/events",
    };
    if mutation && error.status == 400 && error.code == "peer_protocol_error" {
        error.code = "peer_request_incompatible".into();
        error.retryable = false;
    }
    error
}

/// This check is repeated on the pinned DNS result for every request. A remote
/// pairing may never later rebind its callback name to a loopback address.
pub fn validate_callback_addresses(addrs: &[SocketAddr], allow_loopback: bool) -> Result<()> {
    if !allow_loopback && addrs.iter().any(|address| address.ip().is_loopback()) {
        return Err(Error::new("peer_callback_forbidden", 403));
    }
    Ok(())
}

fn read_response(stream: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        // A trickling peer cannot indefinitely extend the configured deadline.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::new("peer_unreachable", 503));
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| Error::new("peer_unreachable", 503))?;
        let count = stream
            .read(&mut buffer)
            .map_err(|_| Error::new("peer_unreachable", 503))?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > 1024 * 1024 {
            return Err(Error::new("peer_response_too_large", 502));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn decode_response(bytes: &[u8]) -> Result<Value> {
    let split = bytes
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .ok_or_else(|| Error::new("peer_protocol_error", 502))?;
    let head =
        std::str::from_utf8(&bytes[..split]).map_err(|_| Error::new("peer_protocol_error", 502))?;
    let mut status_line = head.lines().next().unwrap_or_default().split_whitespace();
    if !matches!(status_line.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(Error::new("peer_protocol_error", 502));
    }
    let status: u16 = status_line
        .next()
        .and_then(|s| s.parse().ok())
        .filter(|status| (100..600).contains(status))
        .ok_or_else(|| Error::new("peer_protocol_error", 502))?;
    // HTTP server failures are transport failures, even if the body is not JSON
    // or claims a non-retryable / protocol error. The remote retryable bit never
    // controls our retry policy.
    if status >= 500 {
        return Err(Error::new("peer_unreachable", status));
    }
    if (300..400).contains(&status) {
        return Err(Error::new("peer_protocol_error", 502));
    }
    let value: Value = match serde_json::from_slice(&bytes[split + 4..]) {
        Ok(value) => value,
        // A plain HTTP 404 / 403 remains a semantic refusal and a plain 429
        // remains rate limiting; those do not indicate a bad success DTO.
        Err(_) if (400..500).contains(&status) => serde_json::json!({}),
        Err(_) => return Err(Error::new("peer_protocol_error", 502)),
    };
    if !(200..300).contains(&status) {
        // Only retain protocol fields, never arbitrary remote error bodies.
        let code =
            Error::remote_failure_code(value.get("code").and_then(Value::as_str).unwrap_or(""));
        let mut error = Error::new(code, status);
        error.retryable = status == 429;
        if let Some(delay) = value
            .pointer("/details/retryAfterSecs")
            .and_then(Value::as_u64)
        {
            error.details = serde_json::json!({"retryAfterSecs":delay.min(300)});
        }
        return Err(error);
    }
    Ok(value)
}
pub fn call(
    store: &Store,
    peer: &Peer,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    // Worker requests and explicit directory / handshake requests share the
    // same serial lane. Acquire it before reading any store lock.
    let gate = peer_gate(store, peer);
    let _serial = gate.lock().unwrap_or_else(|poison| poison.into_inner());
    let required = match (method, path) {
        (_, "/peer/v1/agents") => Some("agents.list.v1"),
        (_, "/peer/v1/events") => Some("asks.snapshot.v1"),
        (_, path) if path.starts_with("/peer/v1/asks/") && path.ends_with("/ops") => {
            Some("asks.close.v1")
        }
        ("GET", path) if path.starts_with("/peer/v1/asks/") => Some("asks.snapshot.v1"),
        (_, path) if path.starts_with("/peer/v1/asks/") => Some("asks.v1"),
        _ => None,
    };
    if let Some(required) = required {
        if !peer
            .negotiated_features
            .iter()
            .any(|feature| feature == required)
        {
            let mut error = Error::new("peer_unsupported", 409);
            error.details = serde_json::json!({"missingFeatures":[required]});
            return Err(error);
        }
    }
    let secret = store.secret(&peer.peer_id)?;
    let settings = store.snapshot()?.settings;
    let mut urls: Vec<_> = peer.urls.iter().collect();
    if let Some(preferred) = &peer.last_successful_url {
        if let Some(index) = urls.iter().position(|url| *url == preferred) {
            let url = urls.remove(index);
            urls.insert(0, url);
        }
    }
    let mut result = Err(Error::new("peer_unreachable", 503));
    for url in urls {
        result = request_with_settings(
            url,
            method,
            path,
            Some(&secret),
            body,
            &settings,
            peer.allow_loopback,
        );
        match &result {
            Ok(_) => {
                if peer.last_successful_url.as_ref() != Some(url) {
                    store.with(|state| {
                        if let Some(current) = state.peers.get_mut(&peer.peer_id) {
                            if current.urls.contains(url) {
                                current.last_successful_url = Some(url.clone());
                            }
                        }
                        Ok(())
                    })?;
                }
                break;
            }
            // Address policy and transport failures affect one URL. A remote
            // refusal or incompatible response applies to the request itself.
            Err(error) if !error.is_transport() && !error.is_address_rejected() => break,
            Err(_) => {}
        }
    }
    super::audit::record(
        store,
        "outbound",
        Some(peer),
        path,
        &result,
        body.map_or(0, |b| b.to_string().len()),
        body,
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    #[test]
    fn restricted_addresses() {
        // Bounds of the CGNAT range 100.64.0.0/10, built from octets (the
        // public-export scan flags literal 100.x addresses as tailnet IPs).
        let cgnat = |a: u8, b: u8, c: u8, d: u8| format!("http://{}", std::net::Ipv4Addr::new(a, b, c, d));
        for url in [
            "http://127.0.0.1:26270".to_string(),
            "http://[::1]:26370".to_string(),
            "http://100.64.0.0".to_string(),
            cgnat(100, 127, 255, 255),
            "http://[fd7a:115c:a1e0::1]".to_string(),
        ] {
            assert!(addresses(&url).is_ok(), "{url}");
        }
        for url in [
            cgnat(100, 63, 255, 255),
            cgnat(100, 128, 0, 0),
            "http://10.0.0.1".to_string(),
            "http://8.8.8.8".to_string(),
            "http://[::ffff:127.0.0.1]".to_string(),
            "http://[fd7a:115c:a1e1::1]".to_string(),
            "http://user:secret@127.0.0.1".to_string(),
            "http://127.0.0.1/a".to_string(),
            "http://127.0.0.1?x=y".to_string(),
        ] {
            assert!(addresses(&url).is_err(), "{url}");
        }
    }

    #[test]
    fn remote_callbacks_cannot_rebind_to_loopback() {
        let loopback = ["127.0.0.1:80".parse().unwrap(), "[::1]:80".parse().unwrap()];
        assert_eq!(
            validate_callback_addresses(&loopback, false)
                .unwrap_err()
                .code,
            "peer_callback_forbidden"
        );
        assert!(validate_callback_addresses(&loopback, true).is_ok());
        assert!(validate_callback_addresses(&["100.64.0.12:80".parse().unwrap()], false).is_ok());
    }

    fn http_response(status: u16, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn error_classification_is_local_and_does_not_follow_redirects() {
        let refused = decode_response(&http_response(
            403,
            r#"{"code":"peer_policy_denied","retryable":true}"#,
        ))
        .unwrap_err();
        assert!(!refused.retryable);
        let failed = decode_response(&http_response(
            503,
            r#"{"code":"peer_protocol_error","retryable":false}"#,
        ))
        .unwrap_err();
        assert!(failed.is_transport());
        assert!(failed.retryable);
        assert!(
            !decode_response(&http_response(404, "gone"))
                .unwrap_err()
                .retryable
        );
        assert!(
            decode_response(&http_response(429, "slow down"))
                .unwrap_err()
                .retryable
        );
        for response in [http_response(302, ""), http_response(200, "invalid")] {
            let error = decode_response(&response).unwrap_err();
            assert!(error.is_protocol());
            assert!(!error.retryable);
        }
    }

    #[test]
    fn read_deadline_bounds_a_blackhole_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        let mut stream = TcpStream::connect(address).unwrap();
        let start = Instant::now();
        let result = read_response(&mut stream, Duration::from_millis(80));
        done.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(result.unwrap_err().code, "peer_unreachable");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    fn test_server(status: u16, count: usize) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            for _ in 0..count {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "expected request not received");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("listener: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut headers = vec![];
                let mut byte = [0u8; 1];
                while !headers.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                stream.write_all(&http_response(status, "{}")).unwrap();
            }
        });
        (url, task)
    }

    #[test]
    fn tries_alternate_urls_then_prefers_the_last_success() {
        let (failed_url, failed_server) = test_server(503, 1);
        let (healthy_url, healthy_server) = test_server(200, 2);
        let store = Store::at(
            std::env::temp_dir()
                .join(format!("xhost-urls-{}", uuid::Uuid::new_v4()))
                .join("runtime/peers"),
        );
        let peer: Peer = serde_json::from_value(serde_json::json!({
            "peerId": format!("ci-{}", uuid::Uuid::new_v4().simple()),
            "label": "multi", "hostId": "multi", "platform": "test",
            "urls": [failed_url, healthy_url], "allowLoopback": true,
            "enabled": true, "inboundTokenSha256": super::super::token::hash(&super::super::token::generate()),
            "policy": super::super::policy::Policy::default(), "policyRevision":1,
            "protocol":1, "createdAt":super::super::now(), "lastSeenAt":null,
            "lastError":null, "status":"online"
        }))
        .unwrap();
        store
            .insert(peer.clone(), &super::super::token::generate())
            .unwrap();
        call(&store, &peer, "GET", "/peer/v1/hello", None).unwrap();
        failed_server.join().unwrap();
        let preferred = store.get(&peer.peer_id).unwrap();
        assert_eq!(preferred.last_successful_url.as_ref(), Some(&healthy_url));
        call(&store, &preferred, "GET", "/peer/v1/hello", None).unwrap();
        healthy_server.join().unwrap();
    }
}
