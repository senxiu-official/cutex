//! Shared scoped formal-name resolution contract.
//!
//! Callers MUST supply only candidates they are authorized to see. Stored
//! action targets and Director role addresses are resolved before this API.
//! The result's session ID is the business target; notes grant no authority.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
pub mod mutation_scope;

pub const FEATURE: &str = "agent-name-resolution-v1";
pub const RENAME_FEATURE: &str = "formal-agent-name-rename-v1";

pub fn scope_schema() -> serde_json::Value {
    serde_json::json!({"oneOf":[
        {"type":"object","properties":{"kind":{"type":"string","enum":["project"]},"projectId":{"type":"string"}},"required":["kind","projectId"],"additionalProperties":false},
        {"type":"object","properties":{"kind":{"type":"string","enum":["unassigned"]}},"required":["kind"],"additionalProperties":false}
    ],"description":"Exact name scope, with no fallback. Local bare names search your project first, then directly linked projects. Scope never grants access."})
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenameRequest {
    pub request_id: String,
    pub expected_revision: u64,
    pub name: String,
}

impl RenameRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_envelope()?;
        validate_new_name(&self.name)
    }

    fn validate_envelope(&self) -> Result<(), &'static str> {
        if self.request_id.is_empty()
            || self.request_id.chars().count() > 256
            || self.request_id.chars().any(char::is_control)
            || self.expected_revision == 0
        {
            return Err("requestId must be 1-256 non-control characters and expectedRevision must be positive");
        }
        Ok(())
    }

    pub fn validate_typed(&self) -> Result<(), NameError> {
        self.validate_envelope().map_err(|message|NameError::new("invalid_request",message))?;
        validate_new_name(&self.name).map_err(|message|NameError::new("agent_name_invalid",message))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameHistoryEvent {
    pub sequence: u64,
    pub event_id: String,
    pub request_id: String,
    pub cutex_session_id: String,
    pub old_name: String,
    pub new_name: String,
    pub scope: NameScope,
    pub committed_at: String,
    /// Taken from the authenticated principal; request bodies cannot set it.
    pub actor: String,
    pub previous_revision: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameHistoryPage {
    pub events: Vec<NameHistoryEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RenameCommit {
    pub event: NameHistoryEvent,
    pub replayed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameError {
    pub code: &'static str,
    pub message: String,
    pub details: serde_json::Value,
}

impl NameError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: serde_json::json!({}),
        }
    }
}
impl std::fmt::Display for NameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for NameError {}

/// Authoritative lookup request for local or negotiated peer resolution.
/// Absent scope means collect scope-local results across the visible host;
/// it never enables host-wide live-name precedence over another scope's alias.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameResolutionRequest {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<NameScope>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum NameScope {
    Project {
        #[serde(rename = "projectId")]
        project_id: String,
    },
    Unassigned,
}

impl NameScope {
    pub fn for_project(project: Option<&str>) -> Self {
        project
            .map(|id| Self::Project {
                project_id: id.into(),
            })
            .unwrap_or(Self::Unassigned)
    }

    pub fn storage_key(&self) -> String {
        match self {
            Self::Project { project_id } => format!("project:{project_id}"),
            Self::Unassigned => "unassigned".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameCandidate {
    pub cutex_session_id: String,
    pub current_name: String,
    pub scope: NameScope,
    pub archived: bool,
    pub retired: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenameNote {
    pub requested_name: String,
    pub current_name: String,
    pub cutex_session_id: String,
    pub scope: NameScope,
}

impl RenameNote {
    pub fn text(&self) -> String {
        format!(
            "{} was renamed by the owner to {}; use {} from now on",
            self.requested_name, self.current_name, self.current_name
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameResolution {
    pub cutex_session_id: String,
    pub current_name: String,
    pub scope: NameScope,
    pub index_revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rename_notes: Vec<RenameNote>,
}

/// Permanent scope claim, pointing directly to the immutable session ID.
/// The displayed old spelling is held in the event log, not required here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameClaim {
    pub scope: NameScope,
    pub normalized_name: String,
    pub cutex_session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ResolutionFailure {
    NotFound,
    Ambiguous { candidates: Vec<NameCandidate> },
    Archived { target: NameCandidate },
    Retired { target: NameCandidate },
}

pub fn normalized_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

pub fn validate_new_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty()
        || name.trim() != name
        || name.chars().count() > 128
        || name.chars().any(char::is_control)
    {
        return Err("name must be trimmed, non-empty, at most 128 characters and contain no control characters");
    }
    if name.eq_ignore_ascii_case("director")
        || name.contains('@')
        || name.get(..6).is_some_and(|prefix|prefix.eq_ignore_ascii_case("cutex."))
        || uuid::Uuid::parse_str(name).is_ok()
    {
        return Err("name conflicts with an ID or a reserved Director/remote address");
    }
    Ok(())
}

/// Resolve an authorized directory snapshot. A missing scope is a host-wide
/// query: collect matches from every scope before deciding, so a new live
/// name in B cannot shadow an old name in A. A selected scope uses live names
/// first, then direct history claims. IDs deliberately ignore name scope.
pub fn resolve(
    query: &str,
    scope: Option<&NameScope>,
    visible: &[NameCandidate],
    claims: &[NameClaim],
    index_revision: u64,
) -> Result<NameResolution, ResolutionFailure> {
    if let Some(exact) = visible
        .iter()
        .find(|candidate| candidate.cutex_session_id == query)
    {
        return finish(query, exact, false, index_revision);
    }
    // An unknown/invisible durable ID must stay an ID-only refusal. A
    // legacy formal name or history claim that looks like it grants no target.
    if query.get(..6).is_some_and(|prefix|prefix.eq_ignore_ascii_case("cutex.")) {
        return Err(ResolutionFailure::NotFound);
    }
    let in_scope = |candidate: &&NameCandidate| scope.is_none_or(|scope| &candidate.scope == scope);
    let current: Vec<_> = visible
        .iter()
        .filter(in_scope)
        .filter(|candidate| {
            !candidate.archived
                && !candidate.retired
                && candidate.current_name.eq_ignore_ascii_case(query)
        })
        .collect();
    if scope.is_some() && !current.is_empty() {
        return choose(
            query,
            current.into_iter().map(|candidate| (candidate, false)),
            index_revision,
        );
    }
    let normalized = normalized_name(query);
    let historical = claims
        .iter()
        .filter(|claim| {
            claim.normalized_name == normalized && scope.is_none_or(|scope| &claim.scope == scope)
        })
        .filter(|claim| !current.iter().any(|live| live.scope == claim.scope))
        .filter_map(|claim| {
            visible
                .iter()
                .find(|candidate| candidate.cutex_session_id == claim.cutex_session_id)
                .map(|candidate| (candidate, true))
        });
    let archived = visible
        .iter()
        .filter(in_scope)
        .filter(|candidate| {
            (candidate.archived || candidate.retired)
                && candidate.current_name.eq_ignore_ascii_case(query)
        })
        .filter(|candidate| !current.iter().any(|live| live.scope == candidate.scope))
        .map(|candidate| (candidate, false));
    choose(
        query,
        current
            .iter()
            .copied()
            .map(|candidate| (candidate, false))
            .chain(historical)
            .chain(archived),
        index_revision,
    )
}

fn choose<'a>(
    query: &str,
    candidates: impl Iterator<Item = (&'a NameCandidate, bool)>,
    revision: u64,
) -> Result<NameResolution, ResolutionFailure> {
    let mut unique = BTreeMap::new();
    for (candidate, alias) in candidates {
        // A current match to the same SID needs no renamed-address note.
        unique
            .entry(&candidate.cutex_session_id)
            .and_modify(|entry: &mut (&NameCandidate, bool)| entry.1 &= alias)
            .or_insert((candidate, alias));
    }
    match unique.len() {
        0 => Err(ResolutionFailure::NotFound),
        1 => {
            let (candidate, alias) = unique.into_values().next().unwrap();
            finish(query, candidate, alias, revision)
        }
        _ => Err(ResolutionFailure::Ambiguous {
            candidates: unique
                .into_values()
                .map(|(candidate, _)| candidate.clone())
                .collect(),
        }),
    }
}

fn finish(
    query: &str,
    candidate: &NameCandidate,
    alias: bool,
    index_revision: u64,
) -> Result<NameResolution, ResolutionFailure> {
    if candidate.retired {
        return Err(ResolutionFailure::Retired {
            target: candidate.clone(),
        });
    }
    if candidate.archived {
        return Err(ResolutionFailure::Archived {
            target: candidate.clone(),
        });
    }
    Ok(NameResolution {
        cutex_session_id: candidate.cutex_session_id.clone(),
        current_name: candidate.current_name.clone(),
        scope: candidate.scope.clone(),
        index_revision,
        rename_notes: if alias {
            vec![RenameNote {
                requested_name: query.into(),
                current_name: candidate.current_name.clone(),
                cutex_session_id: candidate.cutex_session_id.clone(),
                scope: candidate.scope.clone(),
            }]
        } else {
            Vec::new()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn agent(id: &str, name: &str, project: &str) -> NameCandidate {
        NameCandidate {
            cutex_session_id: id.into(),
            current_name: name.into(),
            scope: NameScope::for_project(Some(project)),
            archived: false,
            retired: false,
        }
    }
    fn claim(name: &str, agent: &NameCandidate) -> NameClaim {
        NameClaim {
            scope: agent.scope.clone(),
            normalized_name: normalized_name(name),
            cutex_session_id: agent.cutex_session_id.clone(),
        }
    }
    #[test]
    fn a_b_c_history_points_directly_to_current_identity() {
        let a = agent("cutex.a", "C", "p");
        for name in ["A", "B"] {
            let result = resolve(
                name,
                Some(&a.scope),
                &[a.clone()],
                &[claim("A", &a), claim("B", &a)],
                9,
            )
            .unwrap();
            assert_eq!(result.cutex_session_id, "cutex.a");
            assert_eq!(result.current_name, "C");
            assert_eq!(result.rename_notes[0].current_name, "C");
            assert_eq!(result.index_revision, 9);
        }
    }
    #[test]
    fn another_projects_live_name_cannot_shadow_history() {
        let a = agent("cutex.a", "Reviewer", "A");
        let b = agent("cutex.b", "Worker", "B");
        let visible = [a.clone(), b.clone()];
        let claims = [claim("Worker", &a)];
        assert_eq!(
            resolve("Worker", Some(&a.scope), &visible, &claims, 0)
                .unwrap()
                .cutex_session_id,
            a.cutex_session_id
        );
        assert_eq!(
            resolve("Worker", Some(&b.scope), &visible, &claims, 0)
                .unwrap()
                .cutex_session_id,
            b.cutex_session_id
        );
        assert!(
            matches!(resolve("Worker", None, &visible, &claims, 0), Err(ResolutionFailure::Ambiguous { candidates }) if candidates.len() == 2)
        );
    }
    #[test]
    fn full_id_precedes_any_alias_and_name_scope() {
        let a = agent("cutex.a", "Current", "A");
        let b = agent("cutex.b", "Other", "B");
        let result = resolve(
            "cutex.a",
            Some(&b.scope),
            &[a.clone(), b.clone()],
            &[claim("cutex.a", &b)],
            1,
        )
        .unwrap();
        assert_eq!(result.cutex_session_id, a.cutex_session_id);
        assert!(result.rename_notes.is_empty());
    }
    #[test]
    fn invisible_aliases_leak_neither_names_nor_ambiguity() {
        let a = agent("cutex.visible", "Current", "p");
        let hidden = agent("cutex.hidden", "Secret", "p");
        let result = resolve(
            "Old",
            Some(&a.scope),
            &[a.clone()],
            &[claim("Old", &a), claim("Old", &hidden)],
            0,
        )
        .unwrap();
        assert_eq!(result.cutex_session_id, a.cutex_session_id);
        assert!(matches!(
            resolve("Old", Some(&a.scope), &[], &[claim("Old", &hidden)], 0),
            Err(ResolutionFailure::NotFound)
        ));
    }

    #[test]
    fn unknown_or_invisible_full_ids_never_fall_back_to_legacy_names_or_claims() {
        let mut visible=agent("cutex.visible","Current","p");
        let id="cutex.00000000-0000-0000-0000-000000000001";
        let claims=[claim(id,&visible)];
        assert_eq!(resolve(id,Some(&visible.scope),&[visible.clone()],&claims,0),Err(ResolutionFailure::NotFound));
        visible.current_name=id.into();
        assert_eq!(resolve(id,None,&[visible.clone()],&[],0),Err(ResolutionFailure::NotFound));
        assert_eq!(resolve(&visible.cutex_session_id,None,&[visible.clone()],&claims,0).unwrap().cutex_session_id,visible.cutex_session_id);
    }
    #[test]
    fn scoped_current_name_wins_and_own_history_is_deduplicated() {
        let a = agent("cutex.a", "A", "p");
        let b = agent("cutex.b", "B", "p");
        let result = resolve(
            "a",
            Some(&a.scope),
            &[a.clone(), b.clone()],
            &[claim("A", &a), claim("A", &b)],
            0,
        )
        .unwrap();
        assert_eq!(result.cutex_session_id, a.cutex_session_id);
        assert!(result.rename_notes.is_empty());
        assert!(matches!(
            resolve(
                "OLD",
                Some(&a.scope),
                &[a.clone(), b.clone()],
                &[claim("Old", &a), claim("old", &b)],
                0
            ),
            Err(ResolutionFailure::Ambiguous { .. })
        ));
    }
    #[test]
    fn archived_and_retired_history_never_restores() {
        let mut a = agent("cutex.a", "Current", "p");
        a.archived = true;
        assert!(matches!(
            resolve("Old", Some(&a.scope), &[a.clone()], &[claim("Old", &a)], 0),
            Err(ResolutionFailure::Archived { .. })
        ));
        a.retired = true;
        assert!(matches!(
            resolve("Old", Some(&a.scope), &[a.clone()], &[claim("Old", &a)], 0),
            Err(ResolutionFailure::Retired { .. })
        ));
    }
    #[test]
    fn validation_preserves_spelling_but_rejects_address_syntax() {
        for name in ["Alice", "中文名字", "A name", &"x".repeat(128)] {
            assert!(validate_new_name(name).is_ok());
        }
        for name in [
            "",
            " A",
            "A ",
            "A\nB",
            "director",
            "DIRECTOR",
            "name@host",
            "cutex.id",
            "CuTeX.id",
            "018e0000-0000-7000-8000-0000b0009ae4",
            &"x".repeat(129),
        ] {
            assert!(validate_new_name(name).is_err(), "{name:?}");
        }
    }
}
