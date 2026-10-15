//! Session attachment upload for remote Management clients (root credential
//! or a Management client token).
//!
//! `POST /v2/sessions/{cutexSessionId}/attachments` with the raw image bytes
//! as the body and `Content-Type: image/png|image/jpeg|image/gif|image/webp`
//! → `201 {cutexSessionId, attachmentId, path, mime, size}` (`200` when the
//! same bytes were already stored for the session). `path` is a host-local
//! absolute path the client passes as a `localImage` input on this host.
//!
//! - Files live under `<cutex home>/runtime/attachments/<session dir>/
//!   <sha256>.<ext>` (content-addressed, owner-private directory).
//! - Limits: `attachments.maxBytes` (settings, default 8 MiB; the HTTP layer
//!   refuses bodies above 16 MiB). The bytes must match the declared type.
//! - Cleanup: a session's directory is removed when the session is deleted
//!   (`cutex/session/delete`, delete-empty retire); files older than
//!   `attachments.retentionDays` (default 30, 0 = keep) are pruned at most
//!   once an hour, on upload.
//!
//! Non-owner users (P2 access, capability `users-p2-access`): a user with
//! `send` on a visible agent may upload too. Each such upload leaves an
//! empty marker `<sha256>.<ext>.by-<32 hex of sha256(tokenId)>` beside the
//! file (pruned and removed with it); that user's `localImage` inputs are
//! accepted only for the exact canonical `path` of a file the same token
//! uploaded to the same session ([`uploaded_by`]).
//!
//! Errors: 400 `invalid_attachment` (empty body, bytes do not match the
//! type), 404 `session_not_found`, 409 `session_not_local`, 413
//! `attachment_too_large` (`details.maxBytes`), 415
//! `unsupported_media_type` (`details.supported`).

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::attachments::AttachmentSettings;
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;

pub const SUPPORTED_TYPES: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

pub fn attachments_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("attachments"))
}

/// `Some(cutexSessionId)` for `/v2/sessions/{id}/attachments`.
pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/attachments")?;
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control))
        .then_some(decoded)
}

pub(crate) fn percent_decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = encoded.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn is_attachments_path(path: &str) -> bool {
    session_id_from_path(path).is_some()
}

/// Directory name for a session: the id itself when it is a plain file
/// name, otherwise a hash (never a path separator, `.` or `..`).
fn session_dir_name(cutex_session_id: &str) -> String {
    let plain = cutex_session_id.len() <= 128
        && cutex_session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !cutex_session_id.starts_with('.')
        && !cutex_session_id.ends_with('.');
    if plain {
        cutex_session_id.to_string()
    } else {
        format!("s-{}", &hex(&Sha256::digest(cutex_session_id.as_bytes()))[..32])
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn media_type(request: &SimpleHttpRequest) -> Option<String> {
    request.headers.get("content-type").map(|value| {
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    })
}

fn bytes_match(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

#[derive(Debug)]
pub struct UploadError {
    pub status: u16,
    pub reason: &'static str,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl UploadError {
    fn new(status: u16, reason: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            code,
            message: message.into(),
            details: json!({}),
        }
    }

    fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

/// Validate and store one upload under `root`. Returns (created, resource).
pub fn store_upload(
    root: &Path,
    cutex_session_id: &str,
    mime: Option<&str>,
    body: &[u8],
    settings: AttachmentSettings,
) -> Result<(bool, Value), UploadError> {
    let settings = settings.effective();
    let supported: Vec<&str> = SUPPORTED_TYPES.iter().map(|(mime, _)| *mime).collect();
    let Some((mime, ext)) = mime.and_then(|mime| {
        SUPPORTED_TYPES
            .iter()
            .find(|(candidate, _)| *candidate == mime)
            .copied()
    }) else {
        return Err(UploadError::new(
            415,
            "Unsupported Media Type",
            "unsupported_media_type",
            "Content-Type must be an image type cutex accepts as localImage",
        )
        .with_details(json!({ "supported": supported })));
    };
    if body.is_empty() {
        return Err(UploadError::new(400, "Bad Request", "invalid_attachment", "empty body"));
    }
    if body.len() as u64 > settings.max_bytes {
        return Err(UploadError::new(
            413,
            "Payload Too Large",
            "attachment_too_large",
            "the attachment exceeds attachments.maxBytes",
        )
        .with_details(json!({ "maxBytes": settings.max_bytes, "size": body.len() })));
    }
    if !bytes_match(mime, body) {
        return Err(UploadError::new(
            400,
            "Bad Request",
            "invalid_attachment",
            format!("the bytes are not a {mime} image"),
        ));
    }
    let digest = hex(&Sha256::digest(body));
    let dir = root.join(session_dir_name(cutex_session_id));
    let internal = |error: anyhow::Error| {
        UploadError::new(
            500,
            "Internal Server Error",
            "attachment_store_failed",
            format!("could not store the attachment: {error:#}"),
        )
    };
    crate::management::client_tokens::ensure_private_dir(root).map_err(internal)?;
    crate::management::client_tokens::ensure_private_dir(&dir).map_err(internal)?;
    let path = dir.join(format!("{digest}.{ext}"));
    let created = match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() && meta.len() == body.len() as u64 => {
            // Refresh the age so retention counts from the latest upload.
            let _ = std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(SystemTime::now()));
            false
        }
        _ => {
            crate::config::atomic::write_private_bytes_atomic(&path, body).map_err(internal)?;
            true
        }
    };
    let canonical = std::fs::canonicalize(&path).map_err(|error| internal(error.into()))?;
    Ok((
        created,
        json!({
            "cutexSessionId": cutex_session_id,
            "attachmentId": digest,
            "path": super::workspace::display_path(&canonical),
            "mime": mime,
            "size": body.len(),
        }),
    ))
}

fn uploader_marker_suffix(token_id: &str) -> String {
    format!(".by-{}", &hex(&Sha256::digest(token_id.as_bytes()))[..32])
}

fn is_attachment_file_name(name: &str) -> bool {
    let Some((digest, ext)) = name.split_once('.') else {
        return false;
    };
    digest.len() == 64
        && digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        && SUPPORTED_TYPES.iter().any(|(_, known)| *known == ext)
}

/// Record that `token_id` uploaded the attachment at `path` (inside `root`).
fn record_uploader(path: &Path, token_id: &str) -> anyhow::Result<()> {
    let mut marker = path.as_os_str().to_owned();
    marker.push(uploader_marker_suffix(token_id));
    crate::config::atomic::write_private_bytes_atomic(Path::new(&marker), b"")
}

/// Whether `path` is exactly the canonical path of an attachment that
/// `token_id` uploaded to `cutex_session_id` (P2 non-owner `localImage`).
pub fn uploaded_by(cutex_session_id: &str, token_id: &str, path: &str) -> bool {
    attachments_root().is_ok_and(|root| uploaded_by_in(&root, cutex_session_id, token_id, path))
}

fn uploaded_by_in(root: &Path, cutex_session_id: &str, token_id: &str, path: &str) -> bool {
    let Ok(dir) = std::fs::canonicalize(root.join(session_dir_name(cutex_session_id))) else {
        return false;
    };
    let Ok(file) = std::fs::canonicalize(path) else {
        return false;
    };
    // The exact canonical form only: no symlink or `..` in what the agent opens.
    if super::workspace::display_path(&file) != path && file.as_os_str() != path {
        return false;
    }
    let name = file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    if file.parent() != Some(dir.as_path()) || !is_attachment_file_name(&name) || !file.is_file() {
        return false;
    }
    dir.join(format!("{name}{}", uploader_marker_suffix(token_id))).is_file()
}

/// Remove a deleted session's attachments (best effort).
pub fn remove_session_attachments(cutex_session_id: &str) {
    let Ok(root) = attachments_root() else {
        return;
    };
    remove_session_attachments_in(&root, cutex_session_id);
}

fn remove_session_attachments_in(root: &Path, cutex_session_id: &str) {
    let dir = root.join(session_dir_name(cutex_session_id));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("attachments: could not remove {}: {error}", dir.display()),
    }
}

/// Remove files older than `retention_days` (0 keeps everything) and empty
/// session directories. Returns the number of files removed.
pub fn prune_expired(root: &Path, retention_days: u32, now: SystemTime) -> usize {
    if retention_days == 0 {
        return 0;
    }
    let Some(cutoff) = now.checked_sub(Duration::from_secs(u64::from(retention_days) * 86_400))
    else {
        return 0;
    };
    let mut removed = 0;
    let Ok(sessions) = std::fs::read_dir(root) else {
        return 0;
    };
    for session in sessions.flatten() {
        if !session.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for file in files.flatten() {
            let expired = file
                .metadata()
                .ok()
                .filter(|meta| meta.is_file())
                .and_then(|meta| meta.modified().ok())
                .is_some_and(|modified| modified < cutoff);
            if expired && std::fs::remove_file(file.path()).is_ok() {
                removed += 1;
            }
        }
        // Only succeeds when empty.
        let _ = std::fs::remove_dir(session.path());
    }
    removed
}

fn maybe_prune(root: &Path, retention_days: u32) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|at| at.elapsed() < PRUNE_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    let removed = prune_expired(root, retention_days, SystemTime::now());
    if removed > 0 {
        eprintln!("attachments: pruned {removed} file(s) older than {retention_days} days");
    }
}

pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some(cutex_session_id) = session_id_from_path(path) else {
        return error(stream, UploadError::new(404, "Not Found", "not_found", "unknown route"));
    };
    if request.method != "POST" {
        return error(
            stream,
            UploadError::new(405, "Method Not Allowed", "method_not_allowed", "use POST"),
        );
    }
    if let Err(upload_error) = check_session(&cutex_session_id, context) {
        return error(stream, upload_error);
    }
    let root = match attachments_root() {
        Ok(root) => root,
        Err(store_error) => {
            return error(
                stream,
                UploadError::new(
                    500,
                    "Internal Server Error",
                    "attachment_store_failed",
                    format!("{store_error:#}"),
                ),
            )
        }
    };
    let settings = crate::config::store::load_codez_config().attachments;
    let result = store_upload(
        &root,
        &cutex_session_id,
        media_type(request).as_deref(),
        &request.body,
        settings,
    );
    match result {
        Ok((created, resource)) => {
            // A non-owner's upload is recorded for its token (P2 access).
            let non_owner = crate::management::users::current_principal().is_some_and(|p| !p.is_owner());
            if let Some(token) = crate::management::listeners::current_client_token().filter(|_| non_owner) {
                let path = resource["path"].as_str().unwrap_or_default();
                if let Err(record_error) = record_uploader(Path::new(path), &token.token_id) {
                    return error(
                        stream,
                        UploadError::new(
                            500,
                            "Internal Server Error",
                            "attachment_store_failed",
                            format!("could not record the upload: {record_error:#}"),
                        ),
                    );
                }
            }
            maybe_prune(&root, settings.effective().retention_days);
            if created {
                write_json_response(stream, 201, "Created", &resource)
            } else {
                write_json_response(stream, 200, "OK", &resource)
            }
        }
        Err(upload_error) => error(stream, upload_error),
    }
}

fn check_session(
    cutex_session_id: &str,
    context: ManagementRequestContext,
) -> Result<(), UploadError> {
    let internal = |error: anyhow::Error| {
        UploadError::new(500, "Internal Server Error", "session_lookup_failed", format!("{error:#}"))
    };
    let registry = (context.load_registry)().map_err(internal)?;
    let repository = super::repository::management_v2_repository().map_err(internal)?;
    let session = super::session::session_resource_including_hidden(
        cutex_session_id,
        &registry,
        context.load_runtime_status,
        repository,
    )
    .map_err(internal)?
    .ok_or_else(|| {
        UploadError::new(404, "Not Found", "session_not_found", "the session does not exist")
    })?;
    let host = session.get("hostId").and_then(Value::as_str).unwrap_or("");
    if !crate::runtime::lifecycle::cutex_session_host_is_local(
        host,
        &crate::platform::host::current_host_name(),
    ) {
        return Err(UploadError::new(
            409,
            "Conflict",
            "session_not_local",
            "the session runs on another host; upload to that host's daemon",
        )
        .with_details(json!({ "hostId": host })));
    }
    Ok(())
}

fn error(stream: &mut TcpStream, error: UploadError) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        error.status,
        error.reason,
        error.code,
        &error.message,
        false,
        error.details,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-test-bytes";

    fn temp_root() -> PathBuf {
        std::env::temp_dir()
            .join(format!("cutex-attachments-{}", uuid::Uuid::new_v4()))
            .join("attachments")
    }

    fn settings(max_bytes: u64) -> AttachmentSettings {
        AttachmentSettings {
            max_bytes,
            retention_days: 30,
        }
    }

    #[test]
    fn stores_content_addressed_inside_the_session_directory() {
        let root = temp_root();
        let (created, resource) =
            store_upload(&root, "cutex.s1", Some("image/png"), PNG, settings(1 << 20)).unwrap();
        assert!(created);
        let path = PathBuf::from(resource["path"].as_str().unwrap());
        assert!(path.is_absolute());
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        assert!(std::fs::canonicalize(&path).unwrap().starts_with(&canonical_root));
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
        assert_eq!(resource["mime"], "image/png");
        assert_eq!(resource["size"], PNG.len());
        assert_eq!(resource["attachmentId"].as_str().unwrap().len(), 64);
        assert!(path.to_string_lossy().ends_with(".png"));
        // Same bytes again: not created, same path.
        let (created, again) =
            store_upload(&root, "cutex.s1", Some("image/png"), PNG, settings(1 << 20)).unwrap();
        assert!(!created);
        assert_eq!(again["path"], resource["path"]);
        // Deleting the session removes its directory only.
        store_upload(&root, "cutex.s2", Some("image/png"), PNG, settings(1 << 20)).unwrap();
        remove_session_attachments_in(&root, "cutex.s1");
        assert!(!path.exists());
        assert!(root.join("cutex.s2").is_dir());
    }

    #[test]
    fn hostile_session_ids_stay_contained() {
        let root = temp_root();
        for id in ["../escape", "..", "a\\..\\b", "C:\\x", "x/../../y", ".hidden"] {
            let name = session_dir_name(id);
            assert!(!name.contains(['/', '\\', ':']), "{id} -> {name}");
            assert!(name != "." && name != "..");
            let (_, resource) =
                store_upload(&root, id, Some("image/png"), PNG, settings(1 << 20)).unwrap();
            let path = std::fs::canonicalize(resource["path"].as_str().unwrap()).unwrap();
            assert!(path.starts_with(std::fs::canonicalize(&root).unwrap()), "{id}");
        }
        assert_eq!(session_id_from_path("/v2/sessions/a%2Fb/attachments"), None);
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/attachments").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions//attachments"), None);
        assert_eq!(session_id_from_path("/v2/sessions/a/b/attachments"), None);
    }

    #[test]
    fn size_and_type_limits() {
        let root = temp_root();
        let small = settings(64 * 1024);
        let mut big = PNG.to_vec();
        big.resize(64 * 1024 + 1, 0);
        let error = store_upload(&root, "s", Some("image/png"), &big, small).unwrap_err();
        assert_eq!((error.status, error.code), (413, "attachment_too_large"));
        assert_eq!(error.details["maxBytes"], 64 * 1024);
        big.truncate(64 * 1024);
        assert!(store_upload(&root, "s", Some("image/png"), &big, small).is_ok());

        for mime in [None, Some("image/svg+xml"), Some("text/plain"), Some("application/octet-stream")] {
            let error = store_upload(&root, "s", mime, PNG, small).unwrap_err();
            assert_eq!((error.status, error.code), (415, "unsupported_media_type"));
        }
        let error = store_upload(&root, "s", Some("image/jpeg"), PNG, small).unwrap_err();
        assert_eq!((error.status, error.code), (400, "invalid_attachment"));
        let error = store_upload(&root, "s", Some("image/png"), b"", small).unwrap_err();
        assert_eq!((error.status, error.code), (400, "invalid_attachment"));
        assert!(store_upload(&root, "s", Some("image/jpeg"), b"\xff\xd8\xff\xe0jfif", small).is_ok());
        assert!(store_upload(&root, "s", Some("image/gif"), b"GIF89a....", small).is_ok());
        assert!(store_upload(&root, "s", Some("image/webp"), b"RIFF\0\0\0\0WEBPVP8 ", small).is_ok());
        // Out-of-range configured caps fall back to the default (8 MiB).
        let mut nine_mib = PNG.to_vec();
        nine_mib.resize(9 * 1024 * 1024, 0);
        let error = store_upload(&root, "s", Some("image/png"), &nine_mib, settings(1)).unwrap_err();
        assert_eq!(error.details["maxBytes"], 8 * 1024 * 1024);
    }

    #[test]
    fn uploads_are_usable_only_by_their_token_session_and_exact_path() {
        let root = temp_root();
        let (_, resource) =
            store_upload(&root, "cutex.s1", Some("image/png"), PNG, settings(1 << 20)).unwrap();
        let path = resource["path"].as_str().unwrap().to_string();
        assert!(!uploaded_by_in(&root, "cutex.s1", "mct-a", &path), "nobody recorded yet");
        record_uploader(Path::new(&path), "mct-a").unwrap();
        assert!(uploaded_by_in(&root, "cutex.s1", "mct-a", &path));
        assert!(!uploaded_by_in(&root, "cutex.s1", "mct-b", &path), "another token");
        assert!(!uploaded_by_in(&root, "cutex.s2", "mct-a", &path), "another session");
        // Same bytes in another session: its own directory, not recorded there.
        let (_, other) = store_upload(&root, "cutex.s2", Some("image/png"), PNG, settings(1 << 20)).unwrap();
        assert!(!uploaded_by_in(&root, "cutex.s2", "mct-a", other["path"].as_str().unwrap()));
        // Not the exact canonical path, host paths, markers, missing files.
        let dir = Path::new(&path).parent().unwrap().to_path_buf();
        let dotted = format!("{}/../{}/{}", dir.display(), dir.file_name().unwrap().to_string_lossy(), Path::new(&path).file_name().unwrap().to_string_lossy());
        assert!(!uploaded_by_in(&root, "cutex.s1", "mct-a", &dotted));
        #[cfg(unix)]
        {
            let link = root.join("link.png");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(!uploaded_by_in(&root, "cutex.s1", "mct-a", &link.to_string_lossy()));
        }
        let marker = format!("{path}{}", uploader_marker_suffix("mct-a"));
        assert!(!uploaded_by_in(&root, "cutex.s1", "mct-a", &marker));
        assert!(!uploaded_by_in(&root, "cutex.s1", "mct-a", "/etc/passwd"));
        assert!(!uploaded_by_in(&root, "cutex.s1", "mct-a", "relative.png"));
        // Pruning and session deletion take the marker with the file.
        assert_eq!(prune_expired(&root, 30, SystemTime::now() + Duration::from_secs(31 * 86_400)), 3);
        assert!(!Path::new(&marker).exists());
        assert!(!uploaded_by_in(&root, "cutex.s1", "mct-a", &path));
    }

    #[test]
    fn prune_removes_only_expired_files() {
        let root = temp_root();
        let (_, resource) =
            store_upload(&root, "old", Some("image/png"), PNG, settings(1 << 20)).unwrap();
        let path = PathBuf::from(resource["path"].as_str().unwrap());
        let now = SystemTime::now();
        assert_eq!(prune_expired(&root, 0, now + Duration::from_secs(400 * 86_400)), 0);
        assert_eq!(prune_expired(&root, 30, now), 0);
        assert!(path.exists());
        assert_eq!(prune_expired(&root, 30, now + Duration::from_secs(31 * 86_400)), 1);
        assert!(!path.exists());
        assert!(!root.join("old").exists());
    }
}
