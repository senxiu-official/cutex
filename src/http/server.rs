//! Minimal blocking HTTP helpers used by cutex's local service endpoints.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpStream;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;

#[derive(Debug)]
pub struct SimpleHttpRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct HttpRequestBodyTooLarge {
    pub method: String,
    pub path: String,
    pub content_length: usize,
    pub limit: usize,
}

impl fmt::Display for HttpRequestBodyTooLarge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HTTP request body is {} bytes, exceeding the {} byte limit",
            self.content_length, self.limit
        )
    }
}

impl std::error::Error for HttpRequestBodyTooLarge {}

pub fn read_simple_http_request(stream: &mut TcpStream) -> anyhow::Result<SimpleHttpRequest> {
    read_simple_http_request_with_body_limit(stream, 1024 * 1024)
}

pub fn read_simple_http_request_with_body_limit(
    stream: &mut TcpStream,
    body_limit: usize,
) -> anyhow::Result<SimpleHttpRequest> {
    let mut buffer = Vec::new();
    read_next_http_request(stream, &mut buffer, body_limit, None)?
        .ok_or_else(|| anyhow::anyhow!("Connection closed before HTTP headers"))
}

/// Read one request from a connection that may carry several (keep-alive).
/// `buffer` holds bytes already read past the previous request and keeps any
/// bytes read past this one. With `idle_timeout`, waiting for the first byte
/// of a request may take that long, and a close or timeout before any byte
/// arrives returns `Ok(None)`.
pub fn read_next_http_request(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    body_limit: usize,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<Option<SimpleHttpRequest>> {
    let request_timeout = Some(Duration::from_secs(5));
    let header_end = loop {
        if let Some(pos) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
        if buffer.len() > 1024 * 1024 {
            anyhow::bail!("HTTP request headers are too large");
        }
        let waiting_for_first_byte = buffer.is_empty() && idle_timeout.is_some();
        stream
            .set_read_timeout(if waiting_for_first_byte {
                idle_timeout
            } else {
                request_timeout
            })
            .ok();
        let mut chunk = [0_u8; 4096];
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(error)
                if waiting_for_first_byte
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error).context("Failed to read HTTP request"),
        };
        if n == 0 {
            if waiting_for_first_byte {
                return Ok(None);
            }
            anyhow::bail!("Connection closed before HTTP headers");
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    stream.set_read_timeout(request_timeout).ok();

    let headers_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = headers_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("Missing HTTP request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("Missing HTTP method"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("Missing HTTP path"))?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .map(|value| {
            value
                .parse::<usize>()
                .context("Invalid HTTP Content-Length")
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > body_limit {
        return Err(HttpRequestBodyTooLarge {
            method,
            path,
            content_length,
            limit: body_limit,
        }
        .into());
    }
    while buffer.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let n = stream
            .read(&mut chunk)
            .context("Failed to read HTTP body")?;
        if n == 0 {
            anyhow::bail!("Connection closed before HTTP body");
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body = buffer[header_end..header_end + content_length].to_vec();
    buffer.drain(..header_end + content_length);
    Ok(Some(SimpleHttpRequest {
        method,
        path,
        headers,
        body,
    }))
}

/// Per-connection response behaviour, set by a server loop that supports
/// keep-alive and CORS (the Management listeners). Other servers never set
/// it, so their responses keep `Connection: close`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeepAlive {
    /// Close after the response (default).
    #[default]
    Off,
    /// The next `write_http_response` may keep the connection open.
    Requested,
    /// A complete, length-delimited response was written and the
    /// connection stays open for the next request.
    Kept,
}

thread_local! {
    static KEEP_ALIVE: std::cell::Cell<KeepAlive> = const { std::cell::Cell::new(KeepAlive::Off) };
    static EXTRA_RESPONSE_HEADERS: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

pub fn set_keep_alive(state: KeepAlive) {
    KEEP_ALIVE.with(|cell| cell.set(state));
}

pub fn keep_alive() -> KeepAlive {
    KEEP_ALIVE.with(std::cell::Cell::get)
}

/// Headers (each line ending in `\r\n`) added to every response written on
/// this thread until replaced, e.g. CORS headers.
pub fn set_extra_response_headers(headers: String) {
    EXTRA_RESPONSE_HEADERS.with(|cell| *cell.borrow_mut() = headers);
}

pub fn extra_response_headers() -> String {
    EXTRA_RESPONSE_HEADERS.with(|cell| cell.borrow().clone())
}

/// Status line and headers of a `text/event-stream` response.
pub fn sse_response_head() -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n{}Connection: keep-alive\r\n\r\n",
        extra_response_headers()
    )
}

pub fn require_bridge_token(
    request: &SimpleHttpRequest,
    token: Option<&str>,
) -> anyhow::Result<()> {
    require_service_bridge_token(request, token, "desktop notify")
}

pub fn require_service_bridge_token(
    request: &SimpleHttpRequest,
    token: Option<&str>,
    service: &str,
) -> anyhow::Result<()> {
    let Some(token) = token.filter(|token| !token.is_empty()) else {
        return Ok(());
    };
    let expected = format!("Bearer {token}");
    let actual = request
        .headers
        .get("authorization")
        .map(String::as_str)
        .unwrap_or("");
    if actual != expected {
        anyhow::bail!("Unauthorized {service} request");
    }
    Ok(())
}

thread_local! {
    static ACCEPT_GZIP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// JSON bodies at least this large are gzipped when the client accepts it.
pub const GZIP_MIN_BYTES: usize = 1024;

/// Whether the current request's client accepts gzip (set per request by the
/// Management server; responses on this thread honour it).
pub fn set_accept_gzip(accept: bool) {
    ACCEPT_GZIP.with(|cell| cell.set(accept));
}

/// `Accept-Encoding` lists `gzip` (or `*`) without `q=0`.
pub fn accepts_gzip(header: Option<&str>) -> bool {
    header.is_some_and(|header| {
        header.split(',').any(|part| {
            let mut fields = part.split(';').map(str::trim);
            let coding = fields.next().unwrap_or_default().to_ascii_lowercase();
            let refused = fields.any(|param| {
                param
                    .strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
            });
            (coding == "gzip" || coding == "*") && !refused
        })
    })
}

pub fn gzip_bytes(body: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    let mut encoder = GzEncoder::new(Vec::with_capacity(body.len() / 4), flate2::Compression::fast());
    encoder.write_all(body)?;
    encoder.finish()
}

pub fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let keep = keep_alive() == KeepAlive::Requested;
    let compressed = if ACCEPT_GZIP.with(|cell| cell.get())
        && content_type.starts_with("application/json")
        && body.len() >= GZIP_MIN_BYTES
    {
        gzip_bytes(body).ok()
    } else {
        None
    };
    let (body, encoding) = match &compressed {
        Some(gzipped) => (gzipped.as_slice(), "Content-Encoding: gzip\r\nVary: Accept-Encoding\r\n"),
        None => (body, ""),
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{encoding}{}Connection: {}\r\n\r\n",
        body.len(),
        extra_response_headers(),
        if keep { "keep-alive" } else { "close" },
    );
    stream
        .write_all(response.as_bytes())
        .context("Failed to write HTTP response headers")?;
    stream
        .write_all(body)
        .context("Failed to write HTTP response body")?;
    if keep {
        set_keep_alive(KeepAlive::Kept);
        stream.flush().ok();
    } else {
        let _ = stream.shutdown(Shutdown::Both);
    }
    Ok(())
}

pub fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    value: &Value,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    write_http_response(stream, status, reason, "application/json", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_encoding_parsing() {
        assert!(accepts_gzip(Some("gzip, deflate, br")));
        assert!(accepts_gzip(Some("br;q=1.0, GZIP;q=0.5")));
        assert!(accepts_gzip(Some("*")));
        assert!(!accepts_gzip(Some("gzip;q=0")));
        assert!(!accepts_gzip(Some("identity")));
        assert!(!accepts_gzip(None));
    }

    fn respond_raw(accept: bool, content_type: &'static str, body: Vec<u8>) -> Vec<u8> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            set_accept_gzip(accept);
            write_http_response(&mut stream, 200, "OK", content_type, &body).unwrap();
            set_accept_gzip(false);
        });
        let mut client = TcpStream::connect(address).unwrap();
        let mut raw = Vec::new();
        client.read_to_end(&mut raw).unwrap();
        server.join().unwrap();
        raw
    }

    fn split_head(raw: &[u8]) -> (String, Vec<u8>) {
        let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        (String::from_utf8_lossy(&raw[..at]).into_owned(), raw[at + 4..].to_vec())
    }

    #[test]
    fn json_responses_are_gzipped_only_when_accepted_and_large() {
        use std::io::Read as _;
        let body = serde_json::to_vec(&serde_json::json!({ "text": "x".repeat(4000) })).unwrap();
        let (head, payload) = split_head(&respond_raw(true, "application/json", body.clone()));
        assert!(head.contains("Content-Encoding: gzip"), "{head}");
        assert!(head.contains(&format!("Content-Length: {}", payload.len())), "{head}");
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(payload.as_slice()).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, body);

        let (head, payload) = split_head(&respond_raw(false, "application/json", body.clone()));
        assert!(!head.contains("Content-Encoding"));
        assert_eq!(payload, body);
        let (head, _) = split_head(&respond_raw(true, "application/json", b"{}".to_vec()));
        assert!(!head.contains("Content-Encoding"), "small bodies stay plain");
        let (head, _) = split_head(&respond_raw(true, "text/plain", vec![b'x'; 4000]));
        assert!(!head.contains("Content-Encoding"), "only JSON is compressed");
    }

    #[test]
    fn shared_auth_failure_names_the_calling_service_without_leaking_the_token() {
        let request = SimpleHttpRequest {
            method: "GET".to_string(),
            path: "/api/agents".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let error =
            require_service_bridge_token(&request, Some("secret-bearing-token"), "Agent Bus")
                .unwrap_err()
                .to_string();
        assert_eq!(error, "Unauthorized Agent Bus request");
        assert!(!error.contains("secret-bearing-token"));
        assert!(!error.contains("desktop notify"));
    }

    #[test]
    fn keep_alive_reads_consecutive_requests_from_one_buffer() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(b"POST /a HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET /b HTTP/1.1\r\n\r\n")
                .unwrap();
            stream
        });
        let (mut stream, _) = listener.accept().unwrap();
        let _client = client.join().unwrap();
        let mut buffer = Vec::new();
        let idle = Some(Duration::from_millis(200));
        let first = read_next_http_request(&mut stream, &mut buffer, 1024, idle)
            .unwrap()
            .unwrap();
        assert_eq!((first.path.as_str(), first.body.as_slice()), ("/a", &b"abc"[..]));
        let second = read_next_http_request(&mut stream, &mut buffer, 1024, idle)
            .unwrap()
            .unwrap();
        assert_eq!(second.path, "/b");
        // Idle: nothing more arrives before the timeout.
        assert!(read_next_http_request(&mut stream, &mut buffer, 1024, idle)
            .unwrap()
            .is_none());
    }
}
