//! Owner-private notes, independent of native conversation history and model memory.
//!
//! Scope and anchor identify the immutable source; pins only add findability.
//! Lifecycle hooks mark missing sources without deleting or copying notes.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

mod store;
pub use store::NotesStore;
pub const CHANGED_EVENT: &str = "cutex/note/changed";
const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 64_000;
const MAX_BODY_BYTES: usize = MAX_BODY_CHARS * 4;
const MAX_PINS: usize = 64;
const MAX_QUERY_CHARS: usize = 500;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum NoteScope {
    Session { id: String },
    Project { id: String },
    Host {},
}

impl NoteScope {
    fn columns(&self) -> (&'static str, &str) {
        match self {
            Self::Session { id } => ("session", id),
            Self::Project { id } => ("project", id),
            Self::Host {} => ("host", ""),
        }
    }

    fn validate(&self) -> NotesResult<()> {
        match self {
            Self::Session { id } => validate_identifier(id, "session id"),
            Self::Project { id } => {
                validate_identifier(id, "project id")?;
                crate::agent_management::ProjectId::new(id.clone())
                    .map_err(|_| invalid("invalid project id"))?;
                Ok(())
            }
            Self::Host {} => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoteAnchor {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

impl NoteAnchor {
    fn validate(&self) -> NotesResult<()> {
        validate_identifier(&self.session_id, "anchor.sessionId")?;
        if let Some(id) = &self.thread_id {
            validate_identifier(id, "anchor.threadId")?;
        }
        if let Some(id) = &self.turn_id {
            validate_identifier(id, "anchor.turnId")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub scope: NoteScope,
    pub pins: Vec<NoteScope>,
    pub anchor: Option<NoteAnchor>,
    pub anchor_state: AnchorState,
    pub anchor_state_at: Option<String>,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AnchorState {
    #[default]
    Ok,
    Reverted,
    SessionDeleted,
}
impl AnchorState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Reverted => "reverted",
            Self::SessionDeleted => "sessionDeleted",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteChange {
    pub note_id: String,
    pub change: NoteChangeKind,
    pub revision: u64,
    pub scope: NoteScope,
    pub anchor: Option<NoteAnchor>,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NoteChangeKind {
    Created,
    Updated,
    Deleted,
    Anchor,
}
impl NoteChange {
    fn new(note: &Note, change: NoteChangeKind) -> Self {
        Self {
            note_id: note.id.clone(),
            change,
            revision: note.revision,
            scope: note.scope.clone(),
            anchor: note.anchor.clone(),
        }
    }
}
fn publish_change(change: NoteChange) {
    // Notifications are best effort and follow commit, never part of main history.
    if let Ok(params) = serde_json::to_value(change) {
        crate::management::v2::documents::publish_host_event(CHANGED_EVENT, params);
    }
}

/// Best-effort lifecycle hooks: failure is logged, never prevents the primary action.
pub fn mark_reverted_default(session_id: &str, removed_turn_ids: &[String]) {
    if removed_turn_ids.is_empty() || !NotesStore::default_exists() {
        return;
    }
    if let Err(error) = NotesStore::open_default()
        .and_then(|store| store.mark_reverted(session_id, removed_turn_ids))
    {
        eprintln!("notes: marking reverted anchors failed: {error:?}");
    }
}
pub fn mark_session_deleted_default(session_id: &str) {
    if !NotesStore::default_exists() {
        return;
    }
    if let Err(error) =
        NotesStore::open_default().and_then(|store| store.mark_session_deleted(session_id))
    {
        eprintln!("notes: marking deleted-session anchors failed: {error:?}");
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateNote {
    pub client_nonce: String,
    pub title: String,
    pub body: String,
    pub scope: NoteScope,
    #[serde(default)]
    pub pins: Vec<NoteScope>,
    #[serde(default)]
    pub anchor: Option<NoteAnchor>,
}

impl CreateNote {
    pub(crate) fn validated(mut self) -> NotesResult<Self> {
        if self.client_nonce.is_empty()
            || self.client_nonce.chars().count() > 128
            || self.client_nonce.trim() != self.client_nonce
            || self.client_nonce.chars().any(char::is_control)
        {
            return Err(invalid(
                "clientNonce must be non-empty, trimmed, control-free and at most 128 characters",
            ));
        }
        validate_title(&self.title)?;
        validate_body(&self.body)?;
        self.scope.validate()?;
        self.pins = validate_pins(self.pins)?;
        if let Some(anchor) = &self.anchor {
            anchor.validate()?;
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PatchNote {
    pub expected_revision: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub pins: Option<Vec<NoteScope>>,
}

impl PatchNote {
    pub(crate) fn validated(mut self) -> NotesResult<Self> {
        validate_revision(self.expected_revision)?;
        if self.title.is_none() && self.body.is_none() && self.pins.is_none() {
            return Err(invalid("PATCH requires title, body or pins"));
        }
        if let Some(title) = &self.title {
            validate_title(title)?;
        }
        if let Some(body) = &self.body {
            validate_body(body)?;
        }
        if let Some(pins) = self.pins.take() {
            self.pins = Some(validate_pins(pins)?);
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cursor {
    filter: String,
    updated_at: String,
    id: String,
}

#[derive(Clone, Debug)]
pub struct NotesQuery {
    pub scope: NoteScope,
    pub pinned_only: bool,
    pub query: String,
    pub limit: usize,
    cursor: Option<Cursor>,
}

impl NotesQuery {
    fn fingerprint(&self) -> String {
        // Limit may change on the next page. Scope, search and pin filter may not.
        digest(
            json!([self.scope, self.pinned_only, self.query])
                .to_string()
                .as_bytes(),
        )
    }

    pub fn parse(path: &str) -> NotesResult<Self> {
        let raw = path.split_once('?').map_or("", |(_, query)| query);
        if raw.len() > 8192 {
            return Err(invalid("notes query is too long"));
        }
        let mut fields = BTreeMap::new();
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            if !["scope", "id", "pinnedOnly", "query", "cursor", "limit"].contains(&key.as_ref()) {
                return Err(invalid(format!("unsupported query parameter: {key}")));
            }
            if fields
                .insert(key.into_owned(), value.into_owned())
                .is_some()
            {
                return Err(invalid("query parameters must not be repeated"));
            }
        }
        let id = fields.remove("id");
        let scope = match fields.remove("scope").as_deref().unwrap_or("host") {
            "host" if id.is_none() => NoteScope::Host {},
            "session" => NoteScope::Session {
                id: id.ok_or_else(|| invalid("session scope requires id"))?,
            },
            "project" => NoteScope::Project {
                id: id.ok_or_else(|| invalid("project scope requires id"))?,
            },
            _ => {
                return Err(invalid(
                    "scope must be session, project or host; host has no id",
                ))
            }
        };
        scope.validate()?;
        let pinned_only = match fields.remove("pinnedOnly").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            _ => return Err(invalid("pinnedOnly must be true or false")),
        };
        let query = fields
            .remove("query")
            .unwrap_or_default()
            .trim()
            .to_string();
        if query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control) {
            return Err(invalid(
                "query must be at most 500 characters without control characters",
            ));
        }
        let limit = match fields.remove("limit") {
            None => DEFAULT_LIMIT,
            Some(limit) => limit
                .parse::<usize>()
                .map_err(|_| invalid("limit must be an integer"))?,
        };
        if limit == 0 || limit > MAX_LIMIT {
            return Err(invalid("limit must be between 1 and 200"));
        }
        let cursor = fields
            .remove("cursor")
            .map(|encoded| {
                if encoded.len() > 500 {
                    return Err(invalid("invalid cursor"));
                }
                let bytes = URL_SAFE_NO_PAD
                    .decode(encoded)
                    .map_err(|_| invalid("invalid cursor"))?;
                let cursor: Cursor =
                    serde_json::from_slice(&bytes).map_err(|_| invalid("invalid cursor"))?;
                validate_note_id(&cursor.id)?;
                if chrono::DateTime::parse_from_rfc3339(&cursor.updated_at).is_err() {
                    return Err(invalid("invalid cursor"));
                }
                Ok(cursor)
            })
            .transpose()?;
        let result = Self {
            scope,
            pinned_only,
            query,
            limit,
            cursor,
        };
        if result
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.filter != result.fingerprint())
        {
            return Err(invalid("cursor belongs to a different notes query"));
        }
        Ok(result)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotesPage {
    pub notes: Vec<Note>,
    pub next_cursor: Option<String>,
}

#[derive(Debug)]
pub enum NotesError {
    Invalid(String),
    NotFound,
    RevisionConflict { current_revision: u64 },
    NonceConflict,
    Deleted,
    Storage(anyhow::Error),
}

pub type NotesResult<T> = Result<T, NotesError>;

impl From<rusqlite::Error> for NotesError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

impl From<serde_json::Error> for NotesError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.into())
    }
}

impl From<anyhow::Error> for NotesError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> NotesError {
    NotesError::Invalid(message.into())
}

fn validate_identifier(id: &str, field: &str) -> NotesResult<()> {
    if id.is_empty() || id.len() > 256 || id.trim() != id || id.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{field} must be a non-empty trimmed string of at most 256 bytes"
        )));
    }
    Ok(())
}

pub(crate) fn validate_note_id(id: &str) -> NotesResult<()> {
    if id
        .strip_prefix("note-")
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .is_none()
    {
        return Err(invalid("invalid note id"));
    }
    Ok(())
}

fn validate_title(title: &str) -> NotesResult<()> {
    if title.trim() != title
        || title.chars().count() > MAX_TITLE_CHARS
        || title.chars().any(char::is_control)
    {
        return Err(invalid(
            "title must be trimmed, at most 200 characters and without control characters",
        ));
    }
    Ok(())
}

fn validate_body(body: &str) -> NotesResult<()> {
    if body.trim().is_empty()
        || body.len() > MAX_BODY_BYTES
        || body.chars().count() > MAX_BODY_CHARS
        || body.contains('\0')
    {
        return Err(invalid(
            "body must be non-empty, at most 64000 characters and without NUL bytes",
        ));
    }
    Ok(())
}

pub(crate) fn validate_revision(revision: u64) -> NotesResult<()> {
    if revision == 0 || revision > crate::role_revision::MAX_JSON_SAFE_INTEGER {
        return Err(invalid("expectedRevision must be a positive safe revision"));
    }
    Ok(())
}

fn validate_pins(pins: Vec<NoteScope>) -> NotesResult<Vec<NoteScope>> {
    if pins.len() > MAX_PINS {
        return Err(invalid("at most 64 pins are supported"));
    }
    for scope in &pins {
        scope.validate()?;
    }
    let unique: BTreeSet<_> = pins.iter().cloned().collect();
    if unique.len() != pins.len() {
        return Err(invalid("pins must not contain duplicates"));
    }
    Ok(unique.into_iter().collect())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}
