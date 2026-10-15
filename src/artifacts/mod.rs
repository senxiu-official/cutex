//! Artifact store (cutex-desktop `docs/research/artifacts-api-v1.md`,
//! `research-paradigm-design-20261010.md` §3.2–3.5).
//!
//! Durable, versioned results agents publish for the owner and each other
//! (reports, figures, tables, data, plans, handoffs).
//!
//! - `<cutex home>/runtime/artifacts/artifacts-v1.sqlite3` (WAL): artifacts,
//!   their immutable versions, idempotency records, member turn windows and
//!   the auto capture file state. Shared by the Agent Bus process (MCP tools)
//!   and the management process (Management v2 routes, auto capture).
//! - `blobs/<sha[0..2]>/<sha256>`: content-addressed copies, owner-private.
//!   Files over `artifacts.maxBlobBytes` are registered by path
//!   (`storage: external`) and never copied.
//! - Scope: a project member publishes into its project, any other session
//!   into its own session scope. Same scope + name = next version; the same
//!   sha256 as the latest version adds nothing.
//!
//! New store (no migration): releases without capability `artifacts` ignore the directory.

pub mod capture;
pub mod diff;
pub mod glob;
pub mod host;
pub mod paths;
pub mod publish;
pub mod read;
pub mod show;
pub mod store;
pub mod tools;

use serde::{Deserialize, Serialize};

/// Owner capability name (`GET /v2/capabilities`).
pub const CAPABILITY: &str = "artifacts";
pub const EVENT_PUBLISHED: &str = "cutex/artifact/published";
pub const EVENT_UPDATED: &str = "cutex/artifact/updated";
pub const KINDS: &[&str] = &["report", "figure", "table", "data", "plan", "handoff", "other"];
/// Longest artifact name and note.
pub const MAX_NAME_CHARS: usize = 300;
pub const MAX_NOTE_CHARS: usize = 2000;

pub fn is_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// Whether `id` is an artifact id (`af-<32 lowercase hex>`).
pub fn is_artifact_id(id: &str) -> bool {
    id.strip_prefix("af-")
        .is_some_and(|rest| rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

pub fn new_artifact_id() -> String {
    format!("af-{}", uuid::Uuid::new_v4().simple())
}

/// `af-…@<n>`.
pub fn version_ref(artifact_id: &str, version: u64) -> String {
    format!("{artifact_id}@{version}")
}

/// `cutex:artifact/<ref>`.
pub fn link(reference: &str) -> String {
    format!("cutex:artifact/{reference}")
}

/// `[<name> v<n>](cutex:artifact/<ref>)`; the label is the last path segment
/// of the name, with brackets escaped.
pub fn markdown(name: &str, version: u64, reference: &str) -> String {
    let label = name.rsplit(['/', '\\']).find(|segment| !segment.is_empty()).unwrap_or(name);
    let label: String = label
        .chars()
        .flat_map(|ch| match ch {
            '[' | ']' | '\\' => vec!['\\', ch],
            ch if ch.is_control() => vec![' '],
            ch => vec![ch],
        })
        .collect();
    format!("[{label} v{version}]({})", link(reference))
}

/// The version part of a ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Latest,
    Version(u64),
}

impl Which {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "latest" => Some(Self::Latest),
            value => value.parse::<u64>().ok().filter(|n| *n >= 1).map(Self::Version),
        }
    }
}

/// Parse `af-…@<n>` / `af-…@latest` (also accepts a bare id = latest and a
/// `cutex:artifact/` prefix).
pub fn parse_ref(reference: &str) -> Option<(String, Which)> {
    let reference = reference.trim();
    let reference = reference.strip_prefix("cutex:artifact/").unwrap_or(reference);
    let (id, which) = match reference.split_once('@') {
        Some((id, which)) => (id, Which::parse(which)?),
        None => (reference, Which::Latest),
    };
    is_artifact_id(id).then(|| (id.to_string(), which))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    /// `project` or `session`.
    pub kind: String,
    pub id: String,
}

impl Scope {
    pub fn project(id: impl Into<String>) -> Self {
        Self { kind: "project".into(), id: id.into() }
    }

    pub fn session(id: impl Into<String>) -> Self {
        Self { kind: "session".into(), id: id.into() }
    }

    /// The scope a session publishes to.
    pub fn of_session(session: &str, project: Option<&str>) -> Self {
        match project {
            Some(project) => Self::project(project),
            None => Self::session(session),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub session_id: String,
    pub display_name: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Producer {
    /// `agent`, `owner` or `auto`.
    pub kind: String,
    pub session_id: Option<String>,
    pub display_name: Option<String>,
    pub runtime_generation: Option<u64>,
    pub turn_id: Option<String>,
    pub ask_id: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    /// Auto capture only: `exclusive`, `ambiguous` or `unknown`.
    pub attribution: Option<String>,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
}

impl Producer {
    pub fn owner() -> Self {
        Self { kind: "owner".into(), ..Self::default() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitInfo {
    pub commit: String,
    pub dirty: bool,
}

/// One immutable version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactVersion {
    pub artifact_id: String,
    pub version: u64,
    #[serde(rename = "ref")]
    pub reference: String,
    pub link: String,
    /// `blob` or `external`.
    pub storage: String,
    pub sha256: Option<String>,
    pub bytes: u64,
    pub media_type: String,
    pub source_path: String,
    pub note: Option<String>,
    pub producer: Producer,
    pub git: Option<GitInfo>,
    pub created_at: String,
    /// The source file's mtime when registered (milliseconds; external
    /// change detection). Not on the wire.
    #[serde(skip)]
    pub source_mtime_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSummary {
    pub artifact_id: String,
    pub scope: Scope,
    pub name: String,
    pub kind: String,
    pub latest_version: u64,
    pub revision: u64,
    pub starred: bool,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub latest: ArtifactVersion,
}

/// Whether the name is acceptable (trimmed, 1–[`MAX_NAME_CHARS`], no
/// control characters).
pub fn clean_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && name.chars().count() <= MAX_NAME_CHARS && !name.chars().any(char::is_control)).then(|| name.to_string())
}

/// Trimmed note, `None` when empty; long notes are cut.
pub fn clean_note(note: Option<&str>) -> Option<String> {
    let note = note?.trim();
    (!note.is_empty()).then(|| crate::asks::truncate_chars(note, MAX_NOTE_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_refs_links_and_markdown() {
        let id = new_artifact_id();
        assert!(is_artifact_id(&id));
        assert!(!is_artifact_id("af-XYZ"));
        assert!(!is_artifact_id(&id.to_uppercase()));
        let reference = version_ref(&id, 4);
        assert_eq!(parse_ref(&reference), Some((id.clone(), Which::Version(4))));
        assert_eq!(parse_ref(&format!("{id}@latest")), Some((id.clone(), Which::Latest)));
        assert_eq!(parse_ref(&format!("cutex:artifact/{id}@2")), Some((id.clone(), Which::Version(2))));
        assert_eq!(parse_ref(&id), Some((id.clone(), Which::Latest)));
        assert_eq!(parse_ref(&format!("{id}@0")), None);
        assert_eq!(parse_ref("doc-1@1"), None);
        assert_eq!(link(&reference), format!("cutex:artifact/{id}@4"));
        assert_eq!(markdown("reports/regression.md", 4, &reference), format!("[regression.md v4](cutex:artifact/{id}@4)"));
        assert_eq!(markdown("a [b]", 1, "r"), "[a \\[b\\] v1](cutex:artifact/r)");
        assert_eq!(Scope::of_session("cutex.a", Some("p")), Scope::project("p"));
        assert_eq!(Scope::of_session("cutex.a", None), Scope::session("cutex.a"));
        assert_eq!(clean_name("  x "), Some("x".into()));
        assert_eq!(clean_name(" "), None);
        assert_eq!(clean_note(Some("  ")), None);
    }

    #[test]
    fn producer_serializes_every_key() {
        let value = serde_json::to_value(Producer::owner()).unwrap();
        for key in ["kind", "sessionId", "displayName", "runtimeGeneration", "turnId", "askId", "profile", "model", "attribution", "candidates"] {
            assert!(value.get(key).is_some(), "{key}");
        }
        assert_eq!(value["candidates"], serde_json::json!([]));
    }
}
