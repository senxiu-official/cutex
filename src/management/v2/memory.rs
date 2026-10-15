//! Management v2 project memory routes (root credential only).
//!
//! - `GET    /v2/projects/{projectId}/memory`
//! - `GET    /v2/projects/{projectId}/memory/{file}`
//! - `PUT    /v2/projects/{projectId}/memory/{file}`  body `{content, expectedRevision?, updateIndex?}`
//! - `DELETE /v2/projects/{projectId}/memory/{file}?expectedRevision=`
//! - `GET    /v2/sessions/{cutexSessionId}/memory-scope`
//!
//! Mutations emit `cutex/memory/changed` on every member session's stream.

use std::net::TcpStream;
use std::path::PathBuf;

use serde_json::json;
use serde_json::Value;

use crate::agent_management::ProjectId;
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::memory::ExpectedRevision;
use crate::memory::MemoryError;
use crate::memory::MemoryFileName;
use crate::memory::MemoryStore;

use super::repository::EventRepository;
use super::server::write_v2_error;

pub const MEMORY_CHANGED_EVENT: &str = "cutex/memory/changed";

#[derive(Clone, Debug, PartialEq, Eq)]
enum MemoryRoute {
    Collection(String),
    File(String, String),
    SessionScope(String),
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn parse_route(path: &str) -> Option<MemoryRoute> {
    if let Some(rest) = path.strip_prefix("/v2/projects/") {
        let mut parts = rest.split('/');
        let project = percent_decode(parts.next()?)?;
        if parts.next()? != "memory" {
            return None;
        }
        return match (parts.next(), parts.next()) {
            (None, _) => Some(MemoryRoute::Collection(project)),
            (Some(file), None) => Some(MemoryRoute::File(project, percent_decode(file)?)),
            _ => None,
        };
    }
    let rest = path.strip_prefix("/v2/sessions/")?;
    let session = rest.strip_suffix("/memory-scope")?;
    if session.is_empty() || session.contains('/') {
        return None;
    }
    Some(MemoryRoute::SessionScope(percent_decode(session)?))
}

/// True for every path this module owns.
pub fn is_memory_path(path: &str) -> bool {
    parse_route(path).is_some()
}

/// Project/session facts the routes need; abstracted for tests.
pub trait MemoryScopeSource {
    fn project_known(&self, project_id: &ProjectId) -> bool;
    /// `None` = unknown session; `Some(None)` = independent Agent.
    fn session_project(&self, cutex_session_id: &str) -> Option<Option<ProjectId>>;
    fn project_members(&self, project_id: &ProjectId) -> Vec<String>;
    fn memory_dir(&self, project_id: &ProjectId) -> anyhow::Result<PathBuf>;
}

struct DefaultScopeSource {
    snapshot: crate::agent_management::AgentManagementSnapshot,
    session_ids: std::collections::HashSet<String>,
}

impl MemoryScopeSource for DefaultScopeSource {
    fn project_known(&self, project_id: &ProjectId) -> bool {
        crate::memory::project_is_known(&self.snapshot, project_id)
    }

    fn session_project(&self, cutex_session_id: &str) -> Option<Option<ProjectId>> {
        let project = crate::memory::project_for_session(&self.snapshot, cutex_session_id);
        (project.is_some() || self.session_ids.contains(cutex_session_id)).then_some(project)
    }

    fn project_members(&self, project_id: &ProjectId) -> Vec<String> {
        crate::memory::project_member_session_ids(&self.snapshot, project_id)
    }

    fn memory_dir(&self, project_id: &ProjectId) -> anyhow::Result<PathBuf> {
        crate::memory::project_memory_dir(project_id)
    }
}

#[derive(Debug, PartialEq)]
pub struct MemoryRouteResponse {
    pub status: u16,
    pub body: Value,
    /// `(projectId, file, change, revision)` for a successful mutation.
    pub change: Option<(String, String, &'static str, Option<String>)>,
}

#[derive(Debug, PartialEq)]
pub struct MemoryRouteError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

fn error(status: u16, code: &'static str, message: impl Into<String>) -> MemoryRouteError {
    MemoryRouteError {
        status,
        code,
        message: message.into(),
        details: json!({}),
    }
}

fn memory_error(error_value: MemoryError) -> MemoryRouteError {
    match error_value {
        MemoryError::InvalidFileName => error(400, "invalid_memory_file", error_value.to_string()),
        MemoryError::InvalidContent(message) => error(400, "invalid_memory_content", message),
        MemoryError::NotFound => error(404, "memory_not_found", "memory file not found"),
        MemoryError::RevisionConflict { current } => MemoryRouteError {
            status: 409,
            code: "revision_conflict",
            message: "memory file revision does not match expectedRevision".to_string(),
            details: json!({ "currentRevision": current }),
        },
        MemoryError::Io(error_value) => MemoryRouteError {
            status: 503,
            code: "memory_store_unavailable",
            message: format!("{error_value:#}"),
            details: json!({}),
        },
    }
}

fn parse_expected_body(
    object: &serde_json::Map<String, Value>,
) -> Result<ExpectedRevision, MemoryRouteError> {
    match object.get("expectedRevision") {
        None => Ok(ExpectedRevision::Any),
        Some(Value::Null) => Ok(ExpectedRevision::Absent),
        Some(Value::String(revision)) if !revision.is_empty() => {
            Ok(ExpectedRevision::Exact(revision.clone()))
        }
        Some(_) => Err(error(
            400,
            "invalid_request",
            "expectedRevision must be a non-empty string, or null for create-only",
        )),
    }
}

fn parse_expected_query(path: &str) -> Result<ExpectedRevision, MemoryRouteError> {
    match crate::http::query::query_value(path, "expectedRevision") {
        None => Ok(ExpectedRevision::Any),
        Some(raw) => match percent_decode(&raw.replace('+', " ")) {
            Some(revision) if !revision.is_empty() => Ok(ExpectedRevision::Exact(revision)),
            _ => Err(error(400, "invalid_request", "invalid expectedRevision")),
        },
    }
}

fn require_project(
    source: &dyn MemoryScopeSource,
    project: &str,
) -> Result<(ProjectId, MemoryStore), MemoryRouteError> {
    let project_id = ProjectId::new(project.to_string())
        .map_err(|_| error(400, "invalid_project", "invalid project id"))?;
    if !source.project_known(&project_id) {
        return Err(error(404, "project_not_found", "project not found"));
    }
    let dir = source
        .memory_dir(&project_id)
        .map_err(|error_value| memory_error(MemoryError::Io(error_value)))?;
    Ok((project_id, MemoryStore::new(dir)))
}

fn dir_string(store: &MemoryStore) -> String {
    store.dir().display().to_string()
}

/// Pure route execution (no socket, no events).
pub fn execute_memory_route(
    request: &SimpleHttpRequest,
    source: &dyn MemoryScopeSource,
) -> Result<MemoryRouteResponse, MemoryRouteError> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let route = parse_route(path).ok_or_else(|| error(404, "not_found", "unknown memory route"))?;
    let ok = |body: Value| MemoryRouteResponse {
        status: 200,
        body,
        change: None,
    };
    match (request.method.as_str(), route) {
        ("GET", MemoryRoute::SessionScope(session)) => {
            let project = source
                .session_project(&session)
                .ok_or_else(|| error(404, "session_not_found", "cutex session not found"))?;
            let dir = match &project {
                Some(project_id) => Some(
                    source
                        .memory_dir(project_id)
                        .map_err(|error_value| memory_error(MemoryError::Io(error_value)))?
                        .display()
                        .to_string(),
                ),
                None => None,
            };
            Ok(ok(json!({
                "contractVersion": 2,
                "cutexSessionId": session,
                "projectId": project.as_ref().map(ProjectId::as_str),
                "dir": dir,
            })))
        }
        ("GET", MemoryRoute::Collection(project)) => {
            let (project_id, store) = require_project(source, &project)?;
            let entries = store.list().map_err(memory_error)?;
            let index = store.read_index().map_err(memory_error)?;
            Ok(ok(json!({
                "contractVersion": 2,
                "projectId": project_id.as_str(),
                "dir": dir_string(&store),
                "index": index.as_ref().map(|document| document.content.clone()),
                "indexRevision": index.as_ref().map(|document| document.revision.clone()),
                "entries": entries,
            })))
        }
        ("GET", MemoryRoute::File(project, file)) => {
            let (project_id, store) = require_project(source, &project)?;
            let file = MemoryFileName::parse(&file).map_err(memory_error)?;
            let document = store.read(&file).map_err(memory_error)?;
            Ok(ok(json!({
                "contractVersion": 2,
                "projectId": project_id.as_str(),
                "file": document.file,
                "content": document.content,
                "frontmatter": document.frontmatter,
                "revision": document.revision,
                "bytes": document.bytes,
                "updatedAt": document.updated_at,
            })))
        }
        ("PUT", MemoryRoute::File(project, file)) => {
            let (project_id, store) = require_project(source, &project)?;
            let file = MemoryFileName::parse(&file).map_err(memory_error)?;
            let body: Value = serde_json::from_slice(&request.body)
                .map_err(|_| error(400, "invalid_request", "JSON body required"))?;
            let object = body
                .as_object()
                .ok_or_else(|| error(400, "invalid_request", "JSON object body required"))?;
            if object
                .keys()
                .any(|key| !matches!(key.as_str(), "content" | "expectedRevision" | "updateIndex"))
            {
                return Err(error(
                    400,
                    "invalid_request",
                    "body accepts only content, expectedRevision, updateIndex",
                ));
            }
            let content = object
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| error(400, "invalid_request", "content must be a string"))?;
            let update_index = match object.get("updateIndex") {
                None => true,
                Some(Value::Bool(value)) => *value,
                Some(_) => {
                    return Err(error(
                        400,
                        "invalid_request",
                        "updateIndex must be a boolean",
                    ))
                }
            };
            let expected = parse_expected_body(object)?;
            let outcome = store
                .write(&file, content, &expected, update_index)
                .map_err(memory_error)?;
            let change = outcome.change.as_str();
            Ok(MemoryRouteResponse {
                status: 200,
                body: json!({
                    "contractVersion": 2,
                    "projectId": project_id.as_str(),
                    "file": outcome.document.file,
                    "change": change,
                    "revision": outcome.document.revision,
                    "bytes": outcome.document.bytes,
                    "updatedAt": outcome.document.updated_at,
                    "frontmatter": outcome.document.frontmatter,
                    "indexUpdated": outcome.index_updated,
                }),
                change: Some((
                    project_id.as_str().to_string(),
                    file.as_str().to_string(),
                    change,
                    Some(outcome.document.revision.clone()),
                )),
            })
        }
        ("DELETE", MemoryRoute::File(project, file)) => {
            let (project_id, store) = require_project(source, &project)?;
            let file = MemoryFileName::parse(&file).map_err(memory_error)?;
            let expected = parse_expected_query(&request.path)?;
            let outcome = store.delete(&file, &expected).map_err(memory_error)?;
            Ok(MemoryRouteResponse {
                status: 200,
                body: json!({
                    "contractVersion": 2,
                    "projectId": project_id.as_str(),
                    "file": file.as_str(),
                    "change": "deleted",
                    "indexUpdated": outcome.index_updated,
                }),
                change: Some((
                    project_id.as_str().to_string(),
                    file.as_str().to_string(),
                    "deleted",
                    None,
                )),
            })
        }
        _ => Err(error(
            405,
            "method_not_allowed",
            "method not allowed for this memory route",
        )),
    }
}

/// Append `cutex/memory/changed` to each member session's stream.
pub fn emit_memory_changed(
    repository: &EventRepository,
    members: &[String],
    project_id: &str,
    file: &str,
    change: &str,
    revision: Option<&str>,
) -> anyhow::Result<()> {
    let params = json!({
        "projectId": project_id,
        "file": file,
        "change": change,
        "revision": revision,
        "source": "management",
        "changedAt": chrono::Utc::now().to_rfc3339(),
    });
    for member in members {
        super::server::append_cutex_event(
            repository,
            member,
            super::model::EventCorrelation::default(),
            MEMORY_CHANGED_EVENT,
            params.clone(),
        )?;
    }
    Ok(())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Service Unavailable",
    }
}

fn load_default_source() -> anyhow::Result<DefaultScopeSource> {
    let snapshot = crate::memory::load_agent_management_snapshot()?;
    let session_ids = crate::session::store::load_cutex_session_store()?
        .sessions
        .values()
        .map(|record| record.cutex_session_id.clone())
        .collect();
    Ok(DefaultScopeSource {
        snapshot,
        session_ids,
    })
}

/// Socket entry point; the caller has already verified the root credential.
pub fn handle_memory_request(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    repository: &EventRepository,
) -> anyhow::Result<()> {
    let source = match load_default_source() {
        Ok(source) => source,
        Err(error_value) => {
            return write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "memory_store_unavailable",
                &format!("project state unavailable: {error_value:#}"),
                true,
                json!({}),
            )
        }
    };
    match execute_memory_route(request, &source) {
        Ok(response) => {
            if let Some((project, file, change, revision)) = &response.change {
                if let Ok(project_id) = ProjectId::new(project.clone()) {
                    let members = source.project_members(&project_id);
                    if let Err(error_value) = emit_memory_changed(
                        repository,
                        &members,
                        project,
                        file,
                        change,
                        revision.as_deref(),
                    ) {
                        eprintln!("failed to emit {MEMORY_CHANGED_EVENT}: {error_value:#}");
                    }
                }
            }
            write_json_response(
                stream,
                response.status,
                reason(response.status),
                &response.body,
            )
        }
        Err(route_error) => write_v2_error(
            stream,
            route_error.status,
            reason(route_error.status),
            route_error.code,
            &route_error.message,
            route_error.status == 503,
            route_error.details,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct TestSource {
        dir: PathBuf,
    }

    impl MemoryScopeSource for TestSource {
        fn project_known(&self, project_id: &ProjectId) -> bool {
            project_id.as_str() == "proj:a"
        }
        fn session_project(&self, cutex_session_id: &str) -> Option<Option<ProjectId>> {
            match cutex_session_id {
                "cutex.member" => Some(Some(ProjectId::new("proj:a").unwrap())),
                "cutex.solo" => Some(None),
                _ => None,
            }
        }
        fn project_members(&self, _project_id: &ProjectId) -> Vec<String> {
            vec!["cutex.member".to_string()]
        }
        fn memory_dir(&self, _project_id: &ProjectId) -> anyhow::Result<PathBuf> {
            Ok(self.dir.clone())
        }
    }

    fn request(method: &str, path: &str, body: Value) -> SimpleHttpRequest {
        SimpleHttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers: HashMap::new(),
            body: if body.is_null() {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
        }
    }

    const CONTENT: &str = "---\nname: user-role\ndescription: The user is the repo owner\nmetadata:\n  type: user\n---\n\nOwner of cutex.\n";

    #[test]
    fn routes_parse_and_claim_only_memory_paths() {
        assert!(is_memory_path("/v2/projects/proj%3Aa/memory"));
        assert!(is_memory_path("/v2/projects/p/memory/x.md"));
        assert!(is_memory_path("/v2/sessions/cutex.1/memory-scope"));
        assert!(!is_memory_path("/v2/projects/p/tasks"));
        assert!(!is_memory_path("/v2/projects/p/memory/a/b.md"));
        assert!(!is_memory_path("/v2/sessions/cutex.1"));
        assert!(!is_memory_path("/v2/sessions/a/b/memory-scope"));
        assert_eq!(
            parse_route("/v2/projects/proj%3Aa/memory/x.md"),
            Some(MemoryRoute::File("proj:a".into(), "x.md".into()))
        );
    }

    #[test]
    fn memory_routes_crud_with_revisions_scope_and_events() {
        let root = std::env::temp_dir().join(format!("cutex-v2-memory-{}", uuid::Uuid::new_v4()));
        let source = TestSource {
            dir: root.join("memory"),
        };
        let base = "/v2/projects/proj%3Aa/memory";

        let empty = execute_memory_route(&request("GET", base, Value::Null), &source).unwrap();
        assert_eq!(empty.body["projectId"], "proj:a");
        assert_eq!(empty.body["index"], Value::Null);
        assert_eq!(empty.body["entries"], json!([]));
        assert_eq!(empty.body["dir"], source.dir.display().to_string());

        let missing = execute_memory_route(
            &request("GET", "/v2/projects/other/memory", Value::Null),
            &source,
        )
        .unwrap_err();
        assert_eq!((missing.status, missing.code), (404, "project_not_found"));

        let put = execute_memory_route(
            &request(
                "PUT",
                &format!("{base}/user-role.md"),
                json!({"content": CONTENT, "expectedRevision": null}),
            ),
            &source,
        )
        .unwrap();
        assert_eq!(put.body["change"], "created");
        assert_eq!(put.body["indexUpdated"], true);
        assert_eq!(put.body["frontmatter"]["type"], "user");
        let revision = put.body["revision"].as_str().unwrap().to_string();
        assert!(revision.starts_with("sha256:"));
        assert_eq!(
            put.change,
            Some((
                "proj:a".into(),
                "user-role.md".into(),
                "created",
                Some(revision.clone())
            ))
        );

        let again = execute_memory_route(
            &request(
                "PUT",
                &format!("{base}/user-role.md"),
                json!({"content": CONTENT, "expectedRevision": null}),
            ),
            &source,
        )
        .unwrap_err();
        assert_eq!((again.status, again.code), (409, "revision_conflict"));
        assert_eq!(again.details["currentRevision"], revision);

        for (body, code) in [
            (json!({"content": "nope"}), "invalid_memory_content"),
            (json!({"content": CONTENT, "extra": 1}), "invalid_request"),
            (
                json!({"content": CONTENT, "expectedRevision": 3}),
                "invalid_request",
            ),
        ] {
            let rejected = execute_memory_route(
                &request("PUT", &format!("{base}/user-role.md"), body),
                &source,
            )
            .unwrap_err();
            assert_eq!((rejected.status, rejected.code), (400, code));
        }
        let traversal = execute_memory_route(
            &request("GET", &format!("{base}/..%2Fsecret.md"), Value::Null),
            &source,
        )
        .unwrap_err();
        assert_eq!(traversal.code, "invalid_memory_file");

        let list = execute_memory_route(&request("GET", base, Value::Null), &source).unwrap();
        let entry = &list.body["entries"][0];
        assert_eq!(entry["file"], "user-role.md");
        assert_eq!(entry["name"], "user-role");
        assert_eq!(entry["description"], "The user is the repo owner");
        assert_eq!(entry["type"], "user");
        assert_eq!(entry["bytes"], CONTENT.len());
        assert!(entry["updatedAt"].is_string());
        assert!(list.body["index"]
            .as_str()
            .unwrap()
            .contains("](user-role.md)"));

        let read = execute_memory_route(
            &request("GET", &format!("{base}/user-role.md"), Value::Null),
            &source,
        )
        .unwrap();
        assert_eq!(read.body["content"], CONTENT);
        assert_eq!(read.body["revision"], revision);
        assert_eq!(read.body["frontmatter"]["name"], "user-role");
        let index = execute_memory_route(
            &request("GET", &format!("{base}/MEMORY.md"), Value::Null),
            &source,
        )
        .unwrap();
        assert_eq!(index.body["frontmatter"], Value::Null);

        let stale = execute_memory_route(
            &request(
                "DELETE",
                &format!("{base}/user-role.md?expectedRevision=sha256%3A00"),
                Value::Null,
            ),
            &source,
        )
        .unwrap_err();
        assert_eq!(stale.status, 409);
        let encoded = revision.replace(':', "%3A");
        let deleted = execute_memory_route(
            &request(
                "DELETE",
                &format!("{base}/user-role.md?expectedRevision={encoded}"),
                Value::Null,
            ),
            &source,
        )
        .unwrap();
        assert_eq!(deleted.body["change"], "deleted");
        assert_eq!(deleted.body["indexUpdated"], true);
        let gone = execute_memory_route(
            &request("GET", &format!("{base}/user-role.md"), Value::Null),
            &source,
        )
        .unwrap_err();
        assert_eq!((gone.status, gone.code), (404, "memory_not_found"));

        let scope = execute_memory_route(
            &request("GET", "/v2/sessions/cutex.member/memory-scope", Value::Null),
            &source,
        )
        .unwrap();
        assert_eq!(scope.body["projectId"], "proj:a");
        assert_eq!(scope.body["dir"], source.dir.display().to_string());
        let solo = execute_memory_route(
            &request("GET", "/v2/sessions/cutex.solo/memory-scope", Value::Null),
            &source,
        )
        .unwrap();
        assert_eq!(
            (solo.body["projectId"].clone(), solo.body["dir"].clone()),
            (Value::Null, Value::Null)
        );
        let unknown = execute_memory_route(
            &request("GET", "/v2/sessions/cutex.x/memory-scope", Value::Null),
            &source,
        )
        .unwrap_err();
        assert_eq!(unknown.code, "session_not_found");

        let repository_root = root.join("events");
        let repository =
            EventRepository::open(&repository_root, crate::platform::host::current_host_name())
                .unwrap();
        emit_memory_changed(
            &repository,
            &source.project_members(&ProjectId::new("proj:a").unwrap()),
            "proj:a",
            "user-role.md",
            "created",
            Some(&revision),
        )
        .unwrap();
        let page = repository
            .page(super::super::repository::ReplayQuery::default())
            .unwrap();
        let event = page
            .events
            .iter()
            .find(|event| {
                event
                    .cutex
                    .as_ref()
                    .is_some_and(|cutex| cutex.method == MEMORY_CHANGED_EVENT)
            })
            .expect("memory changed event");
        assert_eq!(event.cutex_session_id, "cutex.member");
        let params = &event.cutex.as_ref().unwrap().params;
        assert_eq!(params["projectId"], "proj:a");
        assert_eq!(params["change"], "created");
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": MEMORY_CHANGED_EVENT,
            "params": params,
        }))
        .expect("event matches the open cutex event contract");
        drop(repository);
        let _ = std::fs::remove_dir_all(root);
    }
}
