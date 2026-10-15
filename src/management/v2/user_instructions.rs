//! Root-only `GET|PUT /v2/user-instructions`: the user-level `AGENTS.md` in
//! the host Codex home that cutex-managed native runtimes run with
//! (`CODEX_HOME = <cutex home>/codex-home`, see
//! `config::paths::host_codex_home_dir`; profiles only swap
//! `CODEX_CONFIG_FILE`/`CODEX_AUTH_FILE`, so this file is shared by every
//! profile). Codex prefers `AGENTS.override.md` in the same directory when it
//! is non-blank; the response reports it so clients can warn.

use std::net::TcpStream;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;

pub const PATH: &str = "/v2/user-instructions";
pub const MAX_BYTES: usize = 256 * 1024;
const MISSING_REVISION: &str = "missing";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PutBody {
    content: String,
    #[serde(default)]
    expected_revision: Option<String>,
}

fn instructions_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::host_codex_home_dir()?.join("AGENTS.md"))
}

pub fn revision(content: Option<&[u8]>) -> String {
    match content {
        None => MISSING_REVISION.to_string(),
        Some(bytes) => format!("sha256:{:x}", Sha256::digest(bytes)),
    }
}

fn read(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context(format!("Failed to read {}", path.display()))),
    }
}

fn resource(path: &Path) -> anyhow::Result<serde_json::Value> {
    let bytes = read(path)?;
    let override_path = path.with_file_name("AGENTS.override.md");
    let override_active = read(&override_path)?
        .is_some_and(|bytes| !String::from_utf8_lossy(&bytes).trim().is_empty());
    Ok(json!({
        "contractVersion": 2,
        "path": path.to_string_lossy(),
        "exists": bytes.is_some(),
        "content": bytes.as_deref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        "revision": revision(bytes.as_deref()),
        "maxBytes": MAX_BYTES,
        "overridePath": override_path.to_string_lossy(),
        "overrideActive": override_active,
    }))
}

pub(super) fn validate(body: &[u8]) -> Result<PutBody, String> {
    let body: PutBody =
        serde_json::from_slice(body).map_err(|error| format!("strict body required: {error}"))?;
    if body.content.len() > MAX_BYTES {
        return Err(format!("content exceeds {MAX_BYTES} bytes"));
    }
    if body.content.contains('\0') {
        return Err("content must not contain NUL".into());
    }
    Ok(body)
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = instructions_path()?;
    match request.method.as_str() {
        "GET" => write_json_response(stream, 200, "OK", &resource(&path)?),
        "PUT" => {
            let body = match validate(&request.body) {
                Ok(body) => body,
                Err(message) => {
                    return super::server::write_v2_error(
                        stream, 400, "Bad Request", "invalid_request", &message, false, json!({}),
                    )
                }
            };
            let current = revision(read(&path)?.as_deref());
            if let Some(expected) = body.expected_revision.as_deref() {
                if expected != current {
                    return super::server::write_v2_error(
                        stream,
                        409,
                        "Conflict",
                        "revision_mismatch",
                        "AGENTS.md changed since it was read",
                        false,
                        json!({ "currentRevision": current }),
                    );
                }
            }
            crate::config::atomic::write_bytes_atomic(&path, body.content.as_bytes())?;
            write_json_response(stream, 200, "OK", &resource(&path)?)
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET or PUT for /v2/user-instructions",
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_body_is_strict_and_capped() {
        let ok = validate(br#"{"content":"hi","expectedRevision":"missing"}"#).unwrap();
        assert_eq!(ok.expected_revision.as_deref(), Some("missing"));
        assert!(validate(br#"{"content":"hi","extra":1}"#).is_err());
        assert!(validate(br#"{}"#).is_err());
        let big = serde_json::to_vec(&json!({ "content": "x".repeat(MAX_BYTES + 1) })).unwrap();
        assert!(validate(&big).is_err());
    }

    #[test]
    fn revision_is_content_hash_or_missing() {
        assert_eq!(revision(None), "missing");
        assert_eq!(revision(Some(b"a")), revision(Some(b"a")));
        assert_ne!(revision(Some(b"a")), revision(Some(b"b")));
        assert!(revision(Some(b"")).starts_with("sha256:"));
    }
}
