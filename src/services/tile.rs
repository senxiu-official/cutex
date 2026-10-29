//! Home page tiles of services (capability `service-tiles`; cutex-desktop
//! docs/research/home-page-20261019.md §2).
//!
//! A tile is a small read-only template the desktop draws in its own style:
//! either static (`tile` on the record) or fetched by the daemon from a path
//! inside the service (`tilePath`, default [`DEFAULT_TILE_PATH`]). Templates
//! are validated strictly ([`validate_tile`]): unknown fields are dropped and
//! every path must stay inside the service (no URLs, `javascript:`, `data:`).
//! The daemon talks to the service the way the service proxy does (loopback
//! port, the proxy's Host header; a static root through the static file
//! resolver), with a short deadline and size limits, follows only redirects
//! that stay inside the service, and caches the answer for `refreshSeconds`.

use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;
use serde_json::Map;
use serde_json::Value;

use super::health::RuntimeSnapshot;
use super::proxy::head::Body;
use super::proxy::head::Head;
use super::store::ServiceRecord;
use super::store::ServiceRuntime;
use super::store::ServicesError;
use super::store::ServicesResult;

pub const CAPABILITY: &str = "service-tiles";
pub const DEFAULT_TILE_PATH: &str = "/.well-known/cutex-tile.json";
pub const MAX_TILE_BYTES: usize = 16 * 1024;
pub const MAX_IMAGE_BYTES: usize = 512 * 1024;
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
pub const MIN_REFRESH_SECONDS: u64 = 60;
pub const MAX_REFRESH_SECONDS: u64 = 86_400;
/// Cache lifetime of a fetched tile without `refreshSeconds`, and of a
/// service that has no tile at the default path.
pub const DEFAULT_REFRESH_SECONDS: u64 = 300;
/// Cache lifetime of a failed fetch.
pub const ERROR_RETRY_SECONDS: u64 = 60;
pub const SIZES: [&str; 4] = ["s", "m", "w", "l"];
const MAX_PATH_BYTES: usize = 512;
const MAX_REDIRECTS: usize = 3;
const MAX_RESPONSE_HEAD: usize = 16 * 1024;
const MAX_CACHE_ENTRIES: usize = 1024;

// ------------------------------------------------------------- validation

fn text(value: &Value, field: &str, max: usize) -> Result<String, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{field} must be a string"))?
        .trim();
    let count = text.chars().count();
    if count == 0 || count > max || text.chars().any(char::is_control) {
        return Err(format!(
            "{field} must be 1-{max} characters without control characters"
        ));
    }
    Ok(text.to_string())
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// A path inside the service: starts with one `/`, no scheme, host,
/// backslash, fragment, whitespace, dot segment or reserved `/__cutex` path
/// (also after percent decoding). The query string is kept.
pub fn validate_tile_path(field: &str, path: &str) -> Result<String, String> {
    let outside = || {
        format!(
            "{field} must be a path inside the service starting with '/' (no URLs, javascript: or data:)"
        )
    };
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(outside());
    }
    if path.len() > MAX_PATH_BYTES
        || path
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\' || c == '#')
    {
        return Err(format!(
            "{field} must be at most {MAX_PATH_BYTES} bytes without whitespace, '\\' or '#'"
        ));
    }
    let route = path.split('?').next().unwrap_or(path);
    let decoded = percent_decode(route).ok_or_else(|| format!("{field} has an invalid percent escape"))?;
    if decoded.starts_with("//")
        || decoded.contains('\\')
        || decoded.chars().any(char::is_control)
        || decoded.split('/').any(|part| part == "." || part == "..")
    {
        return Err(outside());
    }
    if decoded == "/__cutex" || decoded.starts_with("/__cutex/") {
        return Err(format!("{field} must not use the reserved /__cutex path"));
    }
    Ok(path.to_string())
}

/// The normalized template: known fields checked, unknown fields dropped,
/// `refreshSeconds` clamped to 60-86400, unknown `sizes` dropped.
pub fn validate_tile(value: &Value) -> Result<Value, String> {
    let object = value.as_object().ok_or("tile must be an object")?;
    let field = |key: &str| object.get(key).filter(|value| !value.is_null());
    let mut out = Map::new();
    for (key, max) in [("title", 60), ("subtitle", 120), ("badge", 12)] {
        if let Some(value) = field(key) {
            out.insert(key.into(), json!(text(value, key, max)?));
        }
    }
    if let Some(value) = field("lines") {
        let items = value.as_array().ok_or("lines must be an array")?;
        if items.len() > 3 {
            return Err("lines takes at most 3 lines".into());
        }
        let lines = items
            .iter()
            .enumerate()
            .map(|(index, line)| text(line, &format!("lines[{index}]"), 120))
            .collect::<Result<Vec<_>, _>>()?;
        if !lines.is_empty() {
            out.insert("lines".into(), json!(lines));
        }
    }
    if let Some(value) = field("image") {
        let path = value.as_str().ok_or("image must be a string")?;
        out.insert("image".into(), json!(validate_tile_path("image", path)?));
    }
    if let Some(value) = field("progress") {
        let progress = value.as_object().ok_or("progress must be an object")?;
        let fraction = progress
            .get("value")
            .and_then(Value::as_f64)
            .filter(|fraction| (0.0..=1.0).contains(fraction))
            .ok_or("progress.value must be a number from 0 to 1")?;
        let mut checked = Map::new();
        checked.insert("value".into(), json!(fraction));
        if let Some(label) = progress.get("label").filter(|label| !label.is_null()) {
            checked.insert("label".into(), json!(text(label, "progress.label", 40)?));
        }
        out.insert("progress".into(), Value::Object(checked));
    }
    if let Some(value) = field("actions") {
        let items = value.as_array().ok_or("actions must be an array")?;
        if items.len() > 3 {
            return Err("actions takes at most 3 actions".into());
        }
        let mut actions = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let action = item
                .as_object()
                .ok_or_else(|| format!("actions[{index}] must be an object"))?;
            let label = text(
                action.get("label").unwrap_or(&Value::Null),
                &format!("actions[{index}].label"),
                20,
            )?;
            let path = action
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("actions[{index}].path must be a string"))?;
            let path = validate_tile_path(&format!("actions[{index}].path"), path)?;
            actions.push(json!({"label": label, "path": path}));
        }
        if !actions.is_empty() {
            out.insert("actions".into(), json!(actions));
        }
    }
    if let Some(value) = field("refreshSeconds") {
        let seconds = value
            .as_u64()
            .ok_or("refreshSeconds must be a non-negative integer")?;
        out.insert(
            "refreshSeconds".into(),
            json!(seconds.clamp(MIN_REFRESH_SECONDS, MAX_REFRESH_SECONDS)),
        );
    }
    if let Some(value) = field("sizes") {
        let items = value.as_array().ok_or("sizes must be an array")?;
        if items.len() > 16 {
            return Err("sizes takes at most 16 entries".into());
        }
        let mut sizes = Vec::new();
        for item in items {
            let size = item.as_str().ok_or("sizes must be strings")?;
            if SIZES.contains(&size) && !sizes.contains(&size) {
                sizes.push(size);
            }
        }
        sizes.sort_by_key(|size| SIZES.iter().position(|known| known == size));
        if !sizes.is_empty() {
            out.insert("sizes".into(), json!(sizes));
        }
    }
    Ok(Value::Object(out))
}

/// `tile` of a register / update request: 400 `invalid_tile`.
pub fn validate_tile_request(value: &Value) -> ServicesResult<Value> {
    let compact = serde_json::to_vec(value).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
    if compact > MAX_TILE_BYTES {
        return Err(ServicesError::new(400, "invalid_tile", "tile must be at most 16 KiB of JSON"));
    }
    validate_tile(value).map_err(|message| ServicesError::new(400, "invalid_tile", message))
}

/// `tilePath` of a register / update request: 400 `invalid_tile`.
pub fn validate_tile_path_request(path: &str) -> ServicesResult<String> {
    validate_tile_path("tilePath", path.trim())
        .map_err(|message| ServicesError::new(400, "invalid_tile", message))
}

// ----------------------------------------------------------------- fetch

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchError {
    /// `service_stopped`, `unreachable`, `timeout`, `too_large`,
    /// `not_found`, `http_status`, `redirect_outside_service`,
    /// `too_many_redirects`, `invalid_response`, `invalid_json`,
    /// `invalid_tile`.
    pub code: &'static str,
    pub message: String,
}

impl FetchError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message})
    }
}

fn timeout() -> FetchError {
    FetchError::new("timeout", "the service did not answer in time")
}

fn too_large(max: usize) -> FetchError {
    FetchError::new("too_large", format!("the answer is larger than {max} bytes"))
}

fn invalid_response(message: &str) -> FetchError {
    FetchError::new("invalid_response", message)
}

fn not_found(path: &str) -> FetchError {
    FetchError::new("not_found", format!("the service has nothing at {path}"))
}

/// Whether the daemon may contact the service now: a managed service only
/// while its process runs (a tile never starts a stopped service).
pub fn fetchable(record: &ServiceRecord, snapshot: Option<&RuntimeSnapshot>) -> bool {
    record.runtime != ServiceRuntime::Managed
        || (!record.stopped
            && snapshot.is_some_and(|state| state.pid.is_some() && state.next_restart_at.is_none()))
}

/// GET `path` from the service: at most `max_bytes` of body within `timeout`.
pub fn fetch(
    record: &ServiceRecord,
    path: &str,
    max_bytes: usize,
    timeout: Duration,
    accept: &str,
) -> Result<Vec<u8>, FetchError> {
    if record.runtime == ServiceRuntime::Static {
        return fetch_static(record, path, max_bytes);
    }
    let deadline = Instant::now() + timeout;
    let mut path = path.to_string();
    for _ in 0..=MAX_REDIRECTS {
        match http_get(record, &path, max_bytes, deadline, accept)? {
            Answer::Body(body) => return Ok(body),
            Answer::Redirect(location) => path = redirect_target(record, &path, &location)?,
        }
    }
    Err(FetchError::new(
        "too_many_redirects",
        format!("more than {MAX_REDIRECTS} redirects"),
    ))
}

fn fetch_static(record: &ServiceRecord, path: &str, max_bytes: usize) -> Result<Vec<u8>, FetchError> {
    let file = record
        .root
        .as_deref()
        .and_then(|root| super::static_files::resolve(Path::new(root), path))
        .ok_or_else(|| not_found(path))?;
    let file = std::fs::File::open(file).map_err(|_| not_found(path))?;
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| not_found(path))?;
    if bytes.len() > max_bytes {
        return Err(too_large(max_bytes));
    }
    Ok(bytes)
}

enum Answer {
    Body(Vec<u8>),
    Redirect(String),
}

fn remaining(deadline: Instant) -> Result<Duration, FetchError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(timeout)
}

fn io_error(error: std::io::Error) -> FetchError {
    match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => timeout(),
        _ => FetchError::new("unreachable", format!("the service connection failed: {error}")),
    }
}

fn http_get(
    record: &ServiceRecord,
    path: &str,
    max_bytes: usize,
    deadline: Instant,
    accept: &str,
) -> Result<Answer, FetchError> {
    let address = SocketAddr::from(([127, 0, 0, 1], record.port));
    let mut stream = TcpStream::connect_timeout(&address, remaining(deadline)?).map_err(|error| {
        if matches!(error.kind(), std::io::ErrorKind::TimedOut) {
            timeout()
        } else {
            FetchError::new("unreachable", "the service is not reachable")
        }
    })?;
    let proxy_host = format!("{}.svc.localhost", record.slug);
    // The Host the service sees through the proxy.
    let host = if record.proxy_host_header == "upstream" {
        format!("127.0.0.1:{}", record.port)
    } else {
        proxy_host.clone()
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: {accept}\r\nUser-Agent: cutex-tile/1\r\nX-Forwarded-Host: {proxy_host}\r\nX-Forwarded-Proto: http\r\nConnection: close\r\n\r\n"
    );
    stream
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(io_error)?;
    stream.write_all(request.as_bytes()).map_err(io_error)?;
    let cap = max_bytes * 2 + MAX_RESPONSE_HEAD + 4096;
    let mut buffer = Vec::new();
    let mut head: Option<(Body, usize)> = None;
    let mut part = [0u8; 8192];
    loop {
        if head.is_none() {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let parsed = Head::parse(&buffer[..end + 4])
                    .map_err(|_| invalid_response("the service sent an invalid HTTP head"))?;
                let status = parsed
                    .first
                    .strip_prefix("HTTP/1.")
                    .and_then(|rest| rest.split(' ').nth(1))
                    .and_then(|code| code.parse::<u16>().ok())
                    .ok_or_else(|| invalid_response("the service sent an invalid status line"))?;
                match status {
                    200..=299 => {}
                    300..=399 if parsed.get("location").is_some() => {
                        return Ok(Answer::Redirect(parsed.get("location").unwrap().to_string()))
                    }
                    404 | 410 => return Err(not_found(path)),
                    _ => {
                        return Err(FetchError::new(
                            "http_status",
                            format!("the service answered HTTP {status}"),
                        ))
                    }
                }
                let body = if status == 204 {
                    Body::Empty
                } else {
                    parsed
                        .body(true)
                        .map_err(|_| invalid_response("the service sent invalid body framing"))?
                };
                head = Some((body, end + 4));
            } else if buffer.len() > MAX_RESPONSE_HEAD {
                return Err(invalid_response("the HTTP head is too large"));
            }
        }
        if let Some((body, start)) = &head {
            let received = &buffer[*start..];
            match body {
                Body::Empty => return Ok(Answer::Body(Vec::new())),
                Body::Length(length) => {
                    if *length > max_bytes as u64 {
                        return Err(too_large(max_bytes));
                    }
                    if received.len() as u64 >= *length {
                        return Ok(Answer::Body(received[..*length as usize].to_vec()));
                    }
                }
                Body::Chunked => {
                    if let Some(body) = decode_chunked(received, max_bytes)? {
                        return Ok(Answer::Body(body));
                    }
                }
                Body::UntilClose => {
                    if received.len() > max_bytes {
                        return Err(too_large(max_bytes));
                    }
                }
            }
        }
        if buffer.len() > cap {
            return Err(too_large(max_bytes));
        }
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(io_error)?;
        let read = stream.read(&mut part).map_err(io_error)?;
        if read == 0 {
            return match head {
                Some((Body::UntilClose, start)) => Ok(Answer::Body(buffer[start..].to_vec())),
                _ => Err(invalid_response("the service closed the connection early")),
            };
        }
        buffer.extend_from_slice(&part[..read]);
    }
}

/// The decoded body once the last chunk arrived (`None`: more to read).
fn decode_chunked(data: &[u8], max_bytes: usize) -> Result<Option<Vec<u8>>, FetchError> {
    let invalid = || invalid_response("the service sent invalid chunked framing");
    let mut position = 0;
    let mut out = Vec::new();
    loop {
        let Some(line_end) = data[position..].windows(2).position(|pair| pair == b"\r\n") else {
            return if data.len() - position > 1024 { Err(invalid()) } else { Ok(None) };
        };
        let line = std::str::from_utf8(&data[position..position + line_end]).map_err(|_| invalid())?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        if size_text.is_empty() || size_text.len() > 8 {
            return Err(invalid());
        }
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid())?;
        position += line_end + 2;
        if size == 0 {
            return Ok(Some(out));
        }
        if out.len() + size > max_bytes {
            return Err(too_large(max_bytes));
        }
        if data.len() < position + size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&data[position..position + size]);
        if &data[position + size..position + size + 2] != b"\r\n" {
            return Err(invalid());
        }
        position += size + 2;
    }
}

/// A redirect is followed only when it stays inside the service (its proxy
/// host or its own loopback port).
fn redirect_target(record: &ServiceRecord, current: &str, location: &str) -> Result<String, FetchError> {
    let outside = || {
        FetchError::new(
            "redirect_outside_service",
            "the service redirected outside itself",
        )
    };
    let own = format!("{}.svc.localhost", record.slug);
    let base = url::Url::parse(&format!("http://{own}{current}")).map_err(|_| outside())?;
    let next = base.join(location.trim()).map_err(|_| outside())?;
    let host = next.host_str().unwrap_or("");
    let same = next.scheme() == "http"
        && next.username().is_empty()
        && next.password().is_none()
        && (host == own
            || (matches!(host, "127.0.0.1" | "localhost") && next.port() == Some(record.port)));
    if !same {
        return Err(outside());
    }
    let mut path = next.path().to_string();
    if let Some(query) = next.query() {
        path.push('?');
        path.push_str(query);
    }
    validate_tile_path("redirect", &path).map_err(|_| outside())
}

/// Parse and validate a fetched template.
pub fn parse_tile(body: &[u8]) -> Result<Value, FetchError> {
    if body.len() > MAX_TILE_BYTES {
        return Err(too_large(MAX_TILE_BYTES));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| FetchError::new("invalid_json", format!("the tile is not JSON: {error}")))?;
    validate_tile(&value).map_err(|message| FetchError::new("invalid_tile", message))
}

/// The image type by its bytes (PNG, JPEG, WebP; never SVG).
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

// ----------------------------------------------------------------- cache

struct Entry {
    key: String,
    expires: Instant,
    answer: Value,
}

/// Tile answers per service, valid for the template's `refreshSeconds`
/// (errors for [`ERROR_RETRY_SECONDS`]) and only while the record's tile
/// settings, port and revision stay the same.
#[derive(Default)]
pub struct TileCache {
    entries: Mutex<HashMap<String, Entry>>,
}

pub fn global() -> &'static TileCache {
    static CACHE: OnceLock<TileCache> = OnceLock::new();
    CACHE.get_or_init(TileCache::default)
}

fn cache_key(record: &ServiceRecord) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        record.updated_at,
        record.port,
        record.runtime.label(),
        record.root.as_deref().unwrap_or(""),
        record.tile_path.as_deref().unwrap_or(""),
    )
}

fn answer(tile: Option<&Value>, fetched_at: &str, error: Option<&FetchError>, source: &str) -> Value {
    json!({
        "tile": tile,
        "fetchedAt": fetched_at,
        "error": error.map(FetchError::to_json),
        "source": source,
    })
}

/// A failed fetch: the static template (if any) with the error.
fn fallback(record: &ServiceRecord, fetched_at: &str, error: &FetchError) -> Value {
    match &record.tile {
        Some(tile) => answer(Some(tile), fetched_at, Some(error), "static"),
        None => answer(None, fetched_at, Some(error), "service"),
    }
}

impl TileCache {
    /// `GET /v2/services/{slug}/tile`: `{tile, fetchedAt, error, source}`.
    /// `scope` separates registries (the store directory); `fetchable` is
    /// [`fetchable`]; `fetch` reads a path of the service.
    pub fn answer(
        &self,
        scope: &str,
        record: &ServiceRecord,
        fetchable: bool,
        now: Instant,
        fetch: impl FnOnce(&str) -> Result<Vec<u8>, FetchError>,
    ) -> Value {
        if let (Some(tile), None) = (&record.tile, &record.tile_path) {
            return answer(Some(tile), &record.updated_at, None, "static");
        }
        let slot = format!("{scope}\u{0}{}", record.slug);
        let key = cache_key(record);
        if let Some(entry) = self.entries.lock().unwrap().get(&slot) {
            if entry.key == key && entry.expires > now {
                return entry.answer.clone();
            }
        }
        let fetched_at = super::store::now_rfc3339();
        if !fetchable {
            return fallback(
                record,
                &fetched_at,
                &FetchError::new("service_stopped", "the service is not running"),
            );
        }
        let path = record.tile_path.as_deref().unwrap_or(DEFAULT_TILE_PATH);
        let (value, ttl) = match fetch(path).and_then(|body| parse_tile(&body)) {
            Ok(tile) => {
                let ttl = tile["refreshSeconds"].as_u64().unwrap_or(DEFAULT_REFRESH_SECONDS);
                (answer(Some(&tile), &fetched_at, None, "service"), ttl)
            }
            // Most services have no tile at the default path: no error.
            Err(error) if error.code == "not_found" && record.tile_path.is_none() => {
                (answer(None, &fetched_at, None, "service"), DEFAULT_REFRESH_SECONDS)
            }
            Err(error) => (fallback(record, &fetched_at, &error), ERROR_RETRY_SECONDS),
        };
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= MAX_CACHE_ENTRIES {
            entries.retain(|_, entry| entry.expires > now);
            if entries.len() >= MAX_CACHE_ENTRIES {
                entries.clear();
            }
        }
        entries.insert(
            slot,
            Entry {
                key,
                expires: now + Duration::from_secs(ttl),
                answer: value.clone(),
            },
        );
        value
    }
}

#[cfg(test)]
#[path = "tile_tests.rs"]
mod tests;
