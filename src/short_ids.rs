//! Agent-facing identifiers. Storage, authority, HTTP and digests use full IDs.
#[cfg(test)]
mod render_tests;
use crate::asks::store::AskError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Display {
    #[default]
    Short,
    Full,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub display: Display,
    pub min_len: usize,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            display: Display::Short,
            min_len: 6,
        }
    }
}
impl Settings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
    pub fn minimum(&self) -> usize {
        self.min_len.clamp(6, 64)
    }
}
pub fn settings() -> Settings {
    crate::config::store::load_codez_config().agent_short_ids
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Session,
    /// Stored as `ask-…`; agents see and may write `delegation-…`.
    #[serde(rename = "delegation")]
    Ask,
    Question,
    Document,
    Artifact,
}
impl Kind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Session => "cutex.",
            Self::Ask => "ask-",
            Self::Question => "q-",
            Self::Document => "doc-",
            Self::Artifact => "art-",
        }
    }
    /// The prefix agents see (`delegation-` for asks).
    pub fn display_prefix(self) -> &'static str {
        match self {
            Self::Ask => AGENT_ASK_PREFIX,
            other => other.prefix(),
        }
    }
    pub fn of(id: &str) -> Option<Self> {
        Self::split(id).map(|(kind, _)| kind)
    }
    /// The kind of `id` and the length of its prefix (`ask-` or the agent
    /// spelling `delegation-`).
    fn split(id: &str) -> Option<(Self, usize)> {
        let lower = id.to_ascii_lowercase();
        if lower.starts_with(AGENT_ASK_PREFIX) {
            return Some((Self::Ask, AGENT_ASK_PREFIX.len()));
        }
        [
            Self::Session,
            Self::Ask,
            Self::Question,
            Self::Document,
            Self::Artifact,
        ]
        .into_iter()
        .find(|kind| lower.starts_with(kind.prefix()))
        .map(|kind| (kind, kind.prefix().len()))
    }
}

/// Agent spelling of a delegation (ask) id prefix (prompt revision A8):
/// storage, Management v2 and digests keep `ask-…`.
pub const AGENT_ASK_PREFIX: &str = "delegation-";

/// `id` as stored: an agent-spelled `delegation-…` id as `ask-…`; anything
/// else unchanged (trimmed).
pub fn stored_id(input: &str) -> String {
    let input = input.trim();
    match input.get(..AGENT_ASK_PREFIX.len()) {
        Some(head) if head.eq_ignore_ascii_case(AGENT_ASK_PREFIX) => format!("ask-{}", &input[AGENT_ASK_PREFIX.len()..]),
        _ => input.to_string(),
    }
}

/// `id` as agents see it: a stored `ask-…` id as `delegation-…`; anything
/// else unchanged. Idempotent.
pub fn agent_id(id: &str) -> String {
    match id.get(..4) {
        Some(head) if head.eq_ignore_ascii_case("ask-") => format!("{AGENT_ASK_PREFIX}{}", &id[4..]),
        _ => id.to_string(),
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub name: Option<String>,
}
impl Entry {
    pub fn id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
        }
    }
}
pub struct Query {
    pub kind: Option<Kind>,
    pub hex: String,
    pub suffix_only: bool,
    pub valid_hex: bool,
}
/// ID prefixes of other namespaces (see [`query`]).
const FOREIGN_PREFIXES: &[&str] = &["af-"];

pub fn query(input: &str) -> Query {
    let input = input.trim().to_ascii_lowercase();
    let split = Kind::split(&input);
    let kind = split.map(|(kind, _)| kind);
    let rest = split.map_or(input.as_str(), |(_, len)| &input[len..]);
    let suffix_only = rest.starts_with('…') || rest.starts_with("...");
    let rest = rest
        .strip_prefix('…')
        .or_else(|| rest.strip_prefix("..."))
        .unwrap_or(rest);
    let hex = rest.replace('-', "");
    // Artifact store IDs (`af-…`, resolved by the artifact tools only) are
    // not short IDs: never read their hex as an abbreviation of another
    // object.
    let foreign = kind.is_none() && FOREIGN_PREFIXES.iter().any(|prefix| input.starts_with(prefix));
    let valid_hex = !foreign && !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    Query {
        kind,
        hex,
        suffix_only,
        valid_hex,
    }
}

/// Sorted forward and reverse keys make a prefix or suffix lookup a range
/// query. Callers use a scoped index or `resolve_visible`; permissions are
/// always applied before uniqueness and candidate reporting.
pub struct Index {
    entries: BTreeMap<String, Entry>,
    forward: BTreeMap<(Kind, String), BTreeSet<String>>,
    reverse: BTreeMap<(Kind, String), BTreeSet<String>>,
}
impl Index {
    pub fn new(entries: impl IntoIterator<Item = Entry>) -> Self {
        let mut index = Self {
            entries: BTreeMap::new(),
            forward: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        for entry in entries {
            if let Some(kind) = Kind::of(&entry.id) {
                let q = query(&entry.id);
                if q.valid_hex {
                    index
                        .forward
                        .entry((kind, q.hex.clone()))
                        .or_default()
                        .insert(entry.id.clone());
                    index
                        .reverse
                        .entry((kind, q.hex.chars().rev().collect()))
                        .or_default()
                        .insert(entry.id.clone());
                }
            }
            index.entries.insert(entry.id.clone(), entry);
        }
        index
    }
    fn matches(&self, q: &Query) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        if !q.valid_hex {
            return ids;
        }
        let kinds = [
            Kind::Session,
            Kind::Ask,
            Kind::Question,
            Kind::Document,
            Kind::Artifact,
        ];
        for kind in kinds
            .into_iter()
            .filter(|kind| q.kind.is_none_or(|wanted| wanted == *kind))
        {
            for (tree, needle) in [
                (&self.forward, q.hex.clone()),
                (&self.reverse, q.hex.chars().rev().collect()),
            ]
            .into_iter()
            .skip(usize::from(q.suffix_only))
            {
                for ((candidate_kind, key), values) in tree.range((kind, needle.clone())..) {
                    if *candidate_kind != kind || !key.starts_with(&needle) {
                        break;
                    }
                    ids.extend(values.iter().cloned());
                }
            }
        }
        ids
    }
    pub fn resolve(
        &self,
        input: &str,
        universe: &Index,
        minimum: usize,
    ) -> Result<String, AskError> {
        self.resolve_visible(input, universe, minimum, |_| true)
    }

    pub(crate) fn same_entry(&self, id: &str, name: Option<&str>) -> bool {
        self.entries
            .get(id)
            .is_some_and(|entry| entry.name.as_deref() == name)
    }

    pub(crate) fn resolve_visible(
        &self,
        input: &str,
        universe: &Index,
        minimum: usize,
        visible: impl Fn(&str) -> bool,
    ) -> Result<String, AskError> {
        let input = input.trim();
        let q = query(input);
        // Complete exact IDs have priority; a bare complete normalized ID
        // is accepted too. Names and abbreviated matches otherwise merge.
        let exact: Vec<_> = self
            .entries
            .values()
            .filter(|entry| visible(&entry.id))
            .filter(|entry| {
                entry.id.eq_ignore_ascii_case(input)
                    || (q.valid_hex
                        && !q.suffix_only
                        && query(&entry.id).hex == q.hex
                        && q.kind.is_none_or(|kind| Some(kind) == Kind::of(&entry.id)))
            })
            .map(|entry| entry.id.clone())
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0].clone());
        }
        let mut found: BTreeSet<_> = self
            .entries
            .values()
            .filter(|entry| visible(&entry.id))
            // A name is matched as written: a name that happens to start
            // with an ID prefix (`doc-writer`, `q-bot`) still names its agent.
            .filter(|entry| {
                entry
                    .name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(input))
            })
            .map(|entry| entry.id.clone())
            .collect();
        if q.valid_hex && q.hex.len() >= minimum {
            found.extend(self.matches(&q).into_iter().filter(|id| visible(id)));
        }
        if found.len() == 1 {
            return Ok(found.into_iter().next().unwrap());
        }
        if found.len() > 1 || exact.len() > 1 {
            found.extend(exact);
            return Err(AskError::new("id_ambiguous","identifier matches several accessible objects; use a longer ID")
                .with(json!({"input":input,"candidates":found.iter().take(10).map(|id|json!({"kind":Kind::of(id),"id":id,"short":universe.short(id,minimum),"name":self.entries[id].name})).collect::<Vec<_>>()})));
        }
        Err(if q.valid_hex && q.hex.len() < minimum {
            AskError::new(
                "id_too_short",
                format!("ID needs at least {minimum} hex characters"),
            )
        } else {
            AskError::new(
                "id_not_found",
                "no accessible object matches this ID or name",
            )
        })
    }
    pub fn short(&self, id: &str, minimum: usize) -> String {
        let stored = stored_id(id);
        let id = stored.as_str();
        let q = query(id);
        let Some(kind) = q.kind else { return id.into() };
        if !q.valid_hex || !self.entries.contains_key(id) {
            return agent_id(id);
        }
        for n in minimum..=q.hex.len() {
            let hex = if kind == Kind::Session {
                &q.hex[q.hex.len() - n..]
            } else {
                &q.hex[..n]
            };
            let query = Query {
                kind: Some(kind),
                hex: hex.into(),
                suffix_only: kind == Kind::Session,
                valid_hex: true,
            };
            // For non-session displayed prefixes, both matching directions
            // must be unique because the parser accepts either direction.
            if self.matches(&query).len() == 1 {
                return format!(
                    "{}{}{hex}",
                    kind.display_prefix(),
                    if kind == Kind::Session { "…" } else { "" }
                );
            }
        }
        agent_id(id)
    }
}

/// Host session index keyed by the shared snapshot's identity (and held Arc).
/// An unrelated caller's visibility never changes another caller's display.
pub(crate) fn session_index() -> anyhow::Result<std::sync::Arc<Index>> {
    use std::sync::{Arc, Mutex, OnceLock};
    type Cache = Option<(Arc<crate::session::model::CutexSessionStore>, Arc<Index>)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let snapshot = crate::session::store::load_cutex_session_store_shared()?;
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("short-ID cache poisoned"))?;
    if let Some((old, index)) = &*cache {
        if Arc::ptr_eq(old, &snapshot) {
            return Ok(index.clone());
        }
    }
    let index = Arc::new(Index::new(snapshot.sessions.values().map(|r| {
        Entry {
            id: r.cutex_session_id.clone(),
            name: r
                .formal_agent_name
                .clone()
                .or_else(|| r.display_name_override.clone()),
        }
    })));
    *cache = Some((snapshot, index.clone()));
    Ok(index)
}

pub fn display(id: &str) -> String {
    let settings = settings();
    if settings.display == Display::Full {
        return agent_id(id);
    }
    let index = match Kind::of(id) {
        Some(Kind::Session) => {
            return session_index()
                .map(|index| index.short(id, settings.minimum()))
                .unwrap_or_else(|_| id.into())
        }
        Some(Kind::Document) => {
            return crate::documents::DocumentStore::open_default()
                .and_then(|store| store.short_id(id, settings.minimum()))
                .unwrap_or_else(|_| id.into())
        }
        Some(_) => crate::asks::store::AskStore::open_default()
            .and_then(|store| store.display_snapshot())
            .map(|state| {
                Index::new(
                    state
                        .asks
                        .keys()
                        .chain(state.questions.keys())
                        .chain(state.artifacts.keys())
                        .map(|id| Entry::id(id.clone())),
                )
            }),
        None => return id.into(),
    };
    index
        .map(|index| index.short(id, settings.minimum()))
        .unwrap_or_else(|_| agent_id(id))
}

/// Only whole identifier tokens are substituted in model-facing prose.
/// Never call this on stored envelopes, owner APIs, audit or digest inputs.
pub fn text(input: &str) -> String {
    if settings().display == Display::Full {
        return map_text(input, agent_id);
    }
    map_text(input, display)
}

fn map_text(input: &str, show: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(input.len());
    for token in input.split_inclusive(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.')
    {
        let split =
            token.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.');
        // A punctuation period after the ID is not part of its hex.
        let id = split.trim_end_matches('.');
        if Kind::of(id).is_some() && (Kind::of(id) != Some(Kind::Ask) || query(id).valid_hex) {
            out.push_str(&show(id));
        } else {
            out.push_str(id);
        }
        out.push_str(&token[id.len()..]);
    }
    out
}

pub fn render_value(value: &mut Value, index: &Index, settings: Settings) {
    let show = |id: &str| {
        if settings.display == Display::Full {
            agent_id(id)
        } else {
            index.short(id, settings.minimum())
        }
    };
    match value {
        Value::String(id) if Kind::of(id).is_some() && query(id).valid_hex => *id = show(id),
        Value::Array(values) => {
            for value in values {
                render_value(value, index, settings)
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if matches!(key.as_str(), "detail" | "hint") {
                    if let Some(prose) = value.as_str() {
                        *value = json!(map_text(prose, show));
                    }
                    continue;
                }
                // Candidate lists keep complete IDs, in the agent spelling.
                if key == "candidates" {
                    agent_ids(value);
                    continue;
                }
                // Free-form user/agent content and action/digest identities stay exact.
                if !matches!(
                    key.as_str(),
                    "text"
                        | "message"
                        | "content"
                        | "action_id"
                        | "actionId"
                        | "metadata"
                        | "arguments"
                        | "candidates"
                        | "answer"
                        | "answers"
                        | "preview"
                        | "title"
                        | "name"
                        | "note"
                        | "questions"
                ) {
                    render_value(value, index, settings)
                }
            }
        }
        _ => (),
    }
}

/// Complete `ask-…` IDs anywhere in `value` in the agent spelling (no
/// shortening).
fn agent_ids(value: &mut Value) {
    match value {
        Value::String(id) if Kind::of(id) == Some(Kind::Ask) && query(id).valid_hex => *id = agent_id(id),
        Value::Array(values) => values.iter_mut().for_each(agent_ids),
        Value::Object(values) => values.values_mut().for_each(agent_ids),
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn index() -> Index {
        Index::new([
            Entry {
                id: "cutex.018ab123-0000-7000-8000-000000abc123".into(),
                name: Some("Alpha".into()),
            },
            Entry {
                id: "cutex.018ab123-0000-7000-8000-000000def456".into(),
                name: Some("abc123".into()),
            },
            Entry::id("ask-123456abcdef00000000000000000000"),
            Entry::id("doc-123456abcdef11111111111111111111"),
        ])
    }
    #[test]
    fn artifact_store_ids_are_not_short_ids() {
        assert!(!query("af-0123abcd").valid_hex);
        assert!(!query("AF-0123abcd").valid_hex);
        assert!(query("art-0123abcd").valid_hex);
        assert!(query("0123abcd").valid_hex);
    }

    #[test]
    fn normalization_complete_prefix_suffix_and_names() {
        let i = index();
        let a = "cutex.018ab123-0000-7000-8000-000000abc123";
        for input in [
            a,
            "CUTEX.018AB123000070008000000000ABC123",
            "Alpha",
            "cutex.…abc123",
            "...abc123",
        ] {
            assert_eq!(i.resolve(input, &i, 6).unwrap(), a);
        }
        assert_eq!(
            i.resolve("ask-123456", &i, 6).unwrap(),
            "ask-123456abcdef00000000000000000000"
        );
        assert_eq!(i.resolve("abc12", &i, 6).unwrap_err().code, "id_too_short");
        assert_eq!(i.resolve("abc123", &i, 6).unwrap_err().code, "id_ambiguous");
        assert_eq!(i.resolve("123456", &i, 6).unwrap_err().code, "id_ambiguous");
        assert_eq!(
            i.resolve("cutex.018ab123", &i, 6).unwrap_err().code,
            "id_ambiguous"
        );
        assert_eq!(i.short(a, 6), "cutex.…abc123");
    }
    /// Prompt revision A8: agents see `delegation-…`, may write either
    /// spelling; storage keeps `ask-…`.
    #[test]
    fn delegation_ids_are_shown_and_accepted_in_the_agent_spelling() {
        let i = index();
        let full = "ask-123456abcdef00000000000000000000";
        assert_eq!(agent_id(full), "delegation-123456abcdef00000000000000000000");
        assert_eq!(agent_id(&agent_id(full)), agent_id(full), "idempotent");
        assert_eq!(stored_id(&agent_id(full)), full);
        assert_eq!(stored_id(" Delegation-123456 "), "ask-123456");
        assert_eq!(stored_id("doc-1"), "doc-1");
        for input in [full, "delegation-123456abcdef00000000000000000000", "DELEGATION-123456", "delegation-123456", "ask-123456"] {
            assert_eq!(i.resolve(input, &i, 6).unwrap(), full, "{input}");
        }
        assert_eq!(i.short(full, 6), "delegation-123456");
        assert_eq!(i.short("delegation-123456abcdef00000000000000000000", 6), "delegation-123456");
        assert_eq!(Kind::of("delegation-123456"), Some(Kind::Ask));
        assert_eq!(serde_json::to_value(Kind::Ask).unwrap(), "delegation");
        // Prose: whole IDs only; words that merely start with the prefix stay.
        assert_eq!(map_text("see ask-123456abcdef00000000000000000000, delegation-related", agent_id), "see delegation-123456abcdef00000000000000000000, delegation-related");
        // Ambiguity candidates keep complete IDs, in the agent spelling.
        let error = i.resolve("123456", &i, 6).unwrap_err();
        let mut value = error.to_json();
        render_value(&mut value, &i, Settings::default());
        let text = value.to_string();
        assert!(text.contains("\"delegation-123456abcdef00000000000000000000\"") && text.contains("\"kind\":\"delegation\"") && !text.contains("ask-"), "{text}");
    }
    #[test]
    fn candidates_come_only_from_visible_index() {
        let all = index();
        let visible = Index::new([Entry::id("ask-123456abcdef00000000000000000000")]);
        assert_eq!(
            visible.resolve("123456", &all, 6).unwrap(),
            "ask-123456abcdef00000000000000000000"
        );
        assert_eq!(
            visible.resolve("cutex.…abc123", &all, 6).unwrap_err().code,
            "id_not_found"
        );
    }
    #[test]
    fn full_display_and_semantic_values_stay_identical() {
        let index = index();
        let mut value = json!({"id":"cutex.018ab123-0000-7000-8000-000000abc123","text":"ask-123456abcdef00000000000000000000","answer":"ask-123456abcdef00000000000000000000","action_id":"ask-123456abcdef00000000000000000000","detail":"Read ask-123456abcdef00000000000000000000."});
        let original = value.clone();
        render_value(
            &mut value,
            &index,
            Settings {
                display: Display::Full,
                min_len: 6,
            },
        );
        // Full mode keeps IDs whole, in the agent spelling (`delegation-`).
        let mut respelled = original.clone();
        respelled["detail"] = json!("Read delegation-123456abcdef00000000000000000000.");
        assert_eq!(value, respelled);
        render_value(&mut value, &index, Settings::default());
        assert_eq!(value["id"], "cutex.…abc123");
        assert_eq!(value["text"], original["text"]);
        assert_eq!(value["answer"], original["answer"]);
        assert_eq!(value["detail"], "Read delegation-123456.");
        assert_eq!(value["action_id"], original["action_id"]);
        let config: crate::profiles::model::CodezConfig =
            serde_json::from_value(json!({"agent_short_ids":{"display":"full","min_len":8}}))
                .unwrap();
        assert_eq!(
            config.agent_short_ids,
            Settings {
                display: Display::Full,
                min_len: 8
            }
        );
        assert_eq!(
            serde_json::to_value(config).unwrap()["agent_short_ids"]["display"],
            "full"
        );
    }
}
