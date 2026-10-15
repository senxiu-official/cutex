//! Management v2 artifact store routes (capability `artifacts`; cutex-desktop
//! `docs/research/artifacts-api-v1.md`). Root credential only (incl. client
//! / device tokens); the owner reads every scope.
//!
//! - `GET /v2/artifact-store?project|session|forSession&q&kind&producer&archived&limit&cursor`
//!   → `{scope, items, nextCursor}` (newest `updatedAt` first; `scope: null`
//!   when no scope parameter is given: every scope).
//! - `GET /v2/artifact-store/{artifactId}` → `{artifact, versions}`.
//! - `GET /v2/artifact-store/{artifactId}/versions/{n|latest}/content?offset&maxBytes`.
//! - `GET /v2/artifact-store/{artifactId}/diff?from&to`.
//! - `POST /v2/artifact-store/resolve {refs}`.
//! - `POST /v2/artifact-store {requestId, project?|session?, path, name?, kind?, note?}` → 201.
//! - `PATCH /v2/artifact-store/{artifactId} {expectedRevision, name?, kind?, starred?, archived?}`.
//! - `DELETE /v2/artifact-store/{artifactId}`.
//! - `GET /v2/artifact-store/usage`.
//!
//! `/v2/artifacts/{art-…}` (delegation attachments, `asks.rs`) is a
//! different resource.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::artifacts::host::{ArtifactHost, ManagementHost};
use crate::artifacts::publish::{PublishError, PublishRequest};
use crate::artifacts::store::{Archived, ArtifactStore, ListFilter, Patch, StoreError};
use crate::artifacts::{Producer, Scope, Which};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const PATH: &str = "/v2/artifact-store";
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const DEFAULT_MAX_BYTES: usize = 256 * 1024;
const MAX_MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESOLVE: usize = 200;
/// Larger text versions are not diffed (`diff: null`, `truncated: true`).
const MAX_DIFF_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const OWNER: &str = "owner";

/// `schema/export/artifacts.json` (schema export index entry).
pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "artifacts",
    title: "Artifact store",
    routes: &[
        "GET /v2/artifact-store",
        "POST /v2/artifact-store",
        "GET /v2/artifact-store/{artifactId}",
        "PATCH /v2/artifact-store/{artifactId}",
        "DELETE /v2/artifact-store/{artifactId}",
        "GET /v2/artifact-store/{artifactId}/versions/{n|latest}/content",
        "GET /v2/artifact-store/{artifactId}/diff",
        "POST /v2/artifact-store/resolve",
        "GET /v2/artifact-store/usage",
        "host events cutex/artifact/published, cutex/artifact/updated",
    ],
    text: include_str!("schema/export/artifacts.json"),
};

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/artifact-store/")
}

#[derive(Debug)]
pub struct RouteError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

fn error(status: u16, code: &'static str, message: impl Into<String>) -> RouteError {
    RouteError { status, code, message: message.into(), details: json!({}) }
}

fn invalid(message: impl Into<String>) -> RouteError {
    error(400, "invalid_request", message)
}

fn not_found() -> RouteError {
    error(404, "artifact_not_found", "no such artifact")
}

fn unavailable(detail: impl std::fmt::Display) -> RouteError {
    let mut route_error = error(503, "persistence_unavailable", format!("the artifact store is unavailable: {detail}"));
    route_error.details = json!({});
    route_error
}

fn from_store(store_error: StoreError) -> RouteError {
    match store_error {
        StoreError::NotFound => not_found(),
        StoreError::RevisionConflict { revision } => RouteError {
            status: 409,
            code: "artifact_revision_conflict",
            message: "the artifact changed; reload it and retry".into(),
            details: json!({"revision": revision}),
        },
        StoreError::NameConflict => error(409, "artifact_name_conflict", "another artifact in this scope has that name"),
        StoreError::QuotaFull { used, quota } => quota_error(used, quota),
        StoreError::Invalid(message) => invalid(message),
        StoreError::Store(store_error) => unavailable(format!("{store_error:#}")),
    }
}

fn quota_error(used: u64, quota: u64) -> RouteError {
    RouteError {
        status: 507,
        code: "artifact_quota_exceeded",
        message: "the artifact store is full: delete or archive artifacts, or raise artifacts.quotaBytes".into(),
        details: json!({"usedBytes": used, "quotaBytes": quota}),
    }
}

fn query_params(query: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes()).map(|(key, value)| (key.into_owned(), value.into_owned())).collect()
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, RouteError> {
    serde_json::from_slice(body).map_err(|parse_error| invalid(format!("request body: {parse_error}")))
}

fn non_empty(params: &HashMap<String, String>, key: &str) -> Option<String> {
    params.get(key).map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let (path, query) = request.path.split_once('?').unwrap_or((request.path.as_str(), ""));
    match route(&ManagementHost, &request.method, path, query, &request.body, Utc::now()) {
        Ok((status, body)) => write_json_response(stream, status, super::server::http_reason_for(status), &body),
        Err(route_error) => super::server::write_v2_error(
            stream,
            route_error.status,
            super::server::http_reason_for(route_error.status),
            route_error.code,
            &route_error.message,
            route_error.status == 503,
            route_error.details,
        ),
    }
}

/// One request (testable without a socket): `(status, body)`.
pub fn route(host: &dyn ArtifactHost, method: &str, path: &str, query: &str, body: &[u8], now: DateTime<Utc>) -> Result<(u16, Value), RouteError> {
    let rest = path.strip_prefix(PATH).ok_or_else(|| error(404, "route_not_found", "no such artifact store route"))?;
    let segments: Vec<&str> = rest.split('/').filter(|segment| !segment.is_empty()).collect();
    let store = || host.store().map_err(|store_error| unavailable(format!("{store_error:#}")));
    let params = query_params(query);
    match (method, segments.as_slice()) {
        ("GET", []) => list(host, &store()?, &params).map(|body| (200, body)),
        ("POST", []) => publish(host, &store()?, body, now).map(|body| (201, body)),
        ("GET", ["usage"]) => usage(host, &store()?).map(|body| (200, body)),
        ("POST", ["resolve"]) => resolve(&store()?, body).map(|body| (200, body)),
        (_, ["usage"] | ["resolve"]) => Err(error(405, "method_not_allowed", "method not allowed")),
        (_, [id, ..]) if !crate::artifacts::is_artifact_id(id) => Err(not_found()),
        ("GET", [id]) => detail(&store()?, id).map(|body| (200, body)),
        ("PATCH", [id]) => patch(host, &store()?, id, body, now).map(|body| (200, body)),
        ("DELETE", [id]) => delete(host, &store()?, id).map(|body| (200, body)),
        ("GET", [id, "versions", which, "content"]) => {
            let which = Which::parse(which).ok_or_else(|| invalid("version: a number from 1 or latest"))?;
            content(&store()?, id, which, &params).map(|body| (200, body))
        }
        ("GET", [id, "diff"]) => diff(&store()?, id, &params).map(|body| (200, body)),
        ("GET" | "POST" | "PATCH" | "DELETE", _) => Err(error(404, "route_not_found", "no such artifact store route")),
        _ => Err(error(405, "method_not_allowed", "method not allowed")),
    }
}

fn list(host: &dyn ArtifactHost, store: &ArtifactStore, params: &HashMap<String, String>) -> Result<Value, RouteError> {
    let scopes: Vec<Scope> = [
        non_empty(params, "project").map(Scope::project),
        non_empty(params, "session").map(Scope::session),
        non_empty(params, "forSession").map(|session| {
            let project = host.session_project(&session);
            Scope::of_session(&session, project.as_deref())
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    if scopes.len() > 1 {
        return Err(invalid("give at most one of project, session and forSession"));
    }
    let scope = scopes.into_iter().next();
    let kind = non_empty(params, "kind");
    if kind.as_deref().is_some_and(|kind| !crate::artifacts::is_kind(kind)) {
        return Err(invalid(format!("kind: one of {}", crate::artifacts::KINDS.join(", "))));
    }
    let archived = match params.get("archived").map(String::as_str) {
        None | Some("false") => Archived::No,
        Some("true") => Archived::Yes,
        Some("all") => Archived::All,
        Some(_) => return Err(invalid("archived: false, true or all")),
    };
    let limit = match params.get("limit") {
        None => DEFAULT_LIMIT,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or_else(|| invalid(format!("limit: 1-{MAX_LIMIT}")))?,
    };
    let filter = ListFilter {
        scopes: scope.clone().map(|scope| vec![scope]),
        q: non_empty(params, "q"),
        kind,
        producer: non_empty(params, "producer"),
        archived,
        limit,
        cursor: non_empty(params, "cursor"),
    };
    let (items, next) = store.list(&filter).map_err(from_store)?;
    Ok(json!({"scope": scope, "items": items, "nextCursor": next}))
}

fn detail(store: &ArtifactStore, id: &str) -> Result<Value, RouteError> {
    let artifact = store.get(id).map_err(unavailable_any)?.ok_or_else(not_found)?;
    let versions = store.versions(id).map_err(unavailable_any)?;
    Ok(json!({"artifact": artifact, "versions": versions}))
}

fn unavailable_any(store_error: anyhow::Error) -> RouteError {
    unavailable(format!("{store_error:#}"))
}

fn content(store: &ArtifactStore, id: &str, which: Which, params: &HashMap<String, String>) -> Result<Value, RouteError> {
    let offset = match params.get("offset") {
        None => 0,
        Some(value) => value.parse::<u64>().map_err(|_| invalid("offset: a byte offset"))?,
    };
    let max_bytes = match params.get("maxBytes") {
        None => DEFAULT_MAX_BYTES,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|max| (1..=MAX_MAX_BYTES).contains(max))
            .ok_or_else(|| invalid(format!("maxBytes: 1-{MAX_MAX_BYTES}")))?,
    };
    let (_, version) = store.version(id, which).map_err(unavailable_any)?.ok_or_else(|| error(404, "artifact_not_found", "no such artifact version"))?;
    let located = crate::artifacts::read::locate(store, &version);
    let mut value = json!({
        "version": version.version,
        "encoding": "utf8",
        "content": "",
        "offset": offset,
        "totalBytes": version.bytes,
        "truncated": false,
        "mediaType": version.media_type,
        "sha256": version.sha256,
        "externalChanged": located.external_changed,
    });
    let Some(path) = located.path else {
        if version.storage == "blob" {
            return Err(unavailable("the stored content of this version is missing"));
        }
        return Ok(value);
    };
    let (bytes, total) = crate::artifacts::read::window(&path, offset, max_bytes).map_err(|read_error| unavailable(read_error))?;
    let more = offset + (bytes.len() as u64) < total;
    value["totalBytes"] = json!(total);
    let text = crate::artifacts::paths::is_text_media(&version.media_type)
        .then(|| crate::artifacts::read::text_of(&bytes, more))
        .flatten();
    match text {
        Some(text) => {
            value["truncated"] = json!(offset + (text.len() as u64) < total);
            value["content"] = json!(text);
        }
        None => {
            value["encoding"] = json!("base64");
            value["content"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
            value["truncated"] = json!(more);
        }
    }
    Ok(value)
}

fn diff(store: &ArtifactStore, id: &str, params: &HashMap<String, String>) -> Result<Value, RouteError> {
    let number = |key: &str| -> Result<u64, RouteError> {
        params
            .get(key)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|number| *number >= 1)
            .ok_or_else(|| invalid(format!("{key}: a version number")))
    };
    let (from, to) = (number("from")?, number("to")?);
    let version = |number: u64| -> Result<crate::artifacts::ArtifactVersion, RouteError> {
        store
            .version(id, Which::Version(number))
            .map_err(unavailable_any)?
            .map(|(_, version)| version)
            .ok_or_else(|| error(404, "artifact_not_found", format!("no version {number} of this artifact")))
    };
    let (old, new) = (version(from)?, version(to)?);
    let name = store.get(id).map_err(unavailable_any)?.map(|artifact| artifact.name).unwrap_or_default();
    let mut value = json!({"from": from, "to": to, "binary": false, "diff": null, "truncated": false});
    if old.storage != "blob" || new.storage != "blob" || old.bytes > MAX_DIFF_INPUT_BYTES || new.bytes > MAX_DIFF_INPUT_BYTES {
        value["truncated"] = json!(true);
        return Ok(value);
    }
    let text = |version: &crate::artifacts::ArtifactVersion| -> Result<Option<String>, RouteError> {
        if !crate::artifacts::paths::is_text_media(&version.media_type) {
            return Ok(None);
        }
        let sha = version.sha256.as_deref().unwrap_or_default();
        let bytes = store.read_blob(sha).map_err(|read_error| unavailable(read_error))?;
        Ok(crate::artifacts::read::text_of(&bytes, false))
    };
    match (text(&old)?, text(&new)?) {
        (Some(old_text), Some(new_text)) => {
            let (diff, truncated) =
                crate::artifacts::diff::unified(&old_text, &new_text, &format!("{name}@v{from}"), &format!("{name}@v{to}"));
            value["diff"] = json!(diff);
            value["truncated"] = json!(truncated);
        }
        _ => value["binary"] = json!(true),
    }
    Ok(value)
}

fn resolve(store: &ArtifactStore, body: &[u8]) -> Result<Value, RouteError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        refs: Vec<String>,
    }
    let body: Body = parse_body(body)?;
    if body.refs.len() > MAX_RESOLVE {
        return Err(invalid(format!("refs: at most {MAX_RESOLVE}")));
    }
    let mut items = Vec::new();
    for reference in body.refs {
        let found = match crate::artifacts::parse_ref(&reference) {
            Some((id, which)) => store.version(&id, which).map_err(unavailable_any)?,
            None => None,
        };
        items.push(match found {
            Some((artifact, version)) => json!({
                "ref": reference,
                "ok": true,
                "artifactId": artifact.artifact_id,
                "name": artifact.name,
                "kind": artifact.kind,
                "version": version.version,
                "latestVersion": artifact.latest_version,
                "mediaType": version.media_type,
                "archived": artifact.archived_at.is_some(),
            }),
            None => json!({"ref": reference, "ok": false, "error": "artifact_not_found"}),
        });
    }
    Ok(json!({"items": items}))
}

fn publish(host: &dyn ArtifactHost, store: &ArtifactStore, body: &[u8], now: DateTime<Utc>) -> Result<Value, RouteError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct Body {
        request_id: String,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        session: Option<String>,
        path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        note: Option<String>,
    }
    let raw: Value = parse_body(body)?;
    let body: Body = parse_body(body)?;
    let request_id = body.request_id.trim();
    if request_id.is_empty() || request_id.chars().count() > 128 || request_id.chars().any(char::is_control) {
        return Err(invalid("requestId: 1-128 characters"));
    }
    let (scope, base, roots): (Scope, PathBuf, Vec<PathBuf>) = match (body.project.as_deref(), body.session.as_deref()) {
        (Some(project), None) => {
            let root = host
                .project_root(project)
                .ok_or_else(|| invalid("project: no such project, or it has no workspace root"))?;
            (Scope::project(project), root.clone(), vec![root])
        }
        (None, Some(session)) => {
            let paths = host.session_paths(session);
            let cwd = paths.cwd.clone().ok_or_else(|| invalid("session: no such session, or it has no working directory"))?;
            let mut roots = vec![cwd.clone()];
            roots.extend(paths.writable_roots);
            (Scope::session(session), cwd, roots)
        }
        _ => return Err(invalid("give exactly one of project and session")),
    };
    let mut fingerprint = raw;
    if let Some(object) = fingerprint.as_object_mut() {
        object.remove("requestId");
    }
    let digest = crate::approvals::request_digest(&fingerprint);
    match store.replay(OWNER, request_id, &digest).map_err(unavailable_any)? {
        Ok(Some(result)) => return Ok(result),
        Err(()) => return Err(error(409, "idempotency_conflict", "this requestId was used for a different request")),
        Ok(None) => {}
    }
    let request = PublishRequest {
        scope,
        path: body.path,
        base: Some(base.clone()),
        roots,
        name_bases: vec![base],
        name: body.name,
        kind: body.kind,
        note: body.note,
        producer: Producer::owner(),
    };
    let outcome = crate::artifacts::publish::publish(host, &request, now).map_err(|publish_error| match publish_error {
        PublishError::Invalid(message) => invalid(message),
        PublishError::Forbidden(message) => error(403, "artifact_path_forbidden", message),
        PublishError::QuotaFull { used, quota } => quota_error(used, quota),
        PublishError::Store(store_error) => unavailable(format!("{store_error:#}")),
    })?;
    let result = json!({
        "artifact": outcome.artifact,
        "version": outcome.version,
        "newVersion": outcome.new_version,
        "markdown": crate::artifacts::markdown(&outcome.artifact.name, outcome.version.version, &outcome.version.reference),
    });
    if let Err(store_error) = store.remember(OWNER, request_id, &digest, &result, now) {
        eprintln!("artifacts: idempotency record not written: {store_error:#}");
    }
    Ok(result)
}

fn patch(host: &dyn ArtifactHost, store: &ArtifactStore, id: &str, body: &[u8], now: DateTime<Utc>) -> Result<Value, RouteError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct Body {
        expected_revision: u64,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        starred: Option<bool>,
        #[serde(default)]
        archived: Option<bool>,
    }
    let body: Body = parse_body(body)?;
    let (artifact, changes) = store
        .patch(id, body.expected_revision, Patch { name: body.name, kind: body.kind, starred: body.starred, archived: body.archived }, now)
        .map_err(from_store)?;
    for change in changes {
        host.emit(crate::artifacts::EVENT_UPDATED, crate::artifacts::publish::updated_event(&artifact, change));
    }
    Ok(json!({"artifact": artifact}))
}

fn delete(host: &dyn ArtifactHost, store: &ArtifactStore, id: &str) -> Result<Value, RouteError> {
    let artifact = store.delete(id).map_err(from_store)?;
    host.emit(crate::artifacts::EVENT_UPDATED, crate::artifacts::publish::updated_event(&artifact, "deleted"));
    Ok(json!({"deleted": true, "artifactId": artifact.artifact_id}))
}

fn usage(host: &dyn ArtifactHost, store: &ArtifactStore) -> Result<Value, RouteError> {
    let usage = store.usage().map_err(unavailable_any)?;
    let settings = host.settings();
    Ok(json!({
        "bytes": usage.bytes,
        "blobs": usage.blobs,
        "artifacts": usage.artifacts,
        "versions": usage.versions,
        "quotaBytes": settings.quota_bytes,
        "maxBlobBytes": settings.max_blob_bytes,
        "hashExternalMaxBytes": settings.hash_external_max_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::host::tests::TestHost;
    use crate::management::v2::schema_export::assert_valid;

    fn call(host: &TestHost, method: &str, target: &str, body: Value) -> Result<(u16, Value), RouteError> {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let body = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
        route(host, method, path, query, &body, Utc::now())
    }

    fn ok(host: &TestHost, method: &str, target: &str, body: Value) -> Value {
        match call(host, method, target, body) {
            Ok((_, value)) => value,
            Err(route_error) => panic!("{method} {target}: {route_error:?}"),
        }
    }

    fn fails(host: &TestHost, method: &str, target: &str, body: Value) -> RouteError {
        call(host, method, target, body).expect_err(target)
    }

    #[test]
    fn routes_match_the_contract_and_the_schema() {
        let host = TestHost::new("routes");
        let workspace = host.workspace("proj");
        let session_cwd = host.workspace("solo");
        std::fs::create_dir_all(workspace.join("reports")).unwrap();
        std::fs::write(workspace.join("reports/r.md"), "a\nb\nc\n").unwrap();
        std::fs::write(workspace.join("fig.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        std::fs::write(session_cwd.join("notes.txt"), "notes").unwrap();
        host.set_project_root("proj-a", &workspace);
        host.set_session("cutex.solo", &session_cwd);
        host.projects.borrow_mut().insert("cutex.member".into(), "proj-a".into());

        // Owner publish (201), replay, conflict.
        let request = json!({"requestId": "r-1", "project": "proj-a", "path": "reports/r.md", "note": "first"});
        assert_valid("artifacts", "publishRequest", &request);
        let (status, published) = call(&host, "POST", PATH, request.clone()).unwrap();
        assert_eq!(status, 201);
        assert_valid("artifacts", "publishResponse", &published);
        assert_eq!(published["artifact"]["name"], "reports/r.md");
        assert_eq!(published["version"]["producer"]["kind"], "owner");
        assert_eq!(published["newVersion"], true);
        let id = published["artifact"]["artifactId"].as_str().unwrap().to_string();
        assert_eq!(published["markdown"], format!("[r.md v1](cutex:artifact/{id}@1)"));
        assert_eq!(ok(&host, "POST", PATH, request.clone())["version"], published["version"], "replay");
        assert_eq!(fails(&host, "POST", PATH, json!({"requestId": "r-1", "project": "proj-a", "path": "fig.png"})).code, "idempotency_conflict");
        assert_eq!(host.methods(), vec![crate::artifacts::EVENT_PUBLISHED]);
        // Second version and a binary artifact; a session-scope publish.
        std::fs::write(workspace.join("reports/r.md"), "a\nB\nc\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(workspace.join("reports/r.md")).unwrap().set_modified(later).unwrap();
        let v2 = ok(&host, "POST", PATH, json!({"requestId": "r-2", "project": "proj-a", "path": workspace.join("reports/r.md").display().to_string()}));
        assert_eq!(v2["version"]["version"], 2);
        let figure = ok(&host, "POST", PATH, json!({"requestId": "r-3", "project": "proj-a", "path": "fig.png", "kind": "figure"}));
        let figure_id = figure["artifact"]["artifactId"].as_str().unwrap().to_string();
        let solo = ok(&host, "POST", PATH, json!({"requestId": "r-4", "session": "cutex.solo", "path": "notes.txt"}));
        assert_eq!(solo["artifact"]["scope"], json!({"kind": "session", "id": "cutex.solo"}));
        // Publish errors.
        assert_eq!(fails(&host, "POST", PATH, json!({"requestId": "e-1", "path": "x"})).status, 400);
        assert_eq!(fails(&host, "POST", PATH, json!({"requestId": "e-2", "project": "proj-a", "session": "cutex.solo", "path": "x"})).status, 400);
        assert_eq!(fails(&host, "POST", PATH, json!({"requestId": "e-3", "project": "nope", "path": "x"})).status, 400);
        let forbidden = fails(&host, "POST", PATH, json!({"requestId": "e-4", "project": "proj-a", "path": session_cwd.join("notes.txt").display().to_string()}));
        assert_eq!((forbidden.status, forbidden.code), (403, "artifact_path_forbidden"));
        assert_eq!(fails(&host, "POST", PATH, json!({"requestId": "e-5", "project": "proj-a", "path": "reports"})).code, "artifact_path_forbidden");
        assert_eq!(fails(&host, "POST", PATH, json!({"requestId": "e-6", "project": "proj-a", "path": "r.md", "bogus": 1})).status, 400);

        // Lists.
        let all = ok(&host, "GET", PATH, Value::Null);
        assert_valid("artifacts", "listResponse", &all);
        assert_eq!(all["scope"], Value::Null);
        assert_eq!(all["items"].as_array().unwrap().len(), 3);
        let project = ok(&host, "GET", &format!("{PATH}?project=proj-a&limit=1"), Value::Null);
        assert_eq!(project["scope"], json!({"kind": "project", "id": "proj-a"}));
        assert_eq!(project["items"].as_array().unwrap().len(), 1);
        let cursor = project["nextCursor"].as_str().unwrap().to_string();
        let page2 = ok(&host, "GET", &format!("{PATH}?project=proj-a&limit=1&cursor={cursor}"), Value::Null);
        assert_ne!(page2["items"][0]["artifactId"], project["items"][0]["artifactId"]);
        let member = ok(&host, "GET", &format!("{PATH}?forSession=cutex.member"), Value::Null);
        assert_eq!(member["scope"], json!({"kind": "project", "id": "proj-a"}));
        let loner = ok(&host, "GET", &format!("{PATH}?forSession=cutex.solo"), Value::Null);
        assert_eq!(loner["items"][0]["name"], "notes.txt");
        assert_eq!(ok(&host, "GET", &format!("{PATH}?kind=figure"), Value::Null)["items"].as_array().unwrap().len(), 1);
        assert_eq!(ok(&host, "GET", &format!("{PATH}?q=notes"), Value::Null)["items"].as_array().unwrap().len(), 1);
        assert_eq!(fails(&host, "GET", &format!("{PATH}?project=a&session=b"), Value::Null).status, 400);
        assert_eq!(fails(&host, "GET", &format!("{PATH}?limit=0"), Value::Null).status, 400);
        assert_eq!(fails(&host, "GET", &format!("{PATH}?archived=maybe"), Value::Null).status, 400);

        // Detail, content, diff.
        let detail = ok(&host, "GET", &format!("{PATH}/{id}"), Value::Null);
        assert_valid("artifacts", "detailResponse", &detail);
        assert_eq!(detail["versions"].as_array().unwrap().iter().map(|v| v["version"].as_u64().unwrap()).collect::<Vec<_>>(), vec![2, 1]);
        let text = ok(&host, "GET", &format!("{PATH}/{id}/versions/1/content"), Value::Null);
        assert_valid("artifacts", "contentResponse", &text);
        assert_eq!((text["encoding"].as_str(), text["content"].as_str(), text["truncated"].as_bool()), (Some("utf8"), Some("a\nb\nc\n"), Some(false)));
        assert_eq!(text["externalChanged"], Value::Null);
        let window = ok(&host, "GET", &format!("{PATH}/{id}/versions/latest/content?offset=2&maxBytes=2"), Value::Null);
        assert_eq!((window["content"].as_str(), window["truncated"].as_bool(), window["totalBytes"].as_u64()), (Some("B\n"), Some(true), Some(6)));
        let binary = ok(&host, "GET", &format!("{PATH}/{figure_id}/versions/latest/content"), Value::Null);
        assert_eq!(binary["encoding"], "base64");
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(binary["content"].as_str().unwrap()).unwrap(), vec![0x89, b'P', b'N', b'G', 0, 1, 2]);
        assert_eq!(fails(&host, "GET", &format!("{PATH}/{id}/versions/9/content"), Value::Null).status, 404);
        assert_eq!(fails(&host, "GET", &format!("{PATH}/{id}/versions/0/content"), Value::Null).status, 400);
        assert_eq!(fails(&host, "GET", &format!("{PATH}/{id}/versions/1/content?maxBytes=999999999"), Value::Null).status, 400);
        let diff = ok(&host, "GET", &format!("{PATH}/{id}/diff?from=1&to=2"), Value::Null);
        assert_valid("artifacts", "diffResponse", &diff);
        assert!(diff["diff"].as_str().unwrap().contains("-b\n+B\n"), "{diff}");
        assert_eq!(diff["binary"], false);
        assert_eq!(fails(&host, "GET", &format!("{PATH}/{id}/diff?from=1"), Value::Null).status, 400);
        assert_eq!(fails(&host, "GET", &format!("{PATH}/{id}/diff?from=1&to=7"), Value::Null).status, 404);

        // Resolve.
        let resolved = ok(&host, "POST", &format!("{PATH}/resolve"), json!({"refs": [format!("{id}@1"), format!("{id}@latest"), "af-00000000000000000000000000000000@1", "junk"]}));
        assert_valid("artifacts", "resolveResponse", &resolved);
        assert_eq!(resolved["items"][0]["version"], 1);
        assert_eq!(resolved["items"][1]["version"], 2);
        assert_eq!(resolved["items"][1]["latestVersion"], 2);
        assert_eq!(resolved["items"][2]["error"], "artifact_not_found");
        assert_eq!(resolved["items"][3]["ok"], false);
        let too_many: Vec<String> = (0..201).map(|_| "x".to_string()).collect();
        assert_eq!(fails(&host, "POST", &format!("{PATH}/resolve"), json!({"refs": too_many})).status, 400);

        // Patch (CAS), events, delete, usage.
        let revision = detail["artifact"]["revision"].as_u64().unwrap();
        let patched = ok(&host, "PATCH", &format!("{PATH}/{id}"), json!({"expectedRevision": revision, "starred": true, "name": "Regression"}));
        assert_valid("artifacts", "patchResponse", &patched);
        assert_eq!((patched["artifact"]["starred"].as_bool(), patched["artifact"]["name"].as_str()), (Some(true), Some("Regression")));
        let conflict = fails(&host, "PATCH", &format!("{PATH}/{id}"), json!({"expectedRevision": revision, "archived": true}));
        assert_eq!((conflict.status, conflict.code, conflict.details["revision"].as_u64()), (409, "artifact_revision_conflict", Some(revision + 1)));
        let name_conflict = fails(&host, "PATCH", &format!("{PATH}/{id}"), json!({"expectedRevision": revision + 1, "name": "fig.png"}));
        assert_eq!((name_conflict.status, name_conflict.code), (409, "artifact_name_conflict"));
        let archived = ok(&host, "PATCH", &format!("{PATH}/{id}"), json!({"expectedRevision": revision + 1, "archived": true}));
        assert!(archived["artifact"]["archivedAt"].is_string());
        assert_eq!(ok(&host, "GET", &format!("{PATH}?archived=true"), Value::Null)["items"].as_array().unwrap().len(), 1);
        let updates: Vec<String> = host.events.borrow().iter().filter(|(method, _)| method == crate::artifacts::EVENT_UPDATED).map(|(_, params)| params["change"].as_str().unwrap().to_string()).collect();
        assert_eq!(updates, vec!["renamed", "starred", "archived"]);
        let usage = ok(&host, "GET", &format!("{PATH}/usage"), Value::Null);
        assert_valid("artifacts", "usageResponse", &usage);
        assert_eq!((usage["artifacts"].as_u64(), usage["versions"].as_u64()), (Some(3), Some(4)));
        let deleted = ok(&host, "DELETE", &format!("{PATH}/{figure_id}"), Value::Null);
        assert_valid("artifacts", "deleteResponse", &deleted);
        assert_eq!(host.events.borrow().last().unwrap().1["change"], "deleted");
        assert_eq!(fails(&host, "GET", &format!("{PATH}/{figure_id}"), Value::Null).status, 404);
        assert_eq!(fails(&host, "DELETE", &format!("{PATH}/{figure_id}"), Value::Null).status, 404);
        assert_eq!(fails(&host, "GET", &format!("{PATH}/not-an-id"), Value::Null).status, 404);
        assert_eq!(fails(&host, "PUT", &format!("{PATH}/{id}"), Value::Null).status, 405);
        assert_eq!(fails(&host, "DELETE", &format!("{PATH}/usage"), Value::Null).status, 405);
        assert!(is_path(PATH) && is_path(&format!("{PATH}/{id}")) && !is_path("/v2/artifacts/art-1"));
    }

    #[test]
    fn quota_full_is_507_and_notifies_once() {
        let host = TestHost::new("routes-quota");
        let workspace = host.workspace("proj");
        host.set_project_root("p", &workspace);
        host.settings.borrow_mut().quota_bytes = 4;
        std::fs::write(workspace.join("a.md"), "12345").unwrap();
        let full = fails(&host, "POST", PATH, json!({"requestId": "q-1", "project": "p", "path": "a.md"}));
        assert_eq!((full.status, full.code, full.details["quotaBytes"].as_u64()), (507, "artifact_quota_exceeded", Some(4)));
        fails(&host, "POST", PATH, json!({"requestId": "q-2", "project": "p", "path": "a.md"}));
        assert_eq!(host.quota_notices.borrow().len(), 1);
    }
}
