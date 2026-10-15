//! Host directory browsing for the new-agent cwd picker on remote hosts
//! (root credential or a Management client token).
//!
//! `GET /v2/fs/dirs?path=<absolute path>` →
//! `{path, parent, home, roots, entries: [{name, path, isDir: true, hidden}],
//!   truncated}`
//!
//! - Lists directories only (symlinks / junctions to directories included);
//!   never file names or contents.
//! - `path` omitted or empty = the daemon user's home directory. A relative
//!   path is refused (400 `invalid_path`); a missing one is 404
//!   `path_not_found`, a file 400 `not_a_directory`, an unreadable one 403
//!   `permission_denied`.
//! - `path` / `parent` are the canonical form (Windows without the `\\?\`
//!   prefix); `parent` is null at a root. `roots`: drive letters on Windows
//!   (`C:\`), `/` elsewhere.
//! - At most [`MAX_ENTRIES`] entries, sorted case-insensitively;
//!   `truncated: true` when more exist.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const PATH: &str = "/v2/fs/dirs";
pub const MAX_ENTRIES: usize = 2000;

pub fn is_host_dirs_path(path: &str) -> bool {
    path == PATH
}

#[derive(Debug, PartialEq, Eq)]
pub struct DirError {
    pub status: u16,
    pub reason: &'static str,
    pub code: &'static str,
    pub message: String,
}

impl DirError {
    fn new(status: u16, reason: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            code,
            message: message.into(),
        }
    }
}

fn display(path: &Path) -> String {
    super::workspace::display_path(path)
}

pub fn roots() -> Vec<String> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetLogicalDrives() -> u32;
        }
        let mask = unsafe { GetLogicalDrives() };
        (0..26)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| format!("{}:\\", (b'A' + bit as u8) as char))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

fn is_hidden(name: &str, _meta: Option<&std::fs::Metadata>) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        if let Some(meta) = _meta {
            return meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0;
        }
    }
    false
}

pub fn list_dirs(raw: Option<&str>, home: Option<&Path>, max_entries: usize) -> Result<Value, DirError> {
    let home_display = home.map(|home| {
        std::fs::canonicalize(home)
            .map(|path| display(&path))
            .unwrap_or_else(|_| home.display().to_string())
    });
    let requested: PathBuf = match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(raw) => {
            if raw.chars().any(char::is_control) {
                return Err(DirError::new(400, "Bad Request", "invalid_path", "path contains control characters"));
            }
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                return Err(DirError::new(
                    400,
                    "Bad Request",
                    "invalid_path",
                    "path must be absolute",
                ));
            }
            path
        }
        None => home
            .map(Path::to_path_buf)
            .ok_or_else(|| DirError::new(409, "Conflict", "home_unavailable", "the home directory is unknown"))?,
    };
    let canonical = std::fs::canonicalize(&requested).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => DirError::new(404, "Not Found", "path_not_found", "the path does not exist"),
        std::io::ErrorKind::PermissionDenied => {
            DirError::new(403, "Forbidden", "permission_denied", "the path cannot be read")
        }
        _ => DirError::new(400, "Bad Request", "invalid_path", format!("the path cannot be resolved: {error}")),
    })?;
    if !canonical.is_dir() {
        return Err(DirError::new(400, "Bad Request", "not_a_directory", "the path is not a directory"));
    }
    let read = std::fs::read_dir(&canonical).map_err(|error| match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            DirError::new(403, "Forbidden", "permission_denied", "the directory cannot be read")
        }
        _ => DirError::new(500, "Internal Server Error", "fs_io_error", error.to_string()),
    })?;
    let display_dir = display(&canonical);
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in read.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let link_meta = entry.metadata().ok();
        let is_dir = if kind.is_dir() {
            true
        } else if kind.is_symlink() {
            // Follows the link (Windows junctions report as symlinks too).
            std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir())
        } else {
            false
        };
        if !is_dir {
            continue;
        }
        if entries.len() >= max_entries {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let hidden = is_hidden(&name, link_meta.as_ref());
        let path = display(&canonical.join(&name));
        entries.push((name, path, hidden));
    }
    entries.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(name, path, hidden)| json!({ "name": name, "path": path, "isDir": true, "hidden": hidden }))
        .collect();
    Ok(json!({
        "path": display_dir,
        "parent": canonical.parent().map(display),
        "home": home_display,
        "roots": roots(),
        "entries": entries,
        "truncated": truncated,
    }))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET",
            false,
            json!({}),
        );
    }
    let params: HashMap<String, String> = super::workspace::query_params(request);
    let home = dirs::home_dir();
    match list_dirs(params.get("path").map(String::as_str), home.as_deref(), MAX_ENTRIES) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => super::server::write_v2_error(
            stream,
            error.status,
            error.reason,
            error.code,
            &error.message,
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cutex-host-dirs-{}", uuid::Uuid::new_v4()));
        for dir in ["beta", "Alpha", ".hidden", "gamma/nested"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("file.txt"), b"secret contents").unwrap();
        std::fs::write(root.join("gamma/inner.txt"), b"x").unwrap();
        root
    }

    #[test]
    fn lists_only_directories_sorted() {
        let root = tree();
        let value = list_dirs(Some(root.to_str().unwrap()), None, MAX_ENTRIES).unwrap();
        let names: Vec<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, [".hidden", "Alpha", "beta", "gamma"]);
        assert_eq!(value["truncated"], false);
        let canonical = display(&std::fs::canonicalize(&root).unwrap());
        assert_eq!(value["path"], canonical);
        assert_eq!(
            value["parent"],
            display(std::fs::canonicalize(&root).unwrap().parent().unwrap())
        );
        let entries = value["entries"].as_array().unwrap();
        assert!(entries.iter().all(|entry| entry["isDir"] == true));
        assert_eq!(entries[0]["hidden"], true);
        assert_eq!(entries[1]["hidden"], false);
        let gamma = PathBuf::from(entries[3]["path"].as_str().unwrap());
        assert!(gamma.is_absolute());
        let nested = list_dirs(gamma.to_str(), None, MAX_ENTRIES).unwrap();
        assert_eq!(nested["entries"][0]["name"], "nested");
        assert_eq!(nested["entries"].as_array().unwrap().len(), 1);
        let text = value.to_string();
        assert!(!text.contains("file.txt") && !text.contains("secret contents"));
        assert!(!value["roots"].as_array().unwrap().is_empty());
    }

    #[test]
    fn caps_entries_and_defaults_to_home() {
        let root = tree();
        let value = list_dirs(Some(root.to_str().unwrap()), None, 2).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 2);
        assert_eq!(value["truncated"], true);
        let value = list_dirs(None, Some(&root.join("gamma")), MAX_ENTRIES).unwrap();
        assert_eq!(value["entries"][0]["name"], "nested");
        assert_eq!(value["home"], value["path"]);
    }

    #[test]
    fn refuses_relative_missing_and_file_paths() {
        let root = tree();
        let error = list_dirs(Some("relative/dir"), None, MAX_ENTRIES).unwrap_err();
        assert_eq!((error.status, error.code), (400, "invalid_path"));
        let error = list_dirs(Some(".."), None, MAX_ENTRIES).unwrap_err();
        assert_eq!(error.code, "invalid_path");
        let missing = root.join("missing");
        let error = list_dirs(missing.to_str(), None, MAX_ENTRIES).unwrap_err();
        assert_eq!((error.status, error.code), (404, "path_not_found"));
        let file = root.join("file.txt");
        let error = list_dirs(file.to_str(), None, MAX_ENTRIES).unwrap_err();
        assert_eq!((error.status, error.code), (400, "not_a_directory"));
        let error = list_dirs(None, None, MAX_ENTRIES).unwrap_err();
        assert_eq!(error.code, "home_unavailable");
    }
}
