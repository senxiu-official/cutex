//! Offline model check for Codex profiles whose models come from the native
//! list (ChatGPT login and official profiles, see `native_models`): a model
//! the list does not show is refused with `invalid_model` by agent_create /
//! member_replace, by member_runtime online / restart and by the launch
//! review of a new managed agent, unless the caller passes
//! `allow_unlisted: true` (a server model newer than every list here). The
//! list is the shared native home's `models_cache.json`, else the deployed
//! CLI's bundled list, with this profile's model edits applied. When neither
//! can be read the model is allowed and the reply carries a warning.
//!
//! Claude profiles are not checked: their list is a reference catalog
//! filtered by CLI version (with a legacy alias fallback), not what the
//! account accepts. API-key profiles keep their own catalog check
//! (`selected_profile::validate_model`); Kimi and DSH theirs.

use std::cell::{Cell, RefCell};

use serde_json::Value;

use crate::profiles::model::{CliKind, StoredAccount};
use crate::profiles::native_models::NativeModels;

/// The models a profile offers and where that list came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelList {
    /// Every slug, hidden ones included (all are accepted).
    pub slugs: Vec<String>,
    /// The slugs a picker shows (what a refusal lists).
    pub shown: Vec<String>,
    /// Plain-language source, e.g. `models_cache.json (fetched …)`.
    pub source: String,
}

impl ModelList {
    pub fn from_rows(rows: &[Value], source: &Value) -> Self {
        let slug = |row: &Value| row.get("slug").and_then(Value::as_str).map(str::to_string);
        Self {
            slugs: rows.iter().filter_map(slug).collect(),
            shown: rows
                .iter()
                .filter(|row| crate::profiles::native_models::shown_in_picker(row, None))
                .filter_map(slug)
                .collect(),
            source: source_text(source),
        }
    }

    pub fn contains(&self, model: &str) -> bool {
        self.slugs.iter().any(|slug| slug == model)
    }
}

fn source_text(source: &Value) -> String {
    let field = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let version = source.get("native").and_then(|native| field(native, "version"));
    match source.get("kind").and_then(Value::as_str) {
        Some("nativeCache") => {
            let mut details = Vec::new();
            if let Some(fetched) = field(source, "fetchedAt") {
                details.push(format!("fetched {fetched}"));
            }
            if let Some(client) = field(source, "clientVersion") {
                details.push(format!("written by Codex {client}"));
            }
            if details.is_empty() {
                "the shared native models_cache.json".into()
            } else {
                format!("the shared native models_cache.json ({})", details.join(", "))
            }
        }
        Some("nativeBundled") => match version {
            Some(version) => format!("the bundled model list of the deployed Codex CLI ({version})"),
            None => "the bundled model list of the deployed Codex CLI".into(),
        },
        _ => "the profile's native model list".into(),
    }
}

/// The result of checking one model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// The profile is not checked here (API-key, Claude, Kimi, DSH).
    NotChecked,
    Listed,
    Unlisted(ModelList),
    /// No list could be read; the model is allowed with a warning.
    Unreadable(String),
}

/// Whether `account`'s models come from the native list.
pub fn applies(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex && !crate::profiles::catalog_api::discovery_supported(account)
}

/// The native list of `account` with its model edits applied.
pub fn list_for(account: &StoredAccount) -> Result<ModelList, String> {
    let NativeModels { rows, source } = crate::profiles::native_models::upstream_for_profile(account)?;
    let overlay = crate::profiles::catalog_overlay::load(account).map_err(|error| format!("{error:#}"))?;
    Ok(match overlay.filter(|overlay| !overlay.entries.is_empty()) {
        Some(edits) => ModelList::from_rows(&crate::profiles::catalog_overlay::resolve(&rows, &edits).rows, &source),
        None => ModelList::from_rows(&rows, &source),
    })
}

/// Check `model` against `list` (the result of [`list_for`]).
pub fn check_with(list: Result<ModelList, String>, model: &str) -> Check {
    match list {
        Ok(list) if list.contains(model) => Check::Listed,
        Ok(list) => Check::Unlisted(list),
        Err(reason) => Check::Unreadable(reason),
    }
}

/// Check `model` for `account` (reads the list only for profiles it applies to).
pub fn check(account: &StoredAccount, model: &str) -> Check {
    if !applies(account) {
        return Check::NotChecked;
    }
    check_with(list_for(account), model)
}

const SHOWN_MAX: usize = 40;

/// The `invalid_model` refusal text.
pub fn refusal(profile: &str, model: &str, list: &ModelList) -> String {
    let mut shown: Vec<&str> = list.shown.iter().map(String::as_str).take(SHOWN_MAX).collect();
    if list.shown.len() > SHOWN_MAX {
        shown.push("…");
    }
    let allowed = if shown.is_empty() { "(none shown)".to_string() } else { shown.join(", ") };
    format!(
        "Model '{model}' is not on the model list of profile '{profile}' ({}). Allowed models: {allowed}. \
If this is a brand-new server model the list does not show yet, pass allow_unlisted: true; nothing was changed.",
        list.source
    )
}

/// The warning for a model allowed without a check, or by `allow_unlisted`.
pub fn warning(model: &str, check: &Check) -> Option<String> {
    match check {
        Check::Unreadable(reason) => Some(format!(
            "the model list of this profile could not be read ({reason}); model '{model}' was not checked"
        )),
        Check::Unlisted(list) => Some(format!(
            "model '{model}' is not on the profile's model list ({}); allowed by allow_unlisted",
            list.source
        )),
        Check::NotChecked | Check::Listed => None,
    }
}

/// Refusal text of `check` (None: allowed), noting a warning for the reply.
pub fn decide(profile: &str, model: &str, check: &Check, allow_unlisted: bool) -> Result<(), String> {
    if let (Check::Unlisted(list), false) = (check, allow_unlisted) {
        return Err(refusal(profile, model, list));
    }
    if let Some(warning) = warning(model, check) {
        note_warning(warning);
    }
    Ok(())
}

/// A short form of a model (agent_create): `requested`, which is not a slug of
/// the list, names exactly one slug case-insensitively as the whole slug, its
/// tail after a `-` (`6-astra`) or one of its `-` / `/` / `:` separated tokens
/// (`astra` for `gpt-6-astra`); the whole slug wins over the others. Nothing
/// looser is guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alias {
    None,
    One(String),
    Many(Vec<String>),
}

/// Whether `requested` names `slug` as described at [`Alias`].
pub fn names_model(slug: &str, requested: &str) -> bool {
    let (slug, requested) = (slug.to_lowercase(), requested.trim().to_lowercase());
    !requested.is_empty()
        && (slug == requested
            || slug.ends_with(&format!("-{requested}"))
            || slug.split(['-', '/', ':']).any(|token| token == requested))
}

/// The slugs of `list` that `requested` names.
pub fn alias(list: &ModelList, requested: &str) -> Alias {
    if let Some(slug) = list.slugs.iter().find(|slug| slug.eq_ignore_ascii_case(requested.trim())) {
        return Alias::One(slug.clone());
    }
    let mut found: Vec<String> = Vec::new();
    for slug in list.slugs.iter().filter(|slug| names_model(slug, requested)) {
        if !found.contains(slug) {
            found.push(slug.clone());
        }
    }
    match found.len() {
        0 => Alias::None,
        1 => Alias::One(found.remove(0)),
        _ => Alias::Many(found),
    }
}

/// The `invalid_model` refusal text for a short form naming several models.
pub fn ambiguous(profile: &str, model: &str, candidates: &[String]) -> String {
    let mut listed: Vec<&str> = candidates.iter().map(String::as_str).take(SHOWN_MAX).collect();
    if candidates.len() > SHOWN_MAX {
        listed.push("…");
    }
    format!(
        "Model '{model}' is not on the model list of profile '{profile}' and matches several of its models: {}. \
Pass one of them exactly; nothing was changed.",
        listed.join(", ")
    )
}

/// The request resolved the short form `requested` to `resolved` (the reply
/// says so as `model_resolved`).
pub fn note_resolved(requested: &str, resolved: &str) {
    RESOLVED.with(|slot| *slot.borrow_mut() = Some((requested.to_string(), resolved.to_string())));
}

/// member_runtime online / restart: the member's stored model must be on its
/// profile's model list unless `allow_unlisted`. A record or profile that
/// cannot be found is not checked here (the provider answers for it).
pub fn check_runtime_start(cutex_session_id: &str, allow_unlisted: bool) -> Result<(), String> {
    let Ok(sessions) = crate::session::store::load_cutex_session_store_shared() else { return Ok(()) };
    let Some(record) = sessions.sessions.get(cutex_session_id) else { return Ok(()) };
    let (Some(profile), Some(model)) = (record.profile.as_deref(), record.model_defaults.as_deref()) else {
        return Ok(());
    };
    let Ok(store) = crate::profiles::settings_api::load_store() else { return Ok(()) };
    let Some(account) = crate::profiles::settings_api::find(&store, profile) else { return Ok(()) };
    decide(&account.name, model, &check(account, model), allow_unlisted)
}

/// A launch review refusal for an unlisted model (create preflight maps it
/// to `invalid_model`).
#[derive(Debug)]
pub struct UnlistedModel(pub String);

impl std::fmt::Display for UnlistedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnlistedModel {}

thread_local! {
    /// `allow_unlisted` of the agent-management request running on this thread.
    static ALLOW_UNLISTED: Cell<bool> = const { Cell::new(false) };
    /// The warning of the agent-management request running on this thread.
    static WARNING: RefCell<Option<String>> = const { RefCell::new(None) };
    /// The model short form the request resolved: (requested, resolved).
    static RESOLVED: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

/// While alive, the launch review on this thread honours `allow_unlisted`;
/// a new request starts without a warning.
pub struct RequestScope {
    previous: bool,
}

impl RequestScope {
    pub fn enter(allow_unlisted: bool) -> Self {
        WARNING.with(|slot| slot.borrow_mut().take());
        RESOLVED.with(|slot| slot.borrow_mut().take());
        Self { previous: ALLOW_UNLISTED.with(|slot| slot.replace(allow_unlisted)) }
    }
}

impl Drop for RequestScope {
    fn drop(&mut self) {
        ALLOW_UNLISTED.with(|slot| slot.set(self.previous));
    }
}

pub fn unlisted_allowed() -> bool {
    ALLOW_UNLISTED.with(Cell::get)
}

fn note_warning(warning: String) {
    WARNING.with(|slot| *slot.borrow_mut() = Some(warning));
}

/// Adds `model_warning` and `model_resolved` to a completed create / replace /
/// online / restart reply (the stored receipt is unchanged).
pub fn attach_warning(response: &mut Value) {
    let warning = WARNING.with(|slot| slot.borrow_mut().take());
    let resolved = RESOLVED.with(|slot| slot.borrow_mut().take());
    if response["outcome"]["status"] != "complete" {
        return;
    }
    if let Some(result) = response.pointer_mut("/outcome/receipt/result").filter(|result| result.is_object()) {
        if let Some(warning) = warning {
            result["model_warning"] = Value::String(warning);
        }
        if let Some((requested, resolved)) = resolved {
            result["model_resolved"] = serde_json::json!({"requested": requested, "resolved": resolved});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn list() -> ModelList {
        ModelList::from_rows(
            &[json!({"slug": "gpt-6.1-sol"}), json!({"slug": "gpt-6-astra"}), json!({"slug": "gpt-old", "visibility": "hide"})],
            &json!({"kind": "nativeCache", "fetchedAt": "2026-11-03T03:00:00Z", "clientVersion": "0.130.0"}),
        )
    }

    #[test]
    fn listed_and_hidden_models_pass_unlisted_are_refused_with_the_list() {
        assert_eq!(check_with(Ok(list()), "gpt-6.1-sol"), Check::Listed);
        assert_eq!(check_with(Ok(list()), "gpt-old"), Check::Listed, "hidden rows are still accepted");
        let check = check_with(Ok(list()), "gpt-sol-6.1");
        let error = decide("Ada", "gpt-sol-6.1", &check, false).unwrap_err();
        assert!(error.contains("Model 'gpt-sol-6.1' is not on the model list of profile 'Ada'"), "{error}");
        assert!(error.contains("Allowed models: gpt-6.1-sol, gpt-6-astra."), "{error}");
        assert!(!error.contains("gpt-old"), "hidden rows are not offered: {error}");
        assert!(error.contains("models_cache.json (fetched 2026-11-03T03:00:00Z, written by Codex 0.130.0)"), "{error}");
        assert!(error.contains("allow_unlisted: true"), "{error}");
    }

    #[test]
    fn allow_unlisted_and_an_unreadable_list_pass_with_a_warning() {
        let _scope = RequestScope::enter(false);
        let check = check_with(Ok(list()), "gpt-7-new");
        decide("Ada", "gpt-7-new", &check, true).unwrap();
        let mut reply = json!({"outcome": {"status": "complete", "receipt": {"result": {"kind": "created"}}}});
        attach_warning(&mut reply);
        let warning = reply["outcome"]["receipt"]["result"]["model_warning"].as_str().unwrap();
        assert!(warning.contains("allowed by allow_unlisted"), "{warning}");

        let check = check_with(Err("no local runtime deployment is selected".into()), "anything");
        decide("Ada", "anything", &check, false).unwrap();
        let mut reply = json!({"outcome": {"status": "complete", "receipt": {"result": {"kind": "created"}}}});
        attach_warning(&mut reply);
        let warning = reply["outcome"]["receipt"]["result"]["model_warning"].as_str().unwrap();
        assert!(warning.contains("could not be read (no local runtime deployment is selected)"), "{warning}");

        // Listed: no warning; a refused reply never carries one.
        decide("Ada", "gpt-6.1-sol", &Check::Listed, false).unwrap();
        let mut reply = json!({"outcome": {"status": "complete", "receipt": {"result": {"kind": "created"}}}});
        attach_warning(&mut reply);
        assert!(reply["outcome"]["receipt"]["result"].get("model_warning").is_none());
    }

    #[test]
    fn a_unique_short_form_names_one_model_and_nothing_looser_matches() {
        let list = ModelList::from_rows(
            &[json!({"slug": "gpt-6.1-sol"}), json!({"slug": "gpt-6-astra"}), json!({"slug": "gpt-6-astra-mini"}), json!({"slug": "gpt-old", "visibility": "hide"})],
            &json!({"kind": "nativeCache"}),
        );
        assert_eq!(alias(&list, "sol"), Alias::One("gpt-6.1-sol".into()));
        assert_eq!(alias(&list, "6.1-sol"), Alias::One("gpt-6.1-sol".into()), "a tail after '-'");
        assert_eq!(alias(&list, "OLD"), Alias::One("gpt-old".into()), "case-insensitive; hidden rows count");
        assert_eq!(alias(&list, "GPT-6-Astra"), Alias::One("gpt-6-astra".into()), "the whole slug, case-insensitive");
        assert_eq!(alias(&list, "astra"), Alias::Many(vec!["gpt-6-astra".into(), "gpt-6-astra-mini".into()]), "a token of both");
        assert_eq!(alias(&list, "astra-mini"), Alias::One("gpt-6-astra-mini".into()));
        assert_eq!(alias(&list, "astr"), Alias::None, "no prefix / fuzzy match");
        assert_eq!(alias(&list, "6-ast"), Alias::None);
        assert_eq!(alias(&list, " "), Alias::None);
        let refusal = ambiguous("Ada", "astra", &["gpt-6-astra".into(), "gpt-6-astra-mini".into()]);
        assert!(refusal.contains("matches several of its models: gpt-6-astra, gpt-6-astra-mini."), "{refusal}");

        let _scope = RequestScope::enter(false);
        note_resolved("sol", "gpt-6.1-sol");
        let mut refused = json!({"outcome": {"status": "no_write", "code": "x", "detail": "y"}});
        attach_warning(&mut refused);
        assert!(refused["outcome"].get("receipt").is_none());
        note_resolved("sol", "gpt-6.1-sol");
        let mut reply = json!({"outcome": {"status": "complete", "receipt": {"result": {"kind": "created"}}}});
        attach_warning(&mut reply);
        assert_eq!(reply["outcome"]["receipt"]["result"]["model_resolved"], json!({"requested": "sol", "resolved": "gpt-6.1-sol"}));
        assert!(reply["outcome"]["receipt"]["result"].get("model_warning").is_none());
        let _next = RequestScope::enter(false);
        let mut reply = json!({"outcome": {"status": "complete", "receipt": {"result": {"kind": "created"}}}});
        attach_warning(&mut reply);
        assert!(reply["outcome"]["receipt"]["result"].get("model_resolved").is_none(), "a new request starts clean");
    }

    #[test]
    fn the_request_scope_restores_the_previous_allowance() {
        assert!(!unlisted_allowed());
        {
            let _scope = RequestScope::enter(true);
            assert!(unlisted_allowed());
        }
        assert!(!unlisted_allowed());
    }
}
