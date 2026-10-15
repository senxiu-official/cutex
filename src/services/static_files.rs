//! Static service validation and authenticated response writer. Authentication
//! and traffic accounting belong to the service proxy, which calls `serve`.
use super::store::{Caller, ServiceRecord, ServicesError, ServicesResult};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

pub fn validate_root(root: &str, services_dir: &Path, caller: &Caller) -> ServicesResult<String> {
    let path = Path::new(root);
    if !path.is_absolute() {
        return Err(ServicesError::bad_request("root must be absolute"));
    }
    let real = path
        .canonicalize()
        .map_err(|_| ServicesError::bad_request("root does not exist"))?;
    if !(real.is_dir()
        || (real.is_file()
            && real
                .extension()
                .is_some_and(|v| v.eq_ignore_ascii_case("html"))))
    {
        return Err(ServicesError::bad_request(
            "root must be a directory or an .html file",
        ));
    }
    let home = services_dir.parent().and_then(|p| p.canonicalize().ok());
    if home.as_ref().is_some_and(|home| real.starts_with(home)) {
        return Err(ServicesError::new(
            403,
            "static_root_forbidden",
            "root must be outside the cutex home",
        ));
    }
    if let Caller::Agent {
        cutex_session_id, ..
    } = caller
    {
        let sessions = crate::session::store::load_cutex_session_store()
            .map_err(ServicesError::unavailable)?;
        let record = sessions.sessions.get(cutex_session_id).ok_or_else(|| {
            ServicesError::new(
                403,
                "static_root_forbidden",
                "caller workspace is unavailable",
            )
        })?;
        let mut roots = vec![PathBuf::from(&record.cwd)];
        if let Ok(snapshot) = crate::memory::load_agent_management_snapshot() {
            if let Some(project) = crate::memory::project_for_session(&snapshot, cutex_session_id) {
                if let Some(root) = snapshot
                    .project_states
                    .get(&project)
                    .and_then(|state| state.workspace_root.as_ref())
                {
                    roots.push(PathBuf::from(root));
                }
            }
        }
        if !roots
            .into_iter()
            .filter_map(|p| p.canonicalize().ok())
            .any(|p| real.starts_with(p))
        {
            return Err(ServicesError::new(
                403,
                "static_root_forbidden",
                "root must be inside the caller cwd or project workspace",
            ));
        }
    }
    Ok(real.to_string_lossy().into_owned())
}

fn decode_path(target: &str) -> Option<String> {
    let path = target.split('?').next()?;
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(
                u8::from_str_radix(std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?, 16).ok()?,
            );
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    if decoded.contains(['\\', '\0', ':'])
        || decoded.chars().any(char::is_control)
        || decoded
            .split('/')
            .any(|p| p.starts_with('.') || p.ends_with(['.', ' ']))
    {
        return None;
    }
    Some(decoded)
}

pub fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = decode_path(target)?;
    // The stored canonical root cannot silently turn into a symlink to a new root.
    let real_root = root.canonicalize().ok()?;
    if real_root != root {
        return None;
    }
    let base = if root.is_file() { root.parent()? } else { root };
    let mut candidate = if root.is_file() && path == "/" {
        root.to_path_buf()
    } else {
        base.join(path.trim_start_matches('/'))
    };
    candidate = candidate.canonicalize().ok()?;
    if !candidate.starts_with(base) {
        return None;
    }
    if candidate.is_dir() {
        candidate = candidate.join("index.html").canonicalize().ok()?;
    }
    if !candidate.starts_with(base) || !candidate.is_file() {
        return None;
    }
    // An innocently named in-root symlink must not reveal a hidden file, and
    // registering a parent directory must not expose a custom non-dot home.
    if candidate
        .strip_prefix(base)
        .ok()?
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
    {
        return None;
    }
    if crate::config::paths::config_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|home| candidate.starts_with(home))
    {
        return None;
    }
    Some(candidate)
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" | "csv" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub fn serve(
    record: &ServiceRecord,
    method: &str,
    target: &str,
    stream: &mut TcpStream,
) -> io::Result<()> {
    let allowed = matches!(method, "GET" | "HEAD");
    let file = allowed
        .then(|| {
            record
                .root
                .as_deref()
                .and_then(|root| resolve(Path::new(root), target))
        })
        .flatten()
        .and_then(|path| std::fs::File::open(&path).ok().map(|file| (path, file)));
    if let Some((path, mut file)) = file {
        let len = file.metadata()?.len();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {len}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", content_type(&path))?;
        super::traffic::touch(&record.slug);
        if method != "HEAD" {
            let mut buffer = [0; 16384];
            let mut remaining = len;
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let n = file.read(&mut buffer[..limit])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                stream.write_all(&buffer[..n])?;
                remaining -= n as u64;
                super::traffic::touch(&record.slug);
            }
        }
    } else {
        let (status, error) = if allowed {
            ("404 Not Found", "static_not_found")
        } else {
            ("405 Method Not Allowed", "method_not_allowed")
        };
        write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nAllow: GET, HEAD\r\nX-Cutex-Service-Error: {error}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn agent_static_root_is_confined_to_its_workspace() {
        // Other tests mutate HOME and remove it; keep this thread-local instance
        // outside those process-wide environment fixtures.
        let base = std::env::temp_dir()
            .join(format!("static-agent-{}", uuid::Uuid::new_v4()));
        let home = base.join("state");
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        for path in [&home, &workspace, &outside] {
            std::fs::create_dir_all(path).unwrap();
        }
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::config::instance::set_test_selection(
                    crate::config::instance::Selection::Default,
                );
            }
        }
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
            home.clone(),
        ));
        let _reset = Reset;
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let record:crate::session::model::CutexSessionRecord=serde_json::from_value(serde_json::json!({"cutex_session_id":id,"host_id":"fixture","cwd":workspace,"created_at":"2026-10-11T00:00:00Z","updated_at":"2026-10-11T00:00:00Z"})).unwrap();
        let before = crate::session::store::load_cutex_session_store().unwrap();
        let mut after = before.clone();
        after.sessions.insert(id.clone(), record);
        crate::session::store::save_cutex_session_record(&before, &after, &id).unwrap();
        let caller = Caller::Agent {
            cutex_session_id: id,
            project_id: None,
        };
        validate_root(workspace.to_str().unwrap(), &home.join("services"), &caller).unwrap();
        assert_eq!(
            validate_root(outside.to_str().unwrap(), &home.join("services"), &caller)
                .unwrap_err()
                .code,
            "static_root_forbidden"
        );
        assert_eq!(
            validate_root(
                home.to_str().unwrap(),
                &home.join("services"),
                &Caller::Owner
            )
            .unwrap_err()
            .code,
            "static_root_forbidden"
        );
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn static_path_and_single_html_rules() {
        let dir = std::env::temp_dir().join(format!("cutex-static-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "hello").unwrap();
        std::fs::write(dir.join("report.html"), "report").unwrap();
        std::fs::write(dir.join(".env"), "secret").unwrap();
        let root = dir.canonicalize().unwrap();
        assert_eq!(resolve(&root, "/"), Some(root.join("index.html")));
        assert_eq!(
            resolve(&root.join("report.html"), "/"),
            Some(root.join("report.html"))
        );
        for bad in [
            "/../outside",
            "/%2e%2e/x",
            "/.env",
            "/%2eenv",
            "/%5c.env",
            "/a%00b",
            "/a:stream",
            "/a%",
            "/missing",
        ] {
            assert!(resolve(&root, bad).is_none(), "{bad}");
        }
        std::fs::create_dir(root.join("empty")).unwrap();
        assert!(resolve(&root, "/empty").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn static_http_head_and_method_rejection() {
        let dir = std::env::temp_dir().join(format!("cutex-static-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "hello").unwrap();
        let record:ServiceRecord=serde_json::from_value(serde_json::json!({"slug":"static","displayName":"Static","host":"host","port":0,"runtime":"static","root":dir.canonicalize().unwrap(),"createdAt":"now","updatedAt":"now"})).unwrap();
        for (method, status, body) in [
            ("GET", "200 OK", true),
            ("HEAD", "200 OK", false),
            ("POST", "405 Method Not Allowed", false),
        ] {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let record = record.clone();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                serve(&record, method, "/", &mut stream).unwrap();
            });
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            server.join().unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {status}")));
            assert!(response.contains("nosniff"));
            assert!(response.contains("Cache-Control: no-store"));
            assert_eq!(response.ends_with("hello"), body);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn static_symlink_or_junction_cannot_escape() {
        let dir = std::env::temp_dir().join(format!("cutex-static-link-{}", uuid::Uuid::new_v4()));
        let root = dir.join("root");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.html"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(root.join("escape"))
                .arg(&outside)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "could not create private test junction"
            );
        }
        assert!(resolve(&root.canonicalize().unwrap(), "/escape/secret.html").is_none());
        // Remove the link itself before removing the fixture directory.
        #[cfg(windows)]
        std::fs::remove_dir(root.join("escape")).unwrap();
        #[cfg(unix)]
        std::fs::remove_file(root.join("escape")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
