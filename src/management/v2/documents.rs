//! Owner view of input-overflow documents and the record archive
//! (cutex-desktop `docs/research/input-overflow-documents.md` §6), root
//! credential only, read-only:
//!
//! - `GET /v2/documents?source=<kind>:<id>` → `{documents, usage}`;
//!   without `source`: every live document (newest last).
//! - `GET /v2/documents/usage` → `{usage, quotaBytes, archive}`.
//! - `GET /v2/documents/{doc-id}?cursor=` → `{document, cursor, nextCursor,
//!   bytes, text}` (one page; the owner reads every document).
//! - `GET /v2/record-archive?kind=&month=&involving=&q=&limit=&offset=` →
//!   `{records, hasMore}` (index rows, newest first).
//! - `GET /v2/record-archive/{recordId}` → `{record}` (the archived ask with
//!   answers / questions / texts, or the agent message with its document).

use std::net::TcpStream;

use serde_json::{json, Value};

use crate::documents::archive::{ArchiveFilter, RecordArchive};
use crate::documents::DocumentStore;
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const DOCUMENTS_PATH: &str = "/v2/documents";
pub const ARCHIVE_PATH: &str = "/v2/record-archive";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Documents,
    Usage,
    Document(String),
    Archive,
    ArchiveRecord(String),
}

pub fn parse_path(path: &str) -> Option<Route> {
    match path {
        DOCUMENTS_PATH => return Some(Route::Documents),
        "/v2/documents/usage" => return Some(Route::Usage),
        ARCHIVE_PATH => return Some(Route::Archive),
        _ => {}
    }
    if let Some(id) = path.strip_prefix("/v2/documents/") {
        return crate::documents::is_document_id(id).then(|| Route::Document(id.to_string()));
    }
    let id = path.strip_prefix("/v2/record-archive/")?;
    (!id.is_empty() && id.len() <= 120 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'))
        .then(|| Route::ArchiveRecord(id.to_string()))
}

pub fn is_path(path: &str) -> bool {
    path == DOCUMENTS_PATH || path.starts_with("/v2/documents/") || path == ARCHIVE_PATH || path.starts_with("/v2/record-archive/")
}

/// An error answer: (status, code, message).
#[derive(Debug)]
pub struct RouteError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

fn unavailable(error: anyhow::Error) -> RouteError {
    RouteError { status: 503, code: "documents_unavailable", message: format!("{error:#}") }
}

fn bad_request(message: impl Into<String>) -> RouteError {
    RouteError { status: 400, code: "invalid_request", message: message.into() }
}

/// GET routes as pure JSON (testable without a socket).
pub fn route_json(store: &DocumentStore, archive: &RecordArchive, full_path: &str, route: &Route) -> Result<Value, RouteError> {
    let query = |key: &str| crate::http::query::query_value(full_path, key);
    let number = |key: &str, default: usize| -> Result<usize, RouteError> {
        match query(key) {
            None => Ok(default),
            Some(value) => value.parse().map_err(|_| bad_request(format!("{key}: a non-negative integer"))),
        }
    };
    match route {
        Route::Documents => {
            let documents = match query("source") {
                Some(source) => {
                    let (kind, id) = source.split_once(':').ok_or_else(|| bad_request("source: <kind>:<id>"))?;
                    store.of_source(kind, id).map_err(unavailable)?
                }
                None => store.all().map_err(unavailable)?,
            };
            Ok(json!({"documents": documents, "usage": store.usage().map_err(unavailable)?}))
        }
        Route::Usage => {
            let settings = crate::config::documents::current();
            Ok(json!({
                "usage": store.usage().map_err(unavailable)?,
                "quotaBytes": settings.quota_bytes,
                "archive": archive.usage().map_err(unavailable)?,
            }))
        }
        Route::Document(id) => {
            let (record, bytes) = store
                .read(id, None)
                .map_err(unavailable)?
                .ok_or(RouteError { status: 404, code: "document_not_found", message: "no such document".into() })?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let page = crate::asks::answer::page(&text, number("cursor", 0)?, crate::documents::PAGE_BYTES);
            Ok(json!({
                "document": record,
                "cursor": page.cursor,
                "nextCursor": page.next_cursor,
                "bytes": text.len(),
                "text": page.text,
            }))
        }
        Route::Archive => {
            let filter = ArchiveFilter {
                kind: query("kind"),
                month: query("month"),
                involving: query("involving"),
                query: query("q"),
            };
            let limit = number("limit", DEFAULT_LIMIT)?.clamp(1, MAX_LIMIT);
            let (records, has_more) = archive.list(&filter, limit, number("offset", 0)?).map_err(unavailable)?;
            Ok(json!({"records": records, "hasMore": has_more}))
        }
        Route::ArchiveRecord(id) => {
            let record = archive
                .get(id)
                .map_err(unavailable)?
                .ok_or(RouteError { status: 404, code: "record_not_found", message: "no such archived record".into() })?;
            Ok(json!({"record": record}))
        }
    }
}

/// Publish a host event (retention sweep report).
pub fn publish_host_event(method: &str, params: Value) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        method,
        params,
    ) {
        eprintln!("documents: could not publish {method}: {error:#}");
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some(route) = parse_path(path) else {
        return super::server::write_v2_error(stream, 404, "Not Found", "route_not_found", "no such documents route", false, json!({}));
    };
    if request.method != "GET" {
        return super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "documents and the record archive are read-only", false, json!({}));
    }
    let opened = DocumentStore::open_default().and_then(|store| Ok((store, RecordArchive::open_default()?)));
    let (store, archive) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            return super::server::write_v2_error(stream, 503, "Service Unavailable", "documents_unavailable",
                "the document store is unavailable", true, json!({"diagnostic": format!("{error:#}")}))
        }
    };
    match route_json(&store, &archive, &request.path, &route) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => {
            let reason = match error.status {
                400 => "Bad Request",
                404 => "Not Found",
                _ => "Service Unavailable",
            };
            super::server::write_v2_error(stream, error.status, reason, error.code, &error.message, error.status == 503, json!({}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_parse_and_serve_documents_and_archive() {
        assert_eq!(parse_path("/v2/documents"), Some(Route::Documents));
        assert_eq!(parse_path("/v2/documents/usage"), Some(Route::Usage));
        assert_eq!(parse_path("/v2/documents/doc-x"), None);
        assert_eq!(parse_path("/v2/record-archive"), Some(Route::Archive));
        assert_eq!(parse_path("/v2/record-archive/ask-1"), Some(Route::ArchiveRecord("ask-1".into())));
        assert!(is_path("/v2/record-archive/ask-1") && !is_path("/v2/asks"));
        let (store, dir) = crate::documents::tests::temp_store("mgmt");
        let archive = RecordArchive::at(dir.join("archive"));
        let now = chrono::Utc::now();
        let record = store.put(crate::documents::tests::new_doc(&"y".repeat(10_000), &["cutex.a"]), 1 << 30, now).unwrap();
        let path = format!("/v2/documents/{}", record.document_id);
        let route = parse_path(&path).unwrap();
        let page = route_json(&store, &archive, &path, &route).unwrap();
        assert_eq!(page["bytes"], 10_000);
        assert_eq!(page["nextCursor"], crate::documents::PAGE_BYTES);
        let listed = route_json(&store, &archive, "/v2/documents?source=message:msg-1", &Route::Documents).unwrap();
        assert_eq!(listed["documents"].as_array().unwrap().len(), 1);
        assert!(route_json(&store, &archive, "/v2/documents?source=bad", &Route::Documents).is_err());
        let empty = route_json(&store, &archive, "/v2/record-archive?kind=ask", &Route::Archive).unwrap();
        assert_eq!(empty["records"], json!([]));
        let _ = std::fs::remove_dir_all(dir);
    }
}
