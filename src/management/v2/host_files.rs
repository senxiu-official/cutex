//! Host file read for the owner's clients (root credential or a Management
//! client token), e.g. previewing a report an agent linked by absolute path
//! outside its workspace root.
//!
//! `GET /v2/fs/file?path=<absolute path>&maxBytes=<n>` → the workspace
//! `fs/file` body: `{path, size, modifiedAt, encoding: "utf8" | "base64",
//! content, truncated, mime}`.
//!
//! - `path` must be absolute (no `~` or other expansion); a relative path or
//!   one with control characters is 400 `invalid_path`. The response `path`
//!   is the canonical form (Windows without the `\\?\` prefix).
//! - Regular files only (symlinks are followed): a directory is 400
//!   `not_a_file`, a missing path 404 `path_not_found`, an unreadable one 403
//!   `permission_denied`.
//! - At most `maxBytes` (default 2 MiB, 1..=10 MiB, else 400
//!   `invalid_request`) are returned; `truncated: true` when the file is
//!   larger. Binary content (NUL bytes, invalid UTF-8, images, PDF) comes
//!   back base64 with its mime.
//!
//! - The host's Management root credential file (`<cutex home>/config.json`)
//!   is a root-identity secret (owner-local-only): a paired device's token
//!   or a remote listener gets 403 `owner_local_only`; only owner-local
//!   requests (`users::request_is_owner_local`: the root credential, or an
//!   owner token issued by `client-token issue` on the host, on loopback)
//!   read it.
//!
//! Never reachable with per-agent Agent Bus credentials or the ordinary
//! Management bridge token: only the root check in `server::handle_v2_request`
//! lets a request through.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;

use serde_json::Value;

use super::workspace::WorkspaceError;
use super::workspace::WorkspaceResult;

pub const PATH: &str = "/v2/fs/file";

pub fn is_host_files_path(path: &str) -> bool {
    path == PATH
}

pub fn read_host_file(params: &HashMap<String, String>) -> WorkspaceResult<Value> {
    let raw = params
        .get("path")
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
        .ok_or_else(|| WorkspaceError::new(400, "invalid_path", "path is required"))?;
    if raw.chars().any(char::is_control) {
        return Err(WorkspaceError::new(400, "invalid_path", "path contains control characters"));
    }
    let requested = PathBuf::from(raw);
    if !requested.is_absolute() {
        return Err(WorkspaceError::new(400, "invalid_path", "path must be absolute"));
    }
    let max_bytes = super::workspace_fs::max_bytes_param(params)?;
    let canonical = std::fs::canonicalize(&requested).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            WorkspaceError::new(404, "path_not_found", "the path does not exist")
        }
        std::io::ErrorKind::PermissionDenied => {
            WorkspaceError::new(403, "permission_denied", "the path cannot be read")
        }
        _ => WorkspaceError::new(400, "invalid_path", format!("the path cannot be resolved: {error}")),
    })?;
    refuse_root_credential_file(&canonical)?;
    let meta = std::fs::metadata(&canonical).map_err(WorkspaceError::internal)?;
    if !meta.is_file() {
        return Err(WorkspaceError::new(400, "not_a_file", "the path is not a regular file"));
    }
    let display = super::workspace::display_path(&canonical);
    super::workspace_fs::file_content_value(&canonical, &display, &meta, max_bytes)
}

/// 403 `owner_local_only` (`readRootCredential`) when `path` is the root
/// credential file and the request is not owner-local. Also used by the
/// workspace file read (`workspace_fs::file_resource`): a workspace rooted
/// at the home directory must not hand it out either.
pub(crate) fn refuse_root_credential_file(path: &std::path::Path) -> WorkspaceResult<()> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if is_root_credential_file(&canonical) && !crate::management::users::request_is_owner_local() {
        let refused = crate::management::users::owner_local_only("readRootCredential");
        return Err(WorkspaceError::new(403, refused.code, refused.message));
    }
    Ok(())
}

/// `<cutex home>/config.json` (holds `management_api_token`).
fn is_root_credential_file(canonical: &std::path::Path) -> bool {
    crate::config::paths::config_path()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .is_some_and(|config| config == canonical)
}

pub fn handle(stream: &mut TcpStream, request: &crate::http::server::SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::workspace::write_error(
            stream,
            &WorkspaceError::new(405, "method_not_allowed", "use GET"),
        );
    }
    let params = super::workspace::query_params(request);
    match read_host_file(&params) {
        Ok(value) => crate::http::server::write_json_response(stream, 200, "OK", &value),
        Err(error) => super::workspace::write_error(stream, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(path: &str, max: Option<&str>) -> HashMap<String, String> {
        let mut params = HashMap::from([("path".to_string(), path.to_string())]);
        if let Some(max) = max {
            params.insert("maxBytes".to_string(), max.to_string());
        }
        params
    }

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cutex-host-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("report.md"), "# Issue\nline 2\n").unwrap();
        std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        std::fs::write(root.join("pic.png"), [0x89u8, b'P', b'N', b'G']).unwrap();
        root
    }

    #[test]
    fn reads_text_with_canonical_path_and_truncation() {
        let root = tree();
        let file = root.join("report.md");
        let value = read_host_file(&params(file.to_str().unwrap(), None)).unwrap();
        assert_eq!(value["encoding"], "utf8");
        assert_eq!(value["content"], "# Issue\nline 2\n");
        assert_eq!(value["truncated"], false);
        assert_eq!(value["mime"], "text/markdown");
        assert_eq!(value["size"], 15);
        let canonical = super::super::workspace::display_path(&std::fs::canonicalize(&file).unwrap());
        assert_eq!(value["path"], canonical);
        assert!(value["modifiedAt"].is_string());
        let value = read_host_file(&params(file.to_str().unwrap(), Some("7"))).unwrap();
        assert_eq!(value["content"], "# Issue");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["size"], 15);
        // A non-canonical spelling resolves to the same file.
        let dotted = root.join("dir").join("..").join("report.md");
        let value = read_host_file(&params(dotted.to_str().unwrap(), None)).unwrap();
        assert_eq!(value["path"], canonical);
    }

    #[test]
    fn binary_and_images_come_back_base64() {
        use base64::Engine;
        let root = tree();
        let value = read_host_file(&params(root.join("blob.bin").to_str().unwrap(), None)).unwrap();
        assert_eq!(value["encoding"], "base64");
        assert_eq!(value["mime"], "application/octet-stream");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, [0u8, 1, 2, 3, 0, 255]);
        let value = read_host_file(&params(root.join("pic.png").to_str().unwrap(), None)).unwrap();
        assert_eq!((value["encoding"].as_str(), value["mime"].as_str()), (Some("base64"), Some("image/png")));
    }

    /// The route sits behind the root gate in `handle_v2_request`: the root
    /// credential and Management client tokens pass; per-agent Agent Bus
    /// credentials, the ordinary bridge token and anonymous callers do not,
    /// and the Agent Bus never routes it for an agent.
    #[test]
    fn only_the_root_credential_or_a_client_token_passes_the_gate() {
        use crate::agent_bus::agent_auth::{agent_credential, agent_route_allowed};
        use crate::http::server::SimpleHttpRequest;
        use crate::management::client_tokens::{authenticate_management_request_with, ClientTokenStore};
        use crate::management::v2::server::{authenticated_root_request, is_remote_client_root_path};

        assert!(is_remote_client_root_path(PATH));
        assert!(!super::super::workspace::is_workspace_path(PATH));
        let root = "management-root-secret";
        let store = ClientTokenStore::at(
            std::env::temp_dir()
                .join(format!("cutex-host-files-tokens-{}", uuid::Uuid::new_v4()))
                .join("management-clients"),
        );
        let issued = store.issue("desktop").unwrap();
        let allowed = |bearer: Option<&str>, agent_id: Option<&str>| {
            let mut headers = HashMap::new();
            if let Some(bearer) = bearer {
                headers.insert("authorization".to_string(), format!("Bearer {bearer}"));
            }
            if let Some(agent_id) = agent_id {
                headers.insert("x-cutex-agent-id".to_string(), agent_id.to_string());
            }
            let mut request = SimpleHttpRequest {
                method: "GET".into(),
                path: format!("{PATH}?path=%2Fetc%2Fhosts"),
                headers,
                body: Vec::new(),
            };
            authenticate_management_request_with(&mut request, Some(root), &store);
            authenticated_root_request(&request, Some(root))
        };
        assert!(allowed(Some(root), None));
        assert!(allowed(Some(&issued.token), None));
        // Per-agent credentials, even one derived from the same secret.
        let agent = agent_credential(root, "agent-1", "cutex.s1");
        assert!(!allowed(Some(&agent), Some("agent-1")));
        let agent = agent_credential("agent-bus-root", "agent-1", "cutex.s1");
        assert!(!allowed(Some(&agent), Some("agent-1")));
        assert!(!allowed(Some("management-bridge-token"), None));
        assert!(!allowed(Some("cxm1.forged.token"), None));
        assert!(!allowed(None, None));
        store.revoke(&issued.token_id).unwrap();
        assert!(!allowed(Some(&issued.token), None));
        let fenced = HashMap::from([
            ("x-cutex-agent-id".to_string(), "agent-1".to_string()),
            (crate::agent_bus::mcp::THREAD_HEADER.to_string(), "t".to_string()),
            (crate::agent_bus::mcp::GENERATION_HEADER.to_string(), "1".to_string()),
        ]);
        assert!(!agent_route_allowed("GET", PATH, &fenced));
        assert!(!agent_route_allowed("POST", PATH, &fenced));
    }

    #[test]
    fn refuses_relative_missing_directory_and_bad_limits() {
        let root = tree();
        let code = |path: &str, max: Option<&str>| {
            let error = read_host_file(&params(path, max)).unwrap_err();
            (error.status, error.code)
        };
        assert_eq!(code("report.md", None), (400, "invalid_path"));
        assert_eq!(code("~/report.md", None), (400, "invalid_path"));
        assert_eq!(code("", None), (400, "invalid_path"));
        assert_eq!(code("/tmp/a\nb", None), (400, "invalid_path"));
        assert_eq!(code(root.join("missing.md").to_str().unwrap(), None), (404, "path_not_found"));
        assert_eq!(code(root.join("dir").to_str().unwrap(), None), (400, "not_a_file"));
        let file = root.join("report.md");
        assert_eq!(code(file.to_str().unwrap(), Some("0")), (400, "invalid_request"));
        assert_eq!(code(file.to_str().unwrap(), Some("99999999999")), (400, "invalid_request"));
        assert!(read_host_file(&HashMap::new()).is_err());
    }
}
