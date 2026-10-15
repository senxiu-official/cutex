//! Streaming HTTP/1 transport. Parse just enough response framing to ensure an
//! error for a later request is never injected into an earlier response body.
use super::head::{invalid, read_head, Body, Head, MAX_HEAD};
use crate::services::{store::ServiceRecord, traffic};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// On Windows, shutting down a cloned socket need not wake a pending recv.
/// Bound each read and check cancellation so joining a pump cannot stall.
struct CancellableReader<'a> {
    input: &'a mut TcpStream,
    cancelled: &'a AtomicBool,
}
impl Read for CancellableReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            match self.input.read(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                result => return result,
            }
        }
    }
}

/// A quiet SSE stream must not expire, but a closed browser must release its
/// proxy slot even when the upstream never writes again. This runs only while
/// no client-side HTTP read is in progress, and peek preserves pipelined bytes.
struct WatchedReader<'a> {
    input: &'a mut TcpStream,
    peer: TcpStream,
    uploading: &'a AtomicBool,
}
impl Read for WatchedReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.input.read(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    if self.uploading.load(Ordering::Acquire) {
                        continue;
                    }
                    self.peer.set_read_timeout(Some(Duration::from_millis(1)))?;
                    let peek = self.peer.peek(&mut [0; 1]);
                    self.peer.set_read_timeout(Some(Duration::from_secs(30)))?;
                    match peek {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                "browser disconnected",
                            ))
                        }
                        Err(error)
                            if !matches!(
                                error.kind(),
                                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                            ) =>
                        {
                            return Err(error)
                        }
                        _ => {}
                    }
                }
                result => return result,
            }
        }
    }
}

pub fn request_bytes(head: &Head, record: &ServiceRecord, peer: &str) -> io::Result<Vec<u8>> {
    head.request()?;
    let nominated: Vec<_> = head
        .headers
        .iter()
        .filter(|(k, _)| k == "connection")
        .flat_map(|(_, v)| v.split(','))
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    // A hop-by-hop nomination must not disguise security or framing headers.
    if nominated.iter().any(|v| {
        matches!(
            v.as_str(),
            "host"
                | "cookie"
                | "content-length"
                | "transfer-encoding"
                | "origin"
                | "sec-fetch-site"
                | "sec-fetch-mode"
        )
    }) {
        return Err(invalid());
    }
    let mut result = format!("{}\r\n", head.first);
    // `upstream` mode: the service sees itself at 127.0.0.1:<port>. A browser Origin that is
    // exactly this proxy host's own origin moves with it, so a service whose CSRF / WebSocket
    // gate admits only its own loopback origin accepts its own UI. Any other Origin (another
    // site, another service host, `null`) passes through unchanged and the service judges it.
    let own_origin = head.get("host").map(|host| format!("http://{host}"));
    for (key, value) in &head.headers {
        if key == "origin"
            && record.proxy_host_header == "upstream"
            && own_origin
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(value))
        {
            result.push_str(&format!("Origin: http://127.0.0.1:{}\r\n", record.port));
        } else if key == "host" {
            let host = if record.proxy_host_header == "upstream" {
                format!("127.0.0.1:{}", record.port)
            } else {
                value.clone()
            };
            result.push_str(&format!("Host: {host}\r\n"));
        } else if key == "cookie" {
            let cookie: Vec<_> = value
                .split(';')
                .map(str::trim)
                .filter(|part| {
                    let name = part.split('=').next().unwrap_or("");
                    !name.to_ascii_lowercase().starts_with("cutex_svc_")
                })
                .filter(|part| !part.is_empty())
                .collect();
            if !cookie.is_empty() {
                result.push_str(&format!("Cookie: {}\r\n", cookie.join("; ")));
            }
        } else if key.starts_with("x-forwarded-")
            || matches!(
                key.as_str(),
                "forwarded"
                    | "proxy-authorization"
                    | "proxy-authenticate"
                    | "proxy-connection"
                    | "expect"
                    | "keep-alive"
                    | "te"
            )
        {
            continue;
        } else if nominated.contains(key) && key != "upgrade" {
            continue;
        } else {
            result.push_str(&format!("{key}: {value}\r\n"));
        }
    }
    result.push_str(&format!(
        "X-Forwarded-Host: {}\r\nX-Forwarded-Proto: http\r\nX-Forwarded-For: {peer}\r\n\r\n",
        head.get("host").unwrap()
    ));
    Ok(result.into_bytes())
}
fn copy_n(
    reader: &mut impl Read,
    writer: &mut impl Write,
    buffer: &mut Vec<u8>,
    mut n: u64,
    slug: &str,
) -> io::Result<()> {
    let mut scratch = [0; 16384];
    while n > 0 {
        let available = (n.min(buffer.len() as u64)) as usize;
        if available > 0 {
            writer.write_all(&buffer[..available])?;
            buffer.drain(..available);
            n -= available as u64;
        } else {
            let limit = n.min(scratch.len() as u64) as usize;
            let read = reader.read(&mut scratch[..limit])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated HTTP body",
                ));
            }
            writer.write_all(&scratch[..read])?;
            n -= read as u64;
        }
        traffic::touch(slug);
    }
    Ok(())
}
fn line(reader: &mut impl Read, buffer: &mut Vec<u8>, limit: usize) -> io::Result<Vec<u8>> {
    loop {
        if let Some(end) = buffer.windows(2).position(|p| p == b"\r\n") {
            if end + 2 > limit {
                return Err(invalid());
            }
            return Ok(buffer.drain(..end + 2).collect());
        }
        if buffer.len() >= limit {
            return Err(invalid());
        }
        let mut scratch = [0; 8192];
        let n = reader.read(&mut scratch)?;
        if n == 0 {
            return Err(invalid());
        }
        buffer.extend_from_slice(&scratch[..n]);
    }
}
pub fn copy_body(
    reader: &mut impl Read,
    writer: &mut impl Write,
    buffer: &mut Vec<u8>,
    body: Body,
    slug: &str,
) -> io::Result<()> {
    match body {
        Body::Empty => Ok(()),
        Body::Length(n) => copy_n(reader, writer, buffer, n, slug),
        Body::UntilClose => {
            if !buffer.is_empty() {
                writer.write_all(buffer)?;
                buffer.clear();
                traffic::touch(slug);
            }
            let mut part = [0; 16384];
            loop {
                let n = reader.read(&mut part)?;
                if n == 0 {
                    return Ok(());
                }
                writer.write_all(&part[..n])?;
                traffic::touch(slug);
            }
        }
        Body::Chunked => loop {
            let size_line = line(reader, buffer, 8192)?;
            let text =
                std::str::from_utf8(&size_line[..size_line.len() - 2]).map_err(|_| invalid())?;
            let size = text.split(';').next().unwrap_or("");
            if size.is_empty()
                || !size.bytes().all(|b| b.is_ascii_hexdigit())
                || text.bytes().any(|b| b < 32 || b == 127)
            {
                return Err(invalid());
            }
            let n = u64::from_str_radix(size, 16).map_err(|_| invalid())?;
            writer.write_all(&size_line)?;
            traffic::touch(slug);
            if n == 0 {
                let mut total = 0;
                loop {
                    let trailer = line(reader, buffer, 8192)?;
                    total += trailer.len();
                    if total > MAX_HEAD {
                        return Err(invalid());
                    }
                    if trailer == b"\r\n" {
                        writer.write_all(&trailer)?;
                        return Ok(());
                    }
                    let text = std::str::from_utf8(&trailer[..trailer.len() - 2])
                        .map_err(|_| invalid())?;
                    let (key, value) = text.split_once(':').ok_or_else(invalid)?;
                    if !super::head::token(key)
                        || value.bytes().any(|b| (b < 32 && b != 9) || b == 127)
                        || matches!(
                            key.to_ascii_lowercase().as_str(),
                            "host"
                                | "cookie"
                                | "authorization"
                                | "content-length"
                                | "transfer-encoding"
                                | "connection"
                                | "upgrade"
                        )
                    {
                        return Err(invalid());
                    }
                    writer.write_all(&trailer)?;
                }
            }
            copy_n(reader, writer, buffer, n, slug)?;
            if line(reader, buffer, 2)? != b"\r\n" {
                return Err(invalid());
            }
            writer.write_all(b"\r\n")?;
        },
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    KeepAlive,
    Close,
    Upgrade,
}
fn response(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    request: &Head,
    slug: &str,
    uploading: &AtomicBool,
    upload_failed: &AtomicBool,
) -> io::Result<Response> {
    let mut started = false;
    let result = (|| {
        upstream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut interim = 0;
        loop {
            let raw = read_head(upstream, buffer)?.ok_or_else(invalid)?;
            let head = Head::parse(&raw)?;
            let parts: Vec<_> = head.first.splitn(3, ' ').collect();
            if parts.len() < 2 || !matches!(parts[0], "HTTP/1.1" | "HTTP/1.0") {
                return Err(invalid());
            }
            let status = parts[1].parse::<u16>().map_err(|_| invalid())?;
            if !(100..=599).contains(&status) {
                return Err(invalid());
            }
            if status == 101 {
                if !request.websocket() || !head.websocket() {
                    return Err(invalid());
                }
                client.write_all(&raw)?;
                started = true;
                traffic::touch(slug);
                // The request buffer's pre-read bytes are handled by the caller.
                if !buffer.is_empty() {
                    client.write_all(buffer)?;
                    buffer.clear();
                }
                return Ok(Response::Upgrade);
            }
            if status < 200 {
                interim += 1;
                if interim > 16 {
                    return Err(invalid());
                }
                // Expect:100-continue was acknowledged locally, so suppress duplicates.
                if status != 100 {
                    client.write_all(&raw)?;
                }
                continue;
            }
            let body = if request.request()?.0 == "HEAD" || matches!(status, 204 | 304) {
                Body::Empty
            } else {
                head.body(true)?
            };
            let keep = body != Body::UntilClose
                && !head.has_token("connection", "close")
                && parts[0] != "HTTP/1.0";
            client.write_all(&raw)?;
            started = true;
            traffic::touch(slug);
            // SSE and other streaming bodies can remain silent for over 30 seconds.
            upstream.set_read_timeout(Some(Duration::from_secs(1)))?;
            let peer = client.try_clone()?;
            let copied = copy_body(
                &mut WatchedReader {
                    input: upstream,
                    peer,
                    uploading,
                },
                client,
                buffer,
                body,
                slug,
            );
            copied?;
            return Ok(if keep {
                Response::KeepAlive
            } else {
                Response::Close
            });
        }
    })();
    if result.is_err() && !started {
        if upload_failed.load(Ordering::Acquire) {
            let _ = super::error(client, 400, "invalid_request_body");
        } else {
            let _ = super::error(client, 502, "upstream_unreachable");
        }
    }
    result
}

/// Read the upstream while streaming an upload: a server may reject a request
/// before consuming its body. A completed early response cancels that upload
/// and closes the connection instead of waiting for the browser's body.
pub fn exchange(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    pending: &mut Vec<u8>,
    upstream_pending: &mut Vec<u8>,
    request: &Head,
    slug: &str,
) -> io::Result<Response> {
    let body = request.body(false)?;
    let uploading = AtomicBool::new(false);
    let upload_failed = AtomicBool::new(false);
    if matches!(body, Body::Empty | Body::Length(0)) {
        return response(
            client,
            upstream,
            upstream_pending,
            request,
            slug,
            &uploading,
            &upload_failed,
        );
    }
    client.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut reader = client.try_clone()?;
    let mut writer = upstream.try_clone()?;
    let cancelled = AtomicBool::new(false);
    uploading.store(true, Ordering::Release);
    std::thread::scope(|scope| {
        let upload = scope.spawn(|| {
            let result = copy_body(
                &mut CancellableReader {
                    input: &mut reader,
                    cancelled: &cancelled,
                },
                &mut writer,
                pending,
                body,
                slug,
            );
            if result.is_err() {
                upload_failed.store(true, Ordering::Release);
                let _ = writer.shutdown(Shutdown::Both);
            }
            uploading.store(false, Ordering::Release);
            result
        });
        let result = response(
            client,
            upstream,
            upstream_pending,
            request,
            slug,
            &uploading,
            &upload_failed,
        );
        let early = uploading.load(Ordering::Acquire);
        if early {
            cancelled.store(true, Ordering::Release);
            let _ = client.shutdown(Shutdown::Read);
            let _ = upstream.shutdown(Shutdown::Write);
        }
        let upload_result = upload
            .join()
            .map_err(|_| io::Error::other("proxy upload thread failed"))?;
        client.set_read_timeout(Some(Duration::from_secs(30)))?;
        if early || upload_result.is_err() {
            return result.map(|_| Response::Close);
        }
        result
    })
}
pub fn tunnel(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    pending: &mut Vec<u8>,
    slug: &str,
) -> io::Result<()> {
    if !pending.is_empty() {
        upstream.write_all(pending)?;
        pending.clear();
        traffic::touch(slug);
    }
    client.set_read_timeout(Some(Duration::from_secs(1)))?;
    upstream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut cr = client.try_clone()?;
    let mut uw = upstream.try_clone()?;
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let out = scope.spawn(|| {
            let result = copy_body(
                &mut CancellableReader {
                    input: &mut cr,
                    cancelled: &cancelled,
                },
                &mut uw,
                &mut Vec::new(),
                Body::UntilClose,
                slug,
            );
            cancelled.store(true, Ordering::Release);
            let _ = cr.shutdown(Shutdown::Both);
            let _ = uw.shutdown(Shutdown::Both);
            result
        });
        let result = copy_body(
            &mut CancellableReader {
                input: upstream,
                cancelled: &cancelled,
            },
            client,
            &mut Vec::new(),
            Body::UntilClose,
            slug,
        );
        cancelled.store(true, Ordering::Release);
        let _ = client.shutdown(Shutdown::Both);
        let _ = upstream.shutdown(Shutdown::Both);
        let _ = out.join();
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn body_frames_do_not_consume_next_request() {
        for (input, body, expected) in [
            (b"abcNEXT".as_slice(), Body::Length(3), b"abc".as_slice()),
            (
                b"3;x=y\r\nabc\r\n0\r\nX-Foo: ok\r\n\r\nNEXT".as_slice(),
                Body::Chunked,
                b"3;x=y\r\nabc\r\n0\r\nX-Foo: ok\r\n\r\n".as_slice(),
            ),
        ] {
            let mut buffer = input.to_vec();
            let mut output = Vec::new();
            copy_body(
                &mut io::empty(),
                &mut output,
                &mut buffer,
                body,
                "framing-test",
            )
            .unwrap();
            assert_eq!(output, expected);
            assert_eq!(buffer, b"NEXT");
        }
        for input in ["Z\r\n", "1\r\naX\r\n", "0\r\nCookie: cutex_svc_a=x\r\n\r\n"] {
            assert!(copy_body(
                &mut input.as_bytes(),
                &mut Vec::new(),
                &mut Vec::new(),
                Body::Chunked,
                "framing-test"
            )
            .is_err());
        }
    }
}
