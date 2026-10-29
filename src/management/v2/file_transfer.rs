//! Bounded, byte-exact file downloads. No session, ticket, file hash or runtime
//! is created. Every chunk repeats the same authorization/path checks as metadata.
use super::workspace::{ResolvedRoot, WorkspaceError, WorkspaceResult};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{File, Metadata},
    io::{Read, Seek, SeekFrom, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

pub const META_PATH: &str = "/v2/fs/transfer";
pub const CHUNK_PATH: &str = "/v2/fs/transfer/chunk";
pub const MAX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_CHUNK_BYTES: u64 = 1024 * 1024;
const BLOCK: usize = 64 * 1024;
const REQUEST_BUDGET: Duration = Duration::from_secs(15);

pub fn is_host_path(path: &str) -> bool {
    matches!(path, META_PATH | CHUNK_PATH)
}
pub fn is_path(path: &str) -> bool {
    is_host_path(path)
        || matches!(
            super::workspace::parse_route(path),
            Some(super::workspace::WorkspaceRoute::Scoped(
                _,
                super::workspace::ScopedRoute::FsTransferMeta
                    | super::workspace::ScopedRoute::FsTransferChunk
            ))
        )
}

fn io_error(error: std::io::Error) -> WorkspaceError {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            WorkspaceError::new(404, "path_not_found", "the file does not exist")
        }
        std::io::ErrorKind::PermissionDenied => {
            WorkspaceError::new(403, "permission_denied", "the file cannot be read")
        }
        _ => WorkspaceError::internal(error),
    }
}
fn changed() -> WorkspaceError {
    WorkspaceError::new(
        409,
        "file_changed",
        "the source changed; discard partial bytes and request fresh metadata",
    )
}

/// Resolve the actual open object, not only its pre-open pathname. In particular
/// an ancestor junction/symlink swap cannot turn a scoped read into a host read.
#[cfg(target_os = "linux")]
fn opened_path(file: &File) -> WorkspaceResult<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(io_error)
}
#[cfg(windows)]
fn opened_path(file: &File) -> WorkspaceResult<PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buffer = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    };
    if length == 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    if length as usize >= buffer.len() {
        return Err(WorkspaceError::bad_request("resolved path is too long"));
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}
#[cfg(not(any(windows, target_os = "linux")))]
fn opened_path(_: &File) -> WorkspaceResult<PathBuf> {
    Err(WorkspaceError::new(
        501,
        "file_transfer_unsupported",
        "opened-file identity validation is unsupported on this platform",
    ))
}

struct Source {
    file: File,
    path: PathBuf,
}
fn open_source(
    root: Option<&ResolvedRoot>,
    params: &HashMap<String, String>,
) -> WorkspaceResult<Source> {
    let raw = params
        .get("path")
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| WorkspaceError::bad_request("path is required"))?;
    if raw.chars().any(char::is_control) {
        return Err(WorkspaceError::new(
            400,
            "invalid_path",
            "path contains control characters",
        ));
    }
    let path = match root {
        Some(root) => super::workspace_fs::resolve_rel(root, raw)?.0,
        None => {
            let path = Path::new(raw);
            if !path.is_absolute() {
                return Err(WorkspaceError::new(
                    400,
                    "invalid_path",
                    "path must be absolute",
                ));
            }
            std::fs::canonicalize(path).map_err(io_error)?
        }
    };
    super::host_files::refuse_root_credential_file(&path)?;
    if !std::fs::metadata(&path).map_err(io_error)?.is_file() {
        return Err(WorkspaceError::new(
            400,
            "not_a_file",
            "only regular files can be downloaded",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(&path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(WorkspaceError::new(
            400,
            "not_a_file",
            "only regular files can be downloaded",
        ));
    }
    let actual = opened_path(&file)?;
    if !actual.exists() {
        return Err(changed());
    }
    if let Some(root) = root {
        let relative = actual.strip_prefix(&root.canonical).map_err(|_| {
            WorkspaceError::new(
                403,
                "path_outside_root",
                "the open file is outside the workspace",
            )
        })?;
        if relative.components().any(|part| {
            let part = part.as_os_str().to_string_lossy();
            if cfg!(windows) {
                part.eq_ignore_ascii_case(".git")
            } else {
                part == ".git"
            }
        }) {
            return Err(WorkspaceError::new(
                403,
                "path_forbidden",
                ".git internals are not exposed",
            ));
        }
    }
    super::host_files::refuse_root_credential_file(&actual)?;
    Ok(Source { file, path: actual })
}

fn nanos(time: SystemTime) -> String {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos().to_string(),
        Err(error) => format!("-{}", error.duration().as_nanos()),
    }
}
fn revision(source: &Source, meta: &Metadata) -> WorkspaceResult<String> {
    #[cfg(windows)]
    let identity =
        crate::platform::private_fs::identity(&source.file).map_err(WorkspaceError::internal)?;
    #[cfg(windows)]
    let identity = json!([identity.volume, identity.file_id]);
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        json!([meta.dev(), meta.ino()])
    };
    #[cfg(not(any(unix, windows)))]
    let identity = json!(null);
    #[cfg(unix)]
    let platform = {
        use std::os::unix::fs::MetadataExt;
        json!([
            meta.ctime(),
            meta.ctime_nsec(),
            meta.mtime(),
            meta.mtime_nsec()
        ])
    };
    #[cfg(windows)]
    let platform = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
        };
        let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                source.file.as_raw_handle(),
                FileBasicInfo,
                (&mut info as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        json!([
            info.CreationTime,
            info.LastWriteTime,
            info.ChangeTime,
            info.FileAttributes
        ])
    };
    #[cfg(not(any(unix, windows)))]
    let platform = json!(null);
    let stamp = json!([
        "fs-file-transfer-v1",
        super::workspace::display_path(&source.path),
        identity,
        meta.len(),
        nanos(meta.modified().map_err(io_error)?),
        platform
    ]);
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&stamp).map_err(WorkspaceError::internal)?)
    ))
}

pub fn metadata(
    root: Option<&ResolvedRoot>,
    params: &HashMap<String, String>,
) -> WorkspaceResult<Value> {
    let source = open_source(root, params)?;
    let meta = source.file.metadata().map_err(io_error)?;
    if meta.len() > 9_007_199_254_740_991 {
        return Err(WorkspaceError::new(
            400,
            "file_too_large",
            "file size exceeds the supported exact byte-count range",
        ));
    }
    Ok(
        json!({"path":super::workspace::display_path(&source.path),"name":source.path.file_name().unwrap_or_default().to_string_lossy(),
        "size":meta.len(),"modifiedAt":super::workspace::rfc3339(meta.modified().map_err(io_error)?),"revision":revision(&source,&meta)?,"maxChunkBytes":MAX_CHUNK_BYTES}),
    )
}

struct Chunk {
    bytes: Vec<u8>,
    offset: u64,
    total: u64,
    revision: String,
}
impl Chunk {
    fn next(&self) -> u64 {
        self.offset + self.bytes.len() as u64
    }
    fn eof(&self) -> bool {
        self.next() == self.total
    }
}
fn number(params: &HashMap<String, String>, name: &str, default: u64) -> WorkspaceResult<u64> {
    match params.get(name) {
        None => Ok(default),
        Some(value) => value.parse().map_err(|_| {
            WorkspaceError::bad_request(format!("{name} must be an unsigned byte count"))
        }),
    }
}
fn read_chunk(
    root: Option<&ResolvedRoot>,
    params: &HashMap<String, String>,
    mut canceled: impl FnMut() -> bool,
) -> WorkspaceResult<Chunk> {
    let expected = params
        .get("revision")
        .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| WorkspaceError::bad_request("revision from metadata is required"))?;
    let offset = number(params, "offset", 0)?;
    let maximum = number(params, "maxBytes", DEFAULT_CHUNK_BYTES)?;
    if !(1..=MAX_CHUNK_BYTES).contains(&maximum) {
        return Err(WorkspaceError::bad_request("maxBytes must be 1..4194304"));
    }
    if canceled() {
        return Err(WorkspaceError::new(
            499,
            "transfer_canceled",
            "transfer canceled",
        ));
    }
    let mut source = open_source(root, params)?;
    let before = source.file.metadata().map_err(io_error)?;
    if revision(&source, &before)? != *expected {
        return Err(changed());
    }
    let total = before.len();
    if offset > total {
        return Err(WorkspaceError::new(
            416,
            "invalid_offset",
            "offset exceeds the source size",
        ));
    }
    let count = maximum.min(total - offset) as usize;
    source
        .file
        .seek(SeekFrom::Start(offset))
        .map_err(io_error)?;
    let mut bytes = vec![0u8; count];
    let mut received = 0;
    while received < count {
        if canceled() {
            return Err(WorkspaceError::new(
                499,
                "transfer_canceled",
                "transfer canceled",
            ));
        }
        let end = (received + BLOCK).min(count);
        let read = source
            .file
            .read(&mut bytes[received..end])
            .map_err(io_error)?;
        if read == 0 {
            return Err(changed());
        }
        received += read;
    }
    if offset + count as u64 == total {
        // Some virtual "regular" files report size zero while yielding bytes.
        // They cannot be advertised as a complete empty/short ordinary file.
        let mut extra = [0u8; 1];
        if source.file.read(&mut extra).map_err(io_error)? != 0 {
            return Err(changed());
        }
    }
    if revision(&source, &source.file.metadata().map_err(io_error)?)? != *expected {
        return Err(changed());
    }
    // Detect replacement of the pathname too, even when the open handle still
    // refers to the unchanged old object. No source bytes are read here.
    let current = open_source(root, params)?;
    if revision(&current, &current.file.metadata().map_err(io_error)?)? != *expected {
        return Err(changed());
    }
    Ok(Chunk {
        bytes,
        offset,
        total,
        revision: expected.clone(),
    })
}

fn disconnected(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let result = stream.peek(&mut [0u8; 1]);
    let restored = stream.set_nonblocking(false).is_ok();
    !restored
        || match result {
            // FIN may mean a valid HTTP client only shut down its request
            // direction. Only socket errors prove cancellation here; a full
            // graceful close is detected by writes after this bounded read.
            Ok(0) => false,
            Ok(_) => false,
            Err(error) => !matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ),
        }
}

pub fn handle(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
    root: Option<&ResolvedRoot>,
) -> anyhow::Result<()> {
    let timeout = stream.write_timeout()?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let result = handle_inner(stream, request, root);
    let restored = stream.set_write_timeout(timeout);
    if result.is_err() || restored.is_err() {
        crate::http::server::close_after_response(stream);
    }
    result?;
    restored?;
    Ok(())
}

fn handle_inner(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
    root: Option<&ResolvedRoot>,
) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::workspace::write_error(
            stream,
            &WorkspaceError::new(405, "method_not_allowed", "use GET"),
        );
    }
    let params = super::workspace::query_params(request);
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if !path.ends_with("/chunk") {
        return match metadata(root, &params) {
            Ok(value) => crate::http::server::write_json_response(stream, 200, "OK", &value),
            Err(error) => super::workspace::write_error(stream, &error),
        };
    }
    let started = Instant::now();
    let chunk = match read_chunk(root, &params, || {
        started.elapsed() > REQUEST_BUDGET || disconnected(stream)
    }) {
        Ok(chunk) => chunk,
        Err(error) if error.status == 499 => return Ok(()),
        Err(error) => return super::workspace::write_error(stream, &error),
    };
    let timeout = stream.write_timeout()?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut extra = crate::http::server::extra_response_headers();
    if extra
        .to_ascii_lowercase()
        .contains("access-control-allow-origin:")
    {
        extra.push_str("Access-Control-Expose-Headers: X-Cutex-File-Offset, X-Cutex-File-Bytes, X-Cutex-File-Total-Bytes, X-Cutex-File-Next-Offset, X-Cutex-File-Eof\r\n");
    }
    let result = (|| -> std::io::Result<()> {
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\nContent-Length: {}\r\nETag: \"{}\"\r\nX-Cutex-File-Offset: {}\r\nX-Cutex-File-Bytes: {}\r\nX-Cutex-File-Total-Bytes: {}\r\nX-Cutex-File-Next-Offset: {}\r\nX-Cutex-File-Eof: {}\r\n{}Connection: close\r\n\r\n",chunk.bytes.len(),chunk.revision,chunk.offset,chunk.bytes.len(),chunk.total,chunk.next(),chunk.eof(),extra)?;
        for block in chunk.bytes.chunks(BLOCK) {
            if started.elapsed() > REQUEST_BUDGET || disconnected(stream) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "transfer canceled",
                ));
            }
            stream.write_all(block)?;
        }
        stream.flush()
    })();
    let restored = stream.set_write_timeout(timeout);
    // Never let the generic Management error wrapper append a second HTTP
    // response to an advertised binary body, even after partial headers/data.
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    crate::http::server::close_after_response(stream);
    if let Err(error) = result {
        eprintln!("file transfer response interrupted: {}", error.kind());
    }
    restored?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let dir = std::env::temp_dir().join(format!("file-transfer-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("中文 file's.bin");
            std::fs::write(&path, bytes).unwrap();
            Self { dir, path }
        }
        fn params(&self) -> HashMap<String, String> {
            HashMap::from([("path".into(), self.path.to_string_lossy().into_owned())])
        }
        fn chunk_params(&self) -> HashMap<String, String> {
            let mut params = self.params();
            params.insert(
                "revision".into(),
                metadata(None, &params).unwrap()["revision"]
                    .as_str()
                    .unwrap()
                    .into(),
            );
            params
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn large_binary_over_preview_limit_is_exact_in_bounded_chunks() {
        let original = (0..(11 * 1024 * 1024 + 37))
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        let f = Fixture::new(&original);
        let meta = metadata(None, &f.params()).unwrap();
        assert_eq!(meta["size"], original.len());
        assert_eq!(meta["name"], "中文 file's.bin");
        let mut params = f.chunk_params();
        params.insert("maxBytes".into(), MAX_CHUNK_BYTES.to_string());
        let mut output = Vec::new();
        loop {
            params.insert("offset".into(), output.len().to_string());
            let chunk = read_chunk(None, &params, || false).unwrap();
            assert!(chunk.bytes.len() <= MAX_CHUNK_BYTES as usize);
            assert_eq!(chunk.offset, output.len() as u64);
            let eof = chunk.eof();
            output.extend(chunk.bytes);
            if eof {
                break;
            }
        }
        assert_eq!(output, original);
        assert_eq!(
            metadata(None, &f.params()).unwrap()["revision"],
            meta["revision"]
        );
    }
    #[test]
    fn empty_files_and_end_offset_have_explicit_zero_byte_eof() {
        let f = Fixture::new(b"");
        let chunk = read_chunk(None, &f.chunk_params(), || false).unwrap();
        assert!(chunk.eof());
        assert!(chunk.bytes.is_empty());
        assert_eq!(chunk.next(), 0);
        let f = Fixture::new(b"\xef\xbb\xbfline\r\n\0\xff");
        let mut params = f.chunk_params();
        let chunk = read_chunk(None, &params, || false).unwrap();
        assert_eq!(chunk.bytes, b"\xef\xbb\xbfline\r\n\0\xff");
        params.insert("offset".into(), chunk.total.to_string());
        assert!(read_chunk(None, &params, || false).unwrap().eof());
        params.insert("offset".into(), (chunk.total + 1).to_string());
        assert_eq!(
            read_chunk(None, &params, || false).err().unwrap().status,
            416
        );
    }
    #[test]
    fn changed_source_before_or_during_read_is_never_a_successful_chunk() {
        let f = Fixture::new(&vec![7; 3 * BLOCK]);
        let params = f.chunk_params();
        std::fs::write(&f.path, b"different").unwrap();
        assert_eq!(
            read_chunk(None, &params, || false).err().unwrap().code,
            "file_changed"
        );
        let f = Fixture::new(&vec![7; 3 * BLOCK]);
        let params = f.chunk_params();
        let mut calls = 0;
        let error = read_chunk(None, &params, || {
            calls += 1;
            if calls == 3 {
                std::fs::write(&f.path, b"changed while reading").unwrap();
            }
            false
        })
        .err()
        .unwrap();
        assert_eq!(error.code, "file_changed");
    }
    #[test]
    fn replacement_with_same_length_and_content_changes_file_identity() {
        let f = Fixture::new(b"same bytes");
        let params = f.chunk_params();
        std::fs::rename(&f.path, f.dir.join("old.bin")).unwrap();
        std::fs::write(&f.path, b"same bytes").unwrap();
        assert_eq!(
            read_chunk(None, &params, || false).err().unwrap().status,
            409
        );
    }
    #[test]
    fn cancellation_is_checked_before_open_and_between_read_blocks() {
        let f = Fixture::new(&vec![9; 3 * BLOCK]);
        let params = f.chunk_params();
        assert_eq!(
            read_chunk(None, &params, || true).err().unwrap().status,
            499
        );
        let mut calls = 0;
        assert_eq!(
            read_chunk(None, &params, || {
                calls += 1;
                calls == 3
            })
            .err()
            .unwrap()
            .status,
            499
        );
        assert_eq!(calls, 3);
    }
    #[test]
    fn invalid_sizes_revision_offsets_and_non_files_are_refused() {
        let f = Fixture::new(b"small");
        let params = f.chunk_params();
        for (key, value) in [
            ("revision", ""),
            ("revision", "not-a-revision"),
            ("maxBytes", "0"),
            ("maxBytes", "4194305"),
            ("maxBytes", "-1"),
            ("offset", "-1"),
            ("offset", "18446744073709551616"),
        ] {
            let mut changed = params.clone();
            changed.insert(key.into(), value.into());
            assert_eq!(
                read_chunk(None, &changed, || false).err().unwrap().status,
                400,
                "{key}={value}"
            );
        }
        let mut directory = f.params();
        directory.insert("path".into(), f.dir.to_string_lossy().into_owned());
        assert_eq!(metadata(None, &directory).unwrap_err().code, "not_a_file");
        assert_eq!(
            metadata(None, &HashMap::from([("path".into(), "relative".into())]))
                .unwrap_err()
                .status,
            400
        );
    }
    #[test]
    fn scoped_paths_stay_inside_root_and_git_is_denied() {
        let f = Fixture::new(b"original");
        let root = ResolvedRoot::from_dir(
            super::super::workspace::Scope {
                kind: super::super::workspace::ScopeKind::Project,
                id: "p".into(),
            },
            &f.dir,
        )
        .unwrap();
        let params = HashMap::from([("path".into(), "中文 file's.bin".into())]);
        assert_eq!(metadata(Some(&root), &params).unwrap()["size"], 8);
        for path in ["../secret", "/etc/passwd", ".git/config"] {
            assert!(metadata(Some(&root), &HashMap::from([("path".into(), path.into())])).is_err());
        }
    }
    #[test]
    fn raw_http_body_headers_cors_and_half_closed_requests_are_byte_exact() {
        use std::net::{TcpListener, TcpStream};
        let f = Fixture::new(&[0, 255, 13, 10, 33]);
        let params = f.chunk_params();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter())
            .finish();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let request = crate::http::server::SimpleHttpRequest {
            method: "GET".into(),
            path: format!("{CHUNK_PATH}?{query}"),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let thread = std::thread::spawn(move || {
            crate::http::server::set_extra_response_headers("Access-Control-Allow-Origin: http://localhost\r\nAccess-Control-Expose-Headers: ETag, Retry-After\r\n".into());
            handle(&mut server, &request, None).unwrap()
        });
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        thread.join().unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&response[..split]);
        assert!(headers.contains("Content-Length: 5\r\n"));
        assert!(headers.contains("X-Cutex-File-Eof: true\r\n"));
        assert!(headers.contains("X-Cutex-File-Next-Offset: 5\r\n"));
        assert!(headers.contains("Access-Control-Allow-Origin: http://localhost\r\n"));
        assert!(headers.contains("Access-Control-Expose-Headers: X-Cutex-File-Offset"));
        assert_eq!(&response[split + 4..], &[0, 255, 13, 10, 33]);
    }

    #[test]
    fn interrupted_socket_write_never_requests_an_appended_http_error_response() {
        use std::net::{Shutdown, TcpListener, TcpStream};
        let f = Fixture::new(&vec![7; MAX_CHUNK_BYTES as usize]);
        let params = f.chunk_params();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter())
            .finish();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.shutdown(Shutdown::Write).unwrap();
        let request = crate::http::server::SimpleHttpRequest {
            method: "GET".into(),
            path: format!("{CHUNK_PATH}?{query}"),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        // Management's outer error wrapper writes a 500 for Err. A binary
        // response failure is terminal and must never ask it to do that.
        assert!(handle(&mut server, &request, None).is_ok());
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
    }
    #[cfg(windows)]
    #[test]
    fn windows_junction_cannot_escape_the_workspace_and_original_is_untouched() {
        let f = Fixture::new(b"inside");
        let outside = Fixture::new(b"outside unchanged");
        let link = f.dir.join("junction");
        let command = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside.dir)
            .output()
            .unwrap();
        assert!(
            command.status.success(),
            "create owned fixture junction: {}",
            String::from_utf8_lossy(&command.stderr)
        );
        struct Junction(PathBuf);
        impl Drop for Junction {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let _junction = Junction(link);
        let root = ResolvedRoot::from_dir(
            super::super::workspace::Scope {
                kind: super::super::workspace::ScopeKind::Project,
                id: "p".into(),
            },
            &f.dir,
        )
        .unwrap();
        let params = HashMap::from([("path".into(), "junction/中文 file's.bin".into())]);
        assert_eq!(metadata(Some(&root), &params).unwrap_err().status, 403);
        let mut chunk = params;
        chunk.insert(
            "revision".into(),
            metadata(None, &outside.params()).unwrap()["revision"]
                .as_str()
                .unwrap()
                .into(),
        );
        assert_eq!(
            read_chunk(Some(&root), &chunk, || false)
                .err()
                .unwrap()
                .status,
            403
        );
        assert_eq!(std::fs::read(&outside.path).unwrap(), b"outside unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn unix_symlink_escape_and_fifo_are_not_read() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new(b"inside");
        let outside = Fixture::new(b"outside");
        symlink(&outside.path, f.dir.join("escape")).unwrap();
        let root = ResolvedRoot::from_dir(
            super::super::workspace::Scope {
                kind: super::super::workspace::ScopeKind::Session,
                id: "s".into(),
            },
            &f.dir,
        )
        .unwrap();
        assert_eq!(
            metadata(
                Some(&root),
                &HashMap::from([("path".into(), "escape".into())])
            )
            .unwrap_err()
            .status,
            403
        );
        let fifo = f.dir.join("pipe");
        let name = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(
            metadata(
                None,
                &HashMap::from([("path".into(), fifo.to_string_lossy().into_owned())])
            )
            .unwrap_err()
            .code,
            "not_a_file"
        );
    }
}
