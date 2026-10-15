//! Root-only HTTP boundary; persistence and lifecycle hooks live in crate::notes.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::notes::*;
use crate::notes::{invalid, validate_note_id, validate_revision};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::TcpStream;

pub const PATH: &str = "/v2/notes";
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteNote {
    expected_revision: u64,
}

fn project_sessions(scope: &NoteScope) -> NotesResult<Vec<String>> {
    let NoteScope::Project { id } = scope else {
        return Ok(Vec::new());
    };
    let snapshot = crate::agent_management::AgentManagementStore::open_default()?
        .snapshot()
        .map_err(|error| NotesError::Storage(error.into()))?;
    Ok(snapshot
        .agents
        .values()
        .filter(|agent| {
            crate::agent_management::current_project_id(&snapshot, agent)
                .is_some_and(|project| project.as_str() == id)
        })
        .map(|agent| agent.cutex_session_id.as_str().to_string())
        .collect())
}

pub fn is_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == PATH
        || path
            .strip_prefix("/v2/notes/")
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> NotesResult<T> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err(invalid("notes request body is too large"));
    }
    // For optional PATCH fields, null is not omission. Anchor alone may be null.
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| invalid(format!("JSON object body required: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("JSON object body required"))?;
    if object
        .iter()
        .any(|(key, value)| value.is_null() && key != "anchor")
    {
        return Err(invalid("null fields are not supported except anchor"));
    }
    // Deserialize from the original bytes so serde also rejects duplicate fields.
    serde_json::from_slice(body).map_err(|error| invalid(format!("invalid notes request: {error}")))
}

fn execute(request: &SimpleHttpRequest) -> NotesResult<Value> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    match (request.method.as_str(), path) {
        ("GET", PATH) => {
            let query = NotesQuery::parse(&request.path)?;
            let members = if query.pinned_only {
                Vec::new()
            } else {
                project_sessions(&query.scope)?
            };
            Ok(serde_json::to_value(
                NotesStore::open_default()?.list(&query, &members)?,
            )?)
        }
        ("POST", PATH) => {
            if request.path.contains('?') {
                return Err(invalid("POST notes does not accept query parameters"));
            }
            let input = parse_body::<CreateNote>(&request.body)?.validated()?;
            Ok(serde_json::to_value(
                NotesStore::open_default()?.create(input)?,
            )?)
        }
        ("GET" | "PATCH" | "DELETE", _) => {
            if request.path.contains('?') {
                return Err(invalid(
                    "note-by-id operations do not accept query parameters",
                ));
            }
            let id = path
                .strip_prefix("/v2/notes/")
                .ok_or_else(|| invalid("note id required"))?;
            validate_note_id(id)?;
            if request.method == "GET" {
                Ok(serde_json::to_value(NotesStore::open_default()?.get(id)?)?)
            } else if request.method == "PATCH" {
                let input = parse_body::<PatchNote>(&request.body)?.validated()?;
                Ok(serde_json::to_value(
                    NotesStore::open_default()?.patch(id, input)?,
                )?)
            } else {
                let input = parse_body::<DeleteNote>(&request.body)?;
                validate_revision(input.expected_revision)?;
                NotesStore::open_default()?.delete(id, input.expected_revision)?;
                Ok(json!({ "deleted": true }))
            }
        }
        _ => Err(invalid("unsupported notes operation")),
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let allowed = if path == PATH {
        ["GET", "POST"].contains(&request.method.as_str())
    } else {
        ["GET", "PATCH", "DELETE"].contains(&request.method.as_str())
    };
    if !allowed {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET or POST for /v2/notes and GET, PATCH or DELETE for /v2/notes/{id}",
            false,
            json!({}),
        );
    }
    match execute(request) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => {
            let (status, reason, code, message, retryable, details) = match error {
                NotesError::Invalid(message) => (
                    400,
                    "Bad Request",
                    "invalid_request",
                    message,
                    false,
                    json!({}),
                ),
                NotesError::NotFound => (
                    404,
                    "Not Found",
                    "note_not_found",
                    "note does not exist".into(),
                    false,
                    json!({}),
                ),
                NotesError::RevisionConflict { current_revision } => (
                    409,
                    "Conflict",
                    "note_revision_conflict",
                    "note changed; reload before applying edits".into(),
                    false,
                    json!({"currentRevision": current_revision}),
                ),
                NotesError::NonceConflict => (
                    409,
                    "Conflict",
                    "note_nonce_conflict",
                    "clientNonce was already used for different content".into(),
                    false,
                    json!({}),
                ),
                NotesError::Deleted => (
                    409,
                    "Conflict",
                    "note_deleted",
                    "the note created with this clientNonce was deleted".into(),
                    false,
                    json!({}),
                ),
                NotesError::Storage(error) => (
                    503,
                    "Service Unavailable",
                    "notes_unavailable",
                    "the notes store could not be accessed".into(),
                    true,
                    json!({"diagnostic": format!("{error:#}")}),
                ),
            };
            super::server::write_v2_error(
                stream, status, reason, code, &message, retryable, details,
            )
        }
    }
}
