//! Displayed plan label of a profile.
//!
//! Order ([`resolve_plan_label`]):
//! 1. the manual label `plan_label` (PATCH `planLabel`): when the owner set
//!    one it overrides everything, the signed-in account's plan included;
//! 2. the live plan of a Codex profile's own ChatGPT login (below; the same
//!    value as the Management login view's `planType`);
//! 3. the legacy / last-login `plan_type` (API-key profiles: the provider
//!    name written at creation). A stored "custom" (the generic custom
//!    preset's provider name, written before that preset stopped writing
//!    one) counts as no label ([`legacy_label`]); a manual label is never
//!    filtered.
//!
//! `planLabelSource` is "manual" for 1 and 3 (stored in the profile) and
//! "account" for 2.
//!
//! Claude profiles (profile-plan-label-claude) have only 1: no ChatGPT login
//! ([`account_plan`] is `None`) and no legacy `plan_type` meaning (cutex
//! writes `plan_type` on Codex profiles only; [`legacy_plan_type`]).
//!
//! Live plan. The login's id-token claim `chatgpt_plan_type` is only as fresh
//! as the token, and Codex refreshes tokens rarely (an access token lives for
//! days), so a plan change can stay hidden for days. Cutex's native
//! rate-limit snapshot (`management::v2::profile_quota_native`,
//! `rateLimits.planType`, read every few minutes for recently used profiles)
//! reports the account's current plan. It wins when it observed the plan
//! strictly after the token was minted: snapshot `planTypeObservedAt` (older
//! snapshots: `fetchedAt`) > id-token `iat` (else auth.json `last_refresh`; a
//! token with neither counts as older than any observation). Comparing with
//! the token's age rather than "now" keeps a fresh re-login (possibly into
//! another account) authoritative until the next native read. Only a profile
//! whose auth.json holds a ChatGPT login has a live plan: a signed-out
//! profile ignores its old snapshot. A native `unknown` plan is no plan.

use serde_json::Value;

use crate::profiles::import::{id_token_claims, parse_auth_metadata};
use crate::profiles::materialize::materialized_account_files;
use crate::profiles::model::{CliKind, StoredAccount};

/// Longest manual plan label accepted from Management clients (characters).
pub const MAX_PLAN_LABEL_CHARS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLabelSource {
    /// The signed-in ChatGPT account's live plan.
    Account,
    /// A stored label: the manual `plan_label`, or the legacy `plan_type`.
    Manual,
}

impl PlanLabelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Manual => "manual",
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// A legacy `plan_type` worth showing: non-empty and not the generic custom
/// preset's provider name "custom" (any case), which names no plan.
fn legacy_label(plan_type: Option<&str>) -> Option<&str> {
    non_empty(plan_type).filter(|label| !label.eq_ignore_ascii_case(crate::profiles::providers::CUSTOM_PRESET_ID))
}

/// A ChatGPT login read from an auth.json.
struct ChatgptLogin {
    /// The id-token claim `chatgpt_plan_type`.
    plan: Option<String>,
    /// When that claim was minted: id-token `iat`, else `last_refresh`.
    minted_at: Option<i64>,
}

fn chatgpt_login(raw: &str) -> Option<ChatgptLogin> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let tokens = parsed.get("tokens").filter(|tokens| tokens.is_object())?;
    let (_, plan) = parse_auth_metadata(raw);
    let iat = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(id_token_claims)
        .and_then(|claims| claims.get("iat").cloned())
        .and_then(|iat| iat.as_i64().or_else(|| iat.as_f64().map(|secs| secs as i64)));
    let last_refresh = parsed
        .get("last_refresh")
        .and_then(Value::as_str)
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.timestamp());
    Some(ChatgptLogin {
        plan: non_empty(plan.as_deref()).map(str::to_string),
        minted_at: iat.or(last_refresh),
    })
}

/// The id-token plan claim of a ChatGPT login in raw auth.json text; `None`
/// for API-key or signed-out auth files and for logins without a plan claim.
pub fn chatgpt_plan_from_auth(raw: &str) -> Option<String> {
    chatgpt_login(raw)?.plan
}

/// The live plan of a ChatGPT login in raw auth.json text, given the native
/// snapshot's observation `(plan, observed_at)`: the observation when it is
/// newer than the token, else the token claim. `None` without a ChatGPT login.
pub fn live_plan_from_auth(raw: &str, observed: Option<(&str, i64)>) -> Option<String> {
    let login = chatgpt_login(raw)?;
    if let Some((plan, observed_at)) = observed {
        if let Some(plan) = non_empty(Some(plan)) {
            if login.minted_at.is_none_or(|minted_at| observed_at > minted_at) {
                return Some(plan.to_string());
            }
        }
    }
    login.plan
}

/// The account plan cutex's native rate-limit snapshot observed for a
/// profile, and when.
pub fn observed_plan(profile_id: &str) -> Option<(String, i64)> {
    use crate::management::v2::profile_quota_native as native;
    native::load_at(&native::store_dir().ok()?, profile_id)?.plan_observation()
}

/// The live plan of a profile's ChatGPT login (raw auth.json text).
pub fn live_plan_for_profile(profile_id: &str, raw_auth: &str) -> Option<String> {
    chatgpt_login(raw_auth)?;
    let observed = observed_plan(profile_id);
    live_plan_from_auth(raw_auth, observed.as_ref().map(|(plan, at)| (plan.as_str(), *at)))
}

/// The legacy / last-login `plan_type` a profile's label falls back to:
/// Codex profiles only (ChatGPT login or API-key preset provider name). A
/// Claude profile's `plan_type` has no meaning and is never shown.
pub fn legacy_plan_type(account: &StoredAccount) -> Option<&str> {
    (account.cli_kind == CliKind::Codex)
        .then_some(account.plan_type.as_deref())
        .flatten()
}

/// The live plan of a Codex profile's own ChatGPT login, if signed in.
/// Always `None` for Claude profiles (no auth file is read).
pub fn account_plan(account: &StoredAccount) -> Option<String> {
    if account.cli_kind != CliKind::Codex {
        return None;
    }
    let files = materialized_account_files(account).ok()?;
    let raw = crate::config::text::read_optional_text(&files.auth_path).ok().flatten()?;
    live_plan_for_profile(&account.id, &raw)
}

/// The stored label clients edit (`planLabelManual`): the manual
/// `plan_label`, else the legacy `plan_type` while there is no live account
/// plan. With a live plan `plan_type` is never shown, so the field reads
/// empty ("use the account's plan").
pub fn stored_plan_label<'a>(
    plan_label: Option<&'a str>,
    plan_type: Option<&'a str>,
    account_plan: Option<&str>,
) -> Option<&'a str> {
    non_empty(plan_label).or_else(|| {
        non_empty(account_plan)
            .is_none()
            .then(|| legacy_label(plan_type))
            .flatten()
    })
}

/// The label to display and where it came from: manual `plan_label` >
/// live account plan > legacy `plan_type`.
pub fn resolve_plan_label(
    account_plan: Option<&str>,
    plan_label: Option<&str>,
    plan_type: Option<&str>,
) -> Option<(String, PlanLabelSource)> {
    if let Some(label) = non_empty(plan_label) {
        return Some((label.to_string(), PlanLabelSource::Manual));
    }
    if let Some(plan) = non_empty(account_plan) {
        return Some((plan.to_string(), PlanLabelSource::Account));
    }
    legacy_label(plan_type).map(|label| (label.to_string(), PlanLabelSource::Manual))
}

/// Effective label of a stored profile (reads its auth file for Codex).
pub fn effective_plan_label(account: &StoredAccount) -> Option<(String, PlanLabelSource)> {
    resolve_plan_label(
        account_plan(account).as_deref(),
        account.plan_label.as_deref(),
        legacy_plan_type(account),
    )
}

/// Validate a manual label: trimmed, 1-40 characters, no control characters.
pub fn normalize_manual_plan_label(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_PLAN_LABEL_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(format!("must be 1-{MAX_PLAN_LABEL_CHARS} visible characters"));
    }
    Ok(value.to_string())
}

/// A fake ChatGPT auth.json carrying `plan` in its id-token claims.
#[cfg(test)]
pub(crate) fn test_chatgpt_auth(plan: &str) -> String {
    test_chatgpt_auth_at(plan, None)
}

/// [`test_chatgpt_auth`] with an id-token `iat` (Unix seconds).
#[cfg(test)]
pub(crate) fn test_chatgpt_auth_at(plan: &str, iat: Option<i64>) -> String {
    use base64::Engine;
    let mut claims = serde_json::json!({
        "email": "plan-test@example.com",
        "https://api.openai.com/auth": { "chatgpt_plan_type": plan },
    });
    if let Some(iat) = iat {
        claims["iat"] = iat.into();
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": { "id_token": format!("e30.{payload}.sig") },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chatgpt_auth(plan: &str) -> String {
        test_chatgpt_auth(plan)
    }

    #[test]
    fn chatgpt_plan_only_from_token_logins() {
        assert_eq!(chatgpt_plan_from_auth(&chatgpt_auth("pro")).as_deref(), Some("pro"));
        assert_eq!(chatgpt_plan_from_auth(r#"{"OPENAI_API_KEY":"k"}"#), None);
        assert_eq!(chatgpt_plan_from_auth(r#"{"tokens":null}"#), None);
        assert_eq!(chatgpt_plan_from_auth("not json"), None);
        assert_eq!(chatgpt_plan_from_auth(&chatgpt_auth(" ")), None);
    }

    #[test]
    fn manual_label_overrides_then_account_then_legacy_plan_type() {
        // The owner's label wins over the signed-in account's plan.
        assert_eq!(
            resolve_plan_label(Some("pro"), Some("free"), Some("plus")),
            Some(("free".into(), PlanLabelSource::Manual))
        );
        // No manual label: the live account plan beats the last-login plan.
        assert_eq!(
            resolve_plan_label(Some("pro"), None, Some("plus")),
            Some(("pro".into(), PlanLabelSource::Account))
        );
        assert_eq!(
            resolve_plan_label(Some("pro"), Some(" "), Some("plus")),
            Some(("pro".into(), PlanLabelSource::Account))
        );
        // No live plan (API key, signed out): stored labels.
        assert_eq!(
            resolve_plan_label(None, Some("GLM Pro"), Some("GLM")),
            Some(("GLM Pro".into(), PlanLabelSource::Manual))
        );
        assert_eq!(
            resolve_plan_label(Some(""), Some(" "), Some("plus")),
            Some(("plus".into(), PlanLabelSource::Manual))
        );
        assert_eq!(resolve_plan_label(None, None, None), None);
    }

    #[test]
    fn stored_label_hides_the_legacy_plan_type_behind_a_live_plan() {
        assert_eq!(stored_plan_label(Some("free"), Some("plus"), Some("pro")), Some("free"));
        assert_eq!(stored_plan_label(None, Some("plus"), Some("pro")), None);
        assert_eq!(stored_plan_label(None, Some("plus"), None), Some("plus"));
        assert_eq!(stored_plan_label(Some(" "), Some("GLM"), Some(" ")), Some("GLM"));
        assert_eq!(stored_plan_label(None, None, None), None);
    }

    #[test]
    fn legacy_custom_plan_type_is_no_label_but_a_manual_custom_stays() {
        for custom in ["custom", " Custom ", "CUSTOM"] {
            assert_eq!(resolve_plan_label(None, None, Some(custom)), None, "{custom}");
            assert_eq!(stored_plan_label(None, Some(custom), None), None, "{custom}");
        }
        assert_eq!(
            resolve_plan_label(None, None, Some("custom relay")),
            Some(("custom relay".into(), PlanLabelSource::Manual))
        );
        // The owner typed it: shown as typed.
        assert_eq!(
            resolve_plan_label(None, Some("custom"), Some("custom")),
            Some(("custom".into(), PlanLabelSource::Manual))
        );
        assert_eq!(stored_plan_label(Some("Custom"), Some("custom"), None), Some("Custom"));
        assert_eq!(
            resolve_plan_label(Some("pro"), None, Some("custom")),
            Some(("pro".into(), PlanLabelSource::Account))
        );
    }

    #[test]
    fn newer_native_observation_beats_the_token_claim() {
        let issued = 1_791_000_000;
        let auth = test_chatgpt_auth_at("pro", Some(issued));
        // No observation, or one from before the token: the token's plan.
        assert_eq!(live_plan_from_auth(&auth, None).as_deref(), Some("pro"));
        assert_eq!(live_plan_from_auth(&auth, Some(("free", issued - 60))).as_deref(), Some("pro"));
        assert_eq!(live_plan_from_auth(&auth, Some(("free", issued))).as_deref(), Some("pro"));
        // Observed after the token was minted: the account's current plan.
        assert_eq!(live_plan_from_auth(&auth, Some(("free", issued + 1))).as_deref(), Some("free"));
        assert_eq!(live_plan_from_auth(&auth, Some((" ", issued + 1))).as_deref(), Some("pro"));
        // No iat: auth.json last_refresh dates the token.
        let mut refreshed: Value = serde_json::from_str(&test_chatgpt_auth("pro")).unwrap();
        refreshed["last_refresh"] = "2026-10-06T00:00:00Z".into();
        let refreshed = refreshed.to_string();
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-06T00:00:00Z").unwrap().timestamp();
        assert_eq!(live_plan_from_auth(&refreshed, Some(("free", at - 1))).as_deref(), Some("pro"));
        assert_eq!(live_plan_from_auth(&refreshed, Some(("free", at + 1))).as_deref(), Some("free"));
        // Undated token: any observation is newer.
        assert_eq!(live_plan_from_auth(&chatgpt_auth("pro"), Some(("free", 1))).as_deref(), Some("free"));
        // Not a ChatGPT login: no live plan, whatever the old snapshot says.
        assert_eq!(live_plan_from_auth(r#"{"OPENAI_API_KEY":"k"}"#, Some(("free", i64::MAX))), None);
        assert_eq!(live_plan_from_auth(r#"{"tokens":null}"#, Some(("free", i64::MAX))), None);
    }

    #[test]
    fn manual_label_validation() {
        assert_eq!(normalize_manual_plan_label(" GLM Pro ").unwrap(), "GLM Pro");
        assert_eq!(normalize_manual_plan_label(&"套".repeat(40)).unwrap().chars().count(), 40);
        assert!(normalize_manual_plan_label(&"a".repeat(41)).is_err());
        assert!(normalize_manual_plan_label("  ").is_err());
        assert!(normalize_manual_plan_label("a\nb").is_err());
    }
}
