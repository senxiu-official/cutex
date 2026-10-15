//! Root-only workspace routes for desktop clients (files, git, terminal).
//!
//! Scopes share one implementation; the root directory is resolved per call:
//! - `/v2/projects/{projectId}/…` → the Project `workspace_root`
//! - `/v2/sessions/{cutexSessionId}/…` → that session's `cwd`
//!
//! Routes (all require the Management root credential):
//! - `GET  {scope}/fs/dir?path=`             directory listing
//! - `GET  {scope}/fs/file?path=&maxBytes=`  file content (utf8 or base64)
//! - `GET  {scope}/fs/search?q=&limit=`      fuzzy file-name search
//! - `GET  {scope}/fs/watch`                 SSE change notifications
//! - `GET  {scope}/git/status`               branch + changed files
//! - `GET  {scope}/git/diff?path=&staged=`   unified diff
//! - `POST {scope}/terminals`                spawn a PTY in the root
//! - `GET  /v2/terminals`, `DELETE /v2/terminals/{id}`,
//!   `POST /v2/terminals/{id}/input|resize`, `GET /v2/terminals/{id}/stream`
//!
//! These paths are intercepted before the ordinary v2 dispatch (root
//! credential check in `handle`).
//!
//! Non-owner users (P2 access, capability `users-p2-access`): the access
//! gate lets the read routes (`fs/*`, `git/*`, GET only) through for a
//! granted project or a visible agent's session; terminals stay the owner's.
//! Their root must also pass [`non_owner_root_allowed`] (403
//! `workspace_root_too_broad`): never a filesystem root, the OS user's home
//! or an ancestor of it, or a directory containing the cutex home. Paths stay
//! contained as for the owner (canonical, no `..`, no symlink escape, no
//! `.git` internals).

use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;

use serde_json::json;
use serde_json::Value;

use crate::http::server::require_service_bridge_token;
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::management::server::ManagementRequestContext;

/// Error returned by workspace operations; rendered as the v2 error envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl WorkspaceError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(500, "workspace_io_error", error.to_string())
    }
}

pub type WorkspaceResult<T> = Result<T, WorkspaceError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Project,
    Session,
}

impl ScopeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedRoute {
    FsDir,
    FsFile,
    FsSearch,
    FsWatch,
    GitStatus,
    GitDiff,
    Terminals,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceRoute {
    Scoped(Scope, ScopedRoute),
    TerminalList,
    Terminal(String, TerminalRoute),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalRoute {
    Delete,
    Input,
    Resize,
    Stream,
}

/// A resolved workspace root: the canonical directory plus its scope.
#[derive(Debug, Clone)]
pub struct ResolvedRoot {
    pub scope: Scope,
    /// Canonical (verbatim on Windows) path used for containment checks.
    pub canonical: PathBuf,
    /// Display form (Windows verbatim prefix stripped).
    pub display: String,
}

impl ResolvedRoot {
    pub fn from_dir(scope: Scope, dir: &Path) -> WorkspaceResult<Self> {
        let canonical = std::fs::canonicalize(dir).map_err(|_| {
            WorkspaceError::new(
                409,
                "workspace_root_unavailable",
                format!("workspace root does not exist: {}", dir.display()),
            )
        })?;
        if !canonical.is_dir() {
            return Err(WorkspaceError::new(
                409,
                "workspace_root_unavailable",
                "workspace root is not a directory",
            ));
        }
        let display = display_path(&canonical);
        Ok(Self {
            scope,
            canonical,
            display,
        })
    }
}

pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    }
}

/// True for any path handled by this module (checked before v2 dispatch).
pub fn is_workspace_path(path: &str) -> bool {
    parse_route(path).is_some()
}

pub fn parse_route(path: &str) -> Option<WorkspaceRoute> {
    if path == "/v2/terminals" {
        return Some(WorkspaceRoute::TerminalList);
    }
    if let Some(rest) = path.strip_prefix("/v2/terminals/") {
        let (id, tail) = match rest.split_once('/') {
            Some((id, tail)) => (id, Some(tail)),
            None => (rest, None),
        };
        if !valid_terminal_id(id) {
            return None;
        }
        let route = match tail {
            None => TerminalRoute::Delete,
            Some("input") => TerminalRoute::Input,
            Some("resize") => TerminalRoute::Resize,
            Some("stream") => TerminalRoute::Stream,
            Some(_) => return None,
        };
        return Some(WorkspaceRoute::Terminal(id.to_string(), route));
    }
    let (kind, rest) = if let Some(rest) = path.strip_prefix("/v2/projects/") {
        (ScopeKind::Project, rest)
    } else if let Some(rest) = path.strip_prefix("/v2/sessions/") {
        (ScopeKind::Session, rest)
    } else {
        return None;
    };
    let (encoded, tail) = rest.split_once('/')?;
    let route = match tail {
        "fs/dir" => ScopedRoute::FsDir,
        "fs/file" => ScopedRoute::FsFile,
        "fs/search" => ScopedRoute::FsSearch,
        "fs/watch" => ScopedRoute::FsWatch,
        "git/status" => ScopedRoute::GitStatus,
        "git/diff" => ScopedRoute::GitDiff,
        "terminals" => ScopedRoute::Terminals,
        _ => return None,
    };
    let id = percent_decode(encoded)?;
    if id.is_empty() || id.contains('/') || id.chars().any(char::is_control) {
        return None;
    }
    Some(WorkspaceRoute::Scoped(Scope { kind, id }, route))
}

fn valid_terminal_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn percent_decode(encoded: &str) -> Option<String> {
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

/// Query-string parameters as owned pairs (later duplicates win).
pub fn query_params(request: &SimpleHttpRequest) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    if let Some((_, query)) = request.path.split_once('?') {
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            params.insert(name.into_owned(), value.into_owned());
        }
    }
    params
}

pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    root_token: Option<&str>,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    if !is_authorized(request, root_token) {
        return write_error(
            stream,
            &WorkspaceError::new(
                401,
                "unauthorized",
                "the Management root credential is required for workspace routes",
            ),
        );
    }
    let restricted = crate::management::users::current_principal().is_some_and(|principal| !principal.is_owner());
    dispatch(stream, request, |scope| {
        let root = resolve_root(scope, context)?;
        if restricted {
            non_owner_root_allowed(&root)?;
        }
        Ok(root)
    })
}

/// Whether a non-owner may read below `root` (see the module docs): the
/// root must not be a filesystem root, nor contain the OS user's home or
/// the cutex home (tokens, credentials, every agent's state).
pub fn non_owner_root_allowed(root: &ResolvedRoot) -> WorkspaceResult<()> {
    let canonical = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    let mut protected = Vec::new();
    if let Some(home) = crate::config::paths::home_dir() {
        protected.push(canonical(home));
    }
    if let Ok(dir) = crate::config::paths::config_dir() {
        protected.push(canonical(dir));
    }
    non_owner_root_allowed_with(root, &protected)
}

fn non_owner_root_allowed_with(root: &ResolvedRoot, protected: &[PathBuf]) -> WorkspaceResult<()> {
    let too_broad = root.canonical.parent().is_none()
        || protected.iter().any(|path| path.starts_with(&root.canonical));
    if too_broad {
        return Err(WorkspaceError::new(
            403,
            "workspace_root_too_broad",
            "this workspace root is not readable for this user (it is a filesystem root or contains the host home)",
        ));
    }
    Ok(())
}

/// Only a configured, non-empty Management root credential is accepted.
fn is_authorized(request: &SimpleHttpRequest, root_token: Option<&str>) -> bool {
    root_token.is_some_and(|token| !token.is_empty())
        && require_service_bridge_token(request, root_token, "Management workspace").is_ok()
}

/// Route an authenticated workspace request; `resolve` maps a scope to
/// its root (injected so tests can avoid the daemon context).
fn dispatch(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    resolve: impl Fn(&Scope) -> WorkspaceResult<ResolvedRoot>,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some(route) = parse_route(path) else {
        return write_error(
            stream,
            &WorkspaceError::new(404, "not_found", "unknown route"),
        );
    };
    let method = request.method.as_str();
    match route {
        WorkspaceRoute::TerminalList => match method {
            "GET" => write_ok(stream, &super::terminal::list_resource()),
            _ => write_method_not_allowed(stream),
        },
        WorkspaceRoute::Terminal(id, terminal_route) => match (method, terminal_route) {
            ("DELETE", TerminalRoute::Delete) => write_result(stream, super::terminal::kill(&id)),
            ("POST", TerminalRoute::Input) => {
                write_result(stream, super::terminal::input(&id, &request.body))
            }
            ("POST", TerminalRoute::Resize) => {
                write_result(stream, super::terminal::resize(&id, &request.body))
            }
            ("GET", TerminalRoute::Stream) => super::terminal::stream(stream, request, &id),
            _ => write_method_not_allowed(stream),
        },
        WorkspaceRoute::Scoped(scope, scoped) => {
            let expected = match scoped {
                ScopedRoute::Terminals => "POST",
                _ => "GET",
            };
            if method != expected && !(scoped == ScopedRoute::Terminals && method == "GET") {
                return write_method_not_allowed(stream);
            }
            let root = match resolve(&scope) {
                Ok(root) => root,
                Err(error) => return write_error(stream, &error),
            };
            let params = query_params(request);
            match (method, scoped) {
                (_, ScopedRoute::FsDir) => {
                    write_result(stream, super::workspace_fs::dir_resource(&root, &params))
                }
                (_, ScopedRoute::FsFile) => {
                    write_result(stream, super::workspace_fs::file_resource(&root, &params))
                }
                (_, ScopedRoute::FsSearch) => {
                    write_result(stream, super::workspace_fs::search_resource(&root, &params))
                }
                (_, ScopedRoute::FsWatch) => super::workspace_fs::watch_stream(stream, &root),
                (_, ScopedRoute::GitStatus) => {
                    write_result(stream, super::workspace_git::status_resource(&root))
                }
                (_, ScopedRoute::GitDiff) => {
                    write_result(stream, super::workspace_git::diff_resource(&root, &params))
                }
                ("GET", ScopedRoute::Terminals) => {
                    write_ok(stream, &super::terminal::list_resource_for(&root))
                }
                (_, ScopedRoute::Terminals) => {
                    write_result(stream, super::terminal::create(&root, &request.body))
                }
            }
        }
    }
}

fn resolve_root(scope: &Scope, context: ManagementRequestContext) -> WorkspaceResult<ResolvedRoot> {
    match scope.kind {
        ScopeKind::Project => {
            let project_id = crate::agent_management::ProjectId::new(scope.id.clone())
                .map_err(|_| WorkspaceError::new(404, "project_not_found", "invalid project ID"))?;
            let principal =
                crate::management::control_plane::HumanManagementPrincipal::authenticated();
            let workspace =
                (context.read_management_project)(&principal, &project_id).map_err(|error| {
                    use crate::agent_management::AgentManagementError;
                    match error {
                        AgentManagementError::NotFound(_) => WorkspaceError::new(
                            404,
                            "project_not_found",
                            "the Project does not exist on this host",
                        ),
                        other => WorkspaceError::new(
                            409,
                            "project_unavailable",
                            format!("the Project cannot be read: {other}"),
                        ),
                    }
                })?;
            let Some(root) = workspace.workspace_root else {
                return Err(WorkspaceError::new(
                    409,
                    "workspace_root_unset",
                    "the Project has no workspace_root",
                ));
            };
            ResolvedRoot::from_dir(scope.clone(), Path::new(&root))
        }
        ScopeKind::Session => {
            let registry = (context.load_registry)().map_err(WorkspaceError::internal)?;
            let repository =
                super::repository::management_v2_repository().map_err(WorkspaceError::internal)?;
            let session = super::session::session_resource_including_hidden(
                &scope.id,
                &registry,
                context.load_runtime_status,
                repository,
            )
            .map_err(WorkspaceError::internal)?
            .ok_or_else(|| {
                WorkspaceError::new(404, "session_not_found", "the session does not exist")
            })?;
            let cwd = session
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
                .ok_or_else(|| {
                    WorkspaceError::new(409, "session_cwd_unset", "the session has no cwd")
                })?;
            ResolvedRoot::from_dir(scope.clone(), Path::new(cwd))
        }
    }
}

fn write_ok(stream: &mut TcpStream, value: &Value) -> anyhow::Result<()> {
    write_json_response(stream, 200, "OK", value)
}

fn write_result(stream: &mut TcpStream, result: WorkspaceResult<Value>) -> anyhow::Result<()> {
    match result {
        Ok(value) => write_ok(stream, &value),
        Err(error) => write_error(stream, &error),
    }
}

fn write_method_not_allowed(stream: &mut TcpStream) -> anyhow::Result<()> {
    write_error(
        stream,
        &WorkspaceError::new(
            405,
            "method_not_allowed",
            "method not allowed for this route",
        ),
    )
}

pub fn write_error(stream: &mut TcpStream, error: &WorkspaceError) -> anyhow::Result<()> {
    write_json_response(
        stream,
        error.status,
        reason(error.status),
        &json!({
            "contractVersion": 2,
            "error": {
                "source": "cutex",
                "code": error.code,
                "message": error.message,
                "retryable": false,
                "details": {},
            }
        }),
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// RFC 3339 UTC timestamp for a file time.
pub fn rfc3339(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_owner_roots_exclude_filesystem_roots_and_home_ancestors() {
        let base = std::env::temp_dir().join(format!("cutex-ws-root-{}", uuid::Uuid::new_v4()));
        let home = base.join("home");
        let project = home.join("work").join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(home.join(".cutex")).unwrap();
        let protected = vec![
            std::fs::canonicalize(&home).unwrap(),
            std::fs::canonicalize(home.join(".cutex")).unwrap(),
        ];
        let scope = Scope { kind: ScopeKind::Session, id: "s".into() };
        let root = |dir: &Path| ResolvedRoot::from_dir(scope.clone(), dir).unwrap();
        assert!(non_owner_root_allowed_with(&root(&project), &protected).is_ok());
        assert!(non_owner_root_allowed_with(&root(&home.join("work")), &protected).is_ok());
        for broad in [home.clone(), base.clone()] {
            let error = non_owner_root_allowed_with(&root(&broad), &protected).unwrap_err();
            assert_eq!((error.status, error.code), (403, "workspace_root_too_broad"));
        }
        let filesystem_root = std::fs::canonicalize(&base).unwrap().ancestors().last().unwrap().to_path_buf();
        assert!(non_owner_root_allowed_with(&root(&filesystem_root), &[]).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn parses_scoped_and_terminal_routes() {
        assert_eq!(
            parse_route("/v2/projects/p1/fs/dir"),
            Some(WorkspaceRoute::Scoped(
                Scope {
                    kind: ScopeKind::Project,
                    id: "p1".into()
                },
                ScopedRoute::FsDir
            ))
        );
        assert_eq!(
            parse_route("/v2/sessions/a%3Ab/git/status"),
            Some(WorkspaceRoute::Scoped(
                Scope {
                    kind: ScopeKind::Session,
                    id: "a:b".into()
                },
                ScopedRoute::GitStatus
            ))
        );
        assert_eq!(
            parse_route("/v2/terminals"),
            Some(WorkspaceRoute::TerminalList)
        );
        assert_eq!(
            parse_route("/v2/terminals/t-1/stream"),
            Some(WorkspaceRoute::Terminal(
                "t-1".into(),
                TerminalRoute::Stream
            ))
        );
        assert_eq!(
            parse_route("/v2/terminals/t-1"),
            Some(WorkspaceRoute::Terminal(
                "t-1".into(),
                TerminalRoute::Delete
            ))
        );
        // Existing routes are not captured.
        assert_eq!(parse_route("/v2/projects/p1/tasks"), None);
        assert_eq!(parse_route("/v2/sessions/s1/cutex/requests"), None);
        assert_eq!(parse_route("/v2/sessions/s1"), None);
        assert_eq!(parse_route("/v2/terminals/../x/stream"), None);
        assert_eq!(parse_route("/v2/projects/a%2Fb/fs/dir"), None);
    }

    fn request(method: &str, path: &str, auth: Option<&str>) -> SimpleHttpRequest {
        let mut headers = std::collections::HashMap::new();
        if let Some(auth) = auth {
            headers.insert("authorization".to_string(), format!("Bearer {auth}"));
        }
        SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn only_the_root_credential_is_accepted() {
        let get = |auth| request("GET", "/v2/terminals", auth);
        assert!(!is_authorized(&get(None), None));
        assert!(!is_authorized(&get(Some("x")), None));
        assert!(!is_authorized(&get(Some("x")), Some("")));
        assert!(!is_authorized(&get(None), Some("root")));
        assert!(!is_authorized(&get(Some("wrong")), Some("root")));
        assert!(is_authorized(&get(Some("root")), Some("root")));
    }

    /// Serve one raw HTTP request through `dispatch`; the returned client
    /// socket reads the response.
    fn serve(
        raw: String,
        resolve: impl Fn(&Scope) -> WorkspaceResult<ResolvedRoot> + Send + 'static,
    ) -> (TcpStream, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
            dispatch(&mut stream, &request, resolve).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        std::io::Write::write_all(&mut client, raw.as_bytes()).unwrap();
        (client, server)
    }

    fn http(method: &str, path: &str, body: &str) -> String {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn json_response(
        raw: String,
        resolve: impl Fn(&Scope) -> WorkspaceResult<ResolvedRoot> + Send + 'static,
    ) -> (u16, Value) {
        let (mut client, server) = serve(raw, resolve);
        let mut text = String::new();
        std::io::Read::read_to_string(&mut client, &mut text).unwrap();
        server.join().unwrap();
        let status = text[9..12].parse().unwrap();
        let body = text.split_once("\r\n\r\n").unwrap().1;
        (status, serde_json::from_str(body).unwrap())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cutex-wsroute-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dispatch_serves_scoped_routes_and_root_errors() {
        let dir = temp_dir("dispatch");
        std::fs::write(dir.join("hello.txt"), "hi").unwrap();
        let root_dir = dir.clone();
        let resolver = move |scope: &Scope| ResolvedRoot::from_dir(scope.clone(), &root_dir);
        let (status, body) = json_response(
            http("GET", "/v2/sessions/s1/fs/dir?path=", ""),
            resolver.clone(),
        );
        assert_eq!(status, 200);
        assert_eq!(body["entries"][0]["name"], "hello.txt");
        let (status, body) = json_response(
            http("GET", "/v2/projects/p1/fs/file?path=hello.txt", ""),
            resolver.clone(),
        );
        assert_eq!(status, 200);
        assert_eq!(body["content"], "hi");
        let (status, body) = json_response(
            http("GET", "/v2/projects/p1/fs/file?path=..%2Fx", ""),
            resolver.clone(),
        );
        assert_eq!(status, 403);
        assert_eq!(body["error"]["code"], "path_outside_root");
        let (status, _) = json_response(http("POST", "/v2/projects/p1/fs/dir", ""), resolver);
        assert_eq!(status, 405);
        let (status, body) = json_response(
            http("GET", "/v2/projects/p2/git/status", ""),
            |_: &Scope| Err(WorkspaceError::new(409, "workspace_root_unset", "unset")),
        );
        assert_eq!(status, 409);
        assert_eq!(body["error"]["code"], "workspace_root_unset");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn terminal_lifecycle_over_http_with_sse_reattach() {
        use base64::Engine;
        use std::io::BufRead;
        let dir = temp_dir("term");
        let root_dir = dir.clone();
        let resolver = move |scope: &Scope| ResolvedRoot::from_dir(scope.clone(), &root_dir);
        let (status, created) = json_response(
            http(
                "POST",
                "/v2/sessions/s-http/terminals",
                r#"{"cols":80,"rows":24}"#,
            ),
            resolver.clone(),
        );
        assert_eq!(status, 200, "{created}");
        let id = created["terminalId"].as_str().unwrap().to_string();
        assert_eq!(created["scope"], "session");
        assert_eq!(created["rootId"], "s-http");
        let send_input = |data: &str| {
            let body = serde_json::to_string(&json!({ "data": data })).unwrap();
            let (status, _) = json_response(
                http("POST", &format!("/v2/terminals/{id}/input"), &body),
                resolver.clone(),
            );
            assert_eq!(status, 200);
        };

        let (client, stream_server) = serve(
            http("GET", &format!("/v2/terminals/{id}/stream"), ""),
            resolver.clone(),
        );
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        let mut reader = std::io::BufReader::new(client);
        let mut output = String::new();
        let mut events = Vec::new();
        let mut sent_marker = false;
        let mut current_event = String::new();
        let mut line = String::new();
        let mut last_id = 0u64;
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let trimmed = line.trim_end().to_string();
            line.clear();
            if let Some(event) = trimmed.strip_prefix("event: ") {
                current_event = event.to_string();
                events.push(current_event.clone());
                if event == "ready" && !cfg!(windows) {
                    sent_marker = true;
                    send_input("echo cutexhttp77\r");
                }
            } else if let Some(value) = trimmed.strip_prefix("id: ") {
                last_id = value.parse().unwrap();
            } else if let Some(data) = trimmed.strip_prefix("data: ") {
                if current_event != "output" {
                    continue;
                }
                let value: Value = serde_json::from_str(data).unwrap();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(value["data"].as_str().unwrap())
                    .unwrap();
                output.push_str(&String::from_utf8_lossy(&bytes));
                // ConPTY waits for a cursor-position report first.
                if !sent_marker && output.contains("\u{1b}[6n") {
                    sent_marker = true;
                    send_input("\u{1b}[1;1Recho cutexhttp77\r");
                }
                if output.matches("cutexhttp77").count() >= 2 {
                    break;
                }
            }
        }
        assert!(output.contains("cutexhttp77"), "{output:?}");
        assert_eq!(events[0], "ready");
        assert!(last_id > 0);

        // Reattach from the last offset, then kill: the stream ends with `exit`.
        drop(reader);
        stream_server.join().ok();
        let (client2, stream_server2) = serve(
            http(
                "GET",
                &format!("/v2/terminals/{id}/stream?since={last_id}"),
                "",
            ),
            resolver.clone(),
        );
        client2
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        let mut reader2 = std::io::BufReader::new(client2);
        let mut first = String::new();
        reader2.read_line(&mut first).unwrap();
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        let (status, listed) = json_response(http("GET", "/v2/terminals", ""), resolver.clone());
        assert_eq!(status, 200);
        assert!(listed["terminals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|terminal| terminal["terminalId"] == id.as_str()));
        let (status, killed) = json_response(
            http("DELETE", &format!("/v2/terminals/{id}"), ""),
            resolver.clone(),
        );
        assert_eq!(status, 200);
        assert_eq!(killed["killed"], true);
        let mut rest = String::new();
        let _ = std::io::Read::read_to_string(&mut reader2, &mut rest);
        assert!(rest.contains("event: exit"), "{rest}");
        stream_server2.join().ok();
        let (status, _) =
            json_response(http("DELETE", &format!("/v2/terminals/{id}"), ""), resolver);
        assert_eq!(status, 404);
        let _ = std::fs::remove_dir_all(dir);
    }
}
