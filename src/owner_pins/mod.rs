//! Owner pins (备忘): things an agent pinned for the owner with the
//! `owner_pin` tool — something to decide or do, or a note worth keeping —
//! kept until the agent or the owner resolves them. Never delivered, never
//! counted as needs-you; the owner reads them from Management v2
//! (`/v2/owner-pins`, host event `cutex/ownerPin/changed`).
//!
//! Rules the agent cannot get around (design owner-pins-design-20261110 §4.3):
//! at most [`MAX_OPEN_PER_AGENT`] open pins per agent and [`MAX_OPEN_PER_HOST`]
//! per host, [`MAX_WRITES_PER_HOUR`] creates / updates / resolves per agent
//! per hour, an agent touches only its own pins, and nothing expires: only the
//! agent (its own) or the owner resolves or deletes a pin.

pub mod store;
pub mod tool;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use store::OwnerPinsStore;

pub const CHANGED_EVENT: &str = "cutex/ownerPin/changed";
pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_DETAIL_CHARS: usize = 2000;
pub const MAX_LINKS: usize = 5;
pub const MAX_LINK_TARGET_CHARS: usize = 2000;
pub const MAX_LINK_LABEL_CHARS: usize = 80;
pub const MAX_KEY_CHARS: usize = 64;
pub const MAX_NOTE_CHARS: usize = 500;
pub const MAX_OPEN_PER_AGENT: usize = 20;
pub const MAX_OPEN_PER_HOST: usize = 300;
pub const MAX_WRITES_PER_HOUR: usize = 30;
/// Resolved pins returned by an owner `state=all` list (newest first).
pub const RESOLVED_PAGE: usize = 200;
/// Rows of an agent's own list.
pub const AGENT_LIST_LIMIT: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Decide,
    Todo,
    Note,
}

impl Kind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "decide" => Some(Self::Decide),
            "todo" => Some(Self::Todo),
            "note" => Some(Self::Note),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decide => "decide",
            Self::Todo => "todo",
            Self::Note => "note",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    Open,
    Done,
    Dropped,
}

impl State {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "done" => Some(Self::Done),
            "dropped" => Some(Self::Dropped),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Done => "done",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Link {
    pub target: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerPin {
    pub pin_id: String,
    pub key: Option<String>,
    /// "agent" (owner_pin) or "owner" (reserved for owner-pinned messages).
    pub author: String,
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub kind: Kind,
    pub title: String,
    pub detail: Option<String>,
    pub links: Vec<Link>,
    pub turn_id: Option<String>,
    pub state: State,
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
    pub revision: u64,
}

#[derive(Debug)]
pub enum PinError {
    Invalid(String),
    NotFound,
    NotOpen,
    TooManyOpen(usize),
    HostFull,
    RateLimited,
    IdempotencyConflict,
    RevisionConflict(u64),
    Storage(anyhow::Error),
}

impl From<rusqlite::Error> for PinError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<serde_json::Error> for PinError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<anyhow::Error> for PinError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

pub type PinResult<T> = Result<T, PinError>;

impl PinError {
    /// `(code, message)` for the agent's tool result and the HTTP error body.
    pub fn code(&self) -> (&'static str, String) {
        match self {
            Self::Invalid(message) => ("invalid_arguments", message.clone()),
            Self::NotFound => ("pin_not_found", "no such pin".into()),
            Self::NotOpen => ("pin_not_open", "the pin is already resolved".into()),
            Self::TooManyOpen(n) => ("too_many_open_pins", format!(
                "you already have {n} open pins (limit {MAX_OPEN_PER_AGENT}); resolve one (state done/dropped) or update one with its key first")),
            Self::HostFull => ("pinboard_full", format!("the owner's Memos have {MAX_OPEN_PER_HOST} open pins; it takes no more until some are resolved")),
            Self::RateLimited => ("rate_limited", format!("at most {MAX_WRITES_PER_HOUR} pin changes per hour; try again later")),
            Self::IdempotencyConflict => ("idempotency_conflict", "this action_id was used for a different request; use a new one".into()),
            Self::RevisionConflict(current) => ("pin_revision_conflict", format!("the pin changed (revision {current}); reload first")),
            Self::Storage(error) => ("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300)),
        }
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> PinError {
    PinError::Invalid(message.into())
}

/// Trimmed text of 1..=max characters without control characters (newlines
/// and tabs allowed only when `multiline`).
pub(crate) fn text(value: &str, field: &str, max: usize, multiline: bool) -> PinResult<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        return Err(invalid(format!("{field}: 1-{max} characters")));
    }
    if value.chars().any(|c| c.is_control() && !(multiline && (c == '\n' || c == '\t'))) {
        return Err(invalid(format!("{field}: no control characters")));
    }
    Ok(value.to_string())
}

pub(crate) fn checked_links(links: Vec<Link>) -> PinResult<Vec<Link>> {
    if links.len() > MAX_LINKS {
        return Err(invalid(format!("links: at most {MAX_LINKS}")));
    }
    links
        .into_iter()
        .map(|link| {
            let target = text(&link.target, "links.target", MAX_LINK_TARGET_CHARS, false)?;
            let web = target.starts_with("https://") || target.starts_with("http://");
            let artifact = target.starts_with("cutex:artifact/");
            // Links can name files on a different host, so recognise both path
            // syntaxes without asking this host's filesystem parser.
            let drive = target.as_bytes().get(..3).is_some_and(|prefix| {
                prefix[0].is_ascii_alphabetic()
                    && prefix[1] == b':'
                    && matches!(prefix[2], b'/' | b'\\')
            });
            let unc = target.strip_prefix("\\\\").is_some_and(|path| {
                let mut parts = path.split(['\\', '/']);
                parts.next().is_some_and(|server| !server.is_empty())
                    && parts.next().is_some_and(|share| !share.is_empty())
            });
            let absolute = drive || unc || target.starts_with('/') || target.starts_with('~');
            if !(web || artifact || absolute) {
                return Err(invalid("links.target: a cutex:artifact/… link, an absolute file path or an http(s) URL"));
            }
            let label = link.label.as_deref().map(|label| text(label, "links.label", MAX_LINK_LABEL_CHARS, false)).transpose()?;
            Ok(Link { target, label })
        })
        .collect()
}

/// Best effort, after commit (like notes): the desktop also reloads on resync
/// and polls slowly, so a lost event costs latency, not correctness.
pub(crate) fn publish(params: Value) {
    crate::management::v2::documents::publish_host_event(CHANGED_EVENT, params);
}

pub(crate) fn publish_pin(pin: &OwnerPin) {
    if let Ok(value) = serde_json::to_value(pin) {
        publish(json!({"pin": value}));
    }
}

/// Tests: a fresh directory inside the marked private home (owner-private ACLs need it under the profile).
#[cfg(test)]
pub(crate) fn test_root(name: &str) -> std::path::PathBuf {
    let home = std::env::var_os("CUTEX_TEST_PRIVATE_HOME").map(std::path::PathBuf::from).expect("owner pins tests require CUTEX_TEST_PRIVATE_HOME");
    assert!(home.join(".cutex-test-private-home").is_file());
    home.join(format!("{name}-{}", uuid::Uuid::new_v4().simple()))
}

pub(crate) fn publish_deleted(pin_id: &str) {
    publish(json!({"pinId": pin_id, "deleted": true}));
}
