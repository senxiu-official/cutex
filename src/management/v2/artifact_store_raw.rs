//! Artifact byte downloads. Admission uses the bounded file-transfer lane;
//! metadata connections are dropped before the first socket write.
use super::*;
use crate::artifacts::raw::SourceStamp;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    time::{Duration, Instant},
};

const BLOCK: usize = 64 * 1024;
const STALL: Duration = Duration::from_secs(30);

// A very weak link can resume with Range; a byte-at-a-time peer cannot retain
// a transfer slot indefinitely. Check each window separately so a fast initial
// socket-buffer fill cannot subsidize a later trickle forever.
#[derive(Clone, Copy)]
struct RatePolicy {
    grace: Duration,
    window: Duration,
    min_bytes_per_sec: u64,
}
const RATE: RatePolicy = RatePolicy {
    grace: Duration::from_secs(30),
    window: Duration::from_secs(30),
    min_bytes_per_sec: 1024,
};
struct RateGuard {
    policy: RatePolicy,
    started: Instant,
    window_started: Option<Instant>,
    bytes: u64,
}
impl RateGuard {
    fn new(policy: RatePolicy, now: Instant) -> Self {
        Self {
            policy,
            started: now,
            window_started: None,
            bytes: 0,
        }
    }
    fn observe(&mut self, written: usize, now: Instant) -> std::io::Result<()> {
        if now.duration_since(self.started) < self.policy.grace {
            return Ok(());
        }
        let began = *self.window_started.get_or_insert(now);
        self.bytes = self.bytes.saturating_add(written as u64);
        let elapsed = now.duration_since(began);
        if elapsed >= self.policy.window {
            if (self.bytes as u128) * 1_000_000_000
                < (self.policy.min_bytes_per_sec as u128) * elapsed.as_nanos()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "artifact download below minimum sustained rate",
                ));
            }
            self.window_started = Some(now);
            self.bytes = 0;
        }
        Ok(())
    }
}

fn open_file(path: &std::path::Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Builds and owner deletions win over downloads. Changed bytes are
        // detected on this same handle before/after every bounded read.
        options.share_mode(
            windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
        );
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "artifact source is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(file)
}

fn parts(path: &str) -> Option<(&str, &str)> {
    let rest = path
        .split('?')
        .next()?
        .strip_prefix(PATH)?
        .strip_prefix('/')?;
    let parts: Vec<_> = rest.split('/').collect();
    match parts.as_slice() {
        [id, "versions", version, "raw"] => Some((*id, *version)),
        _ => None,
    }
}
pub(crate) fn is_path(path: &str) -> bool {
    parts(path).is_some()
}

struct Download {
    file: File,
    stamp: SourceStamp,
    length: u64,
    version: u64,
    sha: Option<String>,
    media: String,
    filename: String,
}
fn changed() -> RouteError {
    error(
        409,
        "artifact_source_changed",
        "the external source changed; publish it again before downloading",
    )
}
fn io_source(e: std::io::Error, external: bool) -> RouteError {
    if e.kind() == std::io::ErrorKind::NotFound {
        error(
            410,
            "artifact_content_gone",
            "the recorded content is no longer available",
        )
    } else if external {
        changed()
    } else {
        unavailable(e)
    }
}

fn prepare(host: &dyn ArtifactHost, path: &str) -> Result<Download, RouteError> {
    let (id, which) = parts(path).ok_or_else(not_found)?;
    if !crate::artifacts::is_artifact_id(id) {
        return Err(not_found());
    }
    let which = Which::parse(which).ok_or_else(|| invalid("version: a number from 1 or latest"))?;
    let store = host.store().map_err(unavailable_any)?;
    let (_, version) = store
        .version(id, which)
        .map_err(unavailable_any)?
        .ok_or_else(not_found)?;
    let sha = version
        .sha256
        .clone()
        .filter(|sha| sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
    let external = version.storage == "external";
    let path = if external {
        PathBuf::from(&version.source_path)
    } else if version.storage == "blob" {
        store.blob_path(
            sha.as_deref()
                .ok_or_else(|| unavailable("blob version has no valid SHA-256"))?,
        )
    } else {
        return Err(unavailable("unknown artifact storage"));
    };
    if external {
        crate::artifacts::paths::validate_local_path(&path).map_err(|_| changed())?;
        let canonical = std::fs::canonicalize(&path)
            .map(crate::artifacts::paths::strip_verbatim)
            .map_err(|e| io_source(e, true))?;
        if crate::artifacts::paths::key(&canonical) != crate::artifacts::paths::key(&path)
            || crate::artifacts::paths::forbidden_reason(&canonical).is_some()
        {
            return Err(changed());
        }
    }
    if super::super::host_files::narrowed_credential_read() {
        super::super::host_files::refuse_root_credential_file(&path)
            .map_err(|denied| error(denied.status, denied.code, denied.message))?;
    }
    let file = open_file(&path).map_err(|e| io_source(e, external))?;
    let stamp = SourceStamp::of(&file).map_err(|e| io_source(e, external))?;
    if stamp.bytes != version.bytes {
        return Err(changed());
    }
    if external {
        let recorded = store
            .source_stamp(id, version.version)
            .map_err(unavailable_any)?
            .ok_or_else(|| {
                error(
                    409,
                    "artifact_source_identity_unavailable",
                    "this older external version has no recorded file identity; publish it again",
                )
            })?;
        if stamp != recorded
            || Some((stamp.modified_ns / 1_000_000) as i64) != version.source_mtime_ms
        {
            return Err(changed());
        }
    }
    let filename = std::path::Path::new(&version.source_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download")
        .to_string();
    // Header values never contain control bytes, even for hand-written legacy DB rows.
    let media = if version.media_type.bytes().all(|b| (32..127).contains(&b)) {
        version.media_type
    } else {
        "application/octet-stream".into()
    };
    Ok(Download {
        file,
        stamp,
        length: version.bytes,
        version: version.version,
        sha,
        media,
        filename,
    })
}

fn range(value: &str, length: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || length == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    let number = |s: &str| {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse::<u64>().ok()
        } else {
            None
        }
    };
    if start.is_empty() {
        let suffix = number(end)?;
        if suffix == 0 {
            return None;
        }
        return Some((length.saturating_sub(suffix), length - 1));
    }
    let start = number(start)?;
    let end = if end.is_empty() {
        length - 1
    } else {
        number(end)?.min(length - 1)
    };
    (start < length && start <= end).then_some((start, end))
}
fn disposition(filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in filename.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

pub(crate) fn handle(
    host: &dyn ArtifactHost,
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
) -> anyhow::Result<()> {
    let download = match prepare(host, &request.path) {
        Ok(download) => download,
        Err(e) => {
            let reason = if e.status == 410 {
                "Gone"
            } else {
                super::super::server::http_reason_for(e.status)
            };
            if request.method != "HEAD" {
                return super::super::server::write_v2_error(
                    stream,
                    e.status,
                    reason,
                    e.code,
                    &e.message,
                    e.status == 503,
                    e.details,
                );
            }
            let body = serde_json::to_vec(&json!({"error":{"code":e.code,"message":e.message}}))?;
            let extra = crate::http::server::extra_response_headers();
            let header = format!("HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Cutex-Error: {}\r\nAccess-Control-Expose-Headers: X-Cutex-Error\r\n{extra}Connection: close\r\n\r\n", e.status, body.len(), e.code);
            let result =
                crate::http::server::write_response_parts(stream, &[header.as_bytes()], STALL)
                    .map_err(Into::into);
            return finish(stream, result);
        }
    };
    // Once any raw headers were sent, a disconnect must never append JSON.
    let result = send(download, stream, request);
    finish(stream, result)
}

fn finish(stream: &mut TcpStream, result: anyhow::Result<()>) -> anyhow::Result<()> {
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    crate::http::server::close_after_response(stream);
    if let Err(error) = result {
        eprintln!("artifact raw transfer interrupted: {error}");
    }
    Ok(())
}

fn send(
    download: Download,
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
) -> anyhow::Result<()> {
    send_with_policy(download, stream, request, RATE)
}

fn send_with_policy(
    mut download: Download,
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    policy: RatePolicy,
) -> anyhow::Result<()> {
    let mut rate = RateGuard::new(policy, Instant::now());
    let etag = match &download.sha {
        Some(sha) => format!("\"sha256-{sha}\""),
        None => format!(
            "W/\"identity-{}\"",
            crate::asks::sha256_hex(&serde_json::to_vec(&download.stamp)?)
        ),
    };
    let integrity = match &download.sha {
        Some(sha) => format!("X-Cutex-Sha256: {sha}\r\nX-Cutex-Integrity: sha256\r\n"),
        None => "X-Cutex-Integrity: identity-only\r\n".to_string(),
    };
    let extra = crate::http::server::extra_response_headers();
    let exposed = "Access-Control-Expose-Headers: Content-Length, Content-Disposition, ETag, Accept-Ranges, Content-Range, X-Cutex-Sha256, X-Cutex-Integrity, X-Cutex-Artifact-Version, X-Cutex-Error\r\n";
    let requested = request
        .headers
        .get("range")
        .filter(|_| request.method == "GET")
        .filter(|_| {
            request
                .headers
                .get("if-range")
                .is_none_or(|value| !etag.starts_with("W/") && value == &etag)
        });
    let (status, start, count, content_range) = match requested {
        Some(value) => match range(value, download.length) {
            Some((start, end)) => (206, start, end - start + 1, format!("Content-Range: bytes {start}-{end}/{}\r\n", download.length)),
            None => (416, 0, 0, format!("Content-Range: bytes */{}\r\nX-Cutex-Error: artifact_range_not_satisfiable\r\n", download.length)),
        },
        None => (200, 0, download.length, String::new()),
    };
    let version = download.version;
    let reason = match status {
        206 => "Partial Content",
        416 => "Range Not Satisfiable",
        _ => "OK",
    };
    let header = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {count}\r\nContent-Type: {}\r\nContent-Disposition: {}\r\nETag: {etag}\r\nX-Cutex-Artifact-Version: {version}\r\n{integrity}Accept-Ranges: bytes\r\nCache-Control: private, no-transform\r\nX-Content-Type-Options: nosniff\r\n{content_range}{extra}{exposed}Connection: close\r\n\r\n", download.media, disposition(&download.filename));
    download.file.seek(SeekFrom::Start(start))?;
    crate::http::server::write_response_parts_checked(
        stream,
        &[header.as_bytes()],
        STALL,
        |written| rate.observe(written, Instant::now()),
    )?;
    if request.method == "HEAD" || status == 416 {
        return Ok(());
    }
    let mut remaining = count;
    let mut block = [0u8; BLOCK];
    while remaining > 0 {
        if SourceStamp::of(&download.file)? != download.stamp {
            anyhow::bail!("artifact changed during download");
        }
        let limit = remaining.min(BLOCK as u64) as usize;
        let read = download.file.read(&mut block[..limit])?;
        anyhow::ensure!(read > 0, "artifact truncated during download");
        if SourceStamp::of(&download.file)? != download.stamp {
            anyhow::bail!("artifact changed while reading download block");
        }
        crate::http::server::write_response_parts_checked(
            stream,
            &[&block[..read]],
            STALL,
            |written| rate.observe(written, Instant::now()),
        )?;
        remaining -= read as u64;
    }
    Ok(())
}

#[cfg(test)]
#[path = "artifact_store_raw_tests.rs"]
mod tests;
