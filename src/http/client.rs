//! Minimal blocking HTTP client helpers for local cutex services.

use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use url::Url;

pub struct HttpJsonRequest<'a> {
    pub url: &'a str,
    pub method: &'a str,
    pub token: Option<&'a str>,
    pub body: Option<&'a [u8]>,
    pub timeout: Duration,
    pub invalid_url_context: &'a str,
    pub only_http_message: &'a str,
    pub missing_host_message: &'a str,
    pub connect_context: &'a str,
    pub read_context: &'a str,
    pub non_success_prefix: &'a str,
    pub parse_context: &'a str,
    pub ok_text_as_null: bool,
}

pub struct HttpPostStatusRequest<'a> {
    pub url: &'a str,
    pub token: Option<&'a str>,
    pub body: &'a [u8],
    pub timeout: Duration,
    pub invalid_url_context: &'a str,
    pub only_http_message: &'a str,
    pub missing_host_message: &'a str,
    pub connect_context: &'a str,
    pub non_success_message: &'a str,
}

pub fn http_json_request(options: HttpJsonRequest<'_>) -> anyhow::Result<Value> {
    let url = Url::parse(options.url).with_context(|| options.invalid_url_context.to_string())?;
    if url.scheme() != "http" {
        anyhow::bail!("{}", options.only_http_message);
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{}", options.missing_host_message))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = format!("{host}:{port}");
    crate::config::instance::ensure_port_allowed(port).with_context(|| options.connect_context.to_string())?;
    let mut stream =
        TcpStream::connect(addr).with_context(|| options.connect_context.to_string())?;
    stream.set_write_timeout(Some(options.timeout)).ok();
    stream.set_read_timeout(Some(options.timeout)).ok();
    let mut request_path = url.path().to_string();
    if request_path.is_empty() {
        request_path.push('/');
    }
    if let Some(query) = url.query() {
        request_path.push('?');
        request_path.push_str(query);
    }
    let body = options.body.unwrap_or(b"");
    let auth = options
        .token
        .filter(|token| !token.is_empty())
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let content_type = if body.is_empty() {
        String::new()
    } else {
        "Content-Type: application/json\r\n".to_string()
    };
    let request = format!(
        "{} {request_path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n",
        options.method,
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    let response =
        read_http_response(&mut stream).with_context(|| options.read_context.to_string())?;
    let text = String::from_utf8_lossy(&response);
    let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    if !headers.starts_with("HTTP/1.1 2") {
        return Err(
            HttpStatusError::from_response(options.non_success_prefix, headers, body).into(),
        );
    }
    let body = body.trim();
    if body.is_empty() || (options.ok_text_as_null && body == "ok") {
        return Ok(Value::Null);
    }
    serde_json::from_str(body).with_context(|| options.parse_context.to_string())
}

/// A non-2xx answer from a local cutex service. `Display` is the historical
/// `"<prefix>: <message> (<status line>)"` text; callers that need the typed
/// v2 `error.code` or the HTTP status downcast the `anyhow::Error` to this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpStatusError {
    /// HTTP status code (0 when the status line could not be parsed).
    pub status: u16,
    /// `error.code` of a v2 error body, when present.
    pub code: Option<String>,
    /// `error.message` of a v2 error body, when present.
    pub message: Option<String>,
    display: String,
}

impl HttpStatusError {
    fn from_response(prefix: &str, headers: &str, body: &str) -> Self {
        let value = serde_json::from_str::<Value>(body).ok();
        let field = |pointer: &str| {
            value
                .as_ref()
                .and_then(|value| value.pointer(pointer))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let status = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        Self {
            status,
            code: field("/error/code"),
            message: field("/error/message"),
            display: format!("{prefix}: {}", response_error_detail(headers, body)),
        }
    }
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.display)
    }
}

impl std::error::Error for HttpStatusError {}

fn response_error_detail(headers: &str, body: &str) -> String {
    let value = serde_json::from_str::<Value>(body).ok();
    let message = value.as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .unwrap_or(body);
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let status = headers.lines().next().unwrap_or("HTTP error");
    if message.is_empty() { status.to_string() } else { format!("{message} ({status})") }
}

fn read_http_response(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut header_end: Option<usize> = None;
    let mut content_length = None;
    loop {
        if let Some(header_end) = header_end {
            if let Some(length) = content_length {
                let response_end = header_end
                    .checked_add(4)
                    .and_then(|body_start| body_start.checked_add(length))
                    .context("HTTP response length overflow")?;
                if response.len() >= response_end {
                    response.truncate(response_end);
                    return Ok(response);
                }
            }
        }

        let mut buffer = [0_u8; 8192];
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            if header_end.is_some() && content_length.is_none() {
                return Ok(response);
            }
            anyhow::bail!("incomplete HTTP response");
        }
        response.extend_from_slice(&buffer[..count]);

        if header_end.is_none() {
            let Some(end) = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&response[..end]).to_ascii_lowercase();
            for header in headers.lines().skip(1) {
                let Some((name, value)) = header.split_once(':') else {
                    continue;
                };
                if name.trim() != "content-length" {
                    continue;
                }
                if content_length.is_some() {
                    anyhow::bail!("ambiguous HTTP Content-Length");
                }
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .context("invalid HTTP Content-Length")?,
                );
            }
            if headers
                .lines()
                .any(|header| header.starts_with("transfer-encoding:"))
            {
                anyhow::bail!("unsupported HTTP Transfer-Encoding");
            }
            header_end = Some(end);
        }
    }
}

#[cfg(test)]
mod error_detail_tests {
    #[test]
    fn structured_http_failure_puts_cause_before_status_on_one_line() {
        let error = super::response_error_detail(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json",
            r#"{"error":{"code":"owner_action_required","message":"stock runtime protected by active task"}}"#,
        );
        assert_eq!(error, "stock runtime protected by active task (HTTP/1.1 500 Internal Server Error)");
        assert_eq!(super::response_error_detail("HTTP/1.1 500 Error", "first\nsecond"), "first second (HTTP/1.1 500 Error)");
    }

    #[test]
    fn status_error_keeps_display_and_exposes_typed_code() {
        let error = super::HttpStatusError::from_response(
            "cutex management API returned non-success",
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json",
            r#"{"error":{"code":"conflict","message":"conflict: durable_agent_missing"}}"#,
        );
        assert_eq!(error.status, 409);
        assert_eq!(error.code.as_deref(), Some("conflict"));
        assert_eq!(error.message.as_deref(), Some("conflict: durable_agent_missing"));
        assert_eq!(
            error.to_string(),
            "cutex management API returned non-success: conflict: durable_agent_missing (HTTP/1.1 409 Conflict)"
        );
        let plain = super::HttpStatusError::from_response("prefix", "garbage", "not json");
        assert_eq!((plain.status, plain.code, plain.message), (0, None, None));
    }
}

#[cfg(test)]
mod response_framing_tests {
    use super::*;
    use crate::http::server::read_simple_http_request;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn json_request_returns_at_content_length_without_peer_close() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_simple_http_request(&mut stream).unwrap();
            let body = r#"{"ok":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            let _ = held.recv_timeout(Duration::from_secs(2));
        });
        let value = http_json_request(HttpJsonRequest {
            url: &format!("http://127.0.0.1:{port}/agents"),
            method: "GET",
            token: None,
            body: None,
            timeout: Duration::from_millis(300),
            invalid_url_context: "invalid URL",
            only_http_message: "HTTP required",
            missing_host_message: "host required",
            connect_context: "connect failed",
            read_context: "read failed",
            non_success_prefix: "non-success",
            parse_context: "parse failed",
            ok_text_as_null: false,
        })
        .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
        release.send(()).unwrap();
        server.join().unwrap();
    }
}

pub fn http_post_json_expect_success(options: HttpPostStatusRequest<'_>) -> anyhow::Result<()> {
    let url = Url::parse(options.url).with_context(|| options.invalid_url_context.to_string())?;
    if url.scheme() != "http" {
        anyhow::bail!("{}", options.only_http_message);
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{}", options.missing_host_message))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = format!("{host}:{port}");
    crate::config::instance::ensure_port_allowed(port).with_context(|| options.connect_context.to_string())?;
    let mut stream =
        TcpStream::connect(addr).with_context(|| options.connect_context.to_string())?;
    stream.set_write_timeout(Some(options.timeout)).ok();
    stream.set_read_timeout(Some(options.timeout)).ok();
    let mut path = url.path().to_string();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let auth = options
        .token
        .filter(|token| !token.is_empty())
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n",
        options.body.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.write_all(options.body)?;
    let mut response = [0_u8; 64];
    let n = stream.read(&mut response).unwrap_or(0);
    if n > 0 && !String::from_utf8_lossy(&response[..n]).starts_with("HTTP/1.1 2") {
        anyhow::bail!("{}", options.non_success_message);
    }
    Ok(())
}

pub fn http_local_root_status_ok(port: u16, token: Option<&str>, timeout: Duration) -> bool {
    if crate::config::instance::ensure_port_allowed(port).is_err() {
        return false;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    http_root_status_ok_on_stream(
        &mut stream,
        &format!("127.0.0.1:{port}"),
        token,
        timeout,
        false,
    )
}

pub fn http_base_url_root_status_ok(base_url: &str, timeout: Duration) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let port = url.port_or_known_default().unwrap_or(80);
    if crate::config::instance::ensure_port_allowed(port).is_err() {
        return false;
    }
    let Ok(mut stream) = TcpStream::connect(format!("{host}:{port}")) else {
        return false;
    };
    http_root_status_ok_on_stream(&mut stream, &format!("{host}:{port}"), None, timeout, true)
}

pub fn connect_local_port(port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    crate::config::instance::ensure_port_allowed(port)?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, timeout)
}

fn http_root_status_ok_on_stream(
    stream: &mut TcpStream,
    host_header: &str,
    token: Option<&str>,
    timeout: Duration,
    connection_close: bool,
) -> bool {
    stream.set_write_timeout(Some(timeout)).ok();
    stream.set_read_timeout(Some(timeout)).ok();
    let auth = token
        .filter(|token| !token.is_empty())
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let connection = if connection_close {
        "Connection: close\r\n"
    } else {
        ""
    };
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {host_header}\r\n{auth}Content-Length: 0\r\n{connection}\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0_u8; 128];
    match stream.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
        Err(_) => false,
    }
}
